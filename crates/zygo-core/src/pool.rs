//! The warm pool — `Pool`, `WarmFn` (design doc §3.2, ADR-008).
//!
//! This is the product. Everything before it exists to get here: a sandbox that
//! is already built when the request arrives, so serving one costs a `fork()`
//! and a cgroup rather than a container.
//!
//! The library is the API and the CLI is a client of it (ADR-008), so a platform
//! embeds this type directly rather than shelling out. `Pool` owns the warm
//! sandboxes; `WarmFn` is one function's handle.
//!
//! ```no_run
//! use zygo_core::pool::{Pool, PoolConfig};
//! use zygo_core::spec::ResolvedFn;
//!
//! # fn example(resize_fn: &ResolvedFn) -> zygo_core::Result<()> {
//! let pool = Pool::new(PoolConfig::new(Default::default()))?;
//! let resize = pool.serve(resize_fn)?;           // ~200-500 ms, once
//! let out = resize.call(serde_json::json!({ "url": "https://example.com" }))?;
//! assert!(out.succeeded());                      // ~2 ms per call after that
//! # Ok(()) }
//! ```
//!
//! ## The request path
//!
//! ```text
//! EXEC ──► agent ──fork()──► child
//!      ◄── FORKED                      the child exists, and is waiting
//!   [create the request cgroup, move the pid into it]
//! GO   ──►                             only now does tenant code run
//!      ◄── DONE
//!   [remove the request cgroup]
//! ```
//!
//! The window between `FORKED` and `GO` is the whole reason the protocol has
//! those two messages: until the child is in its own cgroup, its allocations
//! are billed to the agent and its limits are the agent's.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use crate::error::{Error, IoContext, Result};
use crate::paths::Paths;
use crate::protocol::{ErrorCode, Message, Metrics, PROTOCOL_VERSION, ProtocolError};
use crate::sandbox::SandboxState;
use crate::spec::ResolvedFn;

/// The reference Python agent, carried inside the binary.
///
/// Requirement N6 is a single static binary with no runtime dependencies, so
/// the agent cannot be a file the user is expected to have. It is written into
/// the data directory on first use and bind-mounted into the sandbox.
pub const PYTHON_AGENT: &str = include_str!("../../../agents/python/zygo_agent.py");

/// How a pool is configured.
#[derive(Debug, Clone, Default)]
pub struct PoolConfig {
    pub paths: Paths,
    /// Give each request its own cgroup (design doc open question A2).
    ///
    /// Measured at 97 µs of a 1.9 ms request — 5% — in exchange for
    /// `cgroup.kill`, which tears down a timed-out request's whole tree in one
    /// write. On by default.
    pub per_request_cgroup: bool,
}

impl PoolConfig {
    pub fn new(paths: Paths) -> Self {
        Self {
            paths,
            per_request_cgroup: true,
        }
    }
}

/// What a completed request produced.
///
/// Serialisable because it crosses the supervisor's control socket on its way
/// back to `zygo exec`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Outcome {
    pub exit_code: i32,
    pub result: serde_json::Value,
    pub stdout: String,
    pub stderr: String,
    /// Set when the handler raised; `result` is then meaningless.
    pub error: Option<String>,
    pub metrics: Metrics,
    /// The supervisor killed this request for overrunning its deadline.
    ///
    /// Recorded here because nothing else can tell: a deadline kill and an OOM
    /// kill both arrive as exit 137, and only the side that enforced the
    /// deadline knows which it was. The HTTP API's 408 depends on it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0 && self.error.is_none()
    }
}

/// Entries kept per function, most recent last. Bounded, because a function
/// that logs a line per request would otherwise grow the supervisor without
/// limit.
pub const LOG_ENTRIES: usize = 500;

/// Longest text kept per entry. A request that prints a megabyte gets the
/// first four kilobytes and a marker; the full output went to the caller.
pub const LOG_TEXT_BYTES: usize = 4096;

/// What a log entry is about.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogKind {
    /// A line the zygote itself wrote — the agent's stderr: import-time
    /// output, a crash, a deprecation warning from a dependency.
    Zygote,
    /// One request. `text` is its stdout.
    Request {
        id: String,
        exit_code: i32,
        wall_ms: f64,
        /// The supervisor killed this one for overrunning its deadline.
        ///
        /// Carried into the log because the exit code cannot say it: a
        /// deadline kill and an OOM kill both arrive as 137, and only the
        /// side that enforced the deadline knows which it was. Without this
        /// the log can count kills and not explain any of them, which is the
        /// difference between "your function is too slow" and "your function
        /// needs more memory".
        ///
        /// `default` so a log written by an older supervisor still parses.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        timed_out: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        stderr: String,
    },
}

/// One entry of a function's log.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    /// Monotonic per function, so `zygo logs -f` can ask for "everything
    /// after the last one I saw".
    pub seq: u64,
    /// Unix time, milliseconds.
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: LogKind,
    pub text: String,
}

impl LogEntry {
    /// Whether this entry is a request that failed.
    pub fn failed(&self) -> bool {
        matches!(
            &self.kind,
            LogKind::Request {
                exit_code, error, ..
            } if *exit_code != 0 || error.is_some()
        )
    }
}

/// A function's recent log, shared between the pool (which writes the
/// zygote's lines), the supervisor (which writes the requests) and whoever
/// reads it. Survives the function being replaced or going cold: it belongs
/// to the *name*, not to one sandbox.
#[derive(Debug, Default)]
pub struct LogRing {
    inner: Mutex<(std::collections::VecDeque<LogEntry>, u64)>,
}

/// The shared handle.
pub type Logs = std::sync::Arc<LogRing>;

impl LogRing {
    pub fn push(&self, kind: LogKind, text: impl Into<String>) {
        let mut text: String = text.into();
        if text.len() > LOG_TEXT_BYTES {
            let mut cut = LOG_TEXT_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("… [truncated]");
        }
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut inner = self.inner.lock().expect("log ring");
        let (entries, next) = &mut *inner;
        let seq = *next;
        *next += 1;
        entries.push_back(LogEntry {
            seq,
            at_ms,
            kind,
            text,
        });
        while entries.len() > LOG_ENTRIES {
            entries.pop_front();
        }
    }

    /// Entries with `seq >= after`, oldest first, at most `limit`, and the
    /// sequence number to ask for next time. `limit` counts from the *end*
    /// when `after` is zero, which is what `--tail` means.
    pub fn since(&self, after: u64, limit: usize, failed_only: bool) -> (Vec<LogEntry>, u64) {
        let inner = self.inner.lock().expect("log ring");
        let (entries, next) = &*inner;
        let matching: Vec<&LogEntry> = entries
            .iter()
            .filter(|e| e.seq >= after)
            .filter(|e| !failed_only || e.failed())
            .collect();
        let start = if after == 0 {
            matching.len().saturating_sub(limit)
        } else {
            0
        };
        let taken: Vec<LogEntry> = matching
            .into_iter()
            .skip(start)
            .take(limit)
            .cloned()
            .collect();
        (taken, *next)
    }
}

/// Live state of one warm function, as `zygo ps` shows it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub name: String,
    pub state: SandboxState,
    pub runtime: String,
    /// Resident memory reported at warm-up.
    pub rss_kb: u64,
    /// How long the one-time warm-up took.
    pub imports_ms: f64,
    pub requests: u64,
    pub failures: u64,
}

/// Counters a `WarmFn` keeps for `zygo ps` and `zygo stats`.
#[derive(Debug, Default)]
struct Counters {
    requests: u64,
    failures: u64,
}

/// Where the agent's socket lands in the sandbox's file descriptor table.
///
/// Passing a connected descriptor rather than a socket path means nothing has
/// to exist in the sandbox's filesystem for the agent to reach the supervisor —
/// no bind mount, no path that the image must happen to have, and nothing on
/// disk for a second sandbox to find.
pub const AGENT_FD: std::os::fd::RawFd = 3;

/// Owns the warm sandboxes.
pub struct Pool {
    config: PoolConfig,
    /// Path the embedded agent was written to, once.
    agent_path: PathBuf,
}

impl Pool {
    pub fn new(config: PoolConfig) -> Result<Pool> {
        config.paths.ensure()?;
        let agent_path = Self::install_agent(&config.paths)?;
        Ok(Pool { config, agent_path })
    }

    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// Where the embedded Python agent lives on the host.
    pub fn agent_path(&self) -> &std::path::Path {
        &self.agent_path
    }

    /// Bring a function up warm and wait for its agent to announce itself.
    ///
    /// The image has to be in the store already: pulling is a network
    /// operation, and an embedder running untrusted tenants should decide when
    /// that happens rather than have a `serve()` call reach out.
    pub fn serve(&self, f: &ResolvedFn) -> Result<Function> {
        self.serve_with_logs(f, Logs::default())
    }

    /// [`Pool::serve`], writing the zygote's own output into `logs`.
    ///
    /// The supervisor passes the ring that belongs to the function's name, so
    /// a replacement's import-time lines land in the same log a `zygo logs -f`
    /// is already following.
    pub fn serve_with_logs(&self, f: &ResolvedFn, logs: Logs) -> Result<Function> {
        use crate::image::{Reference, Store};
        use crate::sandbox::{SandboxConfig, mount};

        let store = Store::new(self.config.paths.clone());
        let reference: Reference = f.image.parse()?;
        let entry = store
            .get(&reference)
            .ok_or_else(|| crate::image::ImageError::NotPulled(f.image.clone()))?;

        // A handler that is not there is caught here rather than at resolve
        // time, because resolution is deliberately filesystem-free — it
        // normalises paths without touching them, which is what makes it
        // testable anywhere. This is the first point that genuinely needs the
        // file, and catching it here is the difference between naming the path
        // and failing later inside the launcher: a mount source that does not
        // exist is treated as a directory (what `docker run -v` does), so the
        // sandbox instead failed with ENOTDIR binding a file onto a directory.
        if let Some(entry) = &f.entry
            && !entry.exists()
        {
            return Err(Error::Spec(crate::spec::SpecError::Invalid {
                field: format!("fn.{}.entry", f.name),
                message: format!("{} does not exist", entry.display()),
                remedy: "\n  → check the path; it is resolved against the spec file's \
                         directory, or the working directory when there is no spec"
                    .into(),
            }));
        }

        // System packages before the venv: a wheel that links against `libpq`
        // has to be built on an image that has it.
        if !f.nix.is_empty() {
            return Err(Error::BackendUnavailable {
                backend: "nix",
                reason: "`nix = [...]` layers are not implemented yet".into(),
                remedy: "use `system = [...]` (apt) for now; track phase 3 in todo.md".into(),
            });
        }
        let entry = if f.system.is_empty() {
            entry
        } else {
            crate::derive::ensure(&store, &entry, &f.system)?.image
        };

        // Dependencies first, because they are the slow part and the part
        // most likely to fail: a bad requirements line should be reported
        // before a sandbox is built, not from inside one.
        let venv = match &f.requirements {
            Some(requirements) => {
                if !requirements.exists() {
                    return Err(Error::Spec(crate::spec::SpecError::Invalid {
                        field: format!("fn.{}.requirements", f.name),
                        message: format!("{} does not exist", requirements.display()),
                        remedy: "\n  → check the path; it is resolved against the spec file's \
                                 directory, or the working directory when there is no spec"
                            .into(),
                    }));
                }
                Some(crate::venv::ensure(&store, &entry, requirements)?)
            }
            None => None,
        };
        let image_env: Vec<(String, String)> = match &venv {
            Some(_) => crate::venv::Venv::env(),
            None => Vec::new(),
        };

        // Which of the two warm modes this is (design doc §3.4). A runtime
        // means an agent in the box that forks per request; none means
        // warm-exec — the sandbox is held and each request is a fresh process
        // running `cmd` with the event on stdin.
        let mode = match &f.runtime {
            Some(crate::spec::Runtime::Builtin(crate::spec::BuiltinRuntime::Python)) => Mode::Agent,
            None if !f.cmd.is_empty() => Mode::Exec,
            other => {
                return Err(Error::BackendUnavailable {
                    backend: "pool",
                    reason: format!(
                        "no warm agent for {}",
                        other
                            .as_ref()
                            .map(|r| r.to_string())
                            .unwrap_or_else(|| "a function with neither runtime nor cmd".into())
                    ),
                    remedy: "only the Python agent and warm-exec (`cmd`) are wired up so far \
                             (todo.md, phase 3)"
                        .into(),
                });
            }
        };

        let mut warm = f.clone();
        warm.mounts = match mode {
            Mode::Agent => agent_mounts(&self.agent_path, f),
            Mode::Exec => f.mounts.clone(),
        };
        if let Some(venv) = &venv {
            warm.mounts.push(venv.mount());
        }
        // The network, if this function has one: the allowlist is resolved
        // here, on the host, and `/etc/resolv.conf` comes in as a mount so
        // nothing has to write into the sandbox's filesystem later.
        let net = crate::net::setup(&self.config.paths, &f.name, f)?;
        if let Some(mount) = net.mount.clone() {
            warm.mounts.push(mount);
        }
        for w in &net.warnings {
            tracing::warn!("{w}");
        }
        let argv = match mode {
            Mode::Agent => python_agent_argv(AGENT_IN_SANDBOX, HANDLER_IN_SANDBOX, f.mode.as_str()),
            Mode::Exec => f.cmd.clone(),
        };

        let mount_points = mount::required_mount_points(&warm.mounts);
        let overlay = crate::doctor::run()
            .checks
            .iter()
            .any(|c| c.name == "overlayfs (userns)" && c.status == crate::doctor::Status::Ok);
        let view = store.rootfs_view(&entry.layers, overlay, &mount_points)?;

        let newroot =
            self.config
                .paths
                .tmp()
                .join(format!("warm-{}-{}", f.name, std::process::id()));
        std::fs::create_dir_all(&newroot).at(&newroot)?;

        let mut config = SandboxConfig::from_resolved(&warm, &view, &newroot, argv, &image_env);
        config.allow_resolved = net.allowed;
        config.pasta_pid_file = net.pid_file;
        // Under `strict`, the agent's forked child installs a second filter —
        // no new program, no new process — before the handler runs. Built
        // here because the syscall numbers are the host's, handed over as
        // bytes because the agent may be written in anything.
        #[cfg(target_os = "linux")]
        if mode == Mode::Agent
            && let Some(prog) =
                crate::backend::ns::seccomp::child_program(f.seccomp).map_err(|e| {
                    Error::primitive(
                        "seccomp",
                        "the child filter could not be built for this architecture",
                        std::io::Error::other(e),
                    )
                })?
        {
            use base64::Engine as _;
            let encoded = base64::engine::general_purpose::STANDARD
                .encode(crate::backend::ns::seccomp::encode(&prog));
            config
                .env
                .push((crate::protocol::CHILD_SECCOMP_ENV.to_string(), encoded));
        }
        let tenant_cgroup = crate::cgroup::Hierarchy::discover()
            .ok()
            .map(|h| h.tenant(&f.name));
        let backend = crate::backend::for_isolation(f.isolation)?;

        match mode {
            Mode::Exec => self.serve_exec(f, config, backend.as_ref(), tenant_cgroup),
            Mode::Agent => {
                // A socket pair rather than a listening socket: no path,
                // nothing on the filesystem, and the sandbox cannot reach a
                // second one.
                let (ours, theirs) = std::os::unix::net::UnixStream::pair()
                    .map_err(|e| Error::primitive("socketpair", "internal pool error", e))?;
                config.agent_fd = Some(std::os::fd::AsRawFd::as_raw_fd(&theirs));

                // The zygote's stdout and stderr go to a pipe rather than to
                // the supervisor's own, so an import-time warning or a crash
                // traceback ends up in the function's log where `zygo logs`
                // can show it — and still in the supervisor's log, through
                // tracing, so nothing that was visible before is lost.
                let (log_read, log_write) = rustix::pipe::pipe()
                    .map_err(|e| Error::primitive("pipe", "internal pool error", e.into()))?;
                config.stdio = Some(std::os::fd::AsRawFd::as_raw_fd(&log_write));

                let sandbox = backend.start(&config)?;
                let generation = sandbox.cgroup().map(|p| p.to_path_buf());
                // The sandbox owns its copies now; holding these ends open
                // would stop either stream ever reporting end of file.
                drop(theirs);
                drop(log_write);
                spawn_zygote_log_reader(&f.name, log_read, std::sync::Arc::clone(&logs));

                let reader = crate::protocol::FrameReader::new(
                    ours.try_clone()
                        .map_err(|e| Error::primitive("dup socket", "internal pool error", e))?,
                );
                let mut reader = reader;
                // Bounded, because the launcher runs one warm-up at a time
                // and a wait with no end here stops every later `serve` for
                // ever. An agent that has not announced itself by now is a
                // failed warm-up, not a slow one: its imports happen *after*
                // the venv and the derived layer are already built.
                let _ = ours.set_read_timeout(Some(AGENT_READY_TIMEOUT));
                let ready = read_ready(&mut reader);
                let _ = ours.set_read_timeout(None);
                let (runtime, rss_kb, imports_ms) = ready.map_err(|e| {
                    if is_timeout(&e) {
                        Error::BackendUnavailable {
                            backend: "pool",
                            reason: format!(
                                "the agent did not send READY within {}s",
                                AGENT_READY_TIMEOUT.as_secs()
                            ),
                            remedy: "check `zygo logs <name>` for what the runtime printed; \
                                     an agent must announce itself before it does any work"
                                .into(),
                        }
                    } else {
                        e
                    }
                })?;

                // One thread per function, routing replies to their callers.
                // It ends when the socket does, which is when the sandbox goes
                // away.
                let conn = std::sync::Arc::new(Conn {
                    writer: Mutex::new(crate::protocol::FrameWriter::new(
                        ours.try_clone().map_err(|e| {
                            Error::primitive("dup socket", "internal pool error", e)
                        })?,
                    )),
                    waiting: Mutex::new(BTreeMap::new()),
                    broken: Mutex::new(None),
                });
                let replies = {
                    let conn = std::sync::Arc::clone(&conn);
                    std::thread::Builder::new()
                        .name(format!("zygo-agent-{}", f.name))
                        .spawn(move || Conn::read_replies(&conn, reader))
                        .map_err(|e| Error::primitive("spawn", "agent reader thread", e))?
                };

                Ok(Function::Agent(Box::new(WarmFn {
                    name: f.name.clone(),
                    conn,
                    replies: Mutex::new(Some(replies)),
                    socket: ours,
                    runtime,
                    rss_kb,
                    imports_ms,
                    counters: Mutex::new(Counters::default()),
                    state: Mutex::new(SandboxState::Warm),
                    tenant_cgroup,
                    generation_cgroup: generation,
                    per_request_cgroup: self.config.per_request_cgroup,
                    timeout: f.limits.timeout.get(),
                    agent_host_pid: sandbox.pid(),
                    secrets: Mutex::new(Secrets::default()),
                    _sandbox: sandbox,
                })))
            }
        }
    }

    /// Bring up a warm-exec function: a held sandbox and the plan every
    /// request will be hardened with.
    #[cfg(target_os = "linux")]
    fn serve_exec(
        &self,
        f: &ResolvedFn,
        mut config: crate::sandbox::SandboxConfig,
        backend: &dyn crate::backend::Backend,
        tenant_cgroup: Option<PathBuf>,
    ) -> Result<Function> {
        // The init holds; the requests run. Two plans from one configuration:
        // the held one for the sandbox, and one with `hold` off whose
        // hardening and `execve` candidates every request reuses. Prepared
        // once here, because preparing allocates and a request may not.
        config.hold = true;
        let sandbox = backend.start(&config)?;
        let generation = sandbox.cgroup().map(|p| p.to_path_buf());
        if sandbox.namespaces().is_none() {
            return Err(Error::BackendUnavailable {
                backend: "pool",
                reason: "the backend did not keep the sandbox's namespaces open".into(),
                remedy: "warm-exec needs the `ns` backend".into(),
            });
        }
        config.hold = false;
        let plan = crate::backend::ns::prepare::prepare(&config).map_err(|e| Error::Primitive {
            operation: "prepare warm-exec",
            remedy: "the sandbox configuration contains a path that cannot be passed to the kernel"
                .into(),
            source: std::io::Error::other(e.to_string()),
        })?;

        // Taken now, once: the sandbox handed it out during its launch, and
        // a duplicate is cheaper than reaching through the lock on the
        // sandbox for every request.
        let secrets_dir = sandbox
            .secrets_dir()
            .and_then(|fd| fd.try_clone_to_owned().ok());

        Ok(Function::Exec(Box::new(WarmExec {
            name: f.name.clone(),
            init_pid: sandbox.pid(),
            secrets_dir,
            plan,
            sandbox: Mutex::new(sandbox),
            counters: Mutex::new(Counters::default()),
            state: Mutex::new(SandboxState::Warm),
            tenant_cgroup,
            generation_cgroup: generation,
            per_request_cgroup: self.config.per_request_cgroup,
            timeout: f.limits.timeout.get(),
            secrets: Mutex::new(Secrets::default()),
        })))
    }

    #[cfg(not(target_os = "linux"))]
    fn serve_exec(
        &self,
        _f: &ResolvedFn,
        _config: crate::sandbox::SandboxConfig,
        _backend: &dyn crate::backend::Backend,
        _tenant_cgroup: Option<PathBuf>,
    ) -> Result<Function> {
        Err(Error::BackendUnavailable {
            backend: "pool",
            reason: "warm-exec enters Linux namespaces".into(),
            remedy: "run Zygo inside a Linux VM or container (macOS shim is phase 5)".into(),
        })
    }

    /// Write the embedded agent out, if it is not already there and current.
    ///
    /// Rewritten whenever the contents differ so an upgraded binary does not
    /// keep running the previous release's agent against the current protocol.
    fn install_agent(paths: &Paths) -> Result<PathBuf> {
        let dir = paths.data().join("agents/python");
        std::fs::create_dir_all(&dir).at(&dir)?;
        let path = dir.join("zygo_agent.py");

        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current != PYTHON_AGENT {
            // Written to a temporary file and renamed, so a sandbox starting
            // concurrently never reads a half-written agent.
            let tmp = dir.join(format!("zygo_agent.py.{}", std::process::id()));
            let mut file = std::fs::File::create(&tmp).at(&tmp)?;
            file.write_all(PYTHON_AGENT.as_bytes()).at(&tmp)?;
            file.flush().at(&tmp)?;
            drop(file);
            std::fs::rename(&tmp, &path).at(&path)?;
        }
        Ok(path)
    }
}

/// One warm function: a sandbox with an agent in it, waiting.
pub struct WarmFn {
    name: String,
    /// The supervisor's end of the connection to the agent.
    conn: std::sync::Arc<Conn>,
    /// The thread routing replies. Joined on drop, after the socket is shut
    /// down so it is guaranteed to be on its way out.
    replies: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Kept so the connection can be closed deliberately rather than only when
    /// the last descriptor happens to go.
    socket: std::os::unix::net::UnixStream,
    /// Announced by the agent in its `READY`.
    runtime: String,
    rss_kb: u64,
    imports_ms: f64,
    counters: Mutex<Counters>,
    /// `Warm` until something goes wrong. Behind a lock because a request that
    /// finds the agent wedged has to record that for every later caller.
    state: Mutex<SandboxState>,
    /// The tenant's cgroup: where its limits are enforced and its CPU is
    /// accounted. Shared with the sandbox this one replaced, for as long as
    /// that one is still being torn down.
    tenant_cgroup: Option<PathBuf>,
    /// This sandbox's own generation under the tenant, where per-request
    /// cgroups are created when they are. Retiring the previous generation
    /// kills that one's cgroup, not this one.
    generation_cgroup: Option<PathBuf>,
    per_request_cgroup: bool,
    /// The function's own wall-clock budget, from its resolved spec.
    ///
    /// This is the limit; a caller's timeout only decides how long *it* waits.
    /// Requirement N4 makes the spec's limits mandatory, so a client asking for
    /// longer cannot get it.
    timeout: std::time::Duration,
    /// The agent's pid as the *host* sees it.
    ///
    /// The agent lives in its own pid namespace, so the pid it reports in
    /// `FORKED` is meaningless here; this is the anchor used to translate it.
    /// See [`WarmFn::host_pid_of`]. It is also the way into the sandbox's
    /// filesystem from outside, via `/proc/<pid>/root`, which is how secrets
    /// get in without ever crossing the agent's connection.
    agent_host_pid: u32,
    /// Secret values by name, and how many requests currently need them.
    ///
    /// Behind one lock, because the count decides when the files exist: the
    /// first request in flight writes them, the last one out removes them, and
    /// two requests racing on that boundary must not leave a caller reading a
    /// file the other just deleted.
    secrets: Mutex<Secrets>,
    /// Kept alive: dropping it kills the sandbox.
    _sandbox: Box<dyn crate::backend::Sandbox>,
}

/// The framed connection, with its reader and writer kept together so a request
/// cannot interleave with another one's reply.
/// The connection to one agent, with a thread demultiplexing its replies.
///
/// Requests are not serialised here. The agent can hold several forks at once,
/// so the only thing that has to be exclusive is the moment a frame is written;
/// replies come back out of order and are routed to their caller by request id.
///
/// The shape this replaced held one lock for the whole `EXEC`→`DONE` round.
/// Measured with the CPU quota lifted, that capped a function at ~600
/// requests/s whether one caller or four were asking, using 1.5 of 4 cores —
/// and because a plain mutex is not fair, one of the four waited 3.7 s.
struct Conn {
    writer: Mutex<crate::protocol::FrameWriter<std::os::unix::net::UnixStream>>,
    /// Callers waiting for a reply, by request id.
    waiting: Mutex<BTreeMap<String, std::sync::mpsc::Sender<Message>>>,
    /// Why the connection stopped working, once it has.
    broken: Mutex<Option<String>>,
}

impl Conn {
    /// Route replies to their callers until the agent goes away.
    fn read_replies(
        conn: &std::sync::Arc<Conn>,
        mut reader: crate::protocol::FrameReader<std::os::unix::net::UnixStream>,
    ) {
        loop {
            let message = match reader.read() {
                Ok(Some(message)) => message,
                Ok(None) => return conn.fail("the agent closed the connection"),
                Err(e) => return conn.fail(&format!("the agent connection failed: {e}")),
            };
            let Some(id) = message.request_id().map(str::to_string) else {
                // `PONG` and anything else without an id belongs to no caller.
                continue;
            };
            let waiting = conn.waiting.lock().expect("waiting").get(&id).cloned();
            if let Some(caller) = waiting {
                // A caller that has given up and gone is not an error: its
                // deadline expired and it has already said so.
                let _ = caller.send(message);
            }
        }
    }

    /// Record why the connection died and wake everyone waiting on it.
    ///
    /// Dropping the senders is what does the waking: every `recv` returns an
    /// error rather than waiting out a deadline for a reply that is never
    /// coming.
    fn fail(&self, reason: &str) {
        *self.broken.lock().expect("broken") = Some(reason.to_string());
        self.waiting.lock().expect("waiting").clear();
    }

    fn failure(&self) -> Option<String> {
        self.broken.lock().expect("broken").clone()
    }

    /// Register a caller and write its request, as one step.
    ///
    /// Registering first matters: the agent can answer before `write` has even
    /// returned, and a reply that arrives with nobody waiting is dropped.
    fn send(&self, id: &str, message: &Message) -> Result<Reply<'_>> {
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut waiting = self.waiting.lock().expect("waiting");
            if let Some(reason) = self.failure() {
                return Err(Error::BackendUnavailable {
                    backend: "pool",
                    reason,
                    remedy: "the function will be rewarmed".into(),
                });
            }
            waiting.insert(id.to_string(), tx);
        }
        let reply = Reply {
            conn: self,
            id: id.to_string(),
            rx,
        };
        self.write(message)?;
        Ok(reply)
    }

    /// Write one frame. Exclusive only for as long as the frame takes.
    fn write(&self, message: &Message) -> Result<()> {
        self.writer
            .lock()
            .expect("writer")
            .write(message)
            .map_err(|e| Error::from(ProtocolError::from(e)))
    }
}

/// A caller's place in the reply queue, removed when it goes out of scope.
struct Reply<'a> {
    conn: &'a Conn,
    id: String,
    rx: std::sync::mpsc::Receiver<Message>,
}

impl Reply<'_> {
    /// Wait for the next reply to this request.
    fn next(&self, timeout: std::time::Duration) -> std::result::Result<Message, ReplyError> {
        match self.rx.recv_timeout(timeout) {
            Ok(message) => Ok(message),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(ReplyError::TimedOut),
            // The sender was dropped, which only happens when the reader
            // thread cleared the table because the connection died.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(ReplyError::Gone(self.conn.failure().unwrap_or_else(|| {
                    "the agent connection was closed".into()
                })))
            }
        }
    }
}

impl Drop for Reply<'_> {
    fn drop(&mut self) {
        self.conn.waiting.lock().expect("waiting").remove(&self.id);
    }
}

enum ReplyError {
    TimedOut,
    Gone(String),
}

impl WarmFn {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Host pid of the sandbox's own first process — the agent.
    ///
    /// What `zygo shell` enters through: `/proc/<pid>/ns/*` names every
    /// namespace this sandbox is made of. Host-side, because the caller is on
    /// the host and a namespace-local pid would mean nothing to it.
    pub fn init_pid(&self) -> u32 {
        self.agent_host_pid
    }

    pub fn status(&self) -> Status {
        let counters = self.counters.lock().expect("counters");
        Status {
            name: self.name.clone(),
            state: self.state(),
            runtime: self.runtime.clone(),
            rss_kb: self.rss_kb,
            imports_ms: self.imports_ms,
            requests: counters.requests,
            failures: counters.failures,
        }
    }

    /// Serve one request.
    ///
    /// Blocks until the handler finishes or its deadline passes, but does not
    /// block anyone else: several callers may be in flight on one `WarmFn` at
    /// once. The agent holds a fork per request and replies are routed back by
    /// id, so what bounds concurrency is the tenant's own `concurrency` and
    /// `pids.max`, not this connection.
    pub fn call(&self, event: serde_json::Value) -> Result<Outcome> {
        self.call_with_timeout(event, self.default_timeout())
    }

    /// The function's own budget, which is also the ceiling for any caller's.
    pub fn timeout(&self) -> std::time::Duration {
        self.timeout
    }

    fn default_timeout(&self) -> std::time::Duration {
        self.timeout
    }

    /// Serve one request, giving up after `timeout` or the function's own
    /// budget, whichever is shorter.
    pub fn call_with_timeout(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<Outcome> {
        self.call_timed(event, timeout).map(|(outcome, _)| outcome)
    }

    /// Serve one request and report where its time went.
    ///
    /// The three phases are the ones the design document asks for in its
    /// observability section: fork, cgroup setup, and the handler itself.
    /// Having them per request is also the only way to localise a tail — three
    /// plausible explanations for one were wrong before these existed.
    pub fn call_timed(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<(Outcome, CallTiming)> {
        let id = next_request_id();
        let started = Instant::now();

        // Registering and writing are one step, and nothing is held afterwards:
        // the agent can be serving several requests at once, so the only
        // exclusive moment is the write itself.
        let reply = match self.conn.send(
            &id,
            &Message::Exec {
                id: id.clone(),
                event,
                timeout_ms: timeout.as_millis() as u64,
                env_overrides: BTreeMap::new(),
            },
        ) {
            Ok(reply) => reply,
            Err(e) => return self.broken(e),
        };
        let locked = Instant::now();

        // The agent forks and reports the child's pid, then waits.
        let pid = match reply.next(FORK_TIMEOUT) {
            Ok(Message::Forked { pid, .. }) => pid,
            // The agent refusing a request is not a broken agent: it answered,
            // and the connection is still in step for the next one.
            Ok(Message::Error { code, message, .. }) => {
                self.record(false);
                return Err(ProtocolError::Agent { code, message }.into());
            }
            Ok(other) => {
                return self.broken(ProtocolError::Unexpected {
                    expected: "FORKED",
                    found: other.kind(),
                });
            }
            Err(ReplyError::Gone(reason)) => return self.broken(agent_gone(&self.name, &reason)),
            Err(ReplyError::TimedOut) => {
                return self.broken(Error::BackendUnavailable {
                    backend: "pool",
                    reason: format!("`{}` did not fork within {FORK_TIMEOUT:?}", self.name),
                    remedy: "the function will be rewarmed".into(),
                });
            }
        };
        let forked = Instant::now();

        // The window the handshake exists for: the child is alive but has run
        // no tenant code, so this is the only moment its limits can be set —
        // and the only moment its secrets can be put where it will find them.
        let request_cgroup = self.admit(&id, pid);
        let _secrets = match self.place_secrets() {
            Ok(lease) => lease,
            Err(e) => {
                // The child is waiting for `GO` that will never come; kill it
                // rather than leave it parked in the agent's fork table.
                self.enforce_deadline(request_cgroup.as_deref(), pid);
                self.record(false);
                return Err(e);
            }
        };
        let admitted = Instant::now();

        if let Err(e) = self.conn.write(&Message::Go { id: id.clone() }) {
            return self.broken(e);
        }

        // From here tenant code is running, so the deadline is ours to enforce.
        // Passing `timeout_ms` to the agent is a courtesy, not a control: the
        // agent's own children are the thing being limited, so trusting it to
        // stop them is trusting the blast radius to contain itself.
        //
        // The function's own limit wins over the caller's: asking for longer
        // than the spec allows is asking past a mandatory limit (N4).
        let budget = timeout.min(self.timeout);
        let deadline = budget.saturating_sub(admitted - started);

        let mut timed_out = false;
        let done_reply = match reply.next(deadline) {
            Ok(message) => message,
            Err(ReplyError::Gone(reason)) => return self.broken(agent_gone(&self.name, &reason)),
            Err(ReplyError::TimedOut) => {
                timed_out = true;
                self.enforce_deadline(request_cgroup.as_deref(), pid);
                // The child is dead, so the agent sees end of file on the
                // result pipe and sends `DONE` by itself. Waiting for it is
                // what keeps this request's reply from arriving later with
                // nobody expecting it.
                match reply.next(KILL_GRACE) {
                    Ok(message) => message,
                    Err(_) => {
                        return self.broken(Error::BackendUnavailable {
                            backend: "pool",
                            reason: format!(
                                "`{}` did not answer after its request was killed; \
                                 the agent is not responding",
                                self.name
                            ),
                            remedy: "the function will be rewarmed; check the handler \
                                     for something that blocks uninterruptibly"
                                .into(),
                        });
                    }
                }
            }
        };

        // A killed request's `DONE` describes a request that *finished*: the
        // agent's child died, the agent noticed end of file and answered, and
        // its `wall_ms` is zero for something that ran for its whole
        // deadline. Recording that makes every timeout the fastest request
        // there was, and drags the percentiles in `zygo stats` down with it —
        // the slowest counted as the quickest. The supervisor holds the only
        // clock that saw the whole of it.
        let measured = started.elapsed();
        let outcome = match done_reply {
            Message::Done {
                exit_code,
                result,
                stdout,
                stderr,
                error,
                metrics,
                ..
            } => Ok(Outcome {
                exit_code,
                result,
                stdout,
                stderr,
                error,
                metrics: if timed_out {
                    Metrics {
                        wall_ms: measured.as_secs_f64() * 1000.0,
                        ..metrics
                    }
                } else {
                    metrics
                },
                timed_out,
            }),
            Message::Error { code, message, .. } => {
                Err(Error::from(ProtocolError::Agent { code, message }))
            }
            other => {
                return self.broken(ProtocolError::Unexpected {
                    expected: "DONE",
                    found: other.kind(),
                });
            }
        };
        let done = Instant::now();

        // Secrets go before the cgroup: the lease is dropped explicitly here so
        // the files are gone by the time `release` is measured, and so nothing
        // that reads `/run/secrets` after `DONE` finds anything.
        drop(_secrets);
        if let Some(dir) = request_cgroup {
            let _ = crate::cgroup::Hierarchy::remove(&dir);
        }
        let cleaned = Instant::now();

        match &outcome {
            Ok(o) => self.record(o.succeeded()),
            Err(_) => self.record(false),
        }

        let timing = CallTiming {
            lock: locked - started,
            fork: forked - locked,
            admit: admitted - forked,
            run: done - admitted,
            release: cleaned - done,
        };
        outcome.map(|o| (o, timing))
    }

    /// Current state, for `zygo ps` and for the supervisor's rewarm decision.
    pub fn state(&self) -> SandboxState {
        *self.state.lock().expect("state")
    }

    /// Whether this function can still serve, possibly after a [`WarmFn::resume`].
    ///
    /// A paused function is healthy: it is frozen, not broken, and thawing it
    /// costs a single write. Only `Failed` needs replacing.
    pub fn is_healthy(&self) -> bool {
        self.state() != SandboxState::Failed
    }

    /// Freeze the tenant so it stops costing CPU while keeping its memory.
    ///
    /// This is the middle tier of the design's memory tiering (§3.9, F12): a
    /// zygote nobody has called for `idle_timeout` keeps its resident pages —
    /// which is the whole asset, since they are what makes the next request
    /// cost a `fork()` — but stops being schedulable. Waking it is one write,
    /// against the few hundred milliseconds a cold start would cost.
    ///
    /// The freeze is applied to the *tenant* cgroup rather than the zygote's, so
    /// anything the tenant has running stops with it.
    pub fn pause(&self) -> Result<()> {
        let mut state = self.state.lock().expect("state");
        if *state != SandboxState::Warm {
            return Ok(());
        }
        if let Some(dir) = &self.tenant_cgroup {
            crate::cgroup::freeze(dir, true)?;
        }
        *state = SandboxState::Paused;
        Ok(())
    }

    /// Thaw a paused function. A no-op on one that was never paused.
    pub fn resume(&self) -> Result<()> {
        let mut state = self.state.lock().expect("state");
        if *state != SandboxState::Paused {
            return Ok(());
        }
        if let Some(dir) = &self.tenant_cgroup {
            crate::cgroup::freeze(dir, false)?;
        }
        *state = SandboxState::Warm;
        Ok(())
    }

    /// Kill a request that overran its deadline.
    ///
    /// The request cgroup is the good path: `cgroup.kill` takes the child and
    /// everything it spawned in a single write, so a handler that forked its own
    /// helpers cannot leave them behind. Without one — `--no-cgroup`, or a
    /// kernel below 5.14 — all we have is the pid the agent reported, which
    /// misses grandchildren. That is a reason to keep per-request cgroups on,
    /// not a reason to skip the kill.
    fn enforce_deadline(&self, request_cgroup: Option<&std::path::Path>, ns_pid: u32) {
        // The pid has to be translated first, or a fallback signal goes to
        // whatever host process happens to hold the agent's namespace-local
        // number. A pid that cannot be translated is not signalled at all;
        // the cgroup path, when there is one, does not need it.
        match self.host_pid_of(ns_pid) {
            Some(host) => kill_request(request_cgroup, host),
            None => {
                if let Some(dir) = request_cgroup {
                    let _ = crate::cgroup::kill(dir);
                }
            }
        }
    }

    /// Put the forked child in its own cgroup. Returns the directory to remove
    /// afterwards, if one was created.
    fn admit(&self, id: &str, pid: u32) -> Option<PathBuf> {
        // The pid the agent reported is namespace-local; translate it first.
        // A pid that cannot be translated still gets its cgroup directory, so
        // `cgroup.kill` has somewhere to aim, even if nothing was moved.
        let host = self.host_pid_of(pid)?;
        admit(
            self.generation_cgroup.as_deref(),
            self.per_request_cgroup,
            id,
            host,
        )
    }

    /// Translate a pid the agent reported into the pid this process must use.
    ///
    /// The agent is pid 1 in its own namespace, so the number in `FORKED` means
    /// nothing here: measured on 5.10, a child the agent called pid 2 was pid 28
    /// on the host. Writing the agent's number into `cgroup.procs` silently
    /// moved nothing, and passing it to `kill` would have signalled whatever
    /// unrelated process happens to hold that number.
    ///
    /// The translation is `NSpid` from `/proc/<host>/status`, whose last field
    /// is the pid in the innermost namespace. Candidates come from the agent's
    /// own children rather than from all of `/proc`, so this stays a couple of
    /// small reads on the request path.
    fn host_pid_of(&self, ns_pid: u32) -> Option<u32> {
        let agent = self.agent_host_pid;
        let children =
            std::fs::read_to_string(format!("/proc/{agent}/task/{agent}/children")).ok()?;
        children
            .split_whitespace()
            .filter_map(|p| p.parse::<u32>().ok())
            .find(|&candidate| innermost_ns_pid(candidate) == Some(ns_pid))
    }

    fn record(&self, ok: bool) {
        let mut counters = self.counters.lock().expect("counters");
        counters.requests += 1;
        if !ok {
            counters.failures += 1;
        }
    }

    /// Mark the function unusable and return the error unchanged.
    ///
    /// A handler that raises is a *result*, not a failure of the function: the
    /// agent reports it in `DONE` and the sandbox stays warm. This is for the
    /// other kind — the connection returned something that was not a reply, so
    /// what the agent will do next is unknown. Continuing to send requests down
    /// a connection in an unknown state is how one bad request turns into a
    /// function that answers nonsense; the honest move is to replace it.
    fn broken<T>(&self, e: impl Into<Error>) -> Result<T> {
        *self.state.lock().expect("state") = SandboxState::Failed;
        self.record(false);
        Err(e.into())
    }

    /// What the tenant's CPU quota is doing right now.
    ///
    /// `None` when there is no cgroup to read — an unlimited run, or a host
    /// without cgroup v2. See [`CpuAccounting`] for why anything reporting warm
    /// latency needs this.
    pub fn cpu_accounting(&self) -> Option<CpuAccounting> {
        CpuAccounting::read(self.tenant_cgroup.as_ref()?)
    }

    /// Ask the agent to finish and exit.
    pub fn shutdown(&self) -> Result<()> {
        self.conn.write(&Message::Shutdown { grace_ms: 5_000 })
    }
}

impl Drop for WarmFn {
    /// Close the connection, then wait for the reply thread to notice.
    ///
    /// Shutting the socket down rather than merely dropping it: the sandbox
    /// holds the other end and the reply thread holds a duplicate of this one,
    /// so letting the last descriptor go is not something this can arrange.
    /// `shutdown` makes the thread's blocking read return end of stream at
    /// once, and joining means a dropped `WarmFn` leaves no thread behind.
    fn drop(&mut self) {
        let _ = self.socket.shutdown(std::net::Shutdown::Both);
        if let Some(thread) = self.replies.lock().expect("replies").take() {
            let _ = thread.join();
        }
    }
}

/// How the supervisor reaches a sandbox's `/run/secrets`.
///
/// Two modes, two routes, for one kernel reason. `/proc/<pid>/root` is
/// traversable only while the process is **dumpable**, and writing the id map
/// clears that for everything in the sandbox. An **agent** is `execve`d
/// afterwards, which resets it, so its own `/proc` entry works. A **held**
/// sandbox's init never execs and is deliberately unreadable, so its init
/// hands a directory descriptor out during the launch instead — the only
/// moment such a thing can be taken.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy)]
enum SecretsAt<'a> {
    /// Through a process's `/proc`, for an agent.
    Proc(u32),
    /// Through a descriptor the sandbox handed out, for warm-exec.
    Dir(std::os::fd::BorrowedFd<'a>),
}

/// Secrets for one function, and the in-flight count that governs their files.
#[derive(Debug, Default)]
struct Secrets {
    values: BTreeMap<String, String>,
    in_flight: u32,
    /// The sandbox's `/run/secrets`, held open from the moment the files are
    /// written until the last request in flight is done with them.
    ///
    /// Shared rather than per-lease because it is the *last* request out that
    /// removes the files, and that is rarely the one that wrote them.
    dir: Option<std::fs::File>,
}

impl Secrets {
    /// A request is starting. `true` when its files have to be written now —
    /// the first request in, with something to deliver.
    fn arrive(&mut self) -> bool {
        self.in_flight += 1;
        self.in_flight == 1 && !self.values.is_empty()
    }

    /// Remove the files, through the descriptor held since they were written.
    ///
    /// `unlinkat` and not a path, because the path went through
    /// `/proc/<pid>/root` of a process that may since have exited — a
    /// directory descriptor keeps the directory reachable regardless. The
    /// (empty, 0700) directory itself is left in place: it carries nothing,
    /// and removing it would need a second descriptor for its parent.
    fn unlink_all(&mut self) {
        let Some(dir) = self.dir.take() else {
            return;
        };
        for name in self.values.keys() {
            let _ = rustix::fs::unlinkat(&dir, name.as_str(), rustix::fs::AtFlags::empty());
        }
    }

    /// A request has finished. `true` when its files have to be removed now —
    /// the last request out, with something to withdraw.
    fn depart(&mut self) -> bool {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.in_flight == 0 && !self.values.is_empty()
    }
}

/// Secrets present in the sandbox for as long as this is alive.
///
/// Dropping it is what withdraws them, so every exit from the request path —
/// the reply, the deadline, a broken connection — takes the files with it.
struct SecretsLease<'a> {
    secrets: &'a Mutex<Secrets>,
}

impl Drop for SecretsLease<'_> {
    fn drop(&mut self) {
        withdraw_secrets(self.secrets);
    }
}

/// Make the secret files exist for a request about to run.
///
/// Written from *outside* the sandbox, through `/proc/<pid>/root`, so no
/// process inside ever receives a value: not the agent (it is not in `EXEC`,
/// not in the zygote's memory, not on the connection), and not a warm-exec
/// request's environment. Only the process that runs the handler can read the
/// file, and only while a request is in flight — the design's "delivered as a
/// file, to the child only" (§3.10), enforced by *where* the write happens
/// rather than by asking anything inside to be careful.
///
/// `via_pid` is whichever process in the sandbox the supervisor may look
/// through, and the two warm modes differ. An **agent** is `execve`d, which
/// resets `PR_SET_DUMPABLE`, so its own `/proc` entry is the supervisor's to
/// write. A **warm-exec** sandbox's init is deliberately *not* dumpable — it
/// is a fork of the supervisor and still maps the supervisor's memory — so
/// its `/proc/<pid>/root` is root-owned and an unprivileged supervisor cannot
/// reach through it at all. There the request's own parked process is used
/// instead: it is the supervisor's child, it has not exec'd yet, and it sees
/// the same mount namespace. Any process in the sandbox reaches the same
/// `/run`.
///
/// Ownership needs no `chown`: the supervisor's host uid is exactly what the
/// sandbox's user namespace maps to the handler's uid, so a file this process
/// creates is the handler's file inside.
fn place_secrets<'a>(secrets: &'a Mutex<Secrets>, at: SecretsAt<'_>) -> Result<SecretsLease<'a>> {
    let mut guard = secrets.lock().expect("secrets");
    // Counted before writing, and the lease taken whatever happens next, so a
    // failed write still departs and the count stays honest.
    let write_now = guard.arrive();

    // Every write happens while the guard is held, and the **lease is not
    // created until the guard is gone**. Dropping a lease calls
    // `withdraw_secrets`, which locks this same mutex; a lease that came into
    // existence above an early `?` return was therefore dropped by a thread
    // already holding the lock, and `Mutex` is not reentrant. That is a
    // permanent self-deadlock, one leaked thread per request, and a client
    // that waits for ever.
    //
    // It only fires when a write *fails*, so it never showed in a container
    // running as root.
    let written = if write_now {
        write_secrets(&mut guard, at)
    } else {
        Ok(())
    };
    drop(guard);

    let lease = SecretsLease { secrets };
    // Safe now: this drops the lease with no lock held, so the count departs
    // and anything already written is taken away again.
    written?;
    Ok(lease)
}

/// Write one request's secret files. Called with the lock held; takes none.
fn write_secrets(secrets: &mut Secrets, at: SecretsAt<'_>) -> Result<()> {
    // Either route ends in one directory descriptor, and everything after is
    // the same: the files are created relative to it. Held from here until
    // the last request in flight is finished with them, because a path
    // through `/proc/<pid>/root` stops resolving the moment that process
    // exits — which for warm-exec is *before* the files come out.
    let dir: std::fs::File = match at {
        SecretsAt::Proc(pid) => {
            let path = secrets_dir(pid);
            std::fs::create_dir_all(&path).at(&path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                    .at(&path)?;
            }
            std::fs::File::open(&path).at(&path)?
        }
        // Already created, at mode 0700, by the sandbox's own init.
        SecretsAt::Dir(fd) => std::fs::File::from(
            fd.try_clone_to_owned()
                .map_err(|e| Error::primitive("dup", "the sandbox's /run/secrets", e))?,
        ),
    };

    for (name, value) in &secrets.values {
        write_secret_file_at(&dir, name, value)?;
    }
    secrets.dir = Some(dir);
    Ok(())
}

/// Remove the secret files once nothing in flight needs them.
fn withdraw_secrets(secrets: &Mutex<Secrets>) {
    let mut guard = secrets.lock().expect("secrets");
    if !guard.depart() {
        return;
    }
    guard.unlink_all();
}

/// The sandbox's `/run/secrets`, as seen from the host.
fn secrets_dir(init_pid: u32) -> PathBuf {
    PathBuf::from(format!("/proc/{init_pid}/root{SECRETS_DIR_IN_SANDBOX}"))
}

/// Put a request's process in its own cgroup. Returns the directory to remove
/// afterwards, if one was created.
///
/// `host_pid` must be a pid in *this* process's namespace — translated first on
/// the agent path (§2.2b), native on the warm-exec path. Failing to move it is
/// not worth failing the request over: the process is still inside the
/// sandbox's generation under the *tenant* cgroup, so the tenant's limits
/// still apply; what is lost is only the per-request accounting and
/// `cgroup.kill`.
fn admit(
    generation: Option<&std::path::Path>,
    per_request: bool,
    id: &str,
    host_pid: u32,
) -> Option<PathBuf> {
    if !per_request {
        return None;
    }
    let dir = crate::cgroup::Hierarchy::request(generation?, id);
    if std::fs::create_dir(&dir).is_err() {
        return None;
    }
    let _ = crate::cgroup::attach(&dir, host_pid);
    Some(dir)
}

/// Kill a request that overran its deadline, given its host pid.
///
/// The request cgroup is the good path: `cgroup.kill` (or its freeze-and-signal
/// fallback) takes the process and everything it spawned. Without one all we
/// have is the pid, which misses grandchildren — a reason to keep per-request
/// cgroups on, not a reason to skip the kill.
fn kill_request(request_cgroup: Option<&std::path::Path>, host_pid: u32) {
    if let Some(dir) = request_cgroup
        && let Ok(true) = crate::cgroup::kill(dir)
    {
        return;
    }
    // SAFETY: `host_pid` names an unreaped process this supervisor created.
    unsafe { libc::kill(host_pid as libc::pid_t, libc::SIGKILL) };
}

/// Where secret files live inside the sandbox (design doc §3.10).
pub const SECRETS_DIR_IN_SANDBOX: &str = "/run/secrets";

impl WarmFn {
    /// Give this function its secret values.
    ///
    /// Values, not names: the names come from the spec, and the values from
    /// the environment of whoever ran `zygo serve`, which is the only process
    /// entitled to have them. They live here and nowhere else — not in the
    /// resolved spec, which `zygo spec explain` prints.
    pub fn set_secrets(&self, values: BTreeMap<String, String>) {
        self.secrets.lock().expect("secrets").values = values;
    }

    fn place_secrets(&self) -> Result<SecretsLease<'_>> {
        place_secrets(&self.secrets, SecretsAt::Proc(self.agent_host_pid))
    }
}

/// Create a secret file readable by its owner and nobody else.
///
/// Created with the mode from the start rather than chmodded afterwards, so
/// there is no moment at which it is readable more widely — `/run` is a tmpfs
/// shared by everything in the sandbox.
fn write_secret_file_at(dir: &std::fs::File, name: &str, value: &str) -> Result<()> {
    use rustix::fs::{Mode, OFlags};
    use std::io::Write as _;

    // `openat`, so the directory is named by the descriptor and not by a path
    // that may no longer resolve. `O_TRUNC` because a rewarm may find the
    // file from a previous generation still there.
    let fd = rustix::fs::openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o400),
    )
    .map_err(|e| {
        Error::primitive(
            "openat",
            "writing a secret into the sandbox",
            std::io::Error::from(e),
        )
    })?;
    let mut file = std::fs::File::from(fd);
    file.write_all(value.as_bytes())
        .map_err(|e| Error::primitive("write", "writing a secret into the sandbox", e))?;
    Ok(())
}

/// A warm-exec function: a held sandbox, and a fresh process per request.
///
/// The other half of the design's warm model (§3.4, layer 1), for everything
/// that is not an interpreter worth keeping warm: a Go or Rust binary starts
/// in a millisecond, so there is nothing to amortise and no agent to write.
/// The sandbox — namespaces, mounts, cgroup, hardening — is what is kept.
///
/// A request costs a fork of the supervisor, six `setns` calls, a second fork,
/// the hardening steps, and an `execve`: the design's "1–3 ms plus the
/// program's own start-up". The event goes in on stdin, the result comes out
/// on stdout as JSON, and stderr is captured separately.
#[cfg(target_os = "linux")]
pub struct WarmExec {
    name: String,
    /// Host pid of the held init: the process whose death means the sandbox
    /// is gone.
    init_pid: u32,
    /// `/run/secrets` inside the sandbox, handed out by its own init before
    /// it hardened.
    ///
    /// A descriptor because there is no path: `/proc/<pid>/root` is
    /// traversable only while a process is dumpable, and writing the id map
    /// clears that for everything in the sandbox. As root the check is
    /// bypassed, which is why this worked in a container for a year and on
    /// nobody's laptop.
    secrets_dir: Option<std::os::fd::OwnedFd>,
    /// Hardening and `execve` candidates, prepared once. Every request is a
    /// process that has to be constrained exactly as the init was.
    plan: crate::backend::ns::prepare::PreparedLaunch,
    /// Owns the init and the namespace descriptors. Dropping it kills the
    /// sandbox, which as pid 1's death takes every request in it along.
    sandbox: Mutex<Box<dyn crate::backend::Sandbox>>,
    counters: Mutex<Counters>,
    state: Mutex<SandboxState>,
    tenant_cgroup: Option<PathBuf>,
    /// The held sandbox's own generation, where its requests are admitted.
    generation_cgroup: Option<PathBuf>,
    per_request_cgroup: bool,
    timeout: std::time::Duration,
    secrets: Mutex<Secrets>,
}

#[cfg(target_os = "linux")]
impl WarmExec {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Host pid of the held init process. See [`WarmFn::init_pid`].
    pub fn init_pid(&self) -> u32 {
        self.init_pid
    }

    pub fn status(&self) -> Status {
        let counters = self.counters.lock().expect("counters");
        Status {
            name: self.name.clone(),
            state: self.state(),
            runtime: "exec".into(),
            rss_kb: resident_kb(self.init_pid).unwrap_or(0),
            imports_ms: 0.0,
            requests: counters.requests,
            failures: counters.failures,
        }
    }

    pub fn state(&self) -> SandboxState {
        *self.state.lock().expect("state")
    }

    pub fn is_healthy(&self) -> bool {
        self.state() != SandboxState::Failed
    }

    pub fn timeout(&self) -> std::time::Duration {
        self.timeout
    }

    pub fn set_secrets(&self, values: BTreeMap<String, String>) {
        self.secrets.lock().expect("secrets").values = values;
    }

    pub fn pause(&self) -> Result<()> {
        let mut state = self.state.lock().expect("state");
        if *state != SandboxState::Warm {
            return Ok(());
        }
        if let Some(dir) = &self.tenant_cgroup {
            crate::cgroup::freeze(dir, true)?;
        }
        *state = SandboxState::Paused;
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        let mut state = self.state.lock().expect("state");
        if *state != SandboxState::Paused {
            return Ok(());
        }
        if let Some(dir) = &self.tenant_cgroup {
            crate::cgroup::freeze(dir, false)?;
        }
        *state = SandboxState::Warm;
        Ok(())
    }

    pub fn cpu_accounting(&self) -> Option<CpuAccounting> {
        CpuAccounting::read(self.tenant_cgroup.as_ref()?)
    }

    /// Serve one request: enter, admit, run, collect.
    pub fn call_timed(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<(Outcome, CallTiming)> {
        use crate::backend::ns::enter;
        use std::io::Write as _;

        let id = next_request_id();
        let started = Instant::now();

        let entered = {
            let sandbox = self.sandbox.lock().expect("sandbox");
            let ns = sandbox
                .namespaces()
                .ok_or_else(|| Error::BackendUnavailable {
                    backend: "pool",
                    reason: "the sandbox has no namespace descriptors".into(),
                    remedy: "internal error; the function will be rewarmed".into(),
                })?;
            enter::enter(&self.plan, ns)
        };
        let entered = match entered {
            Ok(entered) => entered,
            Err(e) => {
                // Could not even get a process into the sandbox. If the init
                // is gone the sandbox is gone; otherwise this request failed
                // and the next may not.
                // SAFETY: signal 0 checks existence, delivers nothing.
                if unsafe { libc::kill(self.init_pid as libc::pid_t, 0) } != 0 {
                    *self.state.lock().expect("state") = SandboxState::Failed;
                }
                self.record(false);
                return Err(e);
            }
        };
        let forked = Instant::now();

        // The request is parked until `go` is written: this is the window in
        // which it can be put in its cgroup and given its secrets, before it
        // has run a single instruction of tenant code. Its pid is already a
        // host pid — `fork()` in the helper returned it in the host's
        // namespace — so nothing needs translating.
        let request_cgroup = admit(
            self.generation_cgroup.as_deref(),
            self.per_request_cgroup,
            &id,
            entered.pid,
        );
        // Through the descriptor the sandbox handed out at launch, never
        // through `/proc`: a held sandbox is not dumpable, and nothing an
        // unprivileged supervisor does changes that.
        let at = match &self.secrets_dir {
            Some(dir) => SecretsAt::Dir(std::os::fd::AsFd::as_fd(dir)),
            // No descriptor means the sandbox could not hand one out, which
            // the launcher already refused to start over — so this is only
            // reachable for a function with no secrets to place.
            None => SecretsAt::Proc(self.init_pid),
        };
        let _secrets = match place_secrets(&self.secrets, at) {
            Ok(lease) => lease,
            Err(e) => {
                kill_request(request_cgroup.as_deref(), entered.pid);
                let _ = entered.reap_helper();
                self.record(false);
                return Err(e);
            }
        };
        let admitted = Instant::now();

        let mut entered = entered;
        if let Some(go) = entered.go.take() {
            let mut go = std::fs::File::from(go);
            let _ = go.write_all(&[1]);
            // Dropped here: closed.
        }
        // The event, then end of file — which is how a program that reads
        // stdin to completion knows the event is whole. Taking the descriptor
        // out and dropping it is the close; a copy left in `entered` would
        // hold the pipe open and such a program would wait for ever.
        if let Some(stdin) = entered.stdin.take() {
            let mut stdin = std::fs::File::from(stdin);
            let body = serde_json::to_vec(&event).unwrap_or_else(|_| b"null".to_vec());
            let _ = stdin.write_all(&body);
        }

        // Everything the request says, with the deadline enforced while it
        // is said. The function's own limit wins over the caller's (N4).
        let budget = timeout.min(self.timeout);
        let deadline = budget.saturating_sub(admitted - started);
        let collected = collect_request(&entered, deadline, || {
            kill_request(request_cgroup.as_deref(), entered.pid)
        });

        let exit_status = read_status(&entered);
        let _ = entered.reap_helper();
        let done = Instant::now();

        drop(_secrets);
        if let Some(dir) = request_cgroup {
            let _ = crate::cgroup::Hierarchy::remove(&dir);
        }
        let cleaned = Instant::now();

        let outcome = collected.and_then(|c| into_outcome(c, exit_status, done - admitted));
        match &outcome {
            Ok(o) => self.record(o.succeeded()),
            Err(_) => self.record(false),
        }

        let timing = CallTiming {
            lock: std::time::Duration::ZERO,
            fork: forked - started,
            admit: admitted - forked,
            run: done - admitted,
            release: cleaned - done,
        };
        outcome.map(|o| (o, timing))
    }

    fn record(&self, ok: bool) {
        let mut counters = self.counters.lock().expect("counters");
        counters.requests += 1;
        if !ok {
            counters.failures += 1;
        }
    }
}

/// What a warm-exec request produced on its pipes.
#[cfg(target_os = "linux")]
struct Collected {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
    /// A hardening or `execve` failure reported before the program ran.
    launch_failure: Option<(crate::backend::ns::prepare::Step, i32)>,
}

/// Drain the request's stdout, stderr and error pipes until all three reach
/// end of file, killing the request at `deadline` and draining on.
///
/// `poll` over the three together, because reading them in turn deadlocks: a
/// program that fills stderr while we block on stdout never gets to finish.
#[cfg(target_os = "linux")]
fn collect_request(
    entered: &crate::backend::ns::enter::Entered,
    deadline: std::time::Duration,
    mut kill: impl FnMut(),
) -> Result<Collected> {
    use std::os::fd::AsRawFd;

    let mut collected = Collected {
        stdout: Vec::new(),
        stderr: Vec::new(),
        timed_out: false,
        launch_failure: None,
    };
    let mut err_buf = Vec::new();
    let mut open = [true; 3];
    let fds = [
        entered.stdout.as_raw_fd(),
        entered.stderr.as_raw_fd(),
        entered.err.as_raw_fd(),
    ];
    let mut due = Instant::now() + deadline;
    let mut killed = false;

    while open.iter().any(|o| *o) {
        let mut polls: Vec<libc::pollfd> = fds
            .iter()
            .zip(open)
            .filter(|(_, o)| *o)
            .map(|(fd, _)| libc::pollfd {
                fd: *fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let left = due.saturating_duration_since(Instant::now());
        let ms = left.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: `polls` is a live slice of `pollfd`s over descriptors we own.
        let rc = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, ms) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::primitive("poll", "warm-exec request pipes", e));
        }
        if rc == 0 {
            if killed {
                // Killed, and still no end of file: something else holds the
                // pipes open. Give up rather than wait for ever.
                break;
            }
            collected.timed_out = true;
            killed = true;
            kill();
            due = Instant::now() + KILL_GRACE;
            continue;
        }
        for p in &polls {
            if p.revents == 0 {
                continue;
            }
            let which = fds.iter().position(|fd| *fd == p.fd).expect("known fd");
            let mut buf = [0u8; 64 * 1024];
            // SAFETY: `buf` is live; the fd is ours.
            let n = unsafe { libc::read(p.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                open[which] = false;
                continue;
            }
            let chunk = &buf[..n as usize];
            match which {
                0 => collected.stdout.extend_from_slice(chunk),
                1 => collected.stderr.extend_from_slice(chunk),
                _ => err_buf.extend_from_slice(chunk),
            }
        }
    }

    if !err_buf.is_empty() {
        collected.launch_failure = crate::backend::ns::child::decode_failure(&err_buf);
    }
    Ok(collected)
}

/// The request's wait status from the helper, or `None` if the helper never
/// reported one.
#[cfg(target_os = "linux")]
fn read_status(entered: &crate::backend::ns::enter::Entered) -> Option<i32> {
    use std::io::Read as _;
    let mut file = std::fs::File::from(entered.status.try_clone().ok()?);
    let mut bytes = [0u8; 4];
    file.read_exact(&mut bytes).ok()?;
    Some(i32::from_ne_bytes(bytes))
}

/// Turn what the request left behind into an [`Outcome`].
#[cfg(target_os = "linux")]
fn into_outcome(
    c: Collected,
    wait_status: Option<i32>,
    elapsed: std::time::Duration,
) -> Result<Outcome> {
    if let Some((step, errno)) = c.launch_failure {
        return Err(Error::Primitive {
            operation: step.describe(),
            remedy: step
                .remedy(errno)
                .unwrap_or(
                    "run `zygo doctor`; a warm-exec request needs the same primitives as a sandbox",
                )
                .to_string(),
            source: std::io::Error::from_raw_os_error(errno),
        });
    }

    let exit_code = match wait_status {
        Some(status) if libc::WIFEXITED(status) => libc::WEXITSTATUS(status),
        Some(status) if libc::WIFSIGNALED(status) => 128 + libc::WTERMSIG(status),
        _ => 1,
    };
    let stdout = String::from_utf8_lossy(&c.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&c.stderr).into_owned();

    // The contract (§3.4): stdout is the JSON result. A program that printed
    // something else has not failed to run, so its exit code stands — but its
    // answer cannot be handed on as a result, and saying so is more useful
    // than a mangled one.
    let (result, error) = if c.timed_out {
        (
            serde_json::Value::Null,
            Some("killed by SIGKILL (the deadline expired)".to_string()),
        )
    } else if exit_code != 0 {
        (
            serde_json::Value::Null,
            Some(format!("the program exited {exit_code}")),
        )
    } else if stdout.trim().is_empty() {
        (serde_json::Value::Null, None)
    } else {
        match serde_json::from_str(&stdout) {
            Ok(value) => (value, None),
            Err(e) => (
                serde_json::Value::Null,
                Some(format!("stdout is not JSON: {e}")),
            ),
        }
    };

    Ok(Outcome {
        exit_code,
        result,
        stdout: if error.is_some() {
            stdout
        } else {
            String::new()
        },
        stderr,
        error,
        metrics: Metrics {
            // Measured here, because on this path there is nobody else to
            // measure it: a warm-exec request is a bare process, not an agent
            // that reports on itself. It was `Metrics::default()` — every
            // warm-exec request in the log timed at zero, so `zygo stats`
            // reported a p50 of `0.0 ms` for a function that was working.
            wall_ms: elapsed.as_secs_f64() * 1000.0,
            ..Metrics::default()
        },
        timed_out: c.timed_out,
    })
}

/// `VmRSS` of a process, for `zygo ps`.
#[cfg(target_os = "linux")]
fn resident_kb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("VmRSS:"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// One warm function, whichever way it is warm.
///
/// The supervisor, the CLI and the benchmarks see this and nothing below it:
/// a function is called, paused, woken and stopped the same way whether an
/// agent forks for it or a fresh process is entered into its sandbox.
pub enum Function {
    /// An agent in the box, forking per request.
    Agent(Box<WarmFn>),
    /// A held sandbox, entered per request.
    #[cfg(target_os = "linux")]
    Exec(Box<WarmExec>),
}

// Both arms are boxed. An enum is as large as its largest arm, and these two
// differ by a factor of two, so unboxed every agent would carry a held
// sandbox's worth of padding. The cost is one allocation per *served
// function* — not per request, and nothing on the request path follows the
// pointer more than once.

macro_rules! each {
    ($self:expr, $f:ident => $body:expr) => {
        match $self {
            Function::Agent($f) => $body,
            #[cfg(target_os = "linux")]
            Function::Exec($f) => $body,
        }
    };
}

impl Function {
    pub fn name(&self) -> &str {
        each!(self, f => f.name())
    }

    /// Host pid of the sandbox's first process, whichever kind it is.
    pub fn init_pid(&self) -> u32 {
        each!(self, f => f.init_pid())
    }

    pub fn status(&self) -> Status {
        each!(self, f => f.status())
    }

    pub fn state(&self) -> SandboxState {
        each!(self, f => f.state())
    }

    pub fn is_healthy(&self) -> bool {
        each!(self, f => f.is_healthy())
    }

    pub fn timeout(&self) -> std::time::Duration {
        each!(self, f => f.timeout())
    }

    pub fn set_secrets(&self, values: BTreeMap<String, String>) {
        each!(self, f => f.set_secrets(values))
    }

    pub fn pause(&self) -> Result<()> {
        each!(self, f => f.pause())
    }

    pub fn resume(&self) -> Result<()> {
        each!(self, f => f.resume())
    }

    pub fn cpu_accounting(&self) -> Option<CpuAccounting> {
        each!(self, f => f.cpu_accounting())
    }

    pub fn call(&self, event: serde_json::Value) -> Result<Outcome> {
        self.call_with_timeout(event, self.timeout())
    }

    pub fn call_with_timeout(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<Outcome> {
        self.call_timed(event, timeout).map(|(o, _)| o)
    }

    pub fn call_timed(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<(Outcome, CallTiming)> {
        each!(self, f => f.call_timed(event, timeout))
    }

    /// Ask the function to wind down. An agent is told so it can finish what
    /// it holds; a held sandbox has nothing to be told — its requests are
    /// children of the supervisor, not of the init, and finish on their own.
    /// Dropping the `Function` is what actually takes the sandbox down.
    pub fn shutdown(&self) -> Result<()> {
        match self {
            Function::Agent(f) => f.shutdown(),
            #[cfg(target_os = "linux")]
            Function::Exec(_) => Ok(()),
        }
    }
}

/// Read the zygote's output line by line into its log, for as long as the
/// sandbox writes any. Ends at end of file, which is when the sandbox is gone.
fn spawn_zygote_log_reader(name: &str, read_end: std::os::fd::OwnedFd, logs: Logs) {
    use std::io::BufRead;
    let name = name.to_string();
    let thread = std::thread::Builder::new()
        .name(format!("zygo-log-{name}"))
        .spawn(move || {
            let reader = std::io::BufReader::new(std::fs::File::from(read_end));
            for line in reader.split(b'\n') {
                let Ok(line) = line else { break };
                let text = String::from_utf8_lossy(&line).into_owned();
                tracing::info!(target: "zygote", function = %name, "{text}");
                logs.push(LogKind::Zygote, text);
            }
        });
    if let Err(e) = thread {
        tracing::warn!("could not start the zygote log reader: {e}");
    }
}

/// Where one request's time went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallTiming {
    /// Registering as a caller and writing `EXEC`.
    ///
    /// This was "waiting for the connection" when a request held the wire for
    /// its whole round trip. It is now only the write, so a number here that is
    /// not near zero means the socket buffer is full.
    pub lock: std::time::Duration,
    /// `EXEC` sent to `FORKED` received — the agent's `fork()`.
    pub fork: std::time::Duration,
    /// Creating the request cgroup and moving the child into it.
    pub admit: std::time::Duration,
    /// `GO` sent to `DONE` received — the handler, and the child's teardown.
    pub run: std::time::Duration,
    /// Removing the request cgroup, after the answer is already in hand.
    pub release: std::time::Duration,
}

impl CallTiming {
    pub fn total(&self) -> std::time::Duration {
        self.lock + self.fork + self.admit + self.run + self.release
    }
}

/// What the tenant's CPU quota is doing, from cgroup v2 `cpu.stat`.
///
/// A warm call forks, so for a moment the tenant has two runnable tasks: the
/// agent and the child it is tearing down. A tenant at `cpu = 1.0` that is
/// asked for back-to-back requests therefore wants slightly more than its
/// quota, and CFS answers by stopping it until the next period. At the default
/// 100 ms period that is a tail of tens of milliseconds which says nothing
/// about how fast Zygo is — it is the quota being enforced, exactly as asked.
///
/// Measured on kernel 5.10 / aarch64: a 1-core tenant driven with no think
/// time was throttled in 27 of 28 periods and saw p99 49 ms; the same tenant at
/// half that offered load was throttled in none and saw p99 1.9 ms. So anything
/// that reports warm latency has to be able to say which of the two it
/// measured, and the supervisor's queue needs it to decide whom to admit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuAccounting {
    /// `usage_usec`: CPU the tenant has consumed.
    pub usage_us: u64,
    /// `nr_periods`: enforcement periods that have elapsed.
    pub periods: u64,
    /// `nr_throttled`: periods in which the quota ran out.
    pub throttled_periods: u64,
    /// `throttled_usec`: total time spent stopped at the quota.
    pub throttled_us: u64,
    /// The quota in cores, `None` when the tenant is uncapped.
    pub quota_cores: Option<f64>,
    /// The enforcement period. A throttled task waits out the rest of one, so
    /// this is the upper bound on the latency a quota can add.
    pub period: std::time::Duration,
}

impl CpuAccounting {
    /// Read `cpu.stat` and `cpu.max` from a cgroup directory.
    ///
    /// The quota is enforced by the nearest ancestor that sets one, so the
    /// counters are read from the tenant rather than from the leaf the
    /// processes happen to live in.
    pub fn read(dir: &std::path::Path) -> Option<CpuAccounting> {
        let stat = std::fs::read_to_string(dir.join("cpu.stat")).ok()?;
        let field = |name: &str| -> u64 {
            stat.lines()
                .find_map(|l| l.strip_prefix(name)?.trim().parse().ok())
                .unwrap_or(0)
        };
        let (quota_cores, period) = read_cpu_max(dir);
        Some(CpuAccounting {
            usage_us: field("usage_usec"),
            periods: field("nr_periods"),
            throttled_periods: field("nr_throttled"),
            throttled_us: field("throttled_usec"),
            quota_cores,
            period,
        })
    }

    /// What happened between two readings.
    pub fn since(&self, earlier: &CpuAccounting) -> CpuAccounting {
        CpuAccounting {
            usage_us: self.usage_us.saturating_sub(earlier.usage_us),
            periods: self.periods.saturating_sub(earlier.periods),
            throttled_periods: self
                .throttled_periods
                .saturating_sub(earlier.throttled_periods),
            throttled_us: self.throttled_us.saturating_sub(earlier.throttled_us),
            ..*self
        }
    }

    /// Whether the quota, rather than the runtime, set the latency.
    ///
    /// One throttled period in a long run is noise; a run that spends a
    /// meaningful share of its periods stopped at the quota is measuring the
    /// quota. The threshold is deliberately low: past 5% the tail is already
    /// dominated by the period, because every throttled request waits out most
    /// of one.
    pub fn saturated(&self) -> bool {
        self.periods > 0 && self.throttled_periods * 20 > self.periods
    }

    /// Share of the quota used, over a window of wall-clock time.
    pub fn demand_cores(&self, elapsed: std::time::Duration) -> f64 {
        if elapsed.is_zero() {
            return 0.0;
        }
        self.usage_us as f64 / elapsed.as_micros() as f64
    }
}

/// Parse `cpu.max`: `"<quota|max> <period>"`, quota in µs per period.
fn read_cpu_max(dir: &std::path::Path) -> (Option<f64>, std::time::Duration) {
    let default_period = std::time::Duration::from_micros(crate::spec::Cpu::PERIOD_US);
    let Ok(text) = std::fs::read_to_string(dir.join("cpu.max")) else {
        return (None, default_period);
    };
    let mut parts = text.split_whitespace();
    let quota = parts.next().unwrap_or("max");
    let period: u64 = parts
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(crate::spec::Cpu::PERIOD_US);
    let cores = quota
        .parse::<f64>()
        .ok()
        .filter(|_| quota != "max")
        .map(|q| q / period as f64);
    (cores, std::time::Duration::from_micros(period))
}

/// How long the agent gets to report a killed request before it is itself
/// declared broken.
///
/// The agent's own work here is one `waitpid` on a process the kernel has
/// already killed and one small frame, so this is generous. What it is really
/// bounding is the case where the agent is wedged too — a lock some
/// import-time thread was holding, say — and the honest answer is to replace
/// the function rather than wait on it.
pub const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the agent gets to fork a child and report its pid.
///
/// No tenant code has run yet at this point — the agent has only to `fork` and
/// write one frame — so an agent that cannot manage it inside this is not busy,
/// it is stuck, and the honest answer is to replace the function rather than
/// let the caller wait out its whole budget for a reply that is not coming.
pub const FORK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The agent went away mid-request.
fn agent_gone(name: &str, reason: &str) -> Error {
    Error::BackendUnavailable {
        backend: "pool",
        reason: format!("`{name}`: {reason}"),
        remedy: "the function will be rewarmed; check `zygo logs` for why the agent died".into(),
    }
}

/// The pid of `host_pid` as seen in its own innermost pid namespace.
///
/// `NSpid` lists a process's pid in each namespace it belongs to, outermost
/// first, so the last entry is the number the process sees for itself. A kernel
/// without `NSpid` (below 4.1) reports nothing, which is treated as "cannot
/// translate" rather than guessed at.
fn innermost_ns_pid(host_pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{host_pid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("NSpid:"))?
        .split_whitespace()
        .next_back()?
        .parse()
        .ok()
}

/// Monotonic request ids. Short, because they become cgroup directory names.
fn next_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{:08x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// How long an agent has to announce itself before the warm-up is failed.
///
/// Generous: the interpreter starts and the handler's imports run under it,
/// and `numpy` on a small board is seconds. Finite, because the launcher
/// serves one warm-up at a time — a wait with no end here is a supervisor
/// that never serves anything again, which is what happened on a Pi.
pub const AGENT_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether an error is a socket read timeout rather than a protocol fault.
fn is_timeout(e: &Error) -> bool {
    let Error::Protocol(ProtocolError::Frame(crate::protocol::frame::FrameError::Io(io))) = e
    else {
        return false;
    };
    matches!(
        io.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Read the agent's `READY` and check that it speaks a protocol this build
/// understands.
pub fn read_ready(
    reader: &mut crate::protocol::FrameReader<std::os::unix::net::UnixStream>,
) -> Result<(String, u64, f64)> {
    match reader.read().map_err(ProtocolError::from)? {
        Some(Message::Ready {
            proto,
            imports_ms,
            rss_kb,
            runtime,
            ..
        }) => {
            if proto != PROTOCOL_VERSION {
                return Err(ProtocolError::VersionMismatch {
                    found: proto,
                    expected: PROTOCOL_VERSION,
                }
                .into());
            }
            Ok((runtime, rss_kb, imports_ms))
        }
        Some(Message::Error { code, message, .. }) => {
            Err(ProtocolError::Agent { code, message }.into())
        }
        other => Err(ProtocolError::Unexpected {
            expected: "READY",
            found: other.map(|m| m.kind()).unwrap_or("end of stream"),
        }
        .into()),
    }
}

/// Argv that starts the reference Python agent inside a sandbox.
///
/// `--fd` rather than a socket path: nothing has to exist in the sandbox's
/// filesystem, so no bind mount and no assumption about the image.
pub fn python_agent_argv(
    agent_in_sandbox: &str,
    handler_in_sandbox: &str,
    mode: &str,
) -> Vec<String> {
    vec![
        "python3".to_string(),
        agent_in_sandbox.to_string(),
        "--fd".to_string(),
        AGENT_FD.to_string(),
        handler_in_sandbox.to_string(),
        mode.to_string(),
    ]
}

/// Where the agent and the handler are bind-mounted inside every sandbox.
pub const AGENT_IN_SANDBOX: &str = "/zygo/agent.py";
pub const HANDLER_IN_SANDBOX: &str = "/zygo/handler.py";

/// The mounts a warm Python function needs on top of its spec's own.
pub fn agent_mounts(agent_host: &std::path::Path, f: &ResolvedFn) -> Vec<crate::spec::Mount> {
    use crate::spec::{Mount, MountMode};

    let mut mounts = f.mounts.clone();
    mounts.push(Mount {
        source: agent_host.to_path_buf(),
        target: PathBuf::from(AGENT_IN_SANDBOX),
        mode: MountMode::Ro,
    });
    if let Some(entry) = &f.entry {
        mounts.push(Mount {
            source: entry.clone(),
            target: PathBuf::from(HANDLER_IN_SANDBOX),
            mode: MountMode::Ro,
        });
    }
    mounts
}

/// Which warm mode a function uses (design doc §3.4).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// An agent in the box forks per request.
    Agent,
    /// The sandbox is held; each request is a fresh process running `cmd`.
    Exec,
}

/// Turn an agent-side failure into the error a caller should see.
pub fn agent_failure(code: ErrorCode, message: String) -> Error {
    ProtocolError::Agent { code, message }.into()
}

/// How long a `WarmFn` took to become ready, for `zygo ps`.
#[derive(Debug, Clone, Copy)]
pub struct WarmupTiming {
    pub started: Instant,
    pub ready_after: std::time::Duration,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::spec::{Layer, ResolveOptions, resolve_standalone};

    // --- secrets -----------------------------------------------------------

    /// A secret that cannot be written must fail the request, not wedge the
    /// supervisor.
    ///
    /// The bug this pins deadlocked a thread for ever: the lease was created
    /// above the `?`, so a failed write dropped it while its own mutex was
    /// still held. It never fired in a container running as root and fired on
    /// the first request on an ordinary user's machine.
    ///
    /// `init_pid` here is a pid that cannot exist, so `/proc/<pid>/root` is
    /// not there and the write fails the way a permission error would. The
    /// test is written to *finish*: a regression makes it hang rather than
    /// fail, so it runs on its own thread and the assertion is the join.
    #[test]
    fn a_secret_that_cannot_be_written_fails_the_request_instead_of_deadlocking() {
        let secrets = Arc::new(Mutex::new(Secrets {
            values: [("STRIPE_KEY".to_string(), "sk_test".to_string())]
                .into_iter()
                .collect(),
            in_flight: 0,
            dir: None,
        }));

        let attempt = {
            let secrets = Arc::clone(&secrets);
            std::thread::spawn(move || {
                // Twice: the first failure must not leave the mutex held, or
                // the second call is what hangs.
                let first = place_secrets(&secrets, SecretsAt::Proc(u32::MAX)).is_err();
                let second = place_secrets(&secrets, SecretsAt::Proc(u32::MAX)).is_err();
                (first, second)
            })
        };

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !attempt.is_finished() {
            assert!(
                Instant::now() < deadline,
                "place_secrets deadlocked on the error path"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (first, second) = attempt.join().expect("the thread did not panic");
        assert!(first, "an unwritable secret must be an error");
        assert!(second, "and the mutex must still be usable afterwards");

        // The count departed both times, so a later request is not told that
        // files it cannot see are already in place.
        assert_eq!(secrets.lock().expect("secrets").in_flight, 0);
    }

    /// The path that must keep working: no secrets to write is not a failure,
    /// and the lease still counts in and out.
    #[test]
    fn a_function_with_no_secrets_places_nothing_and_succeeds() {
        let secrets = Mutex::new(Secrets::default());
        {
            let _lease =
                place_secrets(&secrets, SecretsAt::Proc(u32::MAX)).expect("nothing to write");
            assert_eq!(secrets.lock().expect("secrets").in_flight, 1);
        }
        assert_eq!(secrets.lock().expect("secrets").in_flight, 0);
    }

    // --- the log ring ------------------------------------------------------

    fn request(exit_code: i32, error: Option<&str>) -> LogKind {
        LogKind::Request {
            id: "r".into(),
            exit_code,
            timed_out: false,
            wall_ms: 1.0,
            error: error.map(str::to_string),
            stderr: String::new(),
        }
    }

    #[test]
    fn the_ring_is_bounded_and_keeps_the_newest() {
        let ring = LogRing::default();
        for i in 0..(LOG_ENTRIES + 25) {
            ring.push(LogKind::Zygote, format!("line {i}"));
        }
        let (all, next) = ring.since(0, usize::MAX, false);
        assert_eq!(all.len(), LOG_ENTRIES);
        assert_eq!(all[0].text, "line 25", "the oldest 25 were dropped");
        assert_eq!(
            next,
            (LOG_ENTRIES + 25) as u64,
            "sequence numbers never reset"
        );
    }

    #[test]
    fn tail_is_the_most_recent_and_follow_is_everything_after() {
        let ring = LogRing::default();
        for i in 0..10 {
            ring.push(LogKind::Zygote, format!("{i}"));
        }
        let (tail, next) = ring.since(0, 3, false);
        assert_eq!(
            tail.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["7", "8", "9"]
        );
        assert_eq!(next, 10);

        ring.push(LogKind::Zygote, "10");
        let (more, next) = ring.since(next, usize::MAX, false);
        assert_eq!(more.len(), 1, "only what arrived since");
        assert_eq!(more[0].text, "10");
        assert_eq!(next, 11);
        let (none, _) = ring.since(next, usize::MAX, false);
        assert!(none.is_empty());
    }

    #[test]
    fn failed_only_keeps_requests_that_failed_and_never_zygote_lines() {
        let ring = LogRing::default();
        ring.push(LogKind::Zygote, "warming up");
        ring.push(request(0, None), "fine");
        ring.push(request(1, None), "exit 1");
        ring.push(request(0, Some("ValueError")), "raised");
        let (failed, _) = ring.since(0, usize::MAX, true);
        assert_eq!(
            failed.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["exit 1", "raised"]
        );
    }

    #[test]
    fn long_output_is_cut_at_a_character_boundary_and_marked() {
        let ring = LogRing::default();
        // Multi-byte characters straddling the cut must not split.
        let text = "é".repeat(LOG_TEXT_BYTES);
        ring.push(LogKind::Zygote, text);
        let (entries, _) = ring.since(0, 1, false);
        assert!(entries[0].text.ends_with("… [truncated]"));
        assert!(entries[0].text.len() <= LOG_TEXT_BYTES + "… [truncated]".len());
        assert!(
            entries[0]
                .text
                .trim_end_matches("… [truncated]")
                .chars()
                .all(|c| c == 'é')
        );
    }

    #[test]
    fn a_log_entry_round_trips_the_control_socket() {
        let entry = LogEntry {
            seq: 7,
            at_ms: 1_700_000_000_000,
            kind: request(1, Some("boom")),
            text: "out".into(),
        };
        let json = serde_json::to_value(&entry).expect("json");
        assert_eq!(json["kind"], "request", "the kind is a flat tag");
        assert_eq!(json["exit_code"], 1);
        let back: LogEntry = serde_json::from_value(json).expect("back");
        assert_eq!(back, entry);
        assert!(back.failed());
    }

    // --- secrets -----------------------------------------------------------

    #[test]
    fn the_first_request_in_writes_and_the_last_one_out_removes() {
        let mut s = Secrets {
            values: BTreeMap::from([("KEY".to_string(), "v".to_string())]),
            in_flight: 0,
            dir: None,
        };
        assert!(s.arrive(), "first in: write");
        assert!(!s.arrive(), "second in: already there");
        assert!(!s.depart(), "one still needs them");
        assert!(s.depart(), "last out: remove");
    }

    #[test]
    fn a_function_with_no_secrets_never_touches_the_filesystem() {
        // The count still moves, because whether this request is "last out" is
        // only knowable if every arrival was counted — but no file is ever
        // written for nothing.
        let mut s = Secrets::default();
        assert!(!s.arrive());
        assert!(!s.depart());
        assert_eq!(s.in_flight, 0);
    }

    #[test]
    fn departing_more_than_arriving_cannot_underflow() {
        let mut s = Secrets {
            values: BTreeMap::from([("KEY".to_string(), "v".to_string())]),
            in_flight: 0,
            dir: None,
        };
        assert!(s.depart(), "at zero with values: remove is the safe answer");
        assert_eq!(s.in_flight, 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_secret_file_is_owner_readable_from_the_moment_it_exists() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = std::fs::File::open(tmp.path()).expect("dirfd");
        write_secret_file_at(&dir, "STRIPE_KEY", "sk_live_...").expect("write");
        let path = tmp.path().join("STRIPE_KEY");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o400, "mode {mode:o}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "sk_live_...");
    }

    #[cfg(unix)]
    #[test]
    fn rewriting_a_secret_replaces_it_entirely() {
        // A shorter value must not leave the tail of a longer one behind — a
        // rewarm writes over whatever the last generation left.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = std::fs::File::open(tmp.path()).expect("dirfd");
        let path = tmp.path().join("KEY");
        write_secret_file_at(&dir, "KEY", "a-long-value").expect("write");
        // The file is 0400, so it has to be replaced rather than reopened for
        // writing: the owner of a 0400 file cannot write it.
        std::fs::remove_file(&path).expect("rm");
        write_secret_file_at(&dir, "KEY", "short").expect("rewrite");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "short");
    }

    /// A directory named by a descriptor, which is the whole point: the path
    /// it was opened through can disappear and the writes still land.
    #[cfg(unix)]
    #[test]
    fn a_secret_is_written_through_the_descriptor_not_the_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inner = tmp.path().join("run-secrets");
        std::fs::create_dir(&inner).expect("mkdir");
        let dir = std::fs::File::open(&inner).expect("dirfd");

        // Rename the directory out from under the descriptor. A path-based
        // write would now fail or land in the wrong place.
        let moved = tmp.path().join("moved");
        std::fs::rename(&inner, &moved).expect("rename");

        write_secret_file_at(&dir, "KEY", "v").expect("write through the fd");
        assert_eq!(
            std::fs::read_to_string(moved.join("KEY")).expect("read"),
            "v"
        );
        assert!(!inner.exists(), "the old path really is gone");
    }

    // --- pid translation ---------------------------------------------------

    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_in_the_host_namespace_translates_to_itself() {
        // The identity case, which is also the one that made this bug invisible
        // for so long: in PoC 3 the agent was a plain host subprocess, so the
        // pid it reported happened to be correct.
        let me = std::process::id();
        assert_eq!(innermost_ns_pid(me), Some(me));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_pid_that_does_not_exist_translates_to_nothing() {
        // Better to decline than to guess: a wrong answer here is a SIGKILL
        // sent to an unrelated process.
        assert_eq!(innermost_ns_pid(u32::MAX), None);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn translation_declines_where_there_is_no_procfs() {
        assert_eq!(innermost_ns_pid(std::process::id()), None);
    }

    // --- CpuAccounting ---------------------------------------------------
    //
    // Parsed from files rather than mocked, because the format is the thing
    // being got right: `cpu.stat` gained fields between kernel releases and
    // `cpu.max` has two shapes.

    fn cgroup_with(stat: &str, max: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("cpu.stat"), stat).expect("cpu.stat");
        if let Some(m) = max {
            std::fs::write(dir.path().join("cpu.max"), m).expect("cpu.max");
        }
        dir
    }

    const THROTTLED: &str = "usage_usec 2411882\nuser_usec 1102113\nsystem_usec 1309769\n\
                             nr_periods 28\nnr_throttled 27\nthrottled_usec 2257080\n";

    #[test]
    fn cpu_stat_is_read_with_its_quota_and_period() {
        let dir = cgroup_with(THROTTLED, Some("100000 100000\n"));
        let cpu = CpuAccounting::read(dir.path()).expect("readable");
        assert_eq!(cpu.usage_us, 2_411_882);
        assert_eq!(cpu.periods, 28);
        assert_eq!(cpu.throttled_periods, 27);
        assert_eq!(cpu.throttled_us, 2_257_080);
        assert_eq!(cpu.quota_cores, Some(1.0));
        assert_eq!(cpu.period, std::time::Duration::from_millis(100));
    }

    #[test]
    fn an_uncapped_tenant_reports_no_quota() {
        let dir = cgroup_with(THROTTLED, Some("max 100000\n"));
        let cpu = CpuAccounting::read(dir.path()).expect("readable");
        assert_eq!(cpu.quota_cores, None, "`max` is not a number of cores");
    }

    #[test]
    fn a_fractional_quota_survives_the_round_trip() {
        let dir = cgroup_with(THROTTLED, Some("50000 100000\n"));
        let cpu = CpuAccounting::read(dir.path()).expect("readable");
        assert_eq!(cpu.quota_cores, Some(0.5));
    }

    #[test]
    fn a_shortened_period_is_reported_as_written() {
        // The period is the upper bound on the latency a quota can add, so a
        // reader that assumed 100 ms would misreport a cgroup someone retuned.
        let dir = cgroup_with(THROTTLED, Some("10000 10000\n"));
        let cpu = CpuAccounting::read(dir.path()).expect("readable");
        assert_eq!(cpu.quota_cores, Some(1.0));
        assert_eq!(cpu.period, std::time::Duration::from_millis(10));
    }

    #[test]
    fn a_missing_cpu_max_is_not_a_missing_cgroup() {
        // `cpu.max` is absent until the controller is enabled; the counters are
        // still worth having.
        let dir = cgroup_with(THROTTLED, None);
        let cpu = CpuAccounting::read(dir.path()).expect("readable without cpu.max");
        assert_eq!(cpu.periods, 28);
        assert_eq!(cpu.quota_cores, None);
    }

    #[test]
    fn nothing_to_read_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(CpuAccounting::read(dir.path()).is_none());
    }

    #[test]
    fn unknown_fields_are_zero_not_an_error() {
        // Older kernels omit the burst fields; newer ones may add more.
        let dir = cgroup_with("usage_usec 500\n", Some("100000 100000\n"));
        let cpu = CpuAccounting::read(dir.path()).expect("readable");
        assert_eq!(cpu.usage_us, 500);
        assert_eq!(cpu.throttled_periods, 0);
    }

    #[test]
    fn a_window_is_the_difference_between_two_readings() {
        let before = CpuAccounting {
            usage_us: 1_000,
            periods: 10,
            throttled_periods: 1,
            throttled_us: 50,
            quota_cores: Some(1.0),
            period: std::time::Duration::from_millis(100),
        };
        let after = CpuAccounting {
            usage_us: 3_000,
            periods: 40,
            throttled_periods: 31,
            throttled_us: 2_050,
            ..before
        };
        let window = after.since(&before);
        assert_eq!(window.usage_us, 2_000);
        assert_eq!(window.periods, 30);
        assert_eq!(window.throttled_periods, 30);
        assert_eq!(window.throttled_us, 2_000);
        assert_eq!(window.quota_cores, Some(1.0), "the quota is carried over");
    }

    #[test]
    fn a_counter_that_went_backwards_does_not_underflow() {
        // The tenant cgroup is recreated on a restart, so a later reading can
        // be smaller than an earlier one.
        let later = CpuAccounting {
            usage_us: 5,
            periods: 1,
            throttled_periods: 0,
            throttled_us: 0,
            quota_cores: None,
            period: std::time::Duration::from_millis(100),
        };
        let earlier = CpuAccounting {
            usage_us: 9_000,
            periods: 90,
            throttled_periods: 9,
            throttled_us: 400,
            ..later
        };
        let window = later.since(&earlier);
        assert_eq!(window.usage_us, 0);
        assert_eq!(window.periods, 0);
    }

    #[test]
    fn saturation_is_a_share_of_periods_not_a_single_one() {
        let base = CpuAccounting {
            usage_us: 0,
            periods: 0,
            throttled_periods: 0,
            throttled_us: 0,
            quota_cores: Some(1.0),
            period: std::time::Duration::from_millis(100),
        };
        let idle = CpuAccounting {
            periods: 100,
            ..base
        };
        assert!(!idle.saturated(), "no throttling is not saturation");

        let noise = CpuAccounting {
            periods: 100,
            throttled_periods: 3,
            ..base
        };
        assert!(!noise.saturated(), "3% of periods is noise");

        // The measured run: 27 of 28 periods.
        let measured = CpuAccounting {
            periods: 28,
            throttled_periods: 27,
            ..base
        };
        assert!(measured.saturated());

        assert!(
            !base.saturated(),
            "a window with no periods cannot be judged"
        );
    }

    #[test]
    fn demand_is_cpu_time_over_wall_time() {
        let cpu = CpuAccounting {
            usage_us: 2_000_000,
            periods: 40,
            throttled_periods: 0,
            throttled_us: 0,
            quota_cores: Some(2.0),
            period: std::time::Duration::from_millis(100),
        };
        // Two seconds of CPU in four seconds of wall clock is half a core.
        let demand = cpu.demand_cores(std::time::Duration::from_secs(4));
        assert!((demand - 0.5).abs() < 1e-9, "got {demand}");
        assert_eq!(cpu.demand_cores(std::time::Duration::ZERO), 0.0);
    }

    fn resolved(entry: Option<&str>) -> ResolvedFn {
        let mut layer = Layer {
            image: Some("python:3.12-slim".into()),
            ..Default::default()
        };
        match entry {
            Some(e) => layer.entry = Some(PathBuf::from(e)),
            None => layer.cmd = Some(vec!["/bin/true".into()]),
        }
        resolve_standalone("demo", &layer, &ResolveOptions::default()).unwrap()
    }

    #[test]
    fn the_embedded_agent_is_the_real_one() {
        assert!(
            PYTHON_AGENT.contains("PROTOCOL_VERSION = 1"),
            "the embedded agent is not the reference agent"
        );
        assert!(
            PYTHON_AGENT.contains("gc.freeze()"),
            "the embedded agent lost the copy-on-write protection"
        );
        assert!(
            PYTHON_AGENT.contains("--fd"),
            "the embedded agent cannot take an inherited socket"
        );
    }

    #[test]
    fn installing_the_agent_is_idempotent_and_self_healing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(tmp.path());
        paths.ensure().unwrap();

        let first = Pool::install_agent(&paths).unwrap();
        assert_eq!(std::fs::read_to_string(&first).unwrap(), PYTHON_AGENT);

        let second = Pool::install_agent(&paths).unwrap();
        assert_eq!(first, second);

        // An agent left behind by an older build must be replaced, or the
        // protocol version it speaks may no longer match this one.
        std::fs::write(&first, "# stale agent from a previous release").unwrap();
        Pool::install_agent(&paths).unwrap();
        assert_eq!(std::fs::read_to_string(&first).unwrap(), PYTHON_AGENT);
    }

    #[test]
    fn the_agent_is_started_on_an_inherited_descriptor() {
        let argv = python_agent_argv(AGENT_IN_SANDBOX, HANDLER_IN_SANDBOX, "function");
        assert_eq!(argv[0], "python3");
        assert_eq!(argv[1], AGENT_IN_SANDBOX);
        assert_eq!(argv[2], "--fd");
        assert_eq!(argv[3], AGENT_FD.to_string());
        assert_eq!(argv[4], HANDLER_IN_SANDBOX);
        assert!(
            !argv.iter().any(|a| a.contains(".sock")),
            "a socket path would have to exist inside the sandbox: {argv:?}"
        );
    }

    #[test]
    fn the_agent_and_handler_are_mounted_read_only_alongside_the_specs_mounts() {
        use crate::spec::MountMode;

        let mut f = resolved(Some("/host/handler.py"));
        f.mounts = vec!["/host/cache:/cache:rw".parse().unwrap()];

        let mounts = agent_mounts(std::path::Path::new("/data/agents/zygo_agent.py"), &f);
        let find = |target: &str| {
            mounts
                .iter()
                .find(|m| m.target == std::path::Path::new(target))
        };

        let agent = find(AGENT_IN_SANDBOX).expect("the agent must be mounted");
        assert_eq!(agent.mode, MountMode::Ro);
        assert_eq!(agent.source, PathBuf::from("/data/agents/zygo_agent.py"));

        let handler = find(HANDLER_IN_SANDBOX).expect("the handler must be mounted");
        assert_eq!(handler.mode, MountMode::Ro, "tenant code is not writable");

        // And the spec's own mounts survive.
        assert!(find("/cache").is_some(), "the spec's mounts were dropped");
    }

    #[test]
    fn a_warm_exec_function_needs_no_handler_mount() {
        let f = resolved(None);
        let mounts = agent_mounts(std::path::Path::new("/data/agent.py"), &f);
        assert!(
            !mounts
                .iter()
                .any(|m| m.target == std::path::Path::new(HANDLER_IN_SANDBOX)),
            "warm-exec has no handler to mount"
        );
    }

    #[test]
    fn request_ids_are_unique_and_safe_as_cgroup_names() {
        let ids: Vec<String> = (0..1000).map(|_| next_request_id()).collect();
        let unique: std::collections::BTreeSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "request ids collided");

        for id in &ids {
            assert!(
                id.chars().all(|c| c.is_ascii_hexdigit()),
                "`{id}` is not safe as a directory name"
            );
        }
    }

    #[test]
    fn per_request_cgroups_are_on_by_default() {
        // Measured at 97 µs of a 1.9 ms request; the design's open question A2
        // resolves in favour of keeping them (docs/poc-report.md).
        assert!(PoolConfig::new(Paths::rooted("/x")).per_request_cgroup);
    }

    #[test]
    fn an_outcome_is_only_a_success_if_nothing_went_wrong() {
        let base = Outcome {
            exit_code: 0,
            result: serde_json::Value::Null,
            stdout: String::new(),
            stderr: String::new(),
            error: None,
            metrics: Metrics::default(),
            timed_out: false,
        };
        assert!(base.succeeded());
        assert!(
            !Outcome {
                exit_code: 1,
                ..base.clone()
            }
            .succeeded()
        );
        assert!(
            !Outcome {
                error: Some("ValueError".into()),
                ..base
            }
            .succeeded(),
            "a handler that raised did not succeed, whatever its exit code"
        );
    }
}
