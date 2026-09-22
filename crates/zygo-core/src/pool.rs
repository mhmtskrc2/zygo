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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
///
/// `../agents` is `crates/zygo-core/agents`, a symlink to the repository's
/// `agents/` directory. It has to be reached from inside the crate rather
/// than as `../../../agents`: `cargo package` includes only what is under the
/// package root, so a path that climbs out of it compiles in a checkout and
/// fails for everyone who runs `cargo install zygo-cli`.
pub const PYTHON_AGENT: &str = include_str!("../agents/python/zygo_agent.py");

/// The reference Node agent, carried the same way.
///
/// Node has no `fork()`, so it keeps a pool of pre-loaded workers instead of
/// forking a zygote. Everything above that — the wire, the cgroup window,
/// secrets, the `strict` child filter — is identical, which is the point of
/// having a protocol rather than an interface.
pub const NODE_AGENT: &str = include_str!("../agents/node/zygo_agent.js");

/// One of the agents Zygo carries inside the binary.
///
/// Each is a file in the data directory, a mount inside the sandbox and an
/// argv; nothing else about a runtime reaches the supervisor, which is what
/// keeps adding one small.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinAgent {
    Python,
    Node,
}

impl BuiltinAgent {
    /// The agent Zygo ships for this runtime, or `None` for one it does not.
    pub fn for_runtime(runtime: &crate::spec::Runtime) -> Option<BuiltinAgent> {
        use crate::spec::{BuiltinRuntime, Runtime};
        match runtime {
            Runtime::Builtin(BuiltinRuntime::Python) => Some(BuiltinAgent::Python),
            Runtime::Builtin(BuiltinRuntime::Node) => Some(BuiltinAgent::Node),
            _ => None,
        }
    }

    pub fn source(self) -> &'static str {
        match self {
            BuiltinAgent::Python => PYTHON_AGENT,
            BuiltinAgent::Node => NODE_AGENT,
        }
    }

    /// Where it is written on the host, relative to the data directory.
    pub fn host_relative(self) -> &'static str {
        match self {
            BuiltinAgent::Python => "agents/python/zygo_agent.py",
            BuiltinAgent::Node => "agents/node/zygo_agent.js",
        }
    }

    /// Where it is bind-mounted inside every sandbox.
    ///
    /// The extension matters: `require` and `import` both decide what a file
    /// is by its name.
    pub fn agent_in_sandbox(self) -> &'static str {
        match self {
            BuiltinAgent::Python => AGENT_IN_SANDBOX,
            BuiltinAgent::Node => "/zygo/agent.js",
        }
    }

    /// Where the tenant's handler is bind-mounted inside every sandbox.
    pub fn handler_in_sandbox(self) -> &'static str {
        match self {
            BuiltinAgent::Python => HANDLER_IN_SANDBOX,
            BuiltinAgent::Node => "/zygo/handler.js",
        }
    }

    /// Argv that starts this agent inside a sandbox.
    ///
    /// `--fd` rather than a socket path: nothing has to exist in the
    /// sandbox's filesystem, so no bind mount and no assumption about the
    /// image.
    ///
    /// Without a handler this is a **runtime pool**: the agent is told no
    /// tenant code at all and every request brings its own `script`. The
    /// argument is absent rather than empty, because the agent decides which
    /// shape it is by whether it was given one (`spec/protocol.md` §2).
    pub fn argv(self, mode: &str, handler: bool) -> Vec<String> {
        let interpreter = match self {
            BuiltinAgent::Python => "python3",
            BuiltinAgent::Node => "node",
        };
        let mut argv = vec![
            interpreter.to_string(),
            self.agent_in_sandbox().to_string(),
            "--fd".to_string(),
            AGENT_FD.to_string(),
        ];
        if handler {
            argv.push(self.handler_in_sandbox().to_string());
            argv.push(mode.to_string());
        }
        argv
    }
}

/// How a pool is configured.
#[derive(Debug, Clone, Default)]
pub struct PoolConfig {
    pub paths: Paths,
    /// Give each request its own cgroup (design doc open question A2).
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
    /// The agent stopped saying this request was alive, and it was killed.
    ///
    /// The fourth reading of exit 137, and the one that is not about the
    /// caller's limits at all. A deadline says the work is too slow; an
    /// out-of-memory kill says it is too big; a cancel says somebody changed
    /// their mind. This says the *sandbox* went quiet — an agent that stopped
    /// scheduling, a child wedged where no signal it can send will reach it —
    /// and the advice that follows is about the function, not about the
    /// number in its spec.
    ///
    /// Only reachable for a request long enough to miss a heartbeat. A short
    /// one that wedges is killed by its own deadline, which is the older
    /// backstop and is still the right one when the budget is seconds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stuck: bool,
    /// Somebody asked for this request to stop, and it did.
    ///
    /// The third reading of exit 137, and the same argument as `timed_out`:
    /// a cancel kill, a deadline kill and an out-of-memory kill are one signal
    /// and three different things to tell a caller. Only the side that sent
    /// the signal knows which.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    /// The request's workspace, packed as a tar, when `?out=1` asked for it.
    ///
    /// Base64, because this crosses a JSON control socket and then a JSON HTTP
    /// answer. A tar is bytes and JSON has no way to carry bytes; the
    /// alternative is a second transport for the one route that needs one,
    /// which is more moving parts than the encoding costs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Who this request was for, and what it ran.
    ///
    /// Here rather than reconstructed by whoever wants to bill for it: the
    /// supervisor is the only side that knows all three at once, and a
    /// caller's own tenant and function are not facts it needs protecting
    /// from. See [`Usage`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tenant: String,
    /// The function or runtime pool the request ran in.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub function: String,
    /// The script's digest, for a pool request that named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// The request's own id, which is what `DELETE /requests/<id>` names.
    ///
    /// Returned with the answer as well as in a header, so a caller that kept
    /// the body has what it needs to cancel a *later* identical call — and so
    /// a log line about a slow request can be joined to the request itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0 && self.error.is_none()
    }

    /// One word for how this request ended.
    ///
    /// The four kills exit 137 can mean are already separate fields; this is
    /// the single value something counting requests groups by, so that a
    /// dashboard does not have to re-derive the precedence every time.
    ///
    /// Order matters and is the caller's: `cancelled` first, because a caller
    /// who stopped their own request does not want to read "timeout".
    pub fn outcome(&self) -> &'static str {
        if self.cancelled {
            "cancelled"
        } else if self.stuck {
            "stuck"
        } else if self.timed_out {
            "timeout"
        } else if self.succeeded() {
            "ok"
        } else {
            "error"
        }
    }
}

/// What one finished request cost, and for whom.
///
/// Built from an [`Outcome`] by whoever is counting. It exists as a type
/// rather than a JSON literal in three places because an embedder bills from
/// it, and a field that means one thing in the OTLP export and another in the
/// webhook is a support question nobody can answer.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub tenant: String,
    pub function: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    pub request_id: String,
    pub wall_ms: f64,
    pub cpu_ms: f64,
    pub peak_rss_kb: u64,
    /// `ok`, `error`, `timeout`, `cancelled` or `stuck`. See
    /// [`Outcome::outcome`].
    pub outcome: String,
    /// When the request finished, as milliseconds since the epoch.
    ///
    /// Wall-clock rather than a monotonic reading, because it leaves this
    /// process and has to mean something to whoever receives it.
    pub finished_ms: u64,
}

impl From<&Outcome> for Usage {
    fn from(o: &Outcome) -> Usage {
        Usage {
            tenant: o.tenant.clone(),
            function: o.function.clone(),
            script: o.script.clone(),
            request_id: o.id.clone(),
            wall_ms: o.metrics.wall_ms,
            cpu_ms: o.metrics.cpu_ms,
            peak_rss_kb: o.metrics.peak_rss_kb,
            outcome: o.outcome().to_string(),
            finished_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }
}

/// One request that has been sent and not yet answered.
///
/// A cancel arrives on a different connection, on a different thread, while
/// the request's own thread is blocked waiting for `DONE`. This is what the
/// two share: a flag the waiting thread reads once it wakes, and where to kill
/// the work.
///
/// **The kill is the cancel.** Writing `cgroup.kill` from outside the sandbox
/// is what stops the request; the `CANCEL` frame only tells the agent why, and
/// an agent that ignores it changes nothing. Trusting tenant code to stop
/// itself on request is trusting the blast radius to contain itself — the same
/// argument that makes `timeout_ms` a courtesy rather than a control.
#[derive(Debug, Default)]
pub struct InFlight {
    /// A name the *caller* chose for this request, if they chose one.
    ///
    /// The id is assigned here and only reaches the caller with the answer,
    /// which is too late to cancel the call it belongs to. A caller that wants
    /// to be able to stop its own request before it finishes has to name it on
    /// the way in, and this is that name.
    ///
    /// It is not an id and does not have to be unique: two requests sharing a
    /// key are two requests one cancel stops, which is what a caller who
    /// reused a key meant. Ownership is what keeps it safe — a key only ever
    /// matches the requests of whoever sent it.
    key: Option<String>,
    /// Whose request this is, when it came from a tenant.
    ///
    /// Here because a request id is a **counter**, not a secret: anybody who
    /// has seen one can count. That is fine for an id, which is a name rather
    /// than a capability — the same argument a script digest gets — but it
    /// means the owner has to be recorded, or a tenant sharing a pool could
    /// cancel another tenant's request by counting up from their own.
    owner: Option<String>,
    /// Somebody asked for this request to stop.
    cancelled: AtomicBool,
    /// Where the work is, once there is any.
    ///
    /// `None` between the `EXEC` and the `GO`: the child exists but has run
    /// nothing, and a cancel that lands in that window is answered by never
    /// sending `GO` rather than by killing something that has not started.
    target: Mutex<Option<Target>>,
}

#[derive(Debug)]
struct Target {
    cgroup: Option<PathBuf>,
    host_pid: u32,
}

impl InFlight {
    /// Whether `caller` may cancel this. The operator may cancel anything on
    /// their own host; a tenant may cancel only their own.
    fn is_for(&self, caller: Option<&str>) -> bool {
        match (caller, self.owner.as_deref()) {
            (None, _) => true,
            (Some(caller), Some(owner)) => caller == owner,
            (Some(_), None) => false,
        }
    }

    /// Whether `name` is this request's key.
    fn keyed(&self, name: &str) -> bool {
        self.key.as_deref() == Some(name)
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// Mark it cancelled and kill it if there is anything to kill.
    ///
    /// Returns whether the work had started. `false` means the child is still
    /// parked waiting for `GO`, and the request's own thread will deal with it
    /// — which is the better outcome, because then no tenant code ran at all.
    fn cancel(&self) -> bool {
        self.cancelled.store(true, Ordering::SeqCst);
        match self.target.lock().expect("target").as_ref() {
            Some(target) => {
                kill_request(target.cgroup.as_deref(), target.host_pid);
                true
            }
            None => false,
        }
    }

    fn running_at(&self, cgroup: Option<&std::path::Path>, host_pid: u32) {
        *self.target.lock().expect("target") = Some(Target {
            cgroup: cgroup.map(PathBuf::from),
            host_pid,
        });
    }
}

/// Every request this sandbox has sent and not yet answered.
///
/// Keyed by the request id, which is unique across the host, so the supervisor
/// can ask each function and pool in turn whether an id is theirs rather than
/// keeping a second index that could disagree with this one.
#[derive(Debug, Default)]
struct Requests(Mutex<BTreeMap<String, Arc<InFlight>>>);

impl Requests {
    fn start(&self, id: &str, owner: Option<&str>, key: Option<&str>) -> Arc<InFlight> {
        let entry = Arc::new(InFlight {
            owner: owner.map(str::to_string),
            key: key.map(str::to_string),
            ..InFlight::default()
        });
        self.0
            .lock()
            .expect("in flight")
            .insert(id.to_string(), Arc::clone(&entry));
        entry
    }

    fn finish(&self, id: &str) {
        self.0.lock().expect("in flight").remove(id);
    }

    /// Find a request by its id, or by the key its caller gave it.
    ///
    /// The id first, because it is the unambiguous name. A key is searched for
    /// only when no id matches, so a caller cannot shadow somebody's id with a
    /// key — and could not reach it anyway, since a key only matches a request
    /// they are allowed to cancel.
    fn get(&self, name: &str) -> Option<Arc<InFlight>> {
        let requests = self.0.lock().expect("in flight");
        if let Some(entry) = requests.get(name) {
            return Some(Arc::clone(entry));
        }
        requests.values().find(|e| e.keyed(name)).map(Arc::clone)
    }

    fn ids(&self) -> Vec<String> {
        self.0.lock().expect("in flight").keys().cloned().collect()
    }
}

/// Where a streaming request's output goes as it is produced.
///
/// Called on the thread serving the request, between the `EXEC` and the
/// `DONE`, so it must not block for long: whatever is on the other end is
/// holding up the request that is writing to it. The supervisor's own
/// implementation writes one control frame and returns.
pub type ChunkSink<'a> = &'a (dyn Fn(crate::protocol::Stream, &str) + Send + Sync);

/// What a request brings with it and takes away.
///
/// `inbox` is a tar the caller sent, unpacked into the request's own directory
/// before the handler runs. `collect` asks for the directory back as a tar
/// when the handler is done — which is a separate question, because a request
/// that only *reads* its input should not pay to have it packed again.
#[derive(Debug, Clone, Default)]
pub struct Workspace {
    pub inbox: Option<Vec<u8>>,
    pub collect: bool,
}

impl Workspace {
    /// Whether this request needs a directory at all.
    fn wanted(&self) -> bool {
        self.inbox.is_some() || self.collect
    }
}

/// One request's directory inside the sandbox, removed when this is dropped.
///
/// Held for the whole request by the thread serving it, so the directory goes
/// on every path out — including the ones where the request failed or was
/// killed. A workspace that outlived its request would be a neighbour's to
/// find, and the window is meant to be one request long.
struct WorkspaceLease {
    /// The directory as *this* process can reach it, through the sandbox's
    /// `/proc/<pid>/root`.
    host: PathBuf,
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.host);
    }
}

/// How long a request may go without the agent saying it is still alive.
///
/// Only consulted for a request whose budget is longer than this, so nothing
/// with an ordinary timeout is affected: a thirty-second request is bounded by
/// thirty seconds, and a heartbeat that adds a second bound to it would only
/// be a second thing to get wrong.
///
/// Generous against the agents' own interval, because a missed heartbeat kills
/// a request that may have been running for hours. The reference agents beat
/// once every two seconds, so this is thirty beats' worth of slack.
pub const HEARTBEAT_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// The most output one streaming request may send, in bytes.
///
/// A cap rather than backpressure, because the two ends are a handler and an
/// HTTP client and there is nothing useful to do with a handler that outruns
/// its reader: slowing it down turns a chatty request into a slow one, and
/// buffering it turns the supervisor into the place the memory goes. Past the
/// cap the stream stops and the request carries on — `RESULT` still carries
/// the (separately bounded) captured output, so nothing is lost that was not
/// already going to be truncated.
pub const STREAM_BYTES: usize = 4 * 1024 * 1024;

/// Takes a request off the in-flight list however its thread leaves.
///
/// A `Drop` rather than a call at the end, because `call_script_timed` has
/// eight early returns and the one that forgot would leave an id that a later
/// cancel could find and kill somebody else's child with — the ids are not
/// reused, but the entry would name a cgroup that is.
struct RequestLease<'a> {
    requests: &'a Requests,
    id: &'a str,
}

impl Drop for RequestLease<'_> {
    fn drop(&mut self) {
        self.requests.finish(self.id);
    }
}

/// What a one-shot started for another process carries of that process:
/// its three streams, whether they are one terminal, and the signals it
/// ignores (see [`SandboxConfig::ignored_signals`]).
pub struct ClientStreams {
    pub stdio: [std::os::fd::OwnedFd; 3],
    pub tty: bool,
    pub ignored_signals: u64,
}

/// A one-shot sandbox the supervisor started for a client, not yet waited on.
///
/// The `pivot_root` target is carried so the waiter can remove it once the
/// sandbox is gone — `remove_dir`, not `remove_dir_all`, for the reason
/// `zygo run` gives: a mount that outlived its namespace makes the removal
/// fail and the directory stay, which is the right way round.
pub struct Oneshot {
    pub sandbox: Box<dyn crate::backend::Sandbox>,
    pub newroot: PathBuf,
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

        // Which of the two warm modes this is (design doc §3.4). A runtime
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
    #[allow(clippy::too_many_arguments)]
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
            secrets: Mutex::new(Secrets::default()),
            scripts: Mutex::new(Scripts::default()),
            script_dir: script_dir.to_path_buf(),
        })))
    }

    /// Start a one-shot sandbox with somebody else's standard streams.
    ///
    /// What `zygo run` does for itself, done here on a client's behalf —
    /// because this process already sits in a delegated, built `zygo.slice`
    /// and the client, on an ordinary systemd session, cannot get into one:
    /// cgroup delegation containment forbids it (`zygo-cli/src/scope.rs`).
    /// Measured at 45 ms for a `zygo run` from a login shell against 11 ms
    /// from a cgroup like this one, and the difference is the whole point.
    ///
    /// Started and **not waited on**: this runs on the launcher thread, and
    /// a launcher that waited would hold every other start on the machine
    /// for as long as the program ran. The caller waits, on its own thread.
    ///
    /// The descriptors are the client's own stdin, stdout and stderr, and
    /// they stay three distinct things; `tty` says they are one terminal the
    /// sandbox should adopt as its controlling terminal. Either way they are
    /// numbered 3 or above — they arrived over `SCM_RIGHTS` — which the
    /// launcher's child relies on when it `dup2`s them into place.
    #[cfg(target_os = "linux")]
    pub fn start_oneshot(&self, f: &ResolvedFn, client: ClientStreams) -> Result<Oneshot> {
        use std::os::fd::AsRawFd;

        use crate::image::{Reference, Store};
        use crate::sandbox::{SandboxConfig, mount};

        for fd in &client.stdio {
            if fd.as_raw_fd() < 3 {
                return Err(Error::primitive(
                    "stdio",
                    "internal supervisor error",
                    std::io::Error::other(
                        "a received descriptor landed on 0, 1 or 2, which the child cannot \
                         dup2 over safely",
                    ),
                ));
            }
        }

        let store = Store::new(self.config.paths.clone());
        let reference: Reference = f.image.parse()?;
        // In the store already, like `serve`: pulling is a network operation
        // with output of its own, and it is the client's.
        let entry = store
            .get(&reference)
            .ok_or_else(|| Error::BackendUnavailable {
                backend: "pool",
                reason: format!("image `{}` is not in the local store", f.image),
                remedy: format!("run `zygo pull {}`", f.image),
            })?;
        let entry = if f.system.is_empty() {
            entry
        } else {
            crate::derive::ensure(&store, &entry, &f.system)?.image
        };

        let mut f = f.clone();
        let venv = match &f.requirements {
            Some(requirements) => {
                if !requirements.is_file() {
                    return Err(Error::Spec(crate::spec::SpecError::invalid(
                        "requirements",
                        format!("{} does not exist", requirements.display()),
                    )));
                }
                let venv = crate::venv::ensure(&store, &entry, requirements)?;
                f.mounts.push(venv.mount());
                Some(venv)
            }
            None => None,
        };

        let net = crate::net::setup(&self.config.paths, &f.name, &f)?;
        if let Some(mount) = net.mount.clone() {
            f.mounts.push(mount);
        }
        for w in &net.warnings {
            tracing::warn!("{w}");
        }

        // The image's own config: its default command, and — just as
        // importantly — its `PATH`, without which a bare `python3` cannot be
        // resolved. Read from the store's blob, never fetched.
        let image_config: crate::image::ImageConfig =
            serde_json::from_slice(&store.read_blob(&entry.config)?).map_err(|e| {
                Error::primitive(
                    "image config",
                    "the image's config blob is malformed",
                    std::io::Error::other(e),
                )
            })?;
        let argv = if f.cmd.is_empty() {
            let argv = image_config.default_argv();
            if argv.is_empty() {
                return Err(Error::Spec(crate::spec::SpecError::invalid(
                    "cmd",
                    format!("`{}` declares no entrypoint or cmd", f.image),
                )));
            }
            argv
        } else {
            f.cmd.clone()
        };
        let mut env = image_config.env_pairs();
        if venv.is_some() {
            let image_path = env
                .iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            env.retain(|(k, _)| k != "PATH" && k != "VIRTUAL_ENV");
            env.extend(crate::venv::Venv::env_over(&image_path));
        }

        let mount_points = mount::required_mount_points(&f.mounts);
        let overlay = crate::doctor::cached(&self.config.paths)
            .checks
            .iter()
            .any(|c| c.name == "overlayfs (userns)" && c.status == crate::doctor::Status::Ok);
        let view = store.rootfs_view(&entry.layers, overlay, &mount_points)?;
        let newroot = self.config.paths.tmp().join(format!(
            "run-{}-{}",
            std::process::id(),
            next_request_id()
        ));
        std::fs::create_dir_all(&newroot).at(&newroot)?;

        let mut config = SandboxConfig::from_resolved(&f, &view, &newroot, argv, &env);
        config.allow_resolved = net.allowed;
        config.pasta_pid_file = net.pid_file;
        config.stdio_streams = Some([
            client.stdio[0].as_raw_fd(),
            client.stdio[1].as_raw_fd(),
            client.stdio[2].as_raw_fd(),
        ]);
        let _ = client.tty; // the child adopts a terminal by trying; see `adopt_streams`
        // The client's dispositions, not this process's: see the field.
        config.ignored_signals = Some(client.ignored_signals);

        let backend = crate::backend::for_isolation(f.isolation, &self.config.paths)?;
        let sandbox = backend.start(&config);
        // Held open across the start and no longer: the child has its own
        // copies now, and these must not outlive the request in a process
        // that spawns other things.
        drop(client.stdio);
        let sandbox = match sandbox {
            Ok(s) => s,
            Err(e) => {
                let _ = std::fs::remove_dir(&newroot);
                return Err(e);
            }
        };
        Ok(Oneshot { sandbox, newroot })
    }

    /// Off Linux there is nothing to start in; the same answer `serve_exec`
    /// gives below, so a supervisor built for macOS still compiles and still
    /// says why.
    #[cfg(not(target_os = "linux"))]
    pub fn start_oneshot(&self, _f: &ResolvedFn, _client: ClientStreams) -> Result<Oneshot> {
        Err(Error::BackendUnavailable {
            backend: "pool",
            reason: "a one-shot sandbox enters Linux namespaces".into(),
            remedy: "run Zygo inside a Linux VM or container; on macOS the `zygo` \
                 binary normally forwards into one it manages"
                .into(),
        })
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
            let tmp = dir.join(format!("{}.{}", name.to_string_lossy(), std::process::id()));
            let mut file = std::fs::File::create(&tmp).at(&tmp)?;
            file.write_all(agent.source().as_bytes()).at(&tmp)?;
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
    /// See [`Status::tenant`].
    tenant: String,
    /// See [`Status::image`].
    image: String,
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
    /// Requests sent and not yet answered, so a cancel on another thread can
    /// find one. See [`InFlight`].
    in_flight: Requests,
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
    /// Everything this function was declared with, kept whole.
    ///
    /// The ceiling a tenant's own limits are narrowed against. A pool's
    /// requests come from different tenants and the sandbox is shared, so the
    /// narrowing cannot happen at warm time — it happens per request, on the
    /// request's own cgroup.
    limits: crate::sandbox::limits::Limits,
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
    /// Scripts written into the sandbox for the requests running them, and how
    /// many requests each still has. See [`Scripts`] and [`place_script`].
    scripts: Mutex<Scripts>,
    /// The host side of `/run/script`: this sandbox's alone, removed with it.
    script_dir: PathBuf,
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
            tenant: self.tenant.clone(),
            image: self.image.clone(),
            state: self.state(),
            runtime: self.runtime.clone(),
            // Read now, not remembered from the warm-up. `self.rss_kb` is what
            // the agent announced in `READY` and never changes again, so
            // `zygo ps` reported sixteen megabytes for a function that had
            // grown to five hundred — and any check watching that number for
            // movement was watching a constant. The warm-up figure is the
            // fallback for a process that has gone, where it is the last true
            // thing known about it.
            //
            // The warm-exec path has always done this; only the agent path
            // had the frozen copy.
            rss_kb: resident_kb(self.agent_host_pid).unwrap_or(self.rss_kb),
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
        self.call_script_timed(event, None, timeout)
    }

    /// Serve one request against a script that did not come with the zygote.
    ///
    /// The runtime-pool shape (protocol 1.1): this `WarmFn` is an interpreter
    /// and a dependency set with no tenant code in it, and `script` is what
    /// this one request runs. The agent hands it to the forked child, which
    /// loads it after `GO` — so the zygote stays anonymous and two tenants
    /// sharing it cannot reach each other through it.
    ///
    /// `None` is the original shape: the agent already imported a handler and
    /// every request is a fork of that. It is faster, because the import is
    /// not paid per request, and it is what a hot function should use.
    pub fn call_script_timed(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_script_for(event, script, timeout, None)
    }

    /// The same, for a named tenant.
    ///
    /// `caller` decides one thing and only one: who may cancel this request.
    /// A pool is shared, so the pool's own tenant cannot answer that — the
    /// request's can. Everything else about ownership was settled before the
    /// request got here, by `script_for_request` refusing a digest that is not
    /// the caller's.
    pub fn call_script_for(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_script_keyed(event, script, timeout, caller, None)
    }

    /// The same, under a name the caller chose. See [`InFlight::key`].
    pub fn call_script_keyed(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
        key: Option<&str>,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_streaming(event, script, timeout, caller, key, None, None, None)
    }

    /// The same, with output delivered as it is produced (proto 1.3).
    ///
    /// `sink` is called for every `CHUNK` the agent forwards. Passing `None`
    /// is the ordinary path and is what everything that is not streaming
    /// does: the `EXEC` then does not ask for chunks, the child captures its
    /// output the way it always has, and not one extra frame crosses the
    /// socket.
    #[allow(clippy::too_many_arguments)]
    pub fn call_streaming(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
        key: Option<&str>,
        sink: Option<ChunkSink<'_>>,
        workspace: Option<Workspace>,
        tenant_limits: Option<crate::tenants::TenantLimits>,
    ) -> Result<(Outcome, CallTiming)> {
        if let Some(s) = &script
            && !s.is_loadable()
        {
            return Err(crate::spec::SpecError::invalid(
                "script",
                "a script must carry either a path or its source",
            )
            .into());
        }
        let id = next_request_id();
        let started = Instant::now();

        // Registered before the `EXEC` rather than after the fork, so a cancel
        // that arrives in the first millisecond finds something. The guard
        // takes it off the list on every path out of this function, including
        // the early returns above the `DONE`.
        let request = self.in_flight.start(&id, caller, key);
        let _lease = RequestLease {
            requests: &self.in_flight,
            id: &id,
        };

        // Before the `EXEC`, because the `EXEC` has to say where the script
        // is. The file only has to *exist* by `GO` — the child loads it after
        // that — but the decision cannot wait that long, and placing it here
        // means a sandbox that cannot take the file falls back to sending the
        // bytes rather than failing a request that is already in flight.
        //
        // The lease lives until this function returns, by any route, which is
        // what takes the file away again.
        // Kept before the script is placed, because placing it rewrites the
        // struct — and a usage event that lost the digest would be a bill
        // nobody could check.
        let script_digest = script.as_ref().and_then(|s| s.digest.clone());
        let (script, _script) = self.place_script(script);

        // The caller's files, unpacked into a directory of this request's own
        // before the `EXEC` names it. Like the script, the directory only has
        // to exist by `GO` — but the `EXEC` has to carry the path, so it is
        // made here. The lease removes it on every path out of this function.
        let (workspace_path, _workspace) = match self.place_workspace(workspace.as_ref()) {
            Ok(placed) => placed,
            Err(e) => {
                self.record(false);
                return Err(e);
            }
        };

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
                script,
                stream: sink.is_some(),
                workspace: workspace_path.clone(),
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
        // The host pid is what makes the deadline enforceable, and it is
        // resolved *here*, once, for both of the things that need it: moving
        // the child into its own cgroup, and signalling it if there is no
        // cgroup to kill.
        //
        // B-12: this used to be resolved inside `admit`, which returned `None`
        // when the translation failed — and `None` is also what it returns
        // when per-request cgroups are simply off. The two were
        // indistinguishable, so a child whose pid could not be translated got
        // no cgroup, was never signalled, and **ran past its deadline
        // untouched**. Resolving it again at kill time does not help: by then
        // it can fail for a second reason.
        //
        // A child this supervisor just forked, parked waiting for `GO`, must
        // be in the agent's `children`. If it is not, something is wrong with
        // the sandbox rather than with this request, so the sandbox goes —
        // which is also the only way left to be rid of the parked child.
        let host_pid = match self.host_pid_of(pid) {
            Some(host) => host,
            None => {
                return self.broken(Error::BackendUnavailable {
                    backend: "pool",
                    reason: format!(
                        "`{}` forked a child this supervisor cannot find on the host \
                         (agent-local pid {pid}), so its deadline could not be enforced",
                        self.name
                    ),
                    remedy: "the function will be rewarmed; the request was not run".into(),
                });
            }
        };
        // A tenant's limits, when they narrow this function's. Resolved here
        // rather than at warm time because a pool's requests come from
        // different tenants and the sandbox is shared.
        let narrowed = tenant_limits
            .as_ref()
            .filter(|l| l.narrows(&self.limits))
            .map(|l| l.narrow(&self.limits));
        let request_cgroup = admit(
            self.generation_cgroup.as_deref(),
            self.per_request_cgroup,
            &id,
            host_pid,
            narrowed.as_ref(),
        );
        request.running_at(request_cgroup.as_deref(), host_pid);
        let _secrets = match self.place_secrets() {
            Ok(lease) => lease,
            Err(e) => {
                // The child is waiting for `GO` that will never come; kill it
                // rather than leave it parked in the agent's fork table.
                self.enforce_deadline(request_cgroup.as_deref(), host_pid);
                self.record(false);
                return Err(e);
            }
        };
        let admitted = Instant::now();

        // A cancel that arrived while the child was being admitted is the best
        // possible one: the child is parked waiting for `GO`, so killing it
        // here means not one instruction of the handler ran. `GO` is simply
        // never sent — the agent sees the child die and answers by itself,
        // which is the same path a deadline kill takes.
        if request.cancelled() {
            self.enforce_deadline(request_cgroup.as_deref(), host_pid);
        } else if let Err(e) = self.conn.write(&Message::Go { id: id.clone() }) {
            return self.broken(e);
        }

        // From here tenant code is running, so the deadline is ours to enforce.
        // Passing `timeout_ms` to the agent is a courtesy, not a control: the
        // agent's own children are the thing being limited, so trusting it to
        // stop them is trusting the blast radius to contain itself.
        //
        // The function's own limit wins over the caller's: asking for longer
        // than the spec allows is asking past a mandatory limit (N4).
        // A tenant's own timeout narrows this too: a cgroup cannot enforce a
        // wall clock, so the supervisor's deadline is where that key lands.
        let ceiling = narrowed
            .as_ref()
            .map(|l| l.timeout.get())
            .unwrap_or(self.timeout);
        let budget = timeout.min(ceiling);
        let deadline = budget.saturating_sub(admitted - started);

        // Chunks arrive on the same channel as the `DONE`, because the reply
        // router keys on the request id and a `CHUNK` carries one. So the wait
        // is a loop, and the deadline is measured from *here* rather than
        // renewed per message: a handler that printed in a loop would
        // otherwise extend its own deadline for as long as it kept talking.
        let waiting_from = Instant::now();
        let mut streamed = 0usize;
        let mut timed_out = false;
        let mut stuck = false;
        // Only for a request whose budget outlives the grace period. A
        // thirty-second request is bounded by thirty seconds; a second bound
        // shorter than the first would be a second thing to get wrong and
        // would never be the one that fired.
        let heartbeat = deadline > HEARTBEAT_GRACE;
        let mut heard = Instant::now();
        let done_reply = loop {
            let left = deadline.saturating_sub(waiting_from.elapsed());
            // Whichever runs out first. `heard` moves on every frame the agent
            // sends about this request — a chunk is as good a sign of life as
            // a heartbeat, so a chatty handler needs no other.
            let left = match heartbeat {
                true => left.min(HEARTBEAT_GRACE.saturating_sub(heard.elapsed())),
                false => left,
            };
            let next = reply.next(left);
            if next.is_ok() {
                heard = Instant::now();
            }
            match next {
                // A heartbeat for this request: the agent says the child is
                // still there. Nothing else to do — the wait has restarted.
                Ok(Message::Ping { .. }) => continue,
                Ok(Message::Chunk { stream, data, .. }) => {
                    // Past the cap the stream stops and the request carries
                    // on: `RESULT` still brings the captured output, so a
                    // caller loses the live view and nothing else.
                    if let Some(sink) = sink
                        && streamed < STREAM_BYTES
                    {
                        streamed += data.len();
                        sink(stream, &data);
                    }
                    continue;
                }
                Ok(message) => break message,
                Err(ReplyError::Gone(reason)) => {
                    return self.broken(agent_gone(&self.name, &reason));
                }
                Err(ReplyError::TimedOut) => {
                    // Which clock ran out. A request that still had budget
                    // left was killed for going quiet rather than for being
                    // slow, and the caller is told which — they are different
                    // facts with different answers.
                    stuck = waiting_from.elapsed() < deadline;
                    timed_out = !stuck;
                    self.enforce_deadline(request_cgroup.as_deref(), host_pid);
                    // The child is dead, so the agent sees end of file on the
                    // result pipe and sends `DONE` by itself. Waiting for it
                    // is what keeps this request's reply from arriving later
                    // with nobody expecting it. Chunks already in flight are
                    // drained on the way, for the same reason.
                    let killed_at = Instant::now();
                    break loop {
                        let left = KILL_GRACE.saturating_sub(killed_at.elapsed());
                        match reply.next(left) {
                            Ok(Message::Chunk { .. } | Message::Ping { .. }) => continue,
                            Ok(message) => break message,
                            Err(_) => {
                                return self.broken(Error::BackendUnavailable {
                                    backend: "pool",
                                    reason: format!(
                                        "`{}` did not answer after its request was \
                                         killed; the agent is not responding",
                                        self.name
                                    ),
                                    remedy: "the function will be rewarmed; check the \
                                             handler for something that blocks \
                                             uninterruptibly"
                                        .into(),
                                });
                            }
                        }
                    };
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
        // The supervisor's own answer, not the agent's: it is the side that
        // sent the kill, so it is the side that knows a cancel from a
        // deadline. `DONE{cancelled}` from an agent that tracks it is taken as
        // agreement rather than as the source.
        let cancelled = request.cancelled();
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
                id: id.clone(),
                exit_code,
                result,
                stdout,
                stderr,
                error,
                // A killed request's `DONE` reports the wall time of the
                // agent noticing, which is near zero for something that ran
                // for its whole deadline. The supervisor holds the only clock
                // that saw the whole of it — see the note above.
                metrics: if timed_out || cancelled || stuck {
                    Metrics {
                        wall_ms: measured.as_secs_f64() * 1000.0,
                        ..metrics
                    }
                } else {
                    metrics
                },
                timed_out,
                cancelled,
                stuck,
                // Filled in below, once the handler has finished writing.
                workspace: None,
                // Who it was for and what it ran, for whoever is counting.
                tenant: caller.unwrap_or(&self.tenant).to_string(),
                function: self.name.clone(),
                script: script_digest,
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

        // Collected before the lease is dropped, which is what removes the
        // directory. A request that asked for its files back and whose handler
        // failed still gets them: the handler may have written the reason.
        let collected = match (&workspace, &_workspace) {
            (Some(w), Some(lease)) if w.collect => match crate::workspace::pack(&lease.host) {
                Ok(tar) => Some(base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    tar,
                )),
                Err(e) => {
                    tracing::warn!(function = %self.name, error = %e, "cannot pack the workspace");
                    None
                }
            },
            _ => None,
        };
        let outcome = outcome.map(|o| Outcome {
            workspace: collected,
            ..o
        });

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
    /// Kill a request that overran, given the host pid resolved at admission.
    ///
    /// The pid is passed in rather than translated again. Translating it here
    /// can fail for reasons that have nothing to do with this request — the
    /// child may have exited and been reaped between the deadline firing and
    /// this call — and the old code answered that failure by signalling
    /// nothing at all.
    fn enforce_deadline(&self, request_cgroup: Option<&std::path::Path>, host_pid: u32) {
        kill_request(request_cgroup, host_pid);
    }

    /// Make this request its own directory inside the sandbox, if it has one.
    ///
    /// Written through `/proc/<pid>/root`, the same way secrets are: the
    /// supervisor is outside the sandbox and this is how it reaches in. The
    /// name is 128 random bits rather than the request id — see
    /// [`crate::workspace`] for why that, and not a mount namespace, is what
    /// keeps one request's files from another's.
    fn place_workspace(
        &self,
        workspace: Option<&Workspace>,
    ) -> Result<(Option<String>, Option<WorkspaceLease>)> {
        let Some(workspace) = workspace.filter(|w| w.wanted()) else {
            return Ok((None, None));
        };

        let name = crate::workspace::new_name()?;
        let root = PathBuf::from(format!("/proc/{}/root", self.agent_host_pid));
        let host = root
            .join(crate::sandbox::mount::WORKSPACE_DIR.trim_start_matches('/'))
            .join(&name);
        std::fs::create_dir(&host).at(&host)?;
        // The lease from here on, so a failed unpack still removes what it
        // half-wrote rather than leaving it for the next request to find.
        let lease = WorkspaceLease { host: host.clone() };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700)).at(&host)?;
        }

        if let Some(tar) = &workspace.inbox {
            crate::workspace::unpack(tar, &host)?;
        }

        let inside = format!(
            "{}/{name}",
            crate::sandbox::mount::WORKSPACE_DIR.trim_end_matches('/')
        );
        Ok((Some(inside), Some(lease)))
    }

    /// Stop a request this sandbox is running.
    ///
    /// `None` means the id is not here. `Some(started)` says whether the
    /// handler had begun: `false` is the better outcome, because the child was
    /// still parked waiting for `GO` and not one instruction of it ran.
    ///
    /// Safe to call on an id that has already finished, and on one that is
    /// finishing as this runs: the entry is gone by then and the answer is
    /// `None`, which is the truth — there was nothing left to cancel.
    ///
    /// Returns once the kill has been *sent*. The request's own thread is what
    /// reports the outcome, and it is still waiting for the agent's `DONE`.
    pub fn cancel(&self, id: &str, caller: Option<&str>) -> Option<bool> {
        let request = self.in_flight.get(id)?;
        if !request.is_for(caller) {
            return None;
        }
        // Best effort and deliberately not checked: this is how the agent
        // learns *why*, not how the request is stopped. See [`InFlight`].
        let _ = self.conn.write(&Message::Cancel { id: id.to_string() });
        Some(request.cancel())
    }

    /// Ids this sandbox is running, for `zygo ps` and for a drain.
    pub fn in_flight_ids(&self) -> Vec<String> {
        self.in_flight.ids()
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
        // The scripts this sandbox was sent go with it. `remove_dir_all` and
        // not `remove_dir`: a request killed at its deadline may not have run
        // its lease's `Drop`, and one leftover file is not a reason to leave
        // the directory — nothing else can be under here, because the
        // directory is this sandbox's alone and only the supervisor writes
        // into it.
        let _ = std::fs::remove_dir_all(&self.script_dir);
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

    // The directory is recorded **before** the files are written, so a write
    // that fails half way leaves something `unlink_all` can clean. It was set
    // afterwards until the code review (B-01): the second of three
    // writes failing left the first file on the tmpfs at mode 0400, `dir` was
    // `None` so nothing removed it, and every later request on that function
    // failed with EACCES trying to create a file that was already there.
    secrets.dir = Some(dir);
    let dir = secrets.dir.as_ref().expect("just set");

    let mut written = Vec::with_capacity(secrets.values.len());
    for (name, value) in &secrets.values {
        if let Err(e) = write_request_file_at(dir, name, value, "writing a secret into the sandbox")
        {
            // Undo this attempt rather than leaving a partial set: a handler
            // that received two of its three secrets is a worse failure than
            // one that received none and was told why.
            for done in &written {
                let _ = rustix::fs::unlinkat(dir, *done, rustix::fs::AtFlags::empty());
            }
            secrets.dir = None;
            return Err(e);
        }
        written.push(name.as_str());
    }
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
    narrower: Option<&crate::sandbox::limits::Limits>,
) -> Option<PathBuf> {
    if !per_request {
        return None;
    }
    let dir = crate::cgroup::Hierarchy::request(generation?, id);
    if std::fs::create_dir(&dir).is_err() {
        return None;
    }
    // A tenant's own limits, written on *this request's* cgroup before the
    // child is let go. Cgroups nest, so a narrower number here binds whatever
    // the function above was declared with — and a wider one would not,
    // which is why `TenantLimits::narrow` can only produce a smaller value.
    //
    // Best effort, like the attach below: a limit that could not be written
    // leaves the request under the function's own, which is the promise the
    // operator already made. Failing the request instead would turn a tenant's
    // *tightening* into an outage.
    if let Some(limits) = narrower {
        let _ = crate::cgroup::apply(&dir, &limits.cgroup_writes());
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

/// Where a request's own script lands inside the sandbox (protocol 1.1).
///
/// A **read-only bind mount** of a directory on the host that belongs to this
/// sandbox alone. The supervisor writes the scripts into the host side; the
/// sandbox sees them through a mount it cannot write. That asymmetry is the
/// whole control, and it is the mount namespace enforcing it rather than
/// anything the tenant is asked to respect.
///
/// It has to be a mount, and the two obvious alternatives are both worth
/// naming because both were tried:
///
/// * **Mode bits cannot do it.** Everything in a sandbox runs as the uid the
///   supervisor maps to, so the *owner* bits are what a tenant gets: a
///   directory the supervisor can write is a directory the tenant can write,
///   whatever the mode says.
/// * **Landlock cannot do it either.** A rule grants; within one ruleset a
///   read-only rule on `/run/script` is unioned with the read-write rule on
///   the `/run` tmpfs above it rather than overriding it. Measured on 6.8,
///   ABI 4. Subtracting needs a second *layer*, applied in the request's own
///   child, which needs a protocol contract to carry it.
///
/// The host directory is `0311` — write and traverse, no read — so a script
/// can open a digest it already knows and cannot list what else is in flight
/// beside it. In a runtime pool that "what else" is other tenants' code.
///
/// The digest check in the child (`spec/protocol.md` §3.8) stays, and is not
/// redundant: it covers the kernels and backends where this mount is not what
/// it should be, and it is what makes a swapped file a refused request rather
/// than a served one.
pub const SCRIPT_DIR_IN_SANDBOX: &str = "/run/script";

/// The mode the host-side script directory is created with. See above.
#[cfg(target_os = "linux")]
pub const SCRIPT_DIR_MODE: u32 = 0o311;

/// Everywhere else, the same directory without the unreadable part.
///
/// There is no sandbox off Linux and no `O_PATH` to open a directory that
/// denies read to its owner, so a `0311` here is not a boundary — it is only a
/// directory this process cannot open. The tests that exercise the placement
/// logic run on a developer's Mac, and this is what lets them.
#[cfg(not(target_os = "linux"))]
pub const SCRIPT_DIR_MODE: u32 = 0o711;

/// Scripts present in the sandbox for the requests that named them.
///
/// Counted, not merely present: a script is shared, two requests may run the
/// same one at once, and the second must not find the file the first has just
/// removed. The same reason [`Secrets`] counts, for the same boundary — except
/// that the count is per script rather than per function, because two requests
/// in flight on a pool zygote are generally two *different* scripts.
#[derive(Debug, Default)]
struct Scripts {
    /// File name — the digest's hex — to the number of requests that need it.
    resident: BTreeMap<String, u32>,
    /// The host side of the bind mount, held open from the first file written
    /// until the last is gone.
    dir: Option<std::fs::File>,
    /// Whether this function has already said that it cannot deliver a script
    /// as a file. Said once: it is a property of the sandbox, so a request
    /// that logs it logs it for every request after.
    warned: bool,
}

/// A script file that exists in the sandbox for as long as this is alive.
///
/// Dropping it is what takes the file away, so every exit from the request
/// path — the reply, the deadline, a broken connection — removes it, exactly
/// as [`SecretsLease`] does.
struct ScriptLease<'a> {
    scripts: &'a Mutex<Scripts>,
    name: String,
}

impl Drop for ScriptLease<'_> {
    fn drop(&mut self) {
        let mut guard = self.scripts.lock().expect("scripts");
        let last = match guard.resident.get_mut(&self.name) {
            Some(count) => {
                *count -= 1;
                *count == 0
            }
            None => false,
        };
        if !last {
            return;
        }
        guard.resident.remove(&self.name);
        if let Some(dir) = guard.dir.as_ref() {
            let _ = rustix::fs::unlinkat(dir, self.name.as_str(), rustix::fs::AtFlags::empty());
        }
        // The last script out closes the directory too: holding a descriptor
        // into a sandbox that may be replaced under us buys nothing once there
        // is nothing in there to remove.
        if guard.resident.is_empty() {
            guard.dir = None;
        }
    }
}

/// Put one request's script where its child will find it.
///
/// `dir` is the sandbox's `/run/script` as seen from the host — through
/// `/proc/<agent pid>/root`, the same route secrets take and for the same
/// reason: nothing inside the sandbox is asked to cooperate, and the zygote
/// never holds the bytes.
fn place_script<'a>(
    scripts: &'a Mutex<Scripts>,
    dir: &std::path::Path,
    name: &str,
    source: &str,
) -> Result<ScriptLease<'a>> {
    let mut guard = scripts.lock().expect("scripts");
    let count = guard.resident.entry(name.to_string()).or_insert(0);
    *count += 1;
    let write_now = *count == 1;

    let written = if write_now {
        write_script(&mut guard, dir, name, source)
    } else {
        Ok(())
    };
    drop(guard);

    // The lease is built with no lock held, for the reason `place_secrets`
    // spells out: dropping one takes this same mutex, and a lease created
    // above an early `?` return would be dropped by the thread holding it.
    let lease = ScriptLease {
        scripts,
        name: name.to_string(),
    };
    written?;
    Ok(lease)
}

/// Write one script file. Called with the lock held; takes none.
fn write_script(
    scripts: &mut Scripts,
    dir: &std::path::Path,
    name: &str,
    source: &str,
) -> Result<()> {
    if scripts.dir.is_none() {
        // The host side of the bind mount. It was made before the sandbox
        // started — it has to be, because the mount names it — so this
        // ordinarily only opens it.
        ensure_script_dir(dir)?;
        // `O_PATH`, because the mode denies read to this process too: it is
        // the same uid as everything in the sandbox, which is the point. An
        // `O_PATH` descriptor names the directory without opening it for
        // anything, and is exactly what `openat` and `unlinkat` need.
        #[cfg(target_os = "linux")]
        let opened = std::fs::File::from(
            rustix::fs::open(
                dir,
                rustix::fs::OFlags::PATH
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(|e| Error::io(dir, std::io::Error::from(e)))?,
        );
        #[cfg(not(target_os = "linux"))]
        let opened = std::fs::File::open(dir).at(dir)?;
        scripts.dir = Some(opened);
    }
    let dir = scripts.dir.as_ref().expect("just set");
    write_request_file_at(dir, name, source, "writing a script into the sandbox")
}

/// Make the host side of the script mount, at the mode the sandbox needs.
///
/// `0311`: the supervisor writes and traverses, and *nothing* reads the
/// directory itself — not the sandbox, which would otherwise have an
/// inventory of every script in flight, and not this process either, which
/// has no need to list what it put there.
fn ensure_script_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir).at(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(SCRIPT_DIR_MODE)).at(dir)?;
    }
    Ok(())
}

/// A directory for one sandbox's scripts, on the host.
///
/// One per sandbox and never reused: a pool that is replaced must not inherit
/// the scripts of the one before it, and two pools must not be able to see
/// each other's. The counter is what makes a replacement under the same name
/// a different directory.
fn new_script_dir(paths: &Paths, name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static GENERATION: AtomicU64 = AtomicU64::new(1);
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
    paths.tmp().join(format!(
        "scripts-{}-{}-{generation}",
        crate::cgroup::sanitise(name),
        std::process::id()
    ))
}

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

    /// Decide how a request's script reaches its child, and put it there.
    ///
    /// Protocol 1.1 has two shapes and this is where the preference between
    /// them lives. **`path`** is the one to want: the supervisor writes the
    /// script into the sandbox from outside, the `EXEC` carries only its name
    /// and its digest, and the zygote — which in a runtime pool is shared
    /// between tenants — never has the bytes in its address space at all. A
    /// zygote that did would be one that the *next* tenant's fork inherits a
    /// copy-on-write view of.
    ///
    /// **`source`** is the fallback, for a sandbox this process cannot write
    /// into: a backend with no `/proc/<pid>/root` to reach through, or a
    /// kernel that refuses. It still works, and it is still safe from the
    /// filesystem's point of view, but it gives up the property above — so it
    /// is logged, once per function, rather than chosen quietly.
    ///
    /// A script that already carries a `path` is left exactly as it is: the
    /// caller has arranged delivery itself and knows something this does not.
    fn place_script(
        &self,
        script: Option<crate::protocol::Script>,
    ) -> (Option<crate::protocol::Script>, Option<ScriptLease<'_>>) {
        let Some(mut script) = script else {
            return (None, None);
        };
        if script.path.is_some() {
            return (Some(script), None);
        }
        let Some(source) = script.source.take() else {
            return (Some(script), None);
        };

        // Of the bytes about to be written, not of whatever the caller
        // believed: the `digest` field describes the file, and the child's
        // check of it is the only thing standing between a tenant that
        // replaces that file and the request that was going to load it.
        let digest = crate::scripts::ScriptDigest::of(&source);
        match place_script(&self.scripts, &self.script_dir, digest.hex(), &source) {
            Ok(lease) => {
                script.path = Some(format!("{SCRIPT_DIR_IN_SANDBOX}/{}", digest.hex()));
                script.digest = Some(digest.to_string());
                (Some(script), Some(lease))
            }
            Err(e) => {
                let mut guard = self.scripts.lock().expect("scripts");
                if !std::mem::replace(&mut guard.warned, true) {
                    tracing::warn!(
                        function = %self.name,
                        error = %e,
                        "cannot write a script into this sandbox; sending it in the \
                         EXEC instead, so the zygote holds tenant code while the \
                         request runs"
                    );
                }
                drop(guard);
                script.digest = Some(digest.to_string());
                script.source = Some(source);
                (Some(script), None)
            }
        }
    }
}

/// Create a request-scoped file readable by its owner and nobody else.
///
/// A secret value, or the script one request runs: both are written from
/// outside the sandbox into a directory inside it, both last exactly as long
/// as the requests that need them, and both are `0400`.
///
/// Created with the mode from the start rather than chmodded afterwards, so
/// there is no moment at which it is readable more widely — `/run` is a tmpfs
/// shared by everything in the sandbox.
fn write_request_file_at(
    dir: &std::fs::File,
    name: &str,
    value: &str,
    what: &'static str,
) -> Result<()> {
    use rustix::fs::{Mode, OFlags};
    use std::io::Write as _;

    // `openat`, so the directory is named by the descriptor and not by a path
    // that may no longer resolve.
    //
    // `O_EXCL` after an `unlinkat`, not `O_TRUNC`. The old comment said
    // `O_TRUNC` was for "a file from a previous generation", which is not a
    // thing that happens — a rewarm gets a fresh tmpfs. What does happen is a
    // partially written set left by an earlier failure, and opening one of
    // those with `O_TRUNC` fails with EACCES because the file is 0400 and the
    // supervisor is not root. Removing first means the mode of whatever was
    // there cannot decide whether this request works (B-01).
    let _ = rustix::fs::unlinkat(dir, name, rustix::fs::AtFlags::empty());
    let fd = rustix::fs::openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o400),
    )
    .map_err(|e| Error::primitive("openat", what, std::io::Error::from(e)))?;
    let mut file = std::fs::File::from(fd);
    file.write_all(value.as_bytes())
        .map_err(|e| Error::primitive("write", what, e))?;
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
    /// See [`Status::tenant`].
    tenant: String,
    /// See [`Status::image`].
    image: String,
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
    /// See [`WarmFn`]'s field of the same name. A warm-exec request is a
    /// process in a cgroup like any other, so a cancel is the same write.
    in_flight: Requests,
    state: Mutex<SandboxState>,
    tenant_cgroup: Option<PathBuf>,
    /// The held sandbox's own generation, where its requests are admitted.
    generation_cgroup: Option<PathBuf>,
    per_request_cgroup: bool,
    timeout: std::time::Duration,
    secrets: Mutex<Secrets>,
    /// Scripts written into the sandbox for the requests running them. As on
    /// [`WarmFn`], and for a pool of these it is the only way code arrives:
    /// the script is a **file named on a command line**, so there is no
    /// `source` shape to fall back to.
    scripts: Mutex<Scripts>,
    /// The host side of `/run/script`: this sandbox's alone, removed with it.
    script_dir: PathBuf,
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

    /// Stop a request this sandbox is running. See [`WarmFn::cancel`].
    ///
    /// There is no agent to tell, so this is only the kill — which is the
    /// whole of a cancel anyway.
    pub fn cancel(&self, id: &str, caller: Option<&str>) -> Option<bool> {
        let request = self.in_flight.get(id)?;
        if !request.is_for(caller) {
            return None;
        }
        Some(request.cancel())
    }

    pub fn in_flight_ids(&self) -> Vec<String> {
        self.in_flight.ids()
    }

    pub fn status(&self) -> Status {
        let counters = self.counters.lock().expect("counters");
        Status {
            name: self.name.clone(),
            tenant: self.tenant.clone(),
            image: self.image.clone(),
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

    /// Write one request's script into the sandbox and say where it is.
    ///
    /// [`WarmFn::place_script`] falls back to sending the bytes in the `EXEC`
    /// when it cannot write into the sandbox. There is no such fallback here:
    /// the script is named on a command line, so a path is the only shape it
    /// has. A caller that already arranged delivery — a `path` of their own —
    /// is taken at their word, as on the agent path.
    fn place_script(
        &self,
        script: crate::protocol::Script,
    ) -> Result<(String, Option<ScriptLease<'_>>)> {
        if let Some(path) = script.path {
            return Ok((path, None));
        }
        let Some(source) = script.source else {
            return Err(Error::Spec(crate::spec::SpecError::invalid(
                "script",
                "carries neither `source` nor `path`",
            )));
        };
        let digest = crate::scripts::ScriptDigest::of(&source);
        let lease = place_script(&self.scripts, &self.script_dir, digest.hex(), &source)?;
        Ok((
            format!("{SCRIPT_DIR_IN_SANDBOX}/{}", digest.hex()),
            Some(lease),
        ))
    }

    /// Serve one request: enter, admit, run, collect.
    pub fn call_timed(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_script_timed(event, None, timeout)
    }

    /// The same, for a request that brought its own script (todo 3.4).
    ///
    /// The **warm-exec pool** shape: one held sandbox running a program the
    /// operator named — `sh`, a static binary — and each request's script
    /// written into `/run/script/<digest>` and named as the last word of the
    /// command line. For a language whose runtime starts in under a
    /// millisecond there is nothing for an agent to amortise, and the warm
    /// protocol would only be a moving part.
    ///
    /// The script has to be a **file**: `source` on the wire has nowhere to go
    /// when the thing that loads it is `execve`. A sandbox this process cannot
    /// write into is therefore a refusal rather than a fallback — unlike the
    /// agent path, which still has `EXEC.source`.
    ///
    /// Nothing checks the digest here, and nothing needs to: `/run/script` is
    /// a **read-only bind mount**, so the file the supervisor wrote is the
    /// file that is `execve`d. The agent path carries the digest because its
    /// fallback shape puts the bytes on the wire.
    pub fn call_script_timed(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
    ) -> Result<(Outcome, CallTiming)> {
        use crate::backend::ns::enter;
        use std::io::Write as _;

        // Placed before anything is forked, and held for the whole request:
        // the lease is what keeps the file there while it runs and removes it
        // when the last request using it is done.
        let (script_path, _script_lease) = match script {
            None => (None, None),
            Some(script) => {
                let (path, lease) = self.place_script(script)?;
                (Some(path), lease)
            }
        };
        let argv = match &script_path {
            None => None,
            Some(path) => Some(self.plan.argv_with(path).map_err(|e| Error::Primitive {
                operation: "prepare a request's argv",
                remedy: "the script path contains a NUL byte; this is an internal error".into(),
                source: std::io::Error::other(e.to_string()),
            })?),
        };

        let id = next_request_id();
        let started = Instant::now();
        // A warm-exec function is one tenant's, and `owned_by` settled that
        // before the request arrived, so the entry needs no owner of its own.
        let request = self.in_flight.start(&id, None, None);
        let _lease = RequestLease {
            requests: &self.in_flight,
            id: &id,
        };

        let entered = {
            let sandbox = self.sandbox.lock().expect("sandbox");
            let ns = sandbox
                .namespaces()
                .ok_or_else(|| Error::BackendUnavailable {
                    backend: "pool",
                    reason: "the sandbox has no namespace descriptors".into(),
                    remedy: "internal error; the function will be rewarmed".into(),
                })?;
            enter::enter_with(&self.plan, ns, argv.as_ref())
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
            // A warm-exec function is one tenant's and its limits were set at
            // warm time; there is nothing per-request to narrow.
            None,
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
        request.running_at(request_cgroup.as_deref(), entered.pid);
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

        // As on the agent path: a cancel that beat the `go` write means the
        // request never starts, rather than starting and being killed.
        if request.cancelled() {
            kill_request(request_cgroup.as_deref(), entered.pid);
        }

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

        let exit_status = read_status(&entered, STATUS_GRACE);
        let _ = entered.reap_helper();
        let done = Instant::now();

        drop(_secrets);
        if let Some(dir) = request_cgroup {
            let _ = crate::cgroup::Hierarchy::remove(&dir);
        }
        let cleaned = Instant::now();

        let outcome = collected
            .and_then(|c| into_outcome(c, exit_status, done - admitted))
            .map(|o| Outcome {
                id: id.clone(),
                cancelled: request.cancelled(),
                ..o
            });
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
/// reported one *within `grace`*.
///
/// The bound is the whole point (B-13). This read used to block for ever, and
/// it runs immediately after `collect_request` has already given up waiting
/// for the request — so the one case it is reached in is the case where
/// something is wrong, and it answered that by hanging the caller instead of
/// the request. A deadline that is enforced up to the last step and then
/// abandoned at it is not a deadline.
///
/// The descriptor is put in non-blocking mode and polled, because there is no
/// timed `read` for a pipe. Four bytes arrive in one write or not at all.
#[cfg(target_os = "linux")]
fn read_status(
    entered: &crate::backend::ns::enter::Entered,
    grace: std::time::Duration,
) -> Option<i32> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;

    let fd = entered.status.try_clone().ok()?;
    let raw = fd.as_raw_fd();
    let deadline = Instant::now() + grace;
    let mut file = std::fs::File::from(fd);
    let mut bytes = [0u8; 4];
    let mut have = 0;

    loop {
        // SAFETY: `raw` is an open descriptor this process owns.
        let ready = unsafe {
            let mut pfd = libc::pollfd {
                fd: raw,
                events: libc::POLLIN,
                revents: 0,
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let ms = left.as_millis().min(i32::MAX as u128) as libc::c_int;
            libc::poll(&mut pfd, 1, ms)
        };
        if ready <= 0 {
            return None;
        }
        match file.read(&mut bytes[have..]) {
            Ok(0) => return None, // the helper closed without reporting
            Ok(n) => {
                have += n;
                if have == 4 {
                    return Some(i32::from_ne_bytes(bytes));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// How long `read_status` waits for the helper's four bytes.
///
/// Generous against the thing it bounds: the helper writes them immediately
/// after `waitpid` returns, so a second is several orders of magnitude more
/// than the good case needs, and any amount of waiting is better than none.
#[cfg(target_os = "linux")]
const STATUS_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

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
        // Filled in by the caller, which is the side that knows both: it
        // assigned the id and it is holding the request's registry entry.
        id: String::new(),
        cancelled: false,
        stuck: false,
        workspace: None,
        tenant: "default".into(),
        function: "resize".into(),
        script: None,
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
///
/// `None` where there is no `/proc` to read it from, which leaves the caller
/// with the last figure it knew rather than a zero that would read as "this
/// function is using no memory".
#[cfg(not(target_os = "linux"))]
fn resident_kb(_pid: u32) -> Option<u64> {
    None
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

    /// Stop a request this function is running. `None` if it is not here.
    ///
    /// The supervisor asks every function and pool in turn, because request
    /// ids are unique across the host and a second index of "which function
    /// holds id X" would be a thing that could disagree with this one.
    pub fn cancel(&self, id: &str, caller: Option<&str>) -> Option<bool> {
        each!(self, f => f.cancel(id, caller))
    }

    /// Ids this function is running.
    pub fn in_flight_ids(&self) -> Vec<String> {
        each!(self, f => f.in_flight_ids())
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

    /// The same, under a name the caller chose, streaming if asked.
    ///
    /// See [`InFlight::key`] and [`WarmFn::call_streaming`]. Only the agent
    /// shape carries either: a warm-exec function has no agent to tell, and
    /// its output is collected from pipes at the end. That is not a limitation
    /// worth closing until something asks — the pool shape is what an
    /// embedder's long requests run on, and it is the one with an agent.
    pub fn call_keyed(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
        key: Option<&str>,
        sink: Option<ChunkSink<'_>>,
    ) -> Result<Outcome> {
        self.call_full(event, None, timeout, None, key, sink, None, None)
            .map(|(o, _)| o)
    }

    pub fn call_timed(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<(Outcome, CallTiming)> {
        each!(self, f => f.call_timed(event, timeout))
    }

    /// Serve one request against a script that did not come with the zygote.
    ///
    /// The runtime-pool shape, and there are two of them. An **agent** pool
    /// loads the script in the forked child, under the child's own seccomp
    /// filter, and can stream, take a workspace and be narrowed per tenant. A
    /// **warm-exec** pool `execve`s a program the operator named with the
    /// script as its last argument — right for a language that starts in
    /// under a millisecond, and without any of those four things, because
    /// there is no protocol between the supervisor and the program to carry
    /// them.
    pub fn call_script_with_timeout(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
    ) -> Result<Outcome> {
        self.call_script_for(event, script, timeout, None)
            .map(|(outcome, _)| outcome)
    }

    /// The same, for a named tenant and under a name they chose.
    ///
    /// See [`WarmFn::call_script_for`] and [`InFlight::key`].
    pub fn call_script_as(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
        key: Option<&str>,
        sink: Option<ChunkSink<'_>>,
    ) -> Result<Outcome> {
        self.call_script_streaming(event, script, timeout, caller, key, sink)
            .map(|(outcome, _)| outcome)
    }

    /// The same, reporting where the request's time went.
    ///
    /// What `zygo bench warm --pool` measures: the phases are the same three
    /// a function's request has, so the two shapes can be compared line by
    /// line rather than only at the total.
    pub fn call_script_timed(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_script_for(event, script, timeout, None)
    }

    /// The same, recording who the request is for. See
    /// [`WarmFn::call_script_for`].
    pub fn call_script_for(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_script_keyed(event, script, timeout, caller, None)
    }

    /// The same, under a name the caller chose. See [`InFlight::key`].
    pub fn call_script_keyed(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
        key: Option<&str>,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_script_streaming(event, script, timeout, caller, key, None)
    }

    /// The same, streaming output as it is produced.
    ///
    /// See [`WarmFn::call_streaming`].
    pub fn call_script_streaming(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
        key: Option<&str>,
        sink: Option<ChunkSink<'_>>,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_full(event, script, timeout, caller, key, sink, None, None)
    }

    /// The whole of what one request can carry. Everything above narrows it.
    #[allow(clippy::too_many_arguments)]
    pub fn call_full(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
        key: Option<&str>,
        sink: Option<ChunkSink<'_>>,
        workspace: Option<Workspace>,
        tenant_limits: Option<crate::tenants::TenantLimits>,
    ) -> Result<(Outcome, CallTiming)> {
        match self {
            Function::Agent(f) => f.call_streaming(
                event,
                script,
                timeout,
                caller,
                key,
                sink,
                workspace,
                tenant_limits,
            ),
            // A warm-exec pool takes a script as the last word of its
            // command line (todo 3.4); a warm-exec *function* has an `entry`
            // of its own and is not asked for one. Either way there is no
            // agent here, so `sink`, `workspace` and `tenant_limits` have
            // nowhere to go — see the note on `Function::call_full`.
            #[cfg(target_os = "linux")]
            Function::Exec(f) => f.call_script_timed(event, script, timeout),
        }
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

/// Where the Python agent and its handler are bind-mounted inside every
/// sandbox. [`BuiltinAgent`] has the same pair for every runtime.
pub const AGENT_IN_SANDBOX: &str = "/zygo/agent.py";
pub const HANDLER_IN_SANDBOX: &str = "/zygo/handler.py";

/// The mounts a warm agent-backed function needs on top of its spec's own.
pub fn agent_mounts(
    agent: BuiltinAgent,
    agent_host: &std::path::Path,
    f: &ResolvedFn,
) -> Vec<crate::spec::Mount> {
    use crate::spec::{Mount, MountMode};

    let mut mounts = f.mounts.clone();
    mounts.push(Mount {
        source: agent_host.to_path_buf(),
        target: PathBuf::from(agent.agent_in_sandbox()),
        mode: MountMode::Ro,
    });
    if let Some(entry) = &f.entry {
        mounts.push(Mount {
            source: entry.clone(),
            target: PathBuf::from(agent.handler_in_sandbox()),
            mode: MountMode::Ro,
        });
    }
    mounts
}

/// Which warm mode a function uses (design doc §3.4).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// An agent in the box gives each request its own process.
    Agent(BuiltinAgent),
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

    /// What these tests were written against, before a script became the
    /// other thing written into a sandbox the same way.
    fn write_secret_file_at(dir: &std::fs::File, name: &str, value: &str) -> Result<()> {
        write_request_file_at(dir, name, value, "writing a secret into the sandbox")
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
        write_secret_file_at(&dir, "KEY", "short").expect("rewrite");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "short");
    }

    /// A secret left behind by an earlier failure does not stop the next
    /// write.
    ///
    /// The file is `0400` and the supervisor is not root, so opening it with
    /// `O_TRUNC` fails with EACCES — and every later request on that function
    /// failed with it, for ever (B-01). Attempted with a real `0400` file
    /// rather than asserted about the flags, because the flags are not what
    /// broke: the mode of a file nobody expected to be there was.
    #[cfg(unix)]
    #[test]
    fn a_secret_left_by_a_failed_write_does_not_wedge_the_next_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = std::fs::File::open(tmp.path()).expect("dirfd");
        write_secret_file_at(&dir, "KEY", "from-a-failed-attempt").expect("write");

        write_secret_file_at(&dir, "KEY", "the-real-value")
            .expect("a 0400 file from an earlier attempt must not refuse the next write");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("KEY")).expect("read"),
            "the-real-value"
        );
    }

    /// A write that fails part way through leaves nothing behind.
    ///
    /// `write_secrets` records the directory *before* it writes, so the
    /// cleanup has somewhere to aim; it recorded it afterwards until B-01, and
    /// `unlink_all` then saw `dir == None` and removed nothing. The failure is
    /// provoked with a name that cannot be created.
    #[cfg(unix)]
    #[test]
    fn a_partly_written_set_of_secrets_is_removed_rather_than_left() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut secrets = Secrets::default();
        secrets.values.insert("AAA_GOOD".into(), "value".into());
        // A name with a slash cannot be created in this directory, and sorts
        // after the good one, so the first write succeeds and the second does
        // not — which is exactly the shape that left a file behind.
        secrets.values.insert("ZZZ/BAD".into(), "value".into());

        let dir = std::fs::File::open(tmp.path()).expect("dirfd");
        let err = {
            use std::os::fd::AsFd;
            write_secrets(&mut secrets, SecretsAt::Dir(dir.as_fd()))
                .expect_err("a name with a slash cannot be created")
        };
        assert!(format!("{err}").contains("secret"), "{err}");

        assert!(
            !tmp.path().join("AAA_GOOD").exists(),
            "the secret written before the failure was left behind"
        );
        assert!(
            secrets.dir.is_none(),
            "a failed placement must not look like a successful one"
        );
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

    // --- a request's own script (protocol 1.1) ------------------------------

    /// Where a script goes, and what it looks like when it gets there.
    ///
    /// `0400` on the file and `0711` on the directory: a child can open a
    /// script whose digest it knows, and `readdir` tells it nothing about what
    /// else is in flight on a pool zygote it shares with other tenants.
    #[cfg(unix)]
    #[test]
    fn a_script_arrives_at_0400_in_a_directory_that_cannot_be_listed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("run-script");
        let scripts = Mutex::new(Scripts::default());

        let lease = place_script(&scripts, &dir, "abc123", "x = 1\n").expect("place");
        let file = dir.join("abc123");
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "x = 1\n");
        assert_eq!(
            std::fs::metadata(&file).expect("stat").permissions().mode() & 0o777,
            0o400
        );
        assert_eq!(
            std::fs::metadata(&dir).expect("stat").permissions().mode() & 0o777,
            SCRIPT_DIR_MODE,
            "the directory a tenant could list is the one thing this must not be"
        );

        drop(lease);
        assert!(
            !file.exists(),
            "the script outlived the request that named it"
        );
    }

    /// Two requests, one script: the first to finish must not take the file
    /// away from the second. The per-script count is the whole reason
    /// [`Scripts`] holds one.
    #[cfg(unix)]
    #[test]
    fn a_script_two_requests_share_leaves_when_the_second_one_does() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("run-script");
        let scripts = Mutex::new(Scripts::default());

        let first = place_script(&scripts, &dir, "shared", "handler = 1\n").expect("first");
        let second = place_script(&scripts, &dir, "shared", "handler = 1\n").expect("second");
        drop(first);
        assert!(
            dir.join("shared").exists(),
            "the request still running lost its script"
        );
        drop(second);
        assert!(!dir.join("shared").exists());
    }

    /// Two tenants' scripts in flight at once are two files, and neither
    /// departure disturbs the other.
    #[cfg(unix)]
    #[test]
    fn two_scripts_in_flight_are_two_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("run-script");
        let scripts = Mutex::new(Scripts::default());

        let a = place_script(&scripts, &dir, "aaa", "a = 1\n").expect("a");
        let b = place_script(&scripts, &dir, "bbb", "b = 2\n").expect("b");
        assert_eq!(
            std::fs::read_dir(&dir).expect("readdir").count(),
            2,
            "one file per script in flight"
        );
        drop(a);
        assert!(!dir.join("aaa").exists());
        assert!(dir.join("bbb").exists());
        drop(b);
        assert_eq!(std::fs::read_dir(&dir).expect("readdir").count(), 0);
        assert!(
            scripts.lock().expect("scripts").dir.is_none(),
            "the last script out closes the directory it was written through"
        );
    }

    /// A placement that cannot happen leaves the count honest.
    ///
    /// The lease is taken either way — `place_script` builds it after the lock
    /// is released and before it reports the failure — so a sandbox that
    /// refuses one write does not leave a phantom reference behind that stops
    /// the next request's file ever being removed.
    #[cfg(unix)]
    #[test]
    fn a_failed_placement_does_not_leak_a_reference() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A file where the directory should be: `create_dir_all` cannot win.
        let dir = tmp.path().join("run-script");
        std::fs::write(&dir, "not a directory").expect("write");
        let scripts = Mutex::new(Scripts::default());

        assert!(
            place_script(&scripts, &dir, "abc", "x = 1\n").is_err(),
            "a file is not a directory"
        );
        assert!(
            scripts.lock().expect("scripts").resident.is_empty(),
            "the failed request is still counted as holding its script"
        );
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

    /// A runtime pool: an image and an agent, and nothing of anybody's.
    fn resolved_pool() -> ResolvedFn {
        let layer = Layer {
            image: Some("python:3.12-slim".into()),
            runtime: Some(crate::spec::Runtime::Builtin(
                crate::spec::BuiltinRuntime::Python,
            )),
            ..Default::default()
        };
        resolve_standalone(
            "demo-pool",
            &layer,
            &ResolveOptions {
                pool: true,
                ..Default::default()
            },
        )
        .unwrap()
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
    fn the_embedded_node_agent_is_the_real_one() {
        assert!(
            NODE_AGENT.contains("const PROTOCOL_VERSION = 1"),
            "the embedded Node agent is not the reference agent"
        );
        assert!(
            NODE_AGENT.contains("--fd"),
            "the embedded Node agent cannot take an inherited socket"
        );
        // The one thing the Node agent is here to do that the example did
        // not: honour the supervisor's child filter, one way or the other.
        assert!(
            NODE_AGENT.contains(crate::protocol::CHILD_SECCOMP_ENV),
            "the embedded Node agent ignores the strict child filter"
        );
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

    #[test]
    fn the_agent_is_started_on_an_inherited_descriptor() {
        for (agent, interpreter) in [
            (BuiltinAgent::Python, "python3"),
            (BuiltinAgent::Node, "node"),
        ] {
            let argv = agent.argv("function", true);
            assert_eq!(argv[0], interpreter);
            assert_eq!(argv[1], agent.agent_in_sandbox());
            assert_eq!(argv[2], "--fd");
            assert_eq!(argv[3], AGENT_FD.to_string());
            assert_eq!(argv[4], agent.handler_in_sandbox());
            assert!(
                !argv.iter().any(|a| a.contains(".sock")),
                "a socket path would have to exist inside the sandbox: {argv:?}"
            );
        }
    }

    /// A pool zygote is started with no handler *argument*, not with an empty
    /// one: the agent decides which shape it is by whether it was given one,
    /// and an empty string is a path it would try to open.
    #[test]
    fn a_pool_zygote_is_started_without_a_handler_at_all() {
        for agent in [BuiltinAgent::Python, BuiltinAgent::Node] {
            let argv = agent.argv("function", false);
            assert_eq!(argv.len(), 4, "{argv:?}");
            assert_eq!(argv[3], AGENT_FD.to_string());
            assert!(
                !argv.iter().any(|a| a.contains("handler")),
                "a pool zygote must not be told about a handler: {argv:?}"
            );
        }
    }

    /// And nothing of the tenant's is mounted into one either. The pool's
    /// whole isolation claim is that its zygote is anonymous.
    #[test]
    fn a_pool_zygote_mounts_the_agent_and_nothing_else() {
        let pool = resolved_pool();
        let mounts = agent_mounts(
            BuiltinAgent::Python,
            std::path::Path::new("/data/agents/python/zygo_agent.py"),
            &pool,
        );
        assert_eq!(mounts.len(), 1, "{mounts:?}");
        assert_eq!(
            mounts[0].target,
            PathBuf::from(BuiltinAgent::Python.agent_in_sandbox())
        );
    }

    /// The extension is not decoration: `require` and `import` both decide
    /// what a file is by its name, so a Node handler mounted at `.py` is a
    /// handler Node will not load.
    #[test]
    fn each_agent_keeps_its_own_paths_in_the_sandbox() {
        let python = BuiltinAgent::Python;
        let node = BuiltinAgent::Node;
        assert!(python.agent_in_sandbox().ends_with(".py"));
        assert!(python.handler_in_sandbox().ends_with(".py"));
        assert!(node.agent_in_sandbox().ends_with(".js"));
        assert!(node.handler_in_sandbox().ends_with(".js"));
        assert_ne!(python.host_relative(), node.host_relative());
    }

    #[test]
    fn the_agent_and_handler_are_mounted_read_only_alongside_the_specs_mounts() {
        use crate::spec::MountMode;

        let mut f = resolved(Some("/host/handler.py"));
        f.mounts = vec!["/host/cache:/cache:rw".parse().unwrap()];

        let mounts = agent_mounts(
            BuiltinAgent::Python,
            std::path::Path::new("/data/agents/zygo_agent.py"),
            &f,
        );
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
        let mounts = agent_mounts(
            BuiltinAgent::Python,
            std::path::Path::new("/data/agent.py"),
            &f,
        );
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

    /// A request can be found by its id or by the name its caller gave it.
    ///
    /// The key exists because the id only reaches the caller *with the
    /// answer*, which is too late to stop the call it belongs to.
    #[test]
    fn a_request_is_reachable_by_its_id_and_by_its_callers_own_key() {
        let requests = Requests::default();
        requests.start("00000001", None, Some("job-4711"));

        assert!(requests.get("00000001").is_some(), "by id");
        assert!(requests.get("job-4711").is_some(), "by key");
        assert!(requests.get("job-0000").is_none(), "a key nobody used");

        requests.finish("00000001");
        assert!(requests.get("00000001").is_none());
        assert!(
            requests.get("job-4711").is_none(),
            "the key outlived the request"
        );
    }

    /// One cancel stops every request sharing a key, which is what a caller
    /// who reused one meant.
    #[test]
    fn two_requests_under_one_key_are_two_requests_one_cancel_finds() {
        let requests = Requests::default();
        let a = requests.start("00000001", None, Some("batch"));
        let b = requests.start("00000002", None, Some("batch"));

        // One lookup finds one of them; the caller repeats until there is
        // nothing left, which is the same shape as cancelling by id twice.
        for _ in 0..2 {
            let Some(found) = requests.get("batch") else {
                panic!("a request under this key");
            };
            found.cancel();
            requests.finish(if a.cancelled() && !b.cancelled() {
                "00000001"
            } else {
                "00000002"
            });
        }
        assert!(a.cancelled() && b.cancelled());
    }

    /// A request id is a counter, so ownership is what keeps a cancel honest.
    #[test]
    fn a_tenant_can_only_cancel_its_own_requests() {
        let requests = Requests::default();
        let theirs = requests.start("00000001", Some("acme"), None);
        let operators = requests.start("00000002", None, None);

        assert!(theirs.is_for(None), "the operator may cancel anything");
        assert!(theirs.is_for(Some("acme")));
        assert!(
            !theirs.is_for(Some("globex")),
            "another tenant reached it by counting"
        );
        assert!(
            !operators.is_for(Some("acme")),
            "a tenant reached the operator's own request"
        );
    }

    /// Cancelling before the work starts is the better outcome, and is
    /// reported as a different one.
    #[test]
    fn a_cancel_before_the_child_is_admitted_says_the_work_had_not_started() {
        let requests = Requests::default();
        let request = requests.start("00000001", None, None);

        assert!(
            !request.cancel(),
            "nothing had started, so there was nothing to kill"
        );
        assert!(
            request.cancelled(),
            "and it is marked, which is what stops it"
        );
    }

    /// One word for how a request ended, and the order it is decided in.
    ///
    /// The precedence is the caller's: somebody who cancelled their own
    /// request does not want to read "timeout" because the deadline also
    /// happened to pass while the kill landed.
    #[test]
    fn the_four_kills_are_told_apart_and_the_caller_comes_first() {
        let base = Outcome {
            tenant: "acme".into(),
            function: "render".into(),
            script: None,
            id: "00000001".into(),
            exit_code: 0,
            result: serde_json::Value::Null,
            stdout: String::new(),
            stderr: String::new(),
            error: None,
            metrics: Metrics::default(),
            timed_out: false,
            cancelled: false,
            stuck: false,
            workspace: None,
        };

        assert_eq!(base.outcome(), "ok");
        assert_eq!(
            Outcome {
                exit_code: 1,
                error: Some("boom".into()),
                ..base.clone()
            }
            .outcome(),
            "error"
        );
        assert_eq!(
            Outcome {
                exit_code: 137,
                timed_out: true,
                ..base.clone()
            }
            .outcome(),
            "timeout"
        );
        assert_eq!(
            Outcome {
                exit_code: 137,
                stuck: true,
                ..base.clone()
            }
            .outcome(),
            "stuck"
        );
        assert_eq!(
            Outcome {
                exit_code: 137,
                cancelled: true,
                ..base.clone()
            }
            .outcome(),
            "cancelled"
        );

        // A cancel that raced the deadline is still a cancel.
        assert_eq!(
            Outcome {
                exit_code: 137,
                cancelled: true,
                timed_out: true,
                stuck: true,
                ..base.clone()
            }
            .outcome(),
            "cancelled"
        );

        // And the event carries what somebody bills on.
        let usage = Usage::from(&base);
        assert_eq!(usage.tenant, "acme");
        assert_eq!(usage.function, "render");
        assert_eq!(usage.request_id, "00000001");
        assert_eq!(usage.outcome, "ok");
        assert!(usage.finished_ms > 1_700_000_000_000, "{usage:?}");
    }

    #[test]
    fn per_request_cgroups_are_on_by_default() {
        // Measured at 97 µs of a 1.9 ms request; the design's open question A2
        // resolves in favour of keeping them.
        assert!(PoolConfig::new(Paths::rooted("/x")).per_request_cgroup);
    }

    #[test]
    fn an_outcome_is_only_a_success_if_nothing_went_wrong() {
        let base = Outcome {
            id: "00000001".into(),
            cancelled: false,
            stuck: false,
            workspace: None,
            tenant: "default".into(),
            function: "resize".into(),
            script: None,
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
