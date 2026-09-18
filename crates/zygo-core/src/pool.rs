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
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0 && self.error.is_none()
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
    pub fn serve(&self, f: &ResolvedFn) -> Result<WarmFn> {
        use crate::image::{Reference, Store};
        use crate::sandbox::{SandboxConfig, mount};

        let store = Store::new(self.config.paths.clone());
        let reference: Reference = f.image.parse()?;
        let entry = store
            .get(&reference)
            .ok_or_else(|| crate::image::ImageError::NotPulled(f.image.clone()))?;

        // The agent and the handler join the spec's own mounts.
        let mut warm = f.clone();
        warm.mounts = agent_mounts(&self.agent_path, f);

        let mount_points = mount::required_mount_points(&warm.mounts);
        let overlay = crate::doctor::run()
            .checks
            .iter()
            .any(|c| c.name == "overlayfs (userns)" && c.status == crate::doctor::Status::Ok);
        let view = store.rootfs_view(&entry.layers, overlay, &mount_points)?;

        let argv = match &f.runtime {
            Some(crate::spec::Runtime::Builtin(crate::spec::BuiltinRuntime::Python)) => {
                python_agent_argv(AGENT_IN_SANDBOX, HANDLER_IN_SANDBOX, f.mode.as_str())
            }
            other => {
                return Err(Error::BackendUnavailable {
                    backend: "pool",
                    reason: format!(
                        "no warm agent for {}",
                        other
                            .as_ref()
                            .map(|r| r.to_string())
                            .unwrap_or_else(|| "warm-exec".into())
                    ),
                    remedy: "only the Python agent is wired up so far \
                             (todo.md, phases 2.3 and 3)"
                        .into(),
                });
            }
        };

        let newroot =
            self.config
                .paths
                .tmp()
                .join(format!("warm-{}-{}", f.name, std::process::id()));
        std::fs::create_dir_all(&newroot).at(&newroot)?;

        // A socket pair rather than a listening socket: no path, nothing on the
        // filesystem, and the sandbox cannot reach a second one.
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()
            .map_err(|e| Error::primitive("socketpair", "internal pool error", e))?;

        let mut config = SandboxConfig::from_resolved(&warm, &view, &newroot, argv, &[]);
        config.agent_fd = Some(std::os::fd::AsRawFd::as_raw_fd(&theirs));

        let tenant_cgroup = crate::cgroup::Hierarchy::discover()
            .ok()
            .map(|h| h.tenant(&f.name));

        let backend = crate::backend::for_isolation(f.isolation)?;
        let sandbox = backend.start(&config)?;
        // The sandbox owns its copy now; holding this end open would stop the
        // connection ever reporting end of stream.
        drop(theirs);

        let mut reader = crate::protocol::FrameReader::new(
            ours.try_clone()
                .map_err(|e| Error::primitive("dup socket", "internal pool error", e))?,
        );
        let (runtime, rss_kb, imports_ms) = read_ready(&mut reader)?;

        Ok(WarmFn {
            name: f.name.clone(),
            wire: Mutex::new(Wire {
                reader,
                writer: crate::protocol::FrameWriter::new(ours),
            }),
            runtime,
            rss_kb,
            imports_ms,
            counters: Mutex::new(Counters::default()),
            state: Mutex::new(SandboxState::Warm),
            tenant_cgroup,
            per_request_cgroup: self.config.per_request_cgroup,
            _sandbox: sandbox,
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
    wire: Mutex<Wire>,
    /// Announced by the agent in its `READY`.
    runtime: String,
    rss_kb: u64,
    imports_ms: f64,
    counters: Mutex<Counters>,
    /// `Warm` until something goes wrong. Behind a lock because a request that
    /// finds the agent wedged has to record that for every later caller.
    state: Mutex<SandboxState>,
    /// The tenant's cgroup: where its limits are enforced and its CPU is
    /// accounted, and where per-request cgroups are created when they are.
    tenant_cgroup: Option<PathBuf>,
    per_request_cgroup: bool,
    /// Kept alive: dropping it kills the sandbox.
    _sandbox: Box<dyn crate::backend::Sandbox>,
}

/// The framed connection, with its reader and writer kept together so a request
/// cannot interleave with another one's reply.
struct Wire {
    reader: crate::protocol::FrameReader<std::os::unix::net::UnixStream>,
    writer: crate::protocol::FrameWriter<std::os::unix::net::UnixStream>,
}

impl WarmFn {
    pub fn name(&self) -> &str {
        &self.name
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
    /// Blocks until the handler finishes or its deadline passes. Concurrency is
    /// the caller's: a `WarmFn` serialises requests on its own connection,
    /// because the wire protocol is request/response per agent.
    pub fn call(&self, event: serde_json::Value) -> Result<Outcome> {
        self.call_with_timeout(event, self.default_timeout())
    }

    fn default_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(30)
    }

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
        let mut wire = self.wire.lock().expect("wire");
        let locked = Instant::now();

        if let Err(e) = wire.writer.write(&Message::Exec {
            id: id.clone(),
            event,
            timeout_ms: timeout.as_millis() as u64,
            env_overrides: BTreeMap::new(),
        }) {
            return self.broken(ProtocolError::from(e));
        }

        // The agent forks and reports the child's pid, then waits.
        let forked_reply = match wire.reader.read() {
            Ok(reply) => reply,
            Err(e) => return self.broken(ProtocolError::from(e)),
        };
        let pid = match forked_reply {
            Some(Message::Forked { pid, .. }) => pid,
            // The agent refusing a request is not a broken agent: it answered,
            // and the connection is still in step for the next one.
            Some(Message::Error { code, message, .. }) => {
                self.record(false);
                return Err(ProtocolError::Agent { code, message }.into());
            }
            other => {
                return self.broken(ProtocolError::Unexpected {
                    expected: "FORKED",
                    found: other.map(|m| m.kind()).unwrap_or("end of stream"),
                });
            }
        };
        let forked = Instant::now();

        // The window the handshake exists for: the child is alive but has run
        // no tenant code, so this is the only moment its limits can be set.
        let request_cgroup = self.admit(&id, pid);
        let admitted = Instant::now();

        if let Err(e) = wire.writer.write(&Message::Go { id: id.clone() }) {
            return self.broken(ProtocolError::from(e));
        }

        // From here tenant code is running, so the deadline is ours to enforce.
        // Passing `timeout_ms` to the agent is a courtesy, not a control: the
        // agent's own children are the thing being limited, so trusting it to
        // stop them is trusting the blast radius to contain itself.
        let deadline = timeout.saturating_sub(admitted - started);
        if !wait_readable(&wire, deadline)? {
            self.enforce_deadline(request_cgroup.as_deref(), pid);
            // The child is dead, so the agent sees EOF on the result pipe and
            // sends `DONE` by itself — which is what keeps the connection in
            // step for the next request instead of leaving a reply in flight.
            if !wait_readable(&wire, KILL_GRACE)? {
                return self.broken(Error::BackendUnavailable {
                    backend: "pool",
                    reason: format!(
                        "`{}` did not answer after its request was killed; \
                         the agent is not responding",
                        self.name
                    ),
                    remedy: "the function will be rewarmed; \
                             check the handler for something that blocks uninterruptibly"
                        .into(),
                });
            }
        }

        let done_reply = match wire.reader.read() {
            Ok(reply) => reply,
            Err(e) => return self.broken(ProtocolError::from(e)),
        };
        let outcome = match done_reply {
            Some(Message::Done {
                exit_code,
                result,
                stdout,
                stderr,
                error,
                metrics,
                ..
            }) => Ok(Outcome {
                exit_code,
                result,
                stdout,
                stderr,
                error,
                metrics,
            }),
            Some(Message::Error { code, message, .. }) => {
                Err(Error::from(ProtocolError::Agent { code, message }))
            }
            other => {
                // Nothing to clean up on this path: the reply was not a reply,
                // so the connection's position is unknown.
                return self.broken(ProtocolError::Unexpected {
                    expected: "DONE",
                    found: other.map(|m| m.kind()).unwrap_or("end of stream"),
                });
            }
        };

        let done = Instant::now();

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

    /// Whether this function needs replacing rather than another request.
    pub fn is_healthy(&self) -> bool {
        self.state() == SandboxState::Warm
    }

    /// Kill a request that overran its deadline.
    ///
    /// The request cgroup is the good path: `cgroup.kill` takes the child and
    /// everything it spawned in a single write, so a handler that forked its own
    /// helpers cannot leave them behind. Without one — `--no-cgroup`, or a
    /// kernel below 5.14 — all we have is the pid the agent reported, which
    /// misses grandchildren. That is a reason to keep per-request cgroups on,
    /// not a reason to skip the kill.
    fn enforce_deadline(&self, request_cgroup: Option<&std::path::Path>, pid: u32) {
        if let Some(dir) = request_cgroup {
            if let Ok(true) = crate::cgroup::kill(dir) {
                return;
            }
        }
        // SAFETY: `pid` came from the agent's `FORKED` for a child that has not
        // been reaped, so the number still refers to that process.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    }

    /// Put the forked child in its own cgroup. Returns the directory to remove
    /// afterwards, if one was created.
    fn admit(&self, id: &str, pid: u32) -> Option<PathBuf> {
        if !self.per_request_cgroup {
            return None;
        }
        let tenant = self.tenant_cgroup.as_ref()?;
        let dir = tenant.join(format!("req-{id}"));
        if std::fs::create_dir(&dir).is_err() {
            return None;
        }
        // Failing to move the pid is not worth failing the request over: the
        // child is still inside the *tenant* cgroup, so the tenant's limits
        // still apply. What is lost is only the per-request accounting.
        let _ = crate::cgroup::attach(&dir, pid);
        Some(dir)
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
        let mut wire = self.wire.lock().expect("wire");
        wire.writer
            .write(&Message::Shutdown { grace_ms: 5_000 })
            .map_err(ProtocolError::from)?;
        Ok(())
    }
}

/// Where one request's time went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallTiming {
    /// Waiting for the connection: another request was in flight.
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

/// Wait until the agent has something to say, or `timeout` passes.
///
/// `poll` rather than a read timeout on the socket: a read that times out
/// part-way through a frame loses the bytes it consumed and leaves the
/// connection out of step, whereas waiting for readability first means the
/// following `read` runs against a frame that is already there. The agent
/// writes each frame with a single flushed `write_all`, so in practice the
/// whole frame arrives together.
fn wait_readable(wire: &Wire, timeout: std::time::Duration) -> Result<bool> {
    use std::os::fd::AsRawFd;

    let mut fds = libc::pollfd {
        fd: wire.reader.get_ref().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ms = left.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: one live `pollfd` describing a descriptor the `Wire` owns.
        let rc = unsafe { libc::poll(&raw mut fds, 1, ms) };
        match rc {
            1 => return Ok(true),
            0 if left.is_zero() => return Ok(false),
            // A signal arrived; the deadline has not moved, so go round again.
            -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            0 => {}
            _ => {
                return Err(Error::primitive(
                    "poll",
                    "agent connection",
                    std::io::Error::last_os_error(),
                ));
            }
        }
    }
}

/// Monotonic request ids. Short, because they become cgroup directory names.
fn next_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{:08x}", NEXT.fetch_add(1, Ordering::Relaxed))
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
    use super::*;
    use crate::spec::{Layer, ResolveOptions, resolve_standalone};

    // --- the request deadline ---------------------------------------------
    //
    // `wait_readable` is what makes a deadline possible at all: without it the
    // supervisor blocks in `read` for as long as a handler cares to loop.

    fn wire_pair() -> (Wire, std::os::unix::net::UnixStream) {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let wire = Wire {
            reader: crate::protocol::FrameReader::new(ours.try_clone().expect("dup")),
            writer: crate::protocol::FrameWriter::new(ours),
        };
        (wire, theirs)
    }

    #[test]
    fn a_silent_agent_times_out_rather_than_blocking() {
        let (wire, _agent) = wire_pair();
        let started = Instant::now();
        let ready = wait_readable(&wire, std::time::Duration::from_millis(80)).expect("poll");
        assert!(!ready, "nothing was sent, so nothing should be readable");
        assert!(
            started.elapsed() >= std::time::Duration::from_millis(70),
            "returned early: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_reply_already_waiting_is_seen_immediately() {
        let (wire, agent) = wire_pair();
        crate::protocol::FrameWriter::new(&agent)
            .write(&Message::Ping { seq: 1 })
            .expect("write");

        let started = Instant::now();
        assert!(wait_readable(&wire, std::time::Duration::from_secs(5)).expect("poll"));
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "waited for a message that was already there"
        );
    }

    #[test]
    fn a_reply_that_arrives_during_the_wait_is_seen() {
        let (wire, agent) = wire_pair();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(40));
            let _ = crate::protocol::FrameWriter::new(&agent).write(&Message::Pong { seq: 1 });
        });
        assert!(wait_readable(&wire, std::time::Duration::from_secs(5)).expect("poll"));
    }

    #[test]
    fn an_agent_that_died_is_readable_not_a_timeout() {
        // A closed peer makes the descriptor readable with zero bytes, which is
        // how `read` gets to return `None` and the caller gets to say "the agent
        // is gone" instead of waiting out the whole deadline.
        let (wire, agent) = wire_pair();
        drop(agent);
        let started = Instant::now();
        assert!(wait_readable(&wire, std::time::Duration::from_secs(5)).expect("poll"));
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
    }

    #[test]
    fn a_zero_deadline_does_not_block() {
        // What a request whose budget was already spent getting here asks for.
        let (wire, _agent) = wire_pair();
        assert!(!wait_readable(&wire, std::time::Duration::ZERO).expect("poll"));
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
        let find = |target: &str| mounts.iter().find(|m| m.target == std::path::Path::new(target));

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
