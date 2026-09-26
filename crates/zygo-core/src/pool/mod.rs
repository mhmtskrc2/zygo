// SPDX-License-Identifier: Apache-2.0
//! The warm pool — `Pool`, `WarmFn`.
//!
//! This is the product. Everything before it exists to get here: a sandbox that
//! is already built when the request arrives, so serving one costs a `fork()`
//! and a cgroup rather than a container.
//!
//! The library is the API and the CLI is a client of it (the API is the surface,
//! not the CLI: `docs/book/adr/0001-embedded-runtime.md`), so a platform
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
//!
//! ## The module
//!
//! This file is the pool itself: [`Pool`], which turns a resolved spec into
//! a warm [`Function`]. The rest lives beside it, by what it is about:
//!
//! - `agent` — the agents carried in the binary, their paths and `READY`.
//! - `warm_fn` — a function with an agent in it: the `EXEC`→`DONE` path.
//! - `conn` — the socket to that agent, with replies routed by request id.
//! - `warm_exec` — a held sandbox entered per request (Linux only).
//! - `function` — the one type the supervisor sees for both shapes.
//! - `request` — a request in flight: its id, cancel, cgroup and files.
//! - `secrets`, `scripts` — what is written into the sandbox per request.
//! - `logs`, `outcome`, `timing` — what comes back out, and what it cost.
//! - `oneshot` — a sandbox started for a client and not waited on.
//!
//! Every public item is re-exported here, so a caller names
//! `zygo_core::pool::WarmFn` and never a file.

mod agent;
mod conn;
mod function;
mod logs;
mod oneshot;
mod outcome;
mod request;
mod scripts;
mod secrets;
mod timing;
#[cfg(target_os = "linux")]
mod warm_exec;
mod warm_fn;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::{Error, IoContext, Result};
use crate::paths::Paths;
use crate::sandbox::SandboxState;
use crate::spec::ResolvedFn;

pub use agent::{
    AGENT_FD, AGENT_IN_SANDBOX, AGENT_READY_TIMEOUT, BuiltinAgent, HANDLER_IN_SANDBOX, NODE_AGENT,
    PYTHON_AGENT, agent_failure, agent_mounts, read_ready,
};
pub use function::Function;
pub use logs::{LOG_ENTRIES, LOG_TEXT_BYTES, LogEntry, LogKind, LogRing, Logs};
pub use oneshot::{ClientStreams, Oneshot};
pub use outcome::{Outcome, Usage};
pub use request::{Call, ChunkSink, InFlight, KILL_GRACE, Workspace};
pub use scripts::{SCRIPT_DIR_IN_SANDBOX, SCRIPT_DIR_MODE};
pub use secrets::SECRETS_DIR_IN_SANDBOX;
pub use timing::{CallTiming, CpuAccounting, WarmupTiming};
#[cfg(target_os = "linux")]
pub use warm_exec::WarmExec;
pub use warm_fn::{FORK_TIMEOUT, HEARTBEAT_GRACE, STREAM_BYTES, WarmFn};

use agent::is_timeout;
use conn::Conn;
use logs::spawn_zygote_log_reader;
use request::Requests;
use scripts::{Scripts, ensure_script_dir, new_script_dir};
use secrets::Secrets;

/// How a pool is configured.
#[derive(Debug, Clone, Default)]
pub struct PoolConfig {
    pub paths: Paths,
    /// Give each request its own cgroup.
    ///
    /// Bought for `cgroup.kill`, which tears down a timed-out request's whole
    /// tree in one write. On by default, and the default is **settled**.
    ///
    /// It was reopened on a measurement that did not survive the hardware it
    /// was repeated on. Under nested virtualisation the `admit` phase reaches
    /// 11–12 ms at p99 and the run misses its p99 budget; on bare metal — a
    /// Raspberry Pi, kernel 6.5, 1000 requests at 100 req/s, well under the
    /// host's capacity — the whole cost is 238 µs at p50 and 360 µs at p99,
    /// `admit` never exceeds 838 µs, and both configurations pass. A cgroup
    /// `mkdir` and `rmdir` per request is expensive in a VM inside a VM and
    /// cheap on a kernel running on metal.
    ///
    /// So `false` is a flag for measuring what this costs, not a production
    /// choice: it saves a quarter of a millisecond and gives up per-request
    /// containment.
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

/// Live state of one warm function, as `zygo ps` shows it.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Status {
    pub name: String,
    /// The image the spec named, as written there. What `zygo image rm`
    /// checks before it removes one: a warm function's rootfs is that
    /// image's layers, mounted, and pulling them out from under it is not a
    /// removal but a corruption. Defaulted for answers from a supervisor
    /// that predates the field.
    #[serde(default)]
    pub image: String,
    /// Whose function this is, as [`crate::spec::resolve`] settled it —
    /// `"default"` when nobody said.
    ///
    /// Here so that a listing can be filtered to one customer: the supervisor
    /// enforces ownership on every route that *acts* on a function, and this
    /// is what lets the route that only *shows* them do the same. Defaulted
    /// for answers from a supervisor that predates the field.
    #[serde(default)]
    pub tenant: String,
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

/// Which warm mode a function uses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// An agent in the box gives each request its own process.
    Agent(BuiltinAgent),
    /// The sandbox is held; each request is a fresh process running `cmd`.
    Exec,
}

/// Owns the warm sandboxes.
pub struct Pool {
    config: PoolConfig,
    /// Paths the embedded agents were written to, once.
    python_agent: PathBuf,
    node_agent: PathBuf,
}

impl Pool {
    pub fn new(config: PoolConfig) -> Result<Pool> {
        config.paths.ensure()?;
        let python_agent = Self::install_agent(&config.paths, BuiltinAgent::Python)?;
        let node_agent = Self::install_agent(&config.paths, BuiltinAgent::Node)?;
        Ok(Pool {
            config,
            python_agent,
            node_agent,
        })
    }

    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// Where the embedded Python agent lives on the host.
    pub fn agent_path(&self) -> &std::path::Path {
        &self.python_agent
    }

    /// Where the given embedded agent lives on the host.
    pub fn agent_path_for(&self, agent: BuiltinAgent) -> &std::path::Path {
        match agent {
            BuiltinAgent::Python => &self.python_agent,
            BuiltinAgent::Node => &self.node_agent,
        }
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
                remedy: "use `system = [...]` (apt) for now".into(),
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
        // After the venv, which is keyed on the image it was built for. See
        // `crate::bytecode`: the slim images ship no `.pyc`.
        let entry = crate::bytecode::ensure(&store, &entry)?.image;

        // Which of the two warm modes this is. A runtime
        // means an agent in the box that forks per request; none means
        // warm-exec — the sandbox is held and each request is a fresh process
        // running `cmd` with the event on stdin.
        let mode = match &f.runtime {
            Some(runtime) => match BuiltinAgent::for_runtime(runtime) {
                Some(agent) => Mode::Agent(agent),
                None => {
                    return Err(Error::BackendUnavailable {
                        backend: "pool",
                        reason: format!("no warm agent for {runtime}"),
                        remedy: "the Python and Node agents and warm-exec (`cmd`) are wired \
                                 up so far"
                            .into(),
                    });
                }
            },
            None if !f.cmd.is_empty() => Mode::Exec,
            None => {
                return Err(Error::BackendUnavailable {
                    backend: "pool",
                    reason: "a function with neither runtime nor cmd".into(),
                    remedy: "the Python and Node agents and warm-exec (`cmd`) are wired up \
                             so far"
                        .into(),
                });
            }
        };

        // A pool zygote holds no tenant code, and this is where that stops
        // being an intention and becomes a fact: nothing of the tenant's is
        // mounted, the agent is started without a handler argument, and the
        // only code that reaches the sandbox afterwards arrives per request
        // and is loaded in the child. Asserted rather than assumed, because
        // every isolation claim about a shared pool rests on it.
        //
        // `cmd` is the exception and not a hole: a **warm-exec pool** runs one
        // program the operator named — `sh`, a static binary — and the script
        // arrives as the last word of its command line. Nothing of the
        // tenant's is warmed into the sandbox there either; what differs is
        // that the request's code is `execve`d rather than imported.
        if f.is_pool() {
            debug_assert!(f.entry.is_none());
            if f.entry.is_some() {
                return Err(Error::BackendUnavailable {
                    backend: "pool",
                    reason: format!(
                        "`{}` is a runtime pool and was given a handler to warm",
                        f.name
                    ),
                    remedy: "a pool's zygotes are shared between tenants, so nothing \
                             may be imported into one"
                        .into(),
                });
            }
        }

        let mut warm = f.clone();
        warm.mounts = match mode {
            Mode::Agent(agent) => agent_mounts(agent, self.agent_path_for(agent), f),
            Mode::Exec => f.mounts.clone(),
        };

        // Where this sandbox's requests will find their own code (protocol
        // 1.1). A directory of its own on the host, bound in **read-only**:
        // the supervisor writes the scripts, and nothing inside can change
        // one. See `SCRIPT_DIR_IN_SANDBOX` for why it has to be a mount.
        //
        // Made for every agent sandbox rather than only for pools, because a
        // function can be sent a script too — `entry` is the fast path, not
        // the only one — and an empty directory costs an inode. A warm-exec
        // *pool* needs it for the same reason and one more: its script has to
        // be a file, because it is named on a command line, so there is no
        // `source` shape to fall back to.
        let script_dir = new_script_dir(&self.config.paths, &f.name);
        if matches!(mode, Mode::Agent(_)) || f.is_pool() {
            ensure_script_dir(&script_dir)?;
            warm.mounts.push(crate::spec::Mount {
                source: script_dir.clone(),
                target: PathBuf::from(SCRIPT_DIR_IN_SANDBOX),
                mode: crate::spec::MountMode::Ro,
            });
        }
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
            Mode::Agent(agent) => agent.argv(f.mode.as_str(), f.entry.is_some()),
            Mode::Exec => f.cmd.clone(),
        };

        let mount_points = mount::required_mount_points(&warm.mounts);
        let overlay = crate::doctor::cached(&self.config.paths)
            .checks
            .iter()
            .any(|c| c.name == "overlayfs (userns)" && c.status == crate::doctor::Status::Ok);
        let view = store.rootfs_view(&entry.layers, overlay, &mount_points)?;

        let newroot =
            self.config
                .paths
                .tmp()
                .join(format!("warm-{}-{}", f.name, crate::process_token()));
        std::fs::create_dir_all(&newroot).at(&newroot)?;

        let mut config = SandboxConfig::from_resolved(&warm, &view, &newroot, argv, &image_env);
        config.allow_resolved = net.allowed;
        config.pasta_pid_file = net.pid_file;
        // Under `strict`, the agent's forked child installs a second filter —
        // no new program, no new process — before the handler runs. Built
        // here because the syscall numbers are the host's, handed over as
        // bytes because the agent may be written in anything.
        #[cfg(target_os = "linux")]
        if matches!(mode, Mode::Agent(_))
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
            .map(|h| h.function(&f.tenant, &f.name));
        let backend = crate::backend::for_isolation(f.isolation, &self.config.paths)?;

        match mode {
            Mode::Exec => self.serve_exec(f, config, backend.as_ref(), tenant_cgroup, &script_dir),
            Mode::Agent(_) => {
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
                    tenant: f.tenant.clone(),
                    image: f.image.clone(),
                    conn,
                    replies: Mutex::new(Some(replies)),
                    socket: ours,
                    runtime,
                    rss_kb,
                    imports_ms,
                    counters: Mutex::new(Counters::default()),
                    in_flight: Requests::default(),
                    state: Mutex::new(SandboxState::Warm),
                    tenant_cgroup,
                    generation_cgroup: generation,
                    per_request_cgroup: self.config.per_request_cgroup,
                    timeout: f.limits.timeout.get(),
                    limits: f.limits.clone(),
                    agent_host_pid: sandbox.pid(),
                    secrets: Mutex::new(Secrets::default()),
                    scripts: Mutex::new(Scripts::default()),
                    script_dir,
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
        script_dir: &std::path::Path,
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
            tenant: f.tenant.clone(),
            image: f.image.clone(),
            init_pid: sandbox.pid(),
            secrets_dir,
            plan,
            sandbox: Mutex::new(sandbox),
            counters: Mutex::new(Counters::default()),
            in_flight: Requests::default(),
            state: Mutex::new(SandboxState::Warm),
            tenant_cgroup,
            generation_cgroup: generation,
            per_request_cgroup: self.config.per_request_cgroup,
            timeout: f.limits.timeout.get(),
            limits: f.limits.clone(),
            secrets: Mutex::new(Secrets::default()),
            scripts: Mutex::new(Scripts::default()),
            script_dir: script_dir.to_path_buf(),
        })))
    }

    #[cfg(not(target_os = "linux"))]
    fn serve_exec(
        &self,
        _f: &ResolvedFn,
        _config: crate::sandbox::SandboxConfig,
        _backend: &dyn crate::backend::Backend,
        _tenant_cgroup: Option<PathBuf>,
        _script_dir: &std::path::Path,
    ) -> Result<Function> {
        Err(Error::BackendUnavailable {
            backend: "pool",
            reason: "warm-exec enters Linux namespaces".into(),
            remedy: "run Zygo inside a Linux VM or container; on macOS the `zygo` \
                 binary normally forwards into one it manages"
                .into(),
        })
    }

    /// Write the embedded agent out, if it is not already there and current.
    ///
    /// Rewritten whenever the contents differ so an upgraded binary does not
    /// keep running the previous release's agent against the current protocol.
    fn install_agent(paths: &Paths, agent: BuiltinAgent) -> Result<PathBuf> {
        let path = paths.data().join(agent.host_relative());
        let dir = path.parent().expect("host_relative has a directory");
        std::fs::create_dir_all(dir).at(dir)?;
        let name = path.file_name().expect("host_relative names a file");

        let current = std::fs::read_to_string(&path).unwrap_or_default();
        if current != agent.source() {
            // Written to a temporary file and renamed, so a sandbox starting
            // concurrently never reads a half-written agent.
            let tmp = dir.join(format!(
                "{}.{}",
                name.to_string_lossy(),
                crate::process_token()
            ));
            let mut file = std::fs::File::create(&tmp).at(&tmp)?;
            file.write_all(agent.source().as_bytes()).at(&tmp)?;
            file.flush().at(&tmp)?;
            drop(file);
            std::fs::rename(&tmp, &path).at(&path)?;
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_request_cgroups_are_on_by_default() {
        // Measured at 97 µs of a 1.9 ms request, which is cheap enough to keep
        // them on.
        assert!(PoolConfig::new(Paths::rooted("/x")).per_request_cgroup);
    }

    #[test]
    fn installing_the_agent_is_idempotent_and_self_healing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(tmp.path());
        paths.ensure().unwrap();

        for agent in [BuiltinAgent::Python, BuiltinAgent::Node] {
            let first = Pool::install_agent(&paths, agent).unwrap();
            assert_eq!(std::fs::read_to_string(&first).unwrap(), agent.source());

            let second = Pool::install_agent(&paths, agent).unwrap();
            assert_eq!(first, second);

            // An agent left behind by an older build must be replaced, or the
            // protocol version it speaks may no longer match this one.
            std::fs::write(&first, "# stale agent from a previous release").unwrap();
            Pool::install_agent(&paths, agent).unwrap();
            assert_eq!(std::fs::read_to_string(&first).unwrap(), agent.source());
        }

        // And they are separate files: installing one must not overwrite the
        // other.
        assert_ne!(
            Pool::install_agent(&paths, BuiltinAgent::Python).unwrap(),
            Pool::install_agent(&paths, BuiltinAgent::Node).unwrap()
        );
    }
}
