//! The supervisor: the process that owns the warm pool between commands.
//!
//! Zygo is daemonless (P5, ADR-005). That is a statement about *whose* process
//! this is, not about whether one exists: a warm sandbox is warm because
//! something holds it open, and that something runs in the user's own session,
//! needs no root, and is not a system service. `zygo serve` starts one if there
//! is not one already; when it dies every sandbox dies with it via `PDEATHSIG`,
//! and nothing but the image store survives on disk.
//!
//! ```text
//!   zygo exec ──unix socket──► supervisor ──agent socket──► zygote ──fork──► request
//! ```
//!
//! One RPC boundary on the request path, which is the whole point of ADR-005:
//! Docker's client → daemon → containerd → shim chain is deliberately absent.
//!
//! Recovery is by rewarming, not by state repair (design doc §3.4). If the
//! supervisor is restarted, its functions are gone and the next `serve` pays
//! the warm-up again — a few hundred milliseconds, against the standing risk of
//! reattaching to sandboxes whose state nobody can vouch for.

pub mod client;
pub mod gate;
pub mod protocol;

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::{Error, IoContext, Result};
use crate::paths::Paths;
use crate::pool::{Function, Pool, PoolConfig, Status};
use crate::spec::{Layer, ResolveOptions, ResolvedFn, Spec};

pub use gate::{Gate, Rejected};
pub use protocol::{CONTROL_VERSION, Change, ControlError, Request, Response};

/// How long a request waits for a concurrency slot before it is told to retry.
///
/// Short on purpose: the caller is blocked on a socket, and a queue that holds
/// someone for longer than their own patience has converted backpressure into a
/// timeout. Everything past this gets a `BUSY` it can act on.
pub const QUEUE_WAIT: Duration = Duration::from_secs(5);

/// Runs work on one thread that lives as long as the supervisor.
///
/// Sandboxes must be created here, not on whichever connection thread happened
/// to handle the command. `PR_SET_PDEATHSIG` — what stops a crashed supervisor
/// from leaving warm sandboxes running with a tenant's mounts still attached —
/// is delivered when the **thread** that created the child exits, not when the
/// process does. A sandbox started on a connection thread therefore dies the
/// moment that CLI command returns.
///
/// Measured on 5.10/aarch64 with a plain `fork` and `prctl`, no Zygo involved: a
/// child whose creating thread then exited was killed inside 300 ms, while the
/// same child created by a thread that stayed was still running. Dropping
/// `PDEATHSIG` would have fixed the symptom by giving up the guarantee; moving
/// creation to a thread that outlives the sandboxes keeps both.
///
/// Warming is serialised as a consequence. That is acceptable and not on the
/// request path: it costs a few hundred milliseconds once per function, and
/// `exec` never comes through here.
struct Launcher {
    jobs: std::sync::mpsc::Sender<Job>,
    thread: Option<std::thread::JoinHandle<()>>,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

impl Launcher {
    fn new() -> Result<Launcher> {
        let (jobs, inbox) = std::sync::mpsc::channel::<Job>();
        let thread = std::thread::Builder::new()
            .name("zygo-launcher".into())
            .spawn(move || {
                for job in inbox {
                    job();
                }
            })
            .map_err(|e| Error::primitive("spawn", "launcher thread", e))?;
        Ok(Launcher {
            jobs,
            thread: Some(thread),
        })
    }

    /// Run `f` on the launcher thread and wait for its result.
    fn run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
        let (done, wait) = std::sync::mpsc::channel();
        self.jobs
            .send(Box::new(move || {
                // A receiver that has gone away means the caller was killed; the
                // work still ran, and dropping the result is the right thing.
                let _ = done.send(f());
            }))
            .map_err(|_| launcher_gone())?;
        wait.recv().map_err(|_| launcher_gone())
    }
}

impl Drop for Launcher {
    /// Closing the channel ends the loop; joining makes sure the thread is
    /// really gone before the sandboxes it created are dropped.
    fn drop(&mut self) {
        let (dead, _) = std::sync::mpsc::channel();
        let _ = std::mem::replace(&mut self.jobs, dead);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn launcher_gone() -> Error {
    Error::BackendUnavailable {
        backend: "supervisor",
        reason: "the launcher thread is gone; a previous sandbox start panicked".into(),
        remedy: "restart the supervisor: `zygo stop --all`, then serve again".into(),
    }
}

/// How soon after a crash the first rewarm may be attempted.
///
/// Immediately: the design document's acceptance criterion is that a function is
/// back within 500 ms of its agent dying, and a warm-up is ~300 ms, so there is
/// no room for a delay before the first try. The backoff exists for the case
/// this cannot fix — a handler that crashes the interpreter on import — where
/// retrying in a tight loop would turn one broken function into a busy host.
pub const REWARM_BACKOFF_BASE: Duration = Duration::from_millis(200);

/// The longest a crashed function waits between rewarm attempts.
pub const REWARM_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Failed rewarm attempts for one name, kept across replacements.
///
/// Deliberately *not* stored in [`Entry`]: a rewarm replaces the entry, so
/// state living there would reset on every attempt and the backoff would never
/// grow.
#[derive(Debug, Default, Clone, Copy)]
struct Backoff {
    failures: u32,
    last_attempt: Option<Instant>,
}

impl Backoff {
    /// How long to wait after `failures` consecutive failures.
    fn delay(failures: u32) -> Duration {
        if failures == 0 {
            return Duration::ZERO;
        }
        REWARM_BACKOFF_BASE
            .saturating_mul(1u32.checked_shl(failures - 1).unwrap_or(u32::MAX))
            .min(REWARM_BACKOFF_MAX)
    }

    /// Whether another attempt is allowed yet, and how long is left if not.
    fn ready(&self, now: Instant) -> std::result::Result<(), Duration> {
        let Some(last) = self.last_attempt else {
            return Ok(());
        };
        let wait = Backoff::delay(self.failures);
        let elapsed = now.saturating_duration_since(last);
        if elapsed >= wait {
            Ok(())
        } else {
            Err(wait - elapsed)
        }
    }
}

/// How often the idle policy is applied.
///
/// `idle_timeout` and `cold_after` default to ten minutes and an hour, so a
/// second of granularity is far finer than anything that depends on it. The
/// cost is one pass over the registry, and it is what lets the thread be a
/// plain sleeper rather than a timer wheel.
pub const TIER_INTERVAL: Duration = Duration::from_secs(1);

/// What a function was built from, as bytes on disk.
///
/// The resolved spec *names* the handler and the requirements file; this is
/// what was in them when the sandbox started. `zygo up` compares it against the
/// disk to decide whether a function is the one already running, because the
/// spec alone cannot tell: editing `handler.py` changes nothing the spec sees,
/// and an edited handler is the most common reason to deploy at all.
///
/// Mounts are deliberately absent. A bind mount is live by design — the sandbox
/// sees a file the moment it changes — so their contents are not something the
/// warm-up captured and a restart would not refresh.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Sources(Vec<(PathBuf, Option<[u8; 32]>)>);

impl Sources {
    /// Hash the files `resolved` will load. A file that cannot be read hashes
    /// to `None`: the warm-up will report that properly, and until then the
    /// comparison only has to say "not what is registered", which it does.
    fn of(resolved: &ResolvedFn) -> Sources {
        use sha2::{Digest, Sha256};
        Sources(
            resolved
                .entry
                .iter()
                .chain(resolved.requirements.iter())
                .map(|path| {
                    let digest = std::fs::read(path)
                        .ok()
                        .map(|bytes| Sha256::digest(&bytes).into());
                    (path.clone(), digest)
                })
                .collect(),
        )
    }
}

/// One registered function.
struct Entry {
    resolved: ResolvedFn,
    /// Secret values, kept apart from `resolved` on purpose: the resolved spec
    /// is what `zygo spec explain` prints, and these must never be in it.
    secrets: BTreeMap<String, String>,
    /// What was on disk when the sandbox started. See [`Sources`].
    sources: Sources,
    function: Function,
    gate: Gate,
    registered: Instant,
    /// When a request last finished. Drives the idle policy.
    last_used: Mutex<Instant>,
}

impl Entry {
    fn status(&self) -> Status {
        self.function.status()
    }

    fn touch(&self) {
        *self.last_used.lock().expect("last_used") = Instant::now();
    }

    fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(*self.last_used.lock().expect("last_used"))
    }
}

/// A function that has been tiered all the way down.
///
/// Only the spec is kept. That is the whole point: a cold function costs
/// nothing but a map entry, and the next request rebuilds it from exactly the
/// configuration it had. The last status is kept alongside so `zygo ps` does not
/// reset someone's request count just because their function went quiet.
struct Cold {
    resolved: ResolvedFn,
    secrets: BTreeMap<String, String>,
    sources: Sources,
    last_status: Status,
    since: Instant,
}

/// What one pass of the idle policy did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Tiered {
    /// Frozen: still resident, no longer schedulable.
    pub paused: Vec<String>,
    /// Stopped: the sandbox is gone, the spec is kept.
    pub cooled: Vec<String>,
}

impl Tiered {
    pub fn is_empty(&self) -> bool {
        self.paused.is_empty() && self.cooled.is_empty()
    }
}

/// The registry plus the pool behind it.
///
/// Shared between connection threads; every field is behind its own lock so a
/// slow `serve` on one function does not stop `zygo ps` on another.
pub struct Supervisor {
    /// Shared so a sandbox start can be handed to the launcher thread.
    pool: Arc<Pool>,
    /// Creates every sandbox, for the lifetime of the supervisor. See [`Launcher`].
    launcher: Launcher,
    paths: Paths,
    functions: Mutex<BTreeMap<String, Arc<Entry>>>,
    /// Functions tiered down to cold: registered, but with no sandbox.
    cold: Mutex<BTreeMap<String, Cold>>,
    /// Rewarm history per name. See [`Backoff`] for why it is not in `Entry`.
    rewarms: Mutex<BTreeMap<String, Backoff>>,
    /// Recent log per name. Kept here rather than in `Entry` so it survives
    /// a replacement and a cold spell: a `zygo logs -f` across a deploy
    /// should show the new zygote coming up, not go quiet.
    logs: Mutex<BTreeMap<String, crate::pool::Logs>>,
    started: Instant,
    /// Set by `SHUTDOWN`; the accept loop notices and stops.
    stopping: AtomicBool,
}

impl Supervisor {
    pub fn new(paths: Paths) -> Result<Supervisor> {
        let pool = Arc::new(Pool::new(PoolConfig::new(paths.clone()))?);

        // Sandboxes die with their supervisor, but cgroups outlive their
        // processes. Without this a restarted supervisor inherits one dead
        // `tenants/<name>` tree per previous lifetime, and would reuse their
        // stale limits for any name it serves again.
        if let Ok(hierarchy) = crate::cgroup::Hierarchy::discover() {
            let cleaned = hierarchy.clean_stale_tenants();
            if !cleaned.is_empty() {
                tracing::info!(
                    tenants = ?cleaned,
                    "removed cgroups left by a previous supervisor"
                );
            }
        }

        Ok(Supervisor {
            pool,
            launcher: Launcher::new()?,
            paths,
            functions: Mutex::new(BTreeMap::new()),
            cold: Mutex::new(BTreeMap::new()),
            rewarms: Mutex::new(BTreeMap::new()),
            logs: Mutex::new(BTreeMap::new()),
            started: Instant::now(),
            stopping: AtomicBool::new(false),
        })
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Resolve a spec and bring the function up warm.
    ///
    /// Replaces any function already under that name: `zygo serve` on a handler
    /// you just edited should give you the new one, and a name that silently
    /// kept serving stale code would be a debugging trap. The replacement is
    /// blue/green — the new sandbox is warm before the old one stops taking
    /// requests, requests the old one accepted finish on it, and requests that
    /// were queued behind it are admitted to the new one.
    ///
    /// With `if_changed`, a registered function that is identical — same
    /// resolved spec, same secret values, same bytes in the handler and
    /// requirements files — is kept instead, so a deploy that changed three of
    /// ten functions restarts three.
    pub fn serve(
        &self,
        name: &str,
        spec: Option<&Spec>,
        layer: &Layer,
        options: &ResolveOptions,
        secrets: BTreeMap<String, String>,
        if_changed: bool,
    ) -> std::result::Result<Response, Response> {
        // Before anything else, including validation: the user has just said
        // what this function should be, so the automatic-recovery history of
        // whatever used to hold the name is stale either way. Leaving it would
        // make the *new* function serve out a backoff the old one earned.
        self.rewarms.lock().expect("rewarms").remove(name);

        let owned;
        let spec = match spec {
            Some(s) => s,
            None => {
                owned = Spec::default();
                &owned
            }
        };
        let resolved = spec
            .resolve_for_serve(name, layer, options)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;

        // Every secret the spec names has to have a value, and the client is
        // the only party that could have supplied one — so a missing value is
        // reported here as the spec problem it is, before a sandbox exists.
        let missing: Vec<&str> = resolved
            .secrets
            .iter()
            .filter(|s| !secrets.contains_key(*s))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(Response::error(
                ControlError::BadSpec,
                format!(
                    "fn.{name}.secrets: no value for {}; \
                     set {} in the environment of the shell running `zygo serve`",
                    missing.join(", "),
                    if missing.len() == 1 { "it" } else { "them" }
                ),
            ));
        }
        // Only what the spec asked for. A client that sent more — its whole
        // environment, say — must not have the surplus delivered.
        let secrets: BTreeMap<String, String> = secrets
            .into_iter()
            .filter(|(k, _)| resolved.secrets.contains(k))
            .collect();

        let warming = Instant::now();
        if if_changed && self.is_registered_as(name, &resolved, &secrets) {
            // Kept, but brought to warm: `up` promises warm functions, and an
            // unchanged one may have been tiered down since the last deploy.
            let entry = self.ensure_warm(name)?;
            let status = entry.status();
            return Ok(Response::Served {
                name: name.to_string(),
                runtime: status.runtime,
                rss_kb: status.rss_kb,
                imports_ms: status.imports_ms,
                warm_ms: warming.elapsed().as_secs_f64() * 1000.0,
                warnings: resolved.warnings,
                change: Change::Unchanged,
            });
        }

        let warmed = self.warm_and_register(name, resolved, secrets)?;
        Ok(Response::Served {
            name: name.to_string(),
            runtime: warmed.status.runtime,
            rss_kb: warmed.status.rss_kb,
            imports_ms: warmed.status.imports_ms,
            warm_ms: warming.elapsed().as_secs_f64() * 1000.0,
            warnings: warmed.warnings,
            change: if warmed.replaced {
                Change::Replaced
            } else {
                Change::Started
            },
        })
    }

    /// Whether `name` is registered — warm, paused or cold — as exactly this
    /// function: the spec the caller resolved, the secret values it sent, and
    /// what is in the handler and requirements files *now*.
    fn is_registered_as(
        &self,
        name: &str,
        resolved: &ResolvedFn,
        secrets: &BTreeMap<String, String>,
    ) -> bool {
        let sources = Sources::of(resolved);
        if let Some(entry) = self.functions.lock().expect("registry").get(name) {
            return entry.resolved == *resolved
                && entry.secrets == *secrets
                && entry.sources == sources;
        }
        if let Some(cold) = self.cold.lock().expect("cold").get(name) {
            return cold.resolved == *resolved
                && cold.secrets == *secrets
                && cold.sources == sources;
        }
        false
    }

    /// Warm a resolved function and put it in the registry under `name`.
    ///
    /// Shared by `serve` and by the rewarm path, so a function that comes back
    /// after a crash comes back with the same limits, the same concurrency and
    /// the same gate as the one the user asked for.
    fn warm_and_register(
        &self,
        name: &str,
        resolved: ResolvedFn,
        secrets: BTreeMap<String, String>,
    ) -> std::result::Result<Warmed, Response> {
        // Hashed before the warm-up rather than after, so an edit that lands
        // while the sandbox is starting counts as a change next time — the
        // sandbox may or may not have read it, and "replace" is the safe answer.
        let sources = Sources::of(&resolved);

        // On the launcher thread, never here: see [`Launcher`].
        let pool = Arc::clone(&self.pool);
        let spec_for_warm = resolved.clone();
        let logs = self.logs_for(name);
        let function = self
            .launcher
            .run(move || pool.serve_with_logs(&spec_for_warm, logs))
            .map_err(|e| Response::error(ControlError::WarmFailed, e))?
            .map_err(|e| Response::error(ControlError::WarmFailed, e))?;
        function.set_secrets(secrets.clone());

        let status = function.status();
        let warnings = resolved.warnings.clone();
        let entry = Arc::new(Entry {
            gate: Gate::new(
                resolved.concurrency,
                Gate::default_queue_limit(resolved.concurrency),
            ),
            resolved,
            secrets,
            sources,
            function,
            registered: Instant::now(),
            last_used: Mutex::new(Instant::now()),
        });

        // The previous entry is dropped after the lock is released: dropping a
        // `WarmFn` tears a sandbox down, and doing that while holding the
        // registry lock would block every other connection on it.
        let previous = {
            let mut functions = self.functions.lock().expect("registry");
            functions.insert(name.to_string(), Arc::clone(&entry))
        };
        // A cold registration under this name is superseded too, whether this
        // is the wake-up that was waiting for it or a deploy over it.
        let was_cold = self.cold.lock().expect("cold").remove(name).is_some();
        let replaced = previous.is_some() || was_cold;
        if let Some(previous) = previous {
            self.retire(previous);
        }
        Ok(Warmed {
            entry,
            status,
            warnings,
            replaced,
        })
    }

    /// Bring a crashed function back, respecting its backoff.
    ///
    /// Recovery is by rewarming rather than repair (§3.4): a sandbox whose agent
    /// died has no state worth recovering, and the spec that built it is still
    /// in the registry. The backoff is what stops a function that cannot start
    /// at all — a handler that segfaults on import — from becoming a rewarm loop
    /// that costs more than the function ever would.
    fn rewarm(&self, name: &str, broken: &Arc<Entry>) -> std::result::Result<Arc<Entry>, Response> {
        let now = Instant::now();
        {
            let mut rewarms = self.rewarms.lock().expect("rewarms");
            let backoff = rewarms.entry(name.to_string()).or_default();
            if let Err(left) = backoff.ready(now) {
                return Err(Response::error(
                    ControlError::WarmFailed,
                    format!(
                        "`{name}` crashed and has failed to restart {} time(s); \
                         retrying in {} ms",
                        backoff.failures,
                        left.as_millis()
                    ),
                ));
            }
            backoff.last_attempt = Some(now);
        }

        tracing::warn!(function = name, "agent is not healthy; rewarming");
        let outcome = self.warm_and_register(name, broken.resolved.clone(), broken.secrets.clone());

        let mut rewarms = self.rewarms.lock().expect("rewarms");
        let backoff = rewarms.entry(name.to_string()).or_default();
        match outcome {
            Ok(warmed) => {
                backoff.failures = 0;
                Ok(warmed.entry)
            }
            Err(response) => {
                backoff.failures = backoff.failures.saturating_add(1);
                Err(response)
            }
        }
    }

    /// Apply the idle policy once (design doc F12, §3.9).
    ///
    /// Two tiers, both driven by how long it has been since a request finished:
    /// past `idle_timeout` a function is frozen, which keeps the resident pages
    /// that make the next request a `fork()` while costing no CPU; past
    /// `cold_after` its sandbox is dropped entirely and only the spec is kept.
    ///
    /// Separated from the thread that calls it so the policy can be tested by
    /// calling it, rather than by sleeping and hoping.
    pub fn tier_idle(&self) -> Tiered {
        let now = Instant::now();
        let candidates: Vec<(String, Arc<Entry>)> = {
            let functions = self.functions.lock().expect("registry");
            functions
                .iter()
                .map(|(name, entry)| (name.clone(), Arc::clone(entry)))
                .collect()
        };

        let mut tiered = Tiered::default();
        for (name, entry) in candidates {
            // A request in flight means the function is in use whatever the
            // clock says; freezing it would stop the request it is serving.
            let (in_flight, queued) = entry.gate.load();
            if in_flight > 0 || queued > 0 {
                continue;
            }
            let idle = entry.idle_for(now);

            if idle >= entry.resolved.cold_after.get() {
                if self.cool(&name, &entry) {
                    tiered.cooled.push(name);
                }
            } else if idle >= entry.resolved.idle_timeout.get()
                && entry.function.state() == crate::sandbox::SandboxState::Warm
                && entry.function.pause().is_ok()
            {
                tiered.paused.push(name);
            }
        }
        tiered
    }

    /// Drop a function's sandbox but keep its registration.
    fn cool(&self, name: &str, entry: &Arc<Entry>) -> bool {
        let mut status = entry.status();
        status.state = crate::sandbox::SandboxState::Cold;

        let removed = {
            let mut functions = self.functions.lock().expect("registry");
            // Only remove the entry we looked at: a `serve` may have replaced it
            // while we were deciding, and that new sandbox is not idle.
            match functions.get(name) {
                Some(current) if Arc::ptr_eq(current, entry) => functions.remove(name),
                _ => None,
            }
        };
        let Some(removed) = removed else {
            return false;
        };

        self.cold.lock().expect("cold").insert(
            name.to_string(),
            Cold {
                resolved: removed.resolved.clone(),
                secrets: removed.secrets.clone(),
                sources: removed.sources.clone(),
                last_status: status,
                since: Instant::now(),
            },
        );
        self.retire(removed);
        true
    }

    /// Route one request to its function.
    pub fn exec(
        &self,
        name: &str,
        mut event: serde_json::Value,
        timeout: Duration,
    ) -> std::result::Result<Response, Response> {
        let mut entry = self.lookup(name)?;

        // At most one redirect. A gate closes for two reasons — the function was
        // stopped, or it was replaced — and a request that was queued behind a
        // function being replaced belongs on its replacement, not on the floor.
        // Whoever replaced it warmed the new one first, so this is a retry that
        // costs one registry lookup. A second closure in a row means someone is
        // redeploying faster than requests are being admitted, and at that
        // point telling the caller is better than looping.
        for _ in 0..2 {
            // A function whose agent died is replaced before the request is
            // sent, not after it fails: the caller should not pay for a crash
            // that happened between their request and somebody else's.
            if !entry.function.is_healthy() {
                entry = self.rewarm(name, &entry)?;
            }

            // Thawing is one write, so a paused function answers at warm speed
            // plus that. This is the whole reason pausing is a tier of its own
            // rather than going straight to cold.
            if let Err(e) = entry.function.resume() {
                return Err(Response::error(ControlError::CallFailed, e));
            }

            event = match self.attempt(&entry, name, event, timeout) {
                Attempt::Done(response) => return response,
                Attempt::Closed(event) => event,
            };
            match self.lookup(name) {
                Ok(current) if !Arc::ptr_eq(&current, &entry) => entry = current,
                _ => break,
            }
        }
        Err(Response::error(
            ControlError::NotFound,
            format!("`{name}` is shutting down"),
        ))
    }

    /// One pass through a function's gate and, if admitted, its sandbox.
    fn attempt(
        &self,
        entry: &Entry,
        name: &str,
        event: serde_json::Value,
        timeout: Duration,
    ) -> Attempt {
        let permit = match entry.gate.enter(QUEUE_WAIT) {
            Ok(permit) => permit,
            Err(Rejected::Closed) => return Attempt::Closed(event),
            Err(rejected) => {
                let (in_flight, queued, limit) = rejected.load().unwrap_or_default();
                return Attempt::Done(Ok(Response::Busy {
                    name: name.to_string(),
                    in_flight,
                    queued,
                    limit,
                }));
            }
        };

        let outcome = entry
            .function
            .call_with_timeout(event, timeout)
            .map_err(|e| Response::error(ControlError::CallFailed, e));
        drop(permit);
        // After the call, not before: the clock should measure how long the
        // function has been idle, not how long ago a slow request started.
        entry.touch();

        if let Ok(outcome) = &outcome {
            self.logs_for(name).push(
                crate::pool::LogKind::Request {
                    id: next_log_request_id(),
                    exit_code: outcome.exit_code,
                    wall_ms: outcome.metrics.wall_ms,
                    timed_out: outcome.timed_out,
                    error: outcome.error.clone(),
                    stderr: outcome.stderr.clone(),
                },
                outcome.stdout.clone(),
            );
        }

        Attempt::Done(outcome.map(|outcome| Response::Executed {
            outcome: Box::new(outcome),
        }))
    }

    /// The log ring for `name`, created on first use.
    fn logs_for(&self, name: &str) -> crate::pool::Logs {
        Arc::clone(
            self.logs
                .lock()
                .expect("logs")
                .entry(name.to_string())
                .or_default(),
        )
    }

    /// A function's recent log.
    ///
    /// A name that was never served has no log and is `not_found`; one that
    /// was served and went cold keeps its log, because the questions people
    /// bring to a log — "why did that fail?" — are asked after the fact.
    pub fn logs(
        &self,
        name: &str,
        after: u64,
        limit: u32,
        failed: bool,
    ) -> std::result::Result<Response, Response> {
        let ring = self
            .logs
            .lock()
            .expect("logs")
            .get(name)
            .cloned()
            .ok_or_else(|| {
                Response::error(
                    ControlError::NotFound,
                    format!("no function named `{name}`; `zygo serve` it first"),
                )
            })?;
        let (entries, next) = ring.since(after, limit.max(1) as usize, failed);
        Ok(Response::Logs {
            name: name.to_string(),
            entries,
            next,
        })
    }

    /// Everything `zygo ps` shows, ordered by name so the output is stable.
    pub fn list(&self) -> Vec<Status> {
        let mut all: BTreeMap<String, Status> = self
            .functions
            .lock()
            .expect("registry")
            .iter()
            .map(|(name, entry)| (name.clone(), entry.status()))
            .collect();
        // Cold functions are still registered, so `ps` has to show them —
        // otherwise a function that went quiet looks like one that was never
        // served, and `zygo exec` on it would be a surprise either way.
        for (name, cold) in self.cold.lock().expect("cold").iter() {
            all.entry(name.clone())
                .or_insert_with(|| cold.last_status.clone());
        }
        all.into_values().collect()
    }

    /// Per-function load, for the columns `zygo ps` adds beyond [`Status`].
    pub fn load(&self) -> Vec<FunctionLoad> {
        let functions = self.functions.lock().expect("registry");
        functions
            .values()
            .map(|e| {
                let (in_flight, queued) = e.gate.load();
                FunctionLoad {
                    name: e.resolved.name.clone(),
                    in_flight,
                    queued,
                    limit: e.gate.limit(),
                    uptime: e.registered.elapsed(),
                }
            })
            .collect()
    }

    /// Bring a registered function to `Warm` without calling it.
    ///
    /// The same path a request takes to find its function — waking it from
    /// cold, rewarming it if its agent died, thawing it if it was paused —
    /// stopped just short of sending anything.
    pub fn warm(&self, name: &str) -> std::result::Result<Response, Response> {
        let entry = self.ensure_warm(name)?;
        Ok(Response::Warmed {
            name: name.to_string(),
            state: entry.function.state(),
        })
    }

    /// Where a debug shell should enter, for `zygo shell`.
    ///
    /// Warms the function first, for the same reason `warm` does: a shell into
    /// a function that went cold should give you the function, not an error
    /// about the tiering policy.
    pub fn shell(&self, name: &str) -> std::result::Result<Response, Response> {
        let entry = self.ensure_warm(name)?;
        Ok(Response::Sandbox {
            name: name.to_string(),
            pid: entry.function.init_pid(),
            workdir: entry.resolved.workdir.clone(),
        })
    }

    /// The registered function called `name`, warm: woken if it was cold,
    /// rewarmed if its agent died, thawed if it was paused.
    fn ensure_warm(&self, name: &str) -> std::result::Result<Arc<Entry>, Response> {
        let mut entry = self.lookup(name)?;
        if !entry.function.is_healthy() {
            entry = self.rewarm(name, &entry)?;
        }
        if let Err(e) = entry.function.resume() {
            return Err(Response::error(ControlError::CallFailed, e));
        }
        entry.touch();
        Ok(entry)
    }

    /// Stop one function, or every function when `name` is `None`.
    pub fn stop(&self, name: Option<&str>) -> std::result::Result<Response, Response> {
        // A cold function has no sandbox left to stop, but it is still
        // registered and would come back on the next request, so `stop` has to
        // deregister it too.
        let cold_names: Vec<String> = {
            let mut cold = self.cold.lock().expect("cold");
            match name {
                Some(name) => cold
                    .remove(name)
                    .map(|_| name.to_string())
                    .into_iter()
                    .collect(),
                None => std::mem::take(&mut *cold).into_keys().collect(),
            }
        };

        let removed = {
            let mut functions = self.functions.lock().expect("registry");
            match name {
                Some(name) => match functions.remove(name) {
                    Some(entry) => vec![entry],
                    None if cold_names.is_empty() => {
                        return Err(Response::error(
                            ControlError::NotFound,
                            format!("no function named `{name}`"),
                        ));
                    }
                    None => vec![],
                },
                None => std::mem::take(&mut *functions).into_values().collect(),
            }
        };

        let mut names = removed
            .iter()
            .map(|e| e.resolved.name.clone())
            .collect::<Vec<_>>();
        names.extend(cold_names);
        names.sort();
        names.dedup();

        // Stopped is deregistered, and a deregistered name's log goes with
        // it: `stop` is the one operation that means "forget this function".
        {
            let mut logs = self.logs.lock().expect("logs");
            for n in &names {
                logs.remove(n);
            }
        }

        for entry in removed {
            self.retire(entry);
        }
        Ok(Response::Stopped { names })
    }

    /// Ask the accept loop to stop taking connections.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    /// Find a function, bringing it back from cold if that is where it is.
    ///
    /// A cold function is still registered, so a request for it is not a
    /// mistake to report — it is a cold start to pay. The caller sees a slower
    /// request, which is exactly the trade `cold_after` was configured to make.
    fn lookup(&self, name: &str) -> std::result::Result<Arc<Entry>, Response> {
        if let Some(entry) = self.functions.lock().expect("registry").get(name) {
            return Ok(Arc::clone(entry));
        }

        let cold = self
            .cold
            .lock()
            .expect("cold")
            .get(name)
            .map(|c| (c.resolved.clone(), c.secrets.clone(), c.since.elapsed()));
        if let Some((resolved, secrets, asleep)) = cold {
            tracing::info!(
                function = name,
                asleep_s = asleep.as_secs(),
                "waking a cold function"
            );
            return Ok(self.warm_and_register(name, resolved, secrets)?.entry);
        }

        Err(Response::error(
            ControlError::NotFound,
            format!("no function named `{name}`; `zygo serve` it first"),
        ))
    }

    /// Close a function to new requests and let it go.
    ///
    /// The gate is closed first so anything queued is turned away rather than
    /// admitted to a sandbox that is about to disappear. The `Arc` may still be
    /// held by a request in flight; the sandbox dies when the last one lets go,
    /// which is the behaviour a caller mid-request wants.
    fn retire(&self, entry: Arc<Entry>) {
        entry.gate.close();
        let _ = entry.function.shutdown();
    }
}

/// Request ids in the log: short, unique per supervisor, and the same shape
/// the agent protocol uses so a line in the log can be matched to a trace.
fn next_log_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{:08x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A function that was just warmed and registered.
struct Warmed {
    entry: Arc<Entry>,
    status: Status,
    warnings: Vec<String>,
    /// Whether something — warm, paused or cold — held the name before.
    replaced: bool,
}

/// One pass at a request, from [`Supervisor::attempt`].
enum Attempt {
    /// The request was admitted and ran, or was turned away with an answer.
    Done(std::result::Result<Response, Response>),
    /// The gate had been closed. The event comes back so it can be offered to
    /// whatever holds the name now.
    Closed(serde_json::Value),
}

/// What a function is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionLoad {
    pub name: String,
    pub in_flight: u32,
    pub queued: u32,
    pub limit: u32,
    pub uptime: Duration,
}

// ---------------------------------------------------------------------------
// The socket
// ---------------------------------------------------------------------------

/// A bound control socket, with the pid file that names its owner.
///
/// Separate from [`Supervisor`] so the socket can be bound — and a conflicting
/// supervisor detected — *before* a background process is spawned. Otherwise
/// the failure would arrive after the parent had already detached.
#[derive(Debug)]
pub struct Listener {
    listener: UnixListener,
    socket: PathBuf,
    pid_file: PathBuf,
}

impl Listener {
    /// Take the control socket, or explain who already has it.
    ///
    /// A socket left behind by a supervisor that died is not a conflict: it is
    /// a file nobody is listening on. The two are told apart by connecting —
    /// the only test that cannot be fooled by a stale pid file, a recycled pid,
    /// or a socket whose owner is wedged.
    pub fn bind(paths: &Paths) -> Result<Listener> {
        paths.ensure()?;
        let socket = paths.supervisor_sock();

        if socket.exists() {
            match UnixStream::connect(&socket) {
                Ok(_) => {
                    return Err(Error::BackendUnavailable {
                        backend: "supervisor",
                        reason: format!(
                            "a supervisor is already listening on {}",
                            socket.display()
                        ),
                        remedy: "use it, or stop it with `zygo stop --all`".into(),
                    });
                }
                // Nobody is home. Anything else — a permission error, say — is
                // a real problem and must not be papered over by unlinking.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(&socket).at(&socket)?;
                }
                Err(e) => return Err(Error::primitive("connect", "supervisor socket", e)),
            }
        }

        let listener = UnixListener::bind(&socket).at(&socket)?;
        restrict(&socket)?;

        let pid_file = paths.supervisor_pid();
        let mut file = std::fs::File::create(&pid_file).at(&pid_file)?;
        writeln!(file, "{}", std::process::id()).at(&pid_file)?;

        Ok(Listener {
            listener,
            socket,
            pid_file,
        })
    }

    pub fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    /// Accept connections until a `SHUTDOWN` arrives.
    ///
    /// One thread per connection. A control connection is a CLI invocation, so
    /// there are units of them rather than thousands, and a thread apiece keeps
    /// the request path a straight line with no executor in it.
    pub fn serve(self, supervisor: Arc<Supervisor>) -> Result<()> {
        // Unblocks `accept` when `SHUTDOWN` sets the flag: without it the loop
        // would sit in `accept` until some unrelated client happened to connect.
        self.listener
            .set_nonblocking(false)
            .map_err(|e| Error::primitive("set_nonblocking", "control socket", e))?;

        // The idle policy runs on its own thread rather than on whichever
        // connection happens to arrive, so a supervisor nobody is talking to
        // still gives its memory back.
        let tiering = {
            let supervisor = Arc::clone(&supervisor);
            std::thread::Builder::new()
                .name("zygo-idle".into())
                .spawn(move || {
                    while !supervisor.is_stopping() {
                        std::thread::sleep(TIER_INTERVAL);
                        let tiered = supervisor.tier_idle();
                        for name in &tiered.paused {
                            tracing::info!(function = %name, "idle: paused");
                        }
                        for name in &tiered.cooled {
                            tracing::info!(function = %name, "idle: gone cold");
                        }
                    }
                })
                .map_err(|e| Error::primitive("spawn", "idle thread", e))?
        };

        let mut threads = Vec::new();
        for stream in self.listener.incoming() {
            if supervisor.is_stopping() {
                break;
            }
            let stream = match stream {
                Ok(stream) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Error::primitive("accept", "control socket", e)),
            };
            let supervisor = Arc::clone(&supervisor);
            threads.push(std::thread::spawn(move || {
                // One bad client must not take the supervisor with it.
                if let Err(e) = handle(&supervisor, stream) {
                    tracing::debug!("control connection ended: {e}");
                }
            }));
            threads.retain(|t: &std::thread::JoinHandle<()>| !t.is_finished());
        }

        for t in threads {
            let _ = t.join();
        }
        let _ = tiering.join();
        let _ = supervisor.stop(None);
        Ok(())
    }
}

impl Drop for Listener {
    /// Take the socket and pid file with us. A supervisor that exits cleanly
    /// must not leave a file that makes the next one think it has a conflict.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.pid_file);
    }
}

/// Serve one connection: `HELLO`, then requests until the peer goes away.
fn handle(supervisor: &Supervisor, stream: UnixStream) -> Result<()> {
    if let Some(response) = reject_foreign_peer(&stream) {
        let mut writer: crate::protocol::frame::FrameWriter<_, Response> =
            crate::protocol::frame::FrameWriter::new(&stream);
        let _ = writer.write(&response);
        return Ok(());
    }

    let mut reader: crate::protocol::frame::FrameReader<_, Request> =
        crate::protocol::frame::FrameReader::new(
            stream
                .try_clone()
                .map_err(|e| Error::primitive("dup", "control socket", e))?,
        );
    let mut writer: crate::protocol::frame::FrameWriter<_, Response> =
        crate::protocol::frame::FrameWriter::new(stream);

    let mut greeted = false;
    while let Some(request) = reader
        .read()
        .map_err(|e| Error::primitive("read", "control socket", std::io::Error::other(e)))?
    {
        let shutting_down = matches!(request, Request::Shutdown);
        let response = dispatch(supervisor, request, &mut greeted);
        writer
            .write(&response)
            .map_err(|e| Error::primitive("write", "control socket", std::io::Error::other(e)))?;
        if shutting_down {
            supervisor.shutdown();
            // Nudge the accept loop out of `accept` so it sees the flag.
            let _ = UnixStream::connect(supervisor.paths().supervisor_sock());
            break;
        }
    }
    Ok(())
}

/// Answer one request.
fn dispatch(supervisor: &Supervisor, request: Request, greeted: &mut bool) -> Response {
    // `HELLO` first, always: a client that has not agreed a control version
    // must not be able to reach anything that changes state.
    if let Request::Hello { control, .. } = &request {
        if *control != CONTROL_VERSION {
            return Response::error(
                ControlError::VersionMismatch,
                format!(
                    "client speaks control v{control}, this supervisor speaks v{CONTROL_VERSION}"
                ),
            );
        }
        *greeted = true;
        return Response::Welcome {
            control: CONTROL_VERSION,
            version: crate::VERSION.to_string(),
            pid: std::process::id(),
        };
    }
    if !*greeted {
        return Response::error(ControlError::BadMessage, "expected HELLO first");
    }

    match request {
        Request::Hello { .. } => unreachable!("handled above"),
        Request::Ping => Response::Pong,
        Request::List => Response::Functions {
            functions: supervisor.list(),
        },
        Request::Serve {
            name,
            spec,
            layer,
            base_dir,
            allow_host_net,
            allow_private_net,
            allow_unlimited,
            secrets,
            if_changed,
        } => {
            let options = ResolveOptions {
                allow_host_net,
                allow_private_net,
                allow_unlimited,
                base_dir: Some(base_dir),
                one_shot: false,
            };
            merge(supervisor.serve(
                &name,
                spec.as_deref(),
                &layer,
                &options,
                secrets,
                if_changed,
            ))
        }
        Request::Exec {
            name,
            event,
            timeout_ms,
        } => merge(supervisor.exec(&name, event, Duration::from_millis(timeout_ms))),
        Request::Stop { name } => merge(supervisor.stop(name.as_deref())),
        Request::Warm { name } => merge(supervisor.warm(&name)),
        Request::Shell { name } => merge(supervisor.shell(&name)),
        Request::Logs {
            name,
            after,
            limit,
            failed,
        } => merge(supervisor.logs(&name, after, limit, failed)),
        Request::Shutdown => Response::Ok,
    }
}

/// Both arms of these results are already responses; the split only exists so
/// the happy path can use `?`.
fn merge(result: std::result::Result<Response, Response>) -> Response {
    match result {
        Ok(r) | Err(r) => r,
    }
}

/// Refuse a connection from another user.
///
/// The socket is `0600` inside a `0700` directory, so this should be
/// unreachable — which is exactly why it is worth checking. File modes are one
/// `umask`, one `--data-root` on a shared path, or one container bind mount
/// away from being wrong, and the consequence is arbitrary code execution as
/// this user.
fn reject_foreign_peer(stream: &UnixStream) -> Option<Response> {
    let peer = peer_uid(stream)?;
    let ours = current_uid();
    (peer != ours).then(|| {
        Response::error(
            ControlError::Unauthorised,
            format!("this supervisor belongs to uid {ours}, not uid {peer}"),
        )
    })
}

#[cfg(target_os = "linux")]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;

    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are live and correctly sized for SO_PEERCRED,
    // and the fd is owned by `stream` for the duration of the call.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

#[cfg(not(target_os = "linux"))]
fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;

    let (mut uid, mut gid) = (0u32, 0u32);
    // SAFETY: both out-params are live for the call and the fd is owned by
    // `stream`. `getpeereid` is the BSD/macOS spelling of `SO_PEERCRED`.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
    (rc == 0).then_some(uid)
}

fn current_uid() -> u32 {
    // SAFETY: `getuid` takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

/// `0600` on a path that only its owner may use.
fn restrict(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).at(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &tempfile::TempDir) -> Paths {
        Paths::rooted(dir.path())
    }

    // --- idle tiering ------------------------------------------------------
    //
    // These exercise the policy without a sandbox, which is what `tier_idle`
    // being a callable pass rather than a thread is for: a test that had to
    // sleep out a ten-minute `idle_timeout` would not be written.

    #[test]
    fn nothing_to_tier_is_not_an_event() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        assert!(supervisor.tier_idle().is_empty());
    }

    #[test]
    fn a_cold_function_is_still_listed_and_still_registered() {
        // The distinction that matters to someone reading `ps`: a function that
        // went quiet is not a function that was never served, and `exec` on it
        // is a cold start rather than an error.
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");

        let status = Status {
            name: "resize".into(),
            state: crate::sandbox::SandboxState::Cold,
            runtime: "python/3.12".into(),
            rss_kb: 0,
            imports_ms: 0.0,
            requests: 17,
            failures: 1,
        };
        supervisor.cold.lock().expect("cold").insert(
            "resize".into(),
            Cold {
                resolved: resolved_fn("resize"),
                secrets: BTreeMap::new(),
                sources: Sources::default(),
                last_status: status.clone(),
                since: Instant::now(),
            },
        );

        let listed = supervisor.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, crate::sandbox::SandboxState::Cold);
        assert_eq!(
            listed[0].requests, 17,
            "going cold must not reset someone's counters"
        );
    }

    #[test]
    fn stopping_a_cold_function_deregisters_it() {
        // Without this it would come back on the next request, having been
        // explicitly stopped.
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        supervisor.cold.lock().expect("cold").insert(
            "resize".into(),
            Cold {
                resolved: resolved_fn("resize"),
                secrets: BTreeMap::new(),
                sources: Sources::default(),
                last_status: cold_status("resize"),
                since: Instant::now(),
            },
        );

        assert_eq!(
            supervisor.stop(Some("resize")),
            Ok(Response::Stopped {
                names: vec!["resize".into()]
            })
        );
        assert!(supervisor.list().is_empty());
        // And it is gone for good, not merely asleep.
        assert!(matches!(
            supervisor.stop(Some("resize")),
            Err(Response::Error {
                code: ControlError::NotFound,
                ..
            })
        ));
    }

    #[test]
    fn stop_all_takes_cold_functions_with_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        for name in ["a", "b"] {
            supervisor.cold.lock().expect("cold").insert(
                name.into(),
                Cold {
                    resolved: resolved_fn(name),
                    secrets: BTreeMap::new(),
                    sources: Sources::default(),
                    last_status: cold_status(name),
                    since: Instant::now(),
                },
            );
        }
        assert_eq!(
            supervisor.stop(None),
            Ok(Response::Stopped {
                names: vec!["a".into(), "b".into()]
            })
        );
        assert!(supervisor.list().is_empty());
    }

    #[test]
    fn a_cold_function_that_cannot_be_rewarmed_reports_why() {
        // There is no image in this scratch store, so waking it fails — the
        // point is that the caller is told, rather than getting "not found" for
        // a function that is registered.
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        supervisor.cold.lock().expect("cold").insert(
            "resize".into(),
            Cold {
                resolved: resolved_fn("resize"),
                secrets: BTreeMap::new(),
                sources: Sources::default(),
                last_status: cold_status("resize"),
                since: Instant::now(),
            },
        );
        let response = supervisor
            .exec("resize", serde_json::Value::Null, Duration::from_secs(1))
            .expect_err("no image to wake it from");
        assert!(
            matches!(
                response,
                Response::Error {
                    code: ControlError::WarmFailed,
                    ..
                }
            ),
            "{response:?}"
        );
    }

    #[test]
    fn the_tiering_thresholds_are_the_resolved_specs() {
        // The policy has no thresholds of its own; a function with an
        // `idle_timeout` of an hour must not be paused because the supervisor
        // felt like it.
        let f = resolved_fn("resize");
        assert_eq!(f.idle_timeout.get(), Duration::from_secs(600));
        assert_eq!(f.cold_after.get(), Duration::from_secs(3600));
        assert!(
            f.cold_after.get() >= f.idle_timeout.get(),
            "resolution should already have rejected this"
        );
    }

    fn resolved_fn(name: &str) -> ResolvedFn {
        crate::spec::resolve_standalone(
            name,
            &Layer {
                image: Some("python:3.12-slim".into()),
                entry: Some(std::path::PathBuf::from("/tmp/handler.py")),
                ..Default::default()
            },
            &ResolveOptions::default(),
        )
        .expect("resolve")
    }

    fn cold_status(name: &str) -> Status {
        Status {
            name: name.into(),
            state: crate::sandbox::SandboxState::Cold,
            runtime: "python/3.12".into(),
            rss_kb: 0,
            imports_ms: 0.0,
            requests: 0,
            failures: 0,
        }
    }

    // --- rewarm backoff ---------------------------------------------------

    #[test]
    fn the_first_rewarm_after_a_crash_is_immediate() {
        // The design document's acceptance criterion is "back within 500 ms",
        // and a warm-up is ~300 ms. Any delay before the first attempt spends
        // budget that is not there.
        assert_eq!(Backoff::delay(0), Duration::ZERO);
        assert_eq!(Backoff::default().ready(Instant::now()), Ok(()));
    }

    #[test]
    fn repeated_failures_back_off_exponentially_up_to_a_ceiling() {
        assert_eq!(Backoff::delay(1), REWARM_BACKOFF_BASE);
        assert_eq!(Backoff::delay(2), REWARM_BACKOFF_BASE * 2);
        assert_eq!(Backoff::delay(3), REWARM_BACKOFF_BASE * 4);
        assert_eq!(Backoff::delay(10), REWARM_BACKOFF_MAX, "capped");
        // A function that has been failing all day must not overflow its way
        // back to retrying instantly.
        assert_eq!(Backoff::delay(u32::MAX), REWARM_BACKOFF_MAX);
        assert_eq!(Backoff::delay(64), REWARM_BACKOFF_MAX);
    }

    #[test]
    fn a_failing_function_is_made_to_wait_and_told_how_long() {
        let now = Instant::now();
        let backoff = Backoff {
            failures: 3,
            last_attempt: Some(now),
        };
        let left = backoff.ready(now).expect_err("too soon");
        assert!(left <= Backoff::delay(3) && !left.is_zero(), "{left:?}");

        // Once the delay has passed, another attempt is allowed.
        let later = now + Backoff::delay(3);
        assert_eq!(backoff.ready(later), Ok(()));
    }

    #[test]
    fn a_first_attempt_is_never_held_back_however_long_ago_it_was() {
        // `last_attempt: None` is the "never tried" case, which must not be
        // confused with "tried a long time ago".
        let backoff = Backoff {
            failures: 9,
            last_attempt: None,
        };
        assert_eq!(backoff.ready(Instant::now()), Ok(()));
    }

    #[test]
    fn a_deliberate_serve_clears_the_crash_history() {
        // Someone who has just edited their handler and run `zygo serve` should
        // not be made to wait out a backoff earned by the previous version.
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        supervisor.rewarms.lock().expect("rewarms").insert(
            "resize".into(),
            Backoff {
                failures: 5,
                last_attempt: Some(Instant::now()),
            },
        );

        // The serve itself fails (no image in this scratch store), but the
        // history is cleared first, which is the behaviour under test.
        let layer = Layer {
            image: Some("python:3.12-slim".into()),
            ..Default::default()
        };
        let _ = supervisor.serve(
            "resize",
            None,
            &layer,
            &ResolveOptions::default(),
            BTreeMap::new(),
            false,
        );
        assert!(
            !supervisor
                .rewarms
                .lock()
                .expect("rewarms")
                .contains_key("resize"),
            "the backoff survived an explicit serve"
        );
    }

    // --- the launcher thread ---------------------------------------------

    #[test]
    fn every_job_runs_on_one_thread_that_is_not_the_callers() {
        // The property the whole design rests on: `PDEATHSIG` is delivered when
        // the creating *thread* exits, so sandbox creation must never happen on
        // a thread that comes and goes with a CLI command.
        let launcher = Launcher::new().expect("launcher");
        let mut seen = std::collections::HashSet::new();
        let mut callers = std::collections::HashSet::new();

        for _ in 0..8 {
            let where_it_ran = std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        callers.insert(std::thread::current().id());
                        launcher
                            .run(|| std::thread::current().id())
                            .expect("job ran")
                    })
                    .join()
                    .expect("join")
            });
            seen.insert(where_it_ran);
        }

        assert_eq!(seen.len(), 1, "work was spread over {} threads", seen.len());
        let launcher_thread = seen.into_iter().next().expect("one");
        assert!(
            !callers.contains(&launcher_thread),
            "work ran on the calling thread, which is exactly what must not happen"
        );
        assert_ne!(launcher_thread, std::thread::current().id());
    }

    #[test]
    fn the_launcher_thread_outlives_the_caller() {
        let launcher = Launcher::new().expect("launcher");
        // Deliberately a thread that ends straight after asking.
        let first = std::thread::scope(|scope| {
            scope
                .spawn(|| launcher.run(|| std::thread::current().id()).expect("job"))
                .join()
                .expect("join")
        });
        let second = launcher.run(|| std::thread::current().id()).expect("job");
        assert_eq!(
            first, second,
            "the thread that ran the first job must still be there for the second"
        );
    }

    #[test]
    fn a_job_returns_its_value_to_the_caller() {
        let launcher = Launcher::new().expect("launcher");
        assert_eq!(launcher.run(|| 6 * 7).expect("job"), 42);
        assert_eq!(
            launcher
                .run(|| "from the launcher".to_string())
                .expect("job"),
            "from the launcher"
        );
        // Results carry errors through unchanged, which is how a failed warm-up
        // reaches the client.
        let failed: std::result::Result<(), String> = launcher
            .run(|| Err("no such image".to_string()))
            .expect("job");
        assert_eq!(failed, Err("no such image".into()));
    }

    /// The mechanism itself, with no Zygo in the way: does a child outlive the
    /// thread that created it when that thread is the launcher's?
    #[cfg(target_os = "linux")]
    #[test]
    fn a_child_created_through_the_launcher_survives_the_caller() {
        use std::time::Duration;

        /// Fork a child that sets `PDEATHSIG` and then sleeps. Returns its pid.
        fn spawn_child() -> i32 {
            let mut fds = [0i32; 2];
            // SAFETY: `fds` is a live array of two ints, which is what pipe wants.
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
            // SAFETY: fork has no preconditions.
            let pid = unsafe { libc::fork() };
            assert!(pid >= 0, "fork");
            if pid == 0 {
                // SAFETY: the child is single-threaded and does nothing that
                // allocates before it exits.
                unsafe {
                    libc::close(fds[0]);
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    libc::write(fds[1], b"1".as_ptr().cast(), 1);
                    libc::close(fds[1]);
                    libc::sleep(30);
                    libc::_exit(0);
                }
            }
            // SAFETY: the parent owns both descriptors and the buffer is live.
            unsafe {
                libc::close(fds[1]);
                let mut byte = 0u8;
                libc::read(fds[0], (&raw mut byte).cast(), 1);
                libc::close(fds[0]);
            }
            pid
        }

        /// Still running, as opposed to killed and waiting to be reaped.
        fn running(pid: i32) -> bool {
            let mut status = 0;
            // SAFETY: `status` is live; WNOHANG makes this non-blocking.
            let got = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
            got == 0
        }

        let launcher = Launcher::new().expect("launcher");
        let through_launcher = std::thread::scope(|scope| {
            scope
                .spawn(|| launcher.run(spawn_child).expect("job"))
                .join()
                .expect("join")
        });

        // And the same thing done the wrong way, as the control.
        let on_a_short_lived_thread = std::thread::spawn(spawn_child).join().expect("join");

        std::thread::sleep(Duration::from_millis(300));
        assert!(
            running(through_launcher),
            "a sandbox created through the launcher must survive the command that asked for it"
        );
        assert!(
            !running(on_a_short_lived_thread),
            "the control did not reproduce the failure, so this test proves nothing"
        );

        // SAFETY: killing a process this test created.
        unsafe {
            libc::kill(through_launcher, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(through_launcher, &raw mut status, 0);
        }
    }

    #[test]
    fn binding_creates_an_owner_only_socket_and_a_pid_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        let listener = Listener::bind(&paths).expect("bind");

        let socket = paths.supervisor_sock();
        assert!(socket.exists());
        let mode = std::fs::metadata(&socket)
            .expect("stat")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "the control socket is owner-only");

        let runtime = std::fs::metadata(paths.runtime()).expect("stat");
        assert_eq!(
            runtime.permissions().mode() & 0o777,
            0o700,
            "and so is the directory holding it"
        );

        let pid = std::fs::read_to_string(paths.supervisor_pid()).expect("pid file");
        assert_eq!(pid.trim(), std::process::id().to_string());
        drop(listener);
    }

    #[test]
    fn dropping_the_listener_takes_the_socket_with_it() {
        // A leftover socket makes the next supervisor think it has a conflict.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        let listener = Listener::bind(&paths).expect("bind");
        assert!(paths.supervisor_sock().exists());
        drop(listener);
        assert!(!paths.supervisor_sock().exists());
        assert!(!paths.supervisor_pid().exists());
    }

    #[test]
    fn a_second_supervisor_is_refused_while_the_first_is_listening() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        let _first = Listener::bind(&paths).expect("bind");

        let err = Listener::bind(&paths).expect_err("the socket is taken");
        assert!(
            err.to_string().contains("already listening"),
            "unhelpful message: {err}"
        );
    }

    #[test]
    fn a_socket_left_by_a_dead_supervisor_is_reclaimed() {
        // The case that would otherwise need a manual `rm`: the process died
        // without running its `Drop`, so the file is there but nothing answers.
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        paths.ensure().expect("ensure");

        let socket = paths.supervisor_sock();
        let orphan = UnixListener::bind(&socket).expect("bind");
        drop(orphan); // closes the listening fd, leaves the path behind
        assert!(socket.exists(), "the stale socket file is still there");

        let listener = Listener::bind(&paths).expect("a stale socket is not a conflict");
        assert!(UnixStream::connect(&socket).is_ok(), "and this one answers");
        drop(listener);
    }

    #[test]
    fn hello_is_required_before_anything_that_changes_state() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let mut greeted = false;

        for request in [
            Request::List,
            Request::Stop { name: None },
            Request::Exec {
                name: "x".into(),
                event: serde_json::Value::Null,
                timeout_ms: 1,
            },
        ] {
            let response = dispatch(&supervisor, request, &mut greeted);
            assert!(
                matches!(
                    response,
                    Response::Error {
                        code: ControlError::BadMessage,
                        ..
                    }
                ),
                "{response:?}"
            );
        }
        assert!(!greeted);
    }

    #[test]
    fn a_client_speaking_another_control_version_is_turned_away() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let mut greeted = false;

        let response = dispatch(
            &supervisor,
            Request::Hello {
                control: CONTROL_VERSION + 1,
                client: "zygo from the future".into(),
            },
            &mut greeted,
        );
        match response {
            Response::Error {
                code: ControlError::VersionMismatch,
                message,
            } => {
                assert!(message.contains(&CONTROL_VERSION.to_string()), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!greeted, "a rejected client is not greeted");
    }

    #[test]
    fn a_greeting_unlocks_the_connection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let mut greeted = false;

        let response = dispatch(
            &supervisor,
            Request::Hello {
                control: CONTROL_VERSION,
                client: "zygo test".into(),
            },
            &mut greeted,
        );
        assert!(matches!(response, Response::Welcome { .. }));
        assert!(greeted);

        assert!(matches!(
            dispatch(&supervisor, Request::Ping, &mut greeted),
            Response::Pong
        ));
        assert!(matches!(
            dispatch(&supervisor, Request::List, &mut greeted),
            Response::Functions { functions } if functions.is_empty()
        ));
    }

    #[test]
    fn calling_a_function_that_was_never_served_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");

        let response = supervisor
            .exec("nope", serde_json::Value::Null, Duration::from_secs(1))
            .expect_err("no such function");
        match response {
            Response::Error {
                code: ControlError::NotFound,
                message,
            } => assert!(message.contains("zygo serve"), "no next step: {message}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stopping_a_function_that_was_never_served_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        assert!(matches!(
            supervisor.stop(Some("nope")),
            Err(Response::Error {
                code: ControlError::NotFound,
                ..
            })
        ));
        // Stopping everything when there is nothing is not an error: `zygo stop
        // --all` is something you run to be sure, not to be told off.
        assert_eq!(
            supervisor.stop(None),
            Ok(Response::Stopped { names: vec![] })
        );
    }

    #[test]
    fn a_secret_the_spec_names_but_nobody_supplied_is_a_spec_problem() {
        // Reported before any sandbox exists, and naming the variable: the
        // alternative is a handler that fails opening a file that is not there.
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let layer = Layer {
            image: Some("python:3.12-slim".into()),
            entry: Some(std::path::PathBuf::from("/tmp/handler.py")),
            secrets: Some(vec!["STRIPE_KEY".into(), "DB_URL".into()]),
            ..Default::default()
        };
        let response = supervisor
            .serve(
                "pay",
                None,
                &layer,
                &ResolveOptions::default(),
                BTreeMap::from([("STRIPE_KEY".to_string(), "sk".to_string())]),
                false,
            )
            .expect_err("DB_URL has no value");
        match response {
            Response::Error {
                code: ControlError::BadSpec,
                message,
            } => {
                assert!(message.contains("DB_URL"), "{message}");
                assert!(
                    !message.contains("STRIPE_KEY"),
                    "only the missing one: {message}"
                );
                assert!(
                    !message.contains("sk"),
                    "a value must never be echoed: {message}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    // --- blue/green ------------------------------------------------------

    fn handler_in(dir: &tempfile::TempDir, body: &str) -> (Layer, ResolvedFn) {
        let path = dir.path().join("handler.py");
        std::fs::write(&path, body).expect("write handler");
        let layer = Layer {
            image: Some("python:3.12-slim".into()),
            entry: Some(path),
            ..Default::default()
        };
        let resolved = crate::spec::resolve_standalone("f", &layer, &ResolveOptions::default())
            .expect("resolve");
        (layer, resolved)
    }

    #[test]
    fn sources_follow_the_bytes_on_disk_not_the_path() {
        // The spec cannot see an edit to the handler; this is what can.
        let dir = tempfile::tempdir().expect("tempdir");
        let (_, before) = handler_in(&dir, "def handler(e): return 1\n");
        let at_start = Sources::of(&before);
        assert_eq!(at_start, Sources::of(&before), "hashing is deterministic");

        std::fs::write(dir.path().join("handler.py"), "def handler(e): return 2\n").expect("edit");
        assert_ne!(
            at_start,
            Sources::of(&before),
            "an edited handler is a change"
        );

        std::fs::remove_file(dir.path().join("handler.py")).expect("remove");
        let gone = Sources::of(&before);
        assert_ne!(at_start, gone, "a deleted handler is a change");
        assert_eq!(
            gone.0[0].1, None,
            "and is recorded as unreadable, not as empty"
        );
    }

    #[test]
    fn a_cold_function_is_unchanged_only_while_its_inputs_are() {
        // Registration is compared, not just the name: `up` after editing the
        // handler, changing a secret, or changing the spec must replace, and
        // `up` after none of those must not.
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let (_, resolved) = handler_in(&dir, "def handler(e): return 1\n");
        let secrets = BTreeMap::from([("KEY".to_string(), "v1".to_string())]);
        supervisor.cold.lock().expect("cold").insert(
            "f".into(),
            Cold {
                resolved: resolved.clone(),
                secrets: secrets.clone(),
                sources: Sources::of(&resolved),
                last_status: cold_status("f"),
                since: Instant::now(),
            },
        );

        assert!(supervisor.is_registered_as("f", &resolved, &secrets));

        let other_secret = BTreeMap::from([("KEY".to_string(), "v2".to_string())]);
        assert!(!supervisor.is_registered_as("f", &resolved, &other_secret));

        let mut other_spec = resolved.clone();
        other_spec.concurrency += 1;
        assert!(!supervisor.is_registered_as("f", &other_spec, &secrets));

        assert!(
            !supervisor.is_registered_as("g", &resolved, &secrets),
            "a different name"
        );

        std::fs::write(dir.path().join("handler.py"), "def handler(e): return 2\n").expect("edit");
        assert!(
            !supervisor.is_registered_as("f", &resolved, &secrets),
            "the handler changed on disk"
        );
    }

    #[test]
    fn a_serve_that_would_change_nothing_does_not_touch_the_registry() {
        // No image in this store, so an actual warm-up would fail loudly; a
        // request for the identical function does not get that far. It does,
        // however, try to *wake* the function, which is a warm-up — so the
        // failure it reports is the wake-up's, not a "bad spec".
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let (layer, resolved) = handler_in(&dir, "def handler(e): return 1\n");
        supervisor.cold.lock().expect("cold").insert(
            "f".into(),
            Cold {
                resolved: resolved.clone(),
                secrets: BTreeMap::new(),
                sources: Sources::of(&resolved),
                last_status: cold_status("f"),
                since: Instant::now(),
            },
        );

        let response = supervisor
            .serve(
                "f",
                None,
                &layer,
                &ResolveOptions::default(),
                BTreeMap::new(),
                true,
            )
            .expect_err("waking needs an image this store does not have");
        assert!(
            matches!(
                response,
                Response::Error {
                    code: ControlError::WarmFailed,
                    ..
                }
            ),
            "{response:?}"
        );
        assert!(
            supervisor.cold.lock().expect("cold").contains_key("f"),
            "a failed wake-up leaves the registration in place"
        );
    }

    #[test]
    fn a_bad_spec_is_reported_as_a_spec_problem_not_a_warm_failure() {
        // The distinction matters to the person reading: one is their file,
        // the other is the machine.
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");

        let layer = Layer {
            image: Some("python:3.12-slim".into()),
            // `network = "host"` without `--allow-host-net` is refused at
            // resolve time.
            network: Some(crate::spec::Network::Host),
            ..Default::default()
        };
        let response = supervisor
            .serve(
                "x",
                None,
                &layer,
                &ResolveOptions::default(),
                BTreeMap::new(),
                false,
            )
            .expect_err("resolution should fail");
        assert!(matches!(
            response,
            Response::Error {
                code: ControlError::BadSpec,
                ..
            }
        ));
    }

    #[test]
    fn shutdown_is_acknowledged_before_the_loop_stops() {
        // The client needs its answer: a supervisor that closed the socket
        // first would look like a crash.
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let mut greeted = true;
        assert_eq!(
            dispatch(&supervisor, Request::Shutdown, &mut greeted),
            Response::Ok
        );
        assert!(!supervisor.is_stopping(), "`dispatch` only answers");
        supervisor.shutdown();
        assert!(supervisor.is_stopping());
    }

    #[test]
    fn the_peer_check_accepts_our_own_connection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(&dir);
        let listener = Listener::bind(&paths).expect("bind");
        let client = UnixStream::connect(paths.supervisor_sock()).expect("connect");
        let (server, _) = listener.listener.accept().expect("accept");

        assert_eq!(peer_uid(&server), Some(current_uid()));
        assert!(
            reject_foreign_peer(&server).is_none(),
            "our own uid must not be refused"
        );
        drop(client);
    }
}
