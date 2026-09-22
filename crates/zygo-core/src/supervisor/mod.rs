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
pub mod runtime;

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
pub use runtime::RuntimeStatus;

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
                    // A job that panics must not take the thread with it.
                    //
                    // There is one launcher thread for the supervisor's whole
                    // life, and every warm-up and every `ns` request runs on
                    // it. A panic in one of them used to end the loop, after
                    // which every later job sat in the channel unanswered and
                    // the supervisor looked hung rather than broken (E-01).
                    //
                    // The panic is still a bug and still prints; `run`'s
                    // caller sees the dropped result channel and reports it.
                    // What changes is that the *next* job gets to run.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
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
        reason: "the launcher never answered: the job panicked, or the thread is gone".into(),
        remedy: "the supervisor's log has the panic; a single panicking job no longer \
                 stops the ones after it"
            .into(),
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
    /// Runtime pools: several anonymous zygotes each, with the script
    /// arriving in the request. See [`runtime`].
    runtimes: Mutex<BTreeMap<String, Arc<runtime::RuntimePool>>>,
    /// Functions tiered down to cold: registered, but with no sandbox.
    cold: Mutex<BTreeMap<String, Cold>>,
    /// Rewarm history per name. See [`Backoff`] for why it is not in `Entry`.
    rewarms: Mutex<BTreeMap<String, Backoff>>,
    /// One lock per function name, held for the length of a rewarm.
    ///
    /// Without it, N requests that all find the same crashed function all
    /// rewarm it: N sandboxes built, N of them registered in turn, and N−1
    /// thrown away — on a function that crashes under load, which is when the
    /// host can least afford it (B-10). The first request through builds the
    /// replacement; the rest wait on this and then find it already in the
    /// registry.
    ///
    /// A lock per name rather than one lock: two different functions crashing
    /// at once are unrelated, and serialising them would make a slow warm-up
    /// of one into a stall of the other.
    warming: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
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
            runtimes: Mutex::new(BTreeMap::new()),
            cold: Mutex::new(BTreeMap::new()),
            rewarms: Mutex::new(BTreeMap::new()),
            warming: Mutex::new(BTreeMap::new()),
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
            gate: Gate::named(
                name,
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
        tracing::debug!(
            function = name,
            replaced = previous.is_some(),
            "registered; retiring the previous entry next"
        );
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
        // One rewarm at a time per function (B-10). Everything below — the
        // backoff, the warm-up, the registration — runs under this, so the
        // second request to arrive waits here rather than building a second
        // sandbox for the same crash.
        let gate = {
            let mut warming = self.warming.lock().expect("warming");
            Arc::clone(
                warming
                    .entry(name.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        let _warming = gate.lock().unwrap_or_else(|e| e.into_inner());

        // Now that we hold it, somebody else may have done the work. A
        // registry entry that is no longer the broken one, and is healthy, is
        // that somebody's replacement — take it and charge this request
        // nothing. Checked *after* the lock, because before it the answer is
        // a guess.
        if let Ok(current) = self.lookup(name)
            && !Arc::ptr_eq(&current, broken)
            && current.function.is_healthy()
        {
            return Ok(current);
        }

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
        self.tier_idle_runtimes(&mut tiered);
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
    /// Run a one-shot sandbox on a client's behalf, with the client's streams.
    ///
    /// The start goes to the launcher thread like every other start (see
    /// [`Launcher`]); the *wait* stays here, on this connection's own thread,
    /// because a launcher that waited would hold every other start on the
    /// machine for as long as this program ran.
    ///
    /// `started` is called with the pid and the cgroup as soon as the sandbox
    /// exists and before it is waited on, so the client can forward its
    /// terminal's signals to it — a Ctrl-C at the client reaches the client,
    /// and the sandbox is this process's child, not the client's — and so the
    /// connection can kill it if the client goes away (see [`ClientWatch`]).
    pub fn run(
        &self,
        spec: Option<&Spec>,
        layer: &Layer,
        options: &ResolveOptions,
        client: crate::pool::ClientStreams,
        mut started: impl FnMut(u32, Option<&std::path::Path>) -> Result<()>,
    ) -> std::result::Result<Response, Response> {
        let owned;
        let spec = match spec {
            Some(s) => s,
            None => {
                owned = Spec::default();
                &owned
            }
        };
        // `resolve`, not `resolve_for_serve`: this is `zygo run`'s own
        // resolution, one-shot rules and all, so a run through the
        // supervisor is the same run it would have been without one.
        let resolved = spec
            .resolve(None, layer, options)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;

        // On the launcher thread, never here: see [`Launcher`].
        let pool = Arc::clone(&self.pool);
        let oneshot = self
            .launcher
            .run(move || pool.start_oneshot(&resolved, client))
            .map_err(|e| Response::error(ControlError::WarmFailed, e))?
            .map_err(|e| Response::error(ControlError::WarmFailed, e))?;
        let crate::pool::Oneshot {
            mut sandbox,
            newroot,
        } = oneshot;

        started(sandbox.pid(), sandbox.cgroup())
            .map_err(|e| Response::error(ControlError::CallFailed, e))?;

        let clock = Instant::now();
        let waited = sandbox.wait();
        // Sampled during teardown, the last moment the cgroup exists.
        let kernel = sandbox.outcome();
        // Its `pivot_root` target: empty once the sandbox is gone, and left
        // in place if it is not, for the reason `zygo run` gives.
        let _ = std::fs::remove_dir(&newroot);

        Ok(Response::Ran {
            exit_code: match &waited {
                Ok(code) => (*code).clamp(0, 255),
                Err(e) => e.exit_code(),
            },
            timed_out: waited.as_ref().err().is_some_and(Error::timed_out),
            oom_killed: kernel.oom_kills > 0,
            peak_rss_kb: kernel.peak_rss_kb,
            wall_ms: clock.elapsed().as_secs_f64() * 1000.0,
        })
    }

    pub fn exec(
        &self,
        name: &str,
        event: serde_json::Value,
        timeout: Duration,
        key: Option<&str>,
    ) -> std::result::Result<Response, Response> {
        self.exec_streaming(name, event, timeout, key, None)
    }

    /// The same, with output delivered as it is produced (v8).
    ///
    /// `sink` is called on this thread, between the `EXEC` and the `DONE`, so
    /// whatever it writes to must not block for long: the request that is
    /// producing the output is what waits.
    pub fn exec_streaming(
        &self,
        name: &str,
        mut event: serde_json::Value,
        timeout: Duration,
        key: Option<&str>,
        sink: Option<crate::pool::ChunkSink<'_>>,
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

            event = match self.attempt(&entry, name, event, timeout, key, sink) {
                Attempt::Done(response) => return response,
                Attempt::Closed(event) => event,
            };
            match self.lookup(name) {
                Ok(current) if !Arc::ptr_eq(&current, &entry) => {
                    tracing::debug!(
                        function = name,
                        "gate closed under a request; redirecting to the replacement"
                    );
                    entry = current;
                }
                _ => {
                    tracing::debug!(
                        function = name,
                        "gate closed under a request and nothing replaced it"
                    );
                    break;
                }
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
        key: Option<&str>,
        sink: Option<crate::pool::ChunkSink<'_>>,
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

        // Thawing is one write, so a paused function answers at warm speed
        // plus that. This is the whole reason pausing is a tier of its own
        // rather than going straight to cold.
        //
        // **After** the permit, and that ordering is the fix for B-11. It used
        // to happen before, in the caller, where `in_flight` was still zero —
        // so `tier_idle` could read the gate, see nothing in flight, and
        // freeze the function in the window between the thaw and the permit.
        // The request then ran against a frozen sandbox. Holding the permit
        // first makes the function visibly busy to `tier_idle`, which skips
        // anything with a request in flight.
        if let Err(e) = entry.function.resume() {
            drop(permit);
            return Attempt::Done(Err(Response::error(ControlError::CallFailed, e)));
        }

        let outcome = entry
            .function
            .call_keyed(event, timeout, key, sink)
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

    /// Register a script and answer with the name the store gave it.
    ///
    /// The supervisor owns the store for the same reason it owns the
    /// sandboxes: it is the process that will hand a script to a child, and a
    /// second writer would be a second opinion about what a digest means.
    /// Content-addressed, so this is idempotent — the same bytes from two
    /// tenants are one file, and `existed` is how the caller finds that out.
    ///
    /// `tenant` records *whose* script it is. The file is shared; the
    /// reference is not, and it is what lets deleting a tenant take its code
    /// and leave everybody else's.
    pub fn put_script(
        &self,
        source: &str,
        tenant: Option<&str>,
    ) -> std::result::Result<Response, Response> {
        let store = crate::scripts::ScriptStore::new(&self.paths);
        let digest = crate::scripts::ScriptDigest::of(source);
        let existed = store.contains(&digest);
        let digest = store
            .put(source)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        if let Some(tenant) = tenant {
            crate::tenants::Tenants::new(&self.paths)
                .add_script(tenant, digest.as_str())
                .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        }
        Ok(Response::Script {
            digest: digest.to_string(),
            size: source.len() as u64,
            existed,
        })
    }

    /// Register a tenant, or find the one already registered.
    pub fn create_tenant(&self, id: &str) -> std::result::Result<Response, Response> {
        let (tenant, existed) = crate::tenants::Tenants::new(&self.paths)
            .create(id)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        Ok(Response::Tenants {
            tenants: vec![tenant],
            existed,
            removed_scripts: Vec::new(),
            stopped: Vec::new(),
        })
    }

    /// Every tenant, or one of them.
    pub fn tenants(&self, id: Option<&str>) -> std::result::Result<Response, Response> {
        let store = crate::tenants::Tenants::new(&self.paths);
        let tenants = match id {
            Some(id) => match store
                .get(id)
                .map_err(|e| Response::error(ControlError::BadSpec, e))?
            {
                Some(tenant) => vec![tenant],
                None => {
                    return Err(Response::error(
                        ControlError::NotFound,
                        format!("no tenant `{id}`"),
                    ));
                }
            },
            None => store
                .list()
                .map_err(|e| Response::error(ControlError::CallFailed, e))?,
        };
        Ok(Response::Tenants {
            tenants,
            existed: false,
            removed_scripts: Vec::new(),
            stopped: Vec::new(),
        })
    }

    /// Forget a tenant: stop what it was running, then take the scripts only
    /// it referred to.
    ///
    /// In that order, and the order matters. A script removed while a request
    /// is still loading it would fail that request for a reason the caller
    /// cannot see; stopping first means there is nothing left to be reading.
    pub fn delete_tenant(&self, id: &str) -> std::result::Result<Response, Response> {
        let store = crate::tenants::Tenants::new(&self.paths);
        let Some(tenant) = store
            .get(id)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?
        else {
            return Err(Response::error(
                ControlError::NotFound,
                format!("no tenant `{id}`"),
            ));
        };

        let stopped = self.stop_everything_for(id);
        // Before the record goes, not after: a token whose tenant no longer
        // exists would resolve to a customer nobody can see, and "the tenant
        // is gone but their key still opens the door" is the failure this
        // whole layer exists to prevent.
        crate::tokens::Tokens::new(&self.paths)
            .revoke_tenants(id)
            .map_err(|e| Response::error(ControlError::CallFailed, e))?;
        let removed_scripts = store
            .remove(id)
            .map_err(|e| Response::error(ControlError::CallFailed, e))?
            .unwrap_or_default();
        let script_store = crate::scripts::ScriptStore::new(&self.paths);
        for digest in &removed_scripts {
            if let Ok(digest) = crate::scripts::ScriptDigest::parse(digest) {
                let _ = script_store.remove(&digest);
            }
        }
        // The tenant's cgroup, now that nothing of its is running. Left behind
        // it would be one empty directory per deleted customer, for ever.
        if let Ok(hierarchy) = crate::cgroup::Hierarchy::discover() {
            let _ = crate::cgroup::Hierarchy::remove(&hierarchy.tenant(id));
        }

        Ok(Response::Tenants {
            tenants: vec![tenant],
            existed: false,
            removed_scripts,
            stopped,
        })
    }

    /// Stop a request that is running.
    ///
    /// Ids are unique across the host, so this searches rather than indexing:
    /// every function and every pool zygote is asked whether the id is theirs.
    /// A second map from id to function would be faster and would be a thing
    /// that could disagree with the lists it summarises — at the counts a
    /// supervisor holds, a walk of a few dozen registry entries is not on any
    /// path worth optimising.
    ///
    /// `caller` is the tenant asking, and a request belonging to somebody else
    /// is `not_found`: the same answer an id that finished a second ago gets,
    /// for the same reason a digest that is not yours is.
    pub fn cancel(
        &self,
        id: &str,
        caller: Option<&str>,
    ) -> std::result::Result<Response, Response> {
        let missing = || {
            Response::error(
                ControlError::NotFound,
                format!("no request `{id}` is running"),
            )
        };

        let functions: Vec<(String, Arc<Entry>)> = self
            .functions
            .lock()
            .expect("registry")
            .iter()
            .map(|(name, entry)| (name.clone(), Arc::clone(entry)))
            .collect();
        for (name, entry) in functions {
            if let Some(caller) = caller
                && entry.resolved.tenant != caller
            {
                continue;
            }
            // `None`, not `caller`: a warm function belongs to one tenant and
            // the loop above already skipped the ones that are not theirs, so
            // a per-request owner would be the same fact twice.
            if let Some(started) = entry.function.cancel(id, None) {
                tracing::info!(function = %name, request = %id, started, "cancelled");
                return Ok(Response::Cancelled {
                    id: id.to_string(),
                    started,
                });
            }
        }

        match self.cancel_in_pools(id, caller) {
            Some(started) => Ok(Response::Cancelled {
                id: id.to_string(),
                started,
            }),
            None => Err(missing()),
        }
    }

    /// Mint an API token, and answer with the secret exactly once.
    ///
    /// Minting for a tenant registers that tenant if it is new, like
    /// `PutScript` does: onboarding a customer should be one call, not two in
    /// an order the embedder has to remember.
    pub fn mint_token(&self, tenant: Option<&str>) -> std::result::Result<Response, Response> {
        let kind = match tenant {
            Some(id) => {
                crate::tenants::Tenants::new(&self.paths)
                    .create(id)
                    .map_err(|e| Response::error(ControlError::BadSpec, e))?;
                crate::tokens::TokenKind::Tenant {
                    tenant: id.to_string(),
                }
            }
            None => crate::tokens::TokenKind::Operator,
        };
        let minted = crate::tokens::Tokens::new(&self.paths)
            .mint(kind)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        Ok(Response::Tokens {
            tokens: vec![minted.token],
            secret: Some(minted.secret),
        })
    }

    /// Every token, hashes and all. The secret is not in the store to return.
    pub fn list_tokens(&self) -> std::result::Result<Response, Response> {
        let tokens = crate::tokens::Tokens::new(&self.paths)
            .list()
            .map_err(|e| Response::error(ControlError::CallFailed, e))?;
        Ok(Response::Tokens {
            tokens,
            secret: None,
        })
    }

    /// Revoke one. The record stays, marked, so a log line naming it still
    /// resolves to something.
    pub fn revoke_token(&self, id: &str) -> std::result::Result<Response, Response> {
        crate::tokens::valid_token_id(id).map_err(|e| Response::error(ControlError::BadSpec, e))?;
        let store = crate::tokens::Tokens::new(&self.paths);
        match store.revoke(id) {
            Ok(true) => self.list_tokens(),
            Ok(false) => Err(Response::error(
                ControlError::NotFound,
                format!("no token `{id}`"),
            )),
            Err(e) => Err(Response::error(ControlError::CallFailed, e)),
        }
    }

    /// Stop every function and pool that belongs to a tenant. Returns their
    /// names.
    fn stop_everything_for(&self, tenant: &str) -> Vec<String> {
        let functions: Vec<String> = self
            .functions
            .lock()
            .expect("registry")
            .iter()
            .filter(|(_, entry)| entry.resolved.tenant == tenant)
            .map(|(name, _)| name.clone())
            .collect();
        let runtimes: Vec<String> = self
            .runtimes
            .lock()
            .expect("runtimes")
            .iter()
            .filter(|(_, pool)| pool.resolved.tenant == tenant)
            .map(|(name, _)| name.clone())
            .collect();

        let mut stopped = Vec::new();
        for name in functions {
            if self.stop(Some(&name)).is_ok() {
                stopped.push(name);
            }
        }
        for name in runtimes {
            if self.stop_runtime(&name).is_ok() {
                stopped.push(format!("runtime.{name}"));
            }
        }
        stopped
    }

    /// Whether this host holds a script, and how big it is.
    pub fn get_script(&self, digest: &str) -> std::result::Result<Response, Response> {
        let store = crate::scripts::ScriptStore::new(&self.paths);
        let digest = crate::scripts::ScriptDigest::parse(digest)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        let size = std::fs::metadata(store.path(&digest))
            .map(|m| m.len())
            .map_err(|_| Response::error(ControlError::NotFound, format!("no script {digest}")))?;
        Ok(Response::Script {
            digest: digest.to_string(),
            size,
            existed: true,
        })
    }

    /// Forget a script.
    ///
    /// Nothing checks whether anything still refers to it, because until
    /// tenants exist (Phase 2) nothing *can* refer to it durably: a request
    /// already in flight has the bytes, and a caller that removes a script it
    /// is about to run has made that call fail on purpose.
    pub fn delete_script(&self, digest: &str) -> std::result::Result<Response, Response> {
        let store = crate::scripts::ScriptStore::new(&self.paths);
        let digest = crate::scripts::ScriptDigest::parse(digest)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        match store.remove(&digest) {
            Ok(true) => Ok(Response::Ok),
            Ok(false) => Err(Response::error(
                ControlError::NotFound,
                format!("no script {digest}"),
            )),
            Err(e) => Err(Response::error(ControlError::CallFailed, e)),
        }
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
    /// Refuse a function that belongs to a different customer.
    ///
    /// `caller` is `None` for the operator, who may reach anything on their
    /// own host. A tenant gets exactly one answer for "no such function" and
    /// for "that one is somebody else's", because the difference between them
    /// is a fact about another customer — the same rule `script_for_request`
    /// follows for digests, and for the same reason.
    ///
    /// Checked here rather than in the API: the supervisor is the authority on
    /// what a function is and who it belongs to, and a boundary enforced only
    /// in the process that happens to be in front of it is a boundary that
    /// moves the day something else talks to this socket.
    fn owned_by(&self, name: &str, caller: Option<&str>) -> std::result::Result<(), Response> {
        let Some(caller) = caller else {
            return Ok(());
        };
        let owner = self
            .functions
            .lock()
            .expect("registry")
            .get(name)
            .map(|e| e.resolved.tenant.clone())
            .or_else(|| {
                self.cold
                    .lock()
                    .expect("cold")
                    .get(name)
                    .map(|c| c.resolved.tenant.clone())
            });
        match owner {
            Some(owner) if owner == caller => Ok(()),
            _ => Err(Response::error(
                ControlError::NotFound,
                format!("no function named `{name}`; `zygo serve` it first"),
            )),
        }
    }

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
        tracing::debug!(function = %entry.resolved.name, "retire: closing the gate");
        entry.gate.close();
        let _ = entry.function.shutdown();
        tracing::debug!(function = %entry.resolved.name, "retire: shut down");
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
                        // Both directions in one pass, on one thread: a pool
                        // that is shrinking must not be growing at the same
                        // time, and the cheapest way to guarantee that is for
                        // the same loop to do both.
                        for name in supervisor.scale_runtimes() {
                            tracing::info!(runtime = %name, "load: grew by one zygote");
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

    let dup = |s: &UnixStream| {
        s.try_clone()
            .map_err(|e| Error::primitive("dup", "control socket", e))
    };
    // The socket itself, for the one request whose payload is not all in the
    // frame: `RUN` is followed by three descriptors over `SCM_RIGHTS`, and a
    // `FrameReader` — which reads exactly a header and exactly a body, never
    // ahead — is positioned on the first of them when the frame is done.
    let raw = dup(&stream)?;
    let mut reader: crate::protocol::frame::FrameReader<_, Request> =
        crate::protocol::frame::FrameReader::new(dup(&stream)?);
    let mut writer: crate::protocol::frame::FrameWriter<_, Response> =
        crate::protocol::frame::FrameWriter::new(stream);

    let mut greeted = false;
    while let Some(request) = reader
        .read()
        .map_err(|e| Error::primitive("read", "control socket", std::io::Error::other(e)))?
    {
        let shutting_down = matches!(request, Request::Shutdown);
        let response = match request {
            Request::Run {
                spec,
                layer,
                base_dir,
                allow_host_net,
                allow_private_net,
                allow_unlimited,
                tty,
                ignored_signals,
            } if greeted => {
                let options = ResolveOptions {
                    allow_host_net,
                    allow_private_net,
                    allow_unlimited,
                    base_dir: Some(base_dir),
                    one_shot: true,
                    pool: false,
                    tenant: None,
                };
                // Received before anything else, and before deciding
                // anything: the client has already sent them, and leaving
                // them on the socket would put the next frame out of
                // alignment.
                let stdio = receive_stdio(&raw);
                match stdio {
                    Ok(stdio) => {
                        let watch = ClientWatch::start(&raw);
                        let client = crate::pool::ClientStreams {
                            stdio,
                            tty,
                            ignored_signals,
                        };
                        let ran = supervisor.run(
                            spec.as_deref(),
                            &layer,
                            &options,
                            client,
                            |pid, cgroup| {
                                watch.started(pid, cgroup);
                                writer.write(&Response::Started { pid }).map_err(|e| {
                                    Error::primitive(
                                        "write",
                                        "control socket",
                                        std::io::Error::other(e),
                                    )
                                })
                            },
                        );
                        watch.finished();
                        merge(ran)
                    }
                    Err(e) => Response::error(
                        ControlError::BadMessage,
                        format!("RUN's descriptors did not arrive: {e}"),
                    ),
                }
            }
            // A streaming request is answered many times: a `CHUNK` per piece
            // of output, then the `EXECUTED`. Intercepted here rather than in
            // `dispatch` for the same reason `RUN` is — this is the scope
            // that has the socket, and `dispatch` returns one response by
            // construction.
            Request::Exec {
                name,
                event,
                timeout_ms,
                tenant,
                key,
                stream: true,
            } if greeted => {
                // Borrowed for the length of the call and released before the
                // final answer is written. One thread, so uncontended: the
                // lock is what lets a `Fn` closure reach a `&mut` writer, not
                // a synchronisation point.
                let out = std::sync::Mutex::new(&mut writer);
                let sink = chunk_sink(&out);
                let answered = supervisor
                    .owned_by(&name, tenant.as_deref())
                    .and_then(|()| {
                        supervisor.exec_streaming(
                            &name,
                            event,
                            Duration::from_millis(timeout_ms),
                            key.as_deref(),
                            Some(&sink),
                        )
                    });
                merge(answered)
            }
            Request::ExecScript {
                runtime,
                script,
                event,
                timeout_ms,
                tenant,
                key,
                stream: true,
            } if greeted => {
                let out = std::sync::Mutex::new(&mut writer);
                let sink = chunk_sink(&out);
                merge(supervisor.exec_script_streaming(
                    &runtime,
                    script,
                    event,
                    Duration::from_millis(timeout_ms),
                    tenant.as_deref(),
                    key.as_deref(),
                    Some(&sink),
                ))
            }
            other => dispatch(supervisor, other, &mut greeted),
        };
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

/// A sink that writes each chunk straight out as a `CHUNK` frame.
///
/// Write errors are dropped on purpose. The client going away mid-stream is
/// ordinary — somebody closed a terminal — and it is not a reason to fail the
/// request, which is still running and whose `EXECUTED` will fail to write for
/// the same reason a moment later. That is where the connection ends.
fn chunk_sink<'a, W: std::io::Write + Send>(
    out: &'a std::sync::Mutex<&'a mut crate::protocol::frame::FrameWriter<W, Response>>,
) -> impl Fn(crate::protocol::Stream, &str) + Send + Sync + use<'a, W> {
    move |stream, data| {
        let _ = out.lock().expect("writer").write(&Response::Chunk {
            stream,
            data: data.to_string(),
        });
    }
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
            tenant,
        } => {
            let options = ResolveOptions {
                allow_host_net,
                allow_private_net,
                allow_unlimited,
                base_dir: Some(base_dir),
                one_shot: false,
                pool: false,
                tenant,
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
        // A streaming `EXEC` is intercepted in `handle`, which has the socket
        // the chunks go out on. One that reaches here asked for none.
        Request::Exec {
            name,
            event,
            timeout_ms,
            tenant,
            key,
            stream: _,
        } => merge(
            supervisor
                .owned_by(&name, tenant.as_deref())
                .and_then(|()| {
                    supervisor.exec(
                        &name,
                        event,
                        Duration::from_millis(timeout_ms),
                        key.as_deref(),
                    )
                }),
        ),
        Request::Stop { name } => merge(supervisor.stop(name.as_deref())),
        Request::Warm { name, tenant } => merge(
            supervisor
                .owned_by(&name, tenant.as_deref())
                .and_then(|()| supervisor.warm(&name)),
        ),
        Request::Shell { name } => merge(supervisor.shell(&name)),
        Request::Logs {
            name,
            after,
            limit,
            failed,
            tenant,
        } => merge(
            supervisor
                .owned_by(&name, tenant.as_deref())
                .and_then(|()| supervisor.logs(&name, after, limit, failed)),
        ),
        Request::ServeRuntime {
            name,
            spec,
            layer,
            base_dir,
            allow_host_net,
            allow_private_net,
            allow_unlimited,
            tenant,
        } => {
            let options = ResolveOptions {
                allow_host_net,
                allow_private_net,
                allow_unlimited,
                base_dir: Some(base_dir),
                one_shot: false,
                pool: true,
                tenant,
            };
            merge(supervisor.serve_runtime(&name, spec.as_deref(), &layer, &options))
        }
        Request::ExecScript {
            runtime,
            script,
            event,
            timeout_ms,
            tenant,
            key,
            stream: _,
        } => merge(supervisor.exec_script(
            &runtime,
            script,
            event,
            Duration::from_millis(timeout_ms),
            tenant.as_deref(),
            key.as_deref(),
        )),
        Request::Runtimes => Response::Runtimes {
            runtimes: supervisor.runtimes(),
        },
        Request::StopRuntime { name } => merge(supervisor.stop_runtime(&name)),
        Request::PutScript { source, tenant } => {
            merge(supervisor.put_script(&source, tenant.as_deref()))
        }
        Request::CreateTenant { id } => merge(supervisor.create_tenant(&id)),
        Request::Tenants { id } => merge(supervisor.tenants(id.as_deref())),
        Request::DeleteTenant { id } => merge(supervisor.delete_tenant(&id)),
        Request::MintToken { tenant } => merge(supervisor.mint_token(tenant.as_deref())),
        Request::Tokens => merge(supervisor.list_tokens()),
        Request::RevokeToken { id } => merge(supervisor.revoke_token(&id)),
        Request::Cancel { id, tenant } => merge(supervisor.cancel(&id, tenant.as_deref())),
        Request::GetScript { digest } => merge(supervisor.get_script(&digest)),
        Request::DeleteScript { digest } => merge(supervisor.delete_script(&digest)),
        Request::Shutdown => Response::Ok,
        // Intercepted in `handle`, which has the socket the descriptors
        // arrive on; a `RUN` that reaches this table was sent to a code path
        // that cannot receive them.
        Request::Run { .. } => Response::error(
            ControlError::BadMessage,
            "RUN carries descriptors and is answered before dispatch",
        ),
    }
}

/// What happens to a `RUN` sandbox when its client goes away.
///
/// A sandbox `zygo run` starts itself has `PR_SET_PDEATHSIG`: when that
/// `zygo` dies, the kernel kills the sandbox. A sandbox started here is this
/// process's child, and the client's death is only a hang-up on its socket —
/// so a thread watches the socket and delivers the kill the kernel would
/// have. Without it a client killed with `SIGKILL`, or a terminal that
/// vanished without a `SIGHUP`, left the program running to its deadline,
/// or for ever under `--allow-unlimited`, writing into a pipe nobody read.
///
/// `poll` for `POLLRDHUP` only, so data on the socket does not wake it, and
/// with a short timeout so the thread notices [`ClientWatch::finished`] and
/// ends soon after the run does rather than living as long as the
/// connection.
struct ClientWatch {
    done: Arc<AtomicBool>,
    target: Arc<Mutex<Option<WatchTarget>>>,
}

/// The sandbox a hang-up kills: its init, and its cgroup when it has one.
#[derive(Clone)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct WatchTarget {
    pid: u32,
    cgroup: Option<PathBuf>,
}

impl ClientWatch {
    fn start(client: &UnixStream) -> ClientWatch {
        let watch = ClientWatch {
            done: Arc::new(AtomicBool::new(false)),
            target: Arc::new(Mutex::new(None)),
        };
        // Its own descriptor, so a number the connection closes and the
        // kernel hands to somebody else is never polled here.
        if let Ok(own) = client.try_clone() {
            let done = Arc::clone(&watch.done);
            let target = Arc::clone(&watch.target);
            std::thread::Builder::new()
                .name("client-watch".into())
                .spawn(move || ClientWatch::watch(own, &done, &target))
                .ok();
        }
        watch
    }

    /// The sandbox exists: this is what a hang-up kills from now on.
    fn started(&self, pid: u32, cgroup: Option<&std::path::Path>) {
        if let Ok(mut target) = self.target.lock() {
            *target = Some(WatchTarget {
                pid,
                cgroup: cgroup.map(PathBuf::from),
            });
        }
    }

    /// The sandbox has been reaped: nothing left to kill, and its pid may be
    /// somebody else's soon.
    fn finished(&self) {
        self.done.store(true, Ordering::SeqCst);
    }

    #[cfg(target_os = "linux")]
    fn watch(own: UnixStream, done: &AtomicBool, target: &Mutex<Option<WatchTarget>>) {
        use std::os::fd::AsRawFd;
        /// `POLLRDHUP`: the peer shut its side down. Kernel ABI since 2.6.17.
        const POLLRDHUP: libc::c_short = 0x2000;
        let mut pfd = libc::pollfd {
            fd: own.as_raw_fd(),
            events: POLLRDHUP,
            revents: 0,
        };
        while !done.load(Ordering::SeqCst) {
            pfd.revents = 0;
            // SAFETY: `pfd` is a live local naming a descriptor this thread
            // owns; a 250 ms timeout bounds the call.
            let rc = unsafe { libc::poll(&mut pfd, 1, 250) };
            let hung_up = rc > 0
                && pfd.revents & (POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
            if !hung_up {
                continue;
            }
            // The start may still be queued on the launcher: wait for a pid
            // rather than letting a sandbox that starts a moment later run
            // for nobody.
            while !done.load(Ordering::SeqCst) {
                let target = target.lock().ok().and_then(|t| t.clone());
                let Some(WatchTarget { pid, cgroup }) = target else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                tracing::info!(pid, "RUN client went away; killing its sandbox");
                // `cgroup.kill` takes the subtree in one write and cannot
                // reach a reused pid; the pid is for a kernel without it.
                let killed = cgroup
                    .as_deref()
                    .is_some_and(|dir| matches!(crate::cgroup::kill(dir), Ok(true)));
                if !killed && !done.load(Ordering::SeqCst) {
                    // SAFETY: `kill` on a pid this process has not reaped.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                }
                return;
            }
            return;
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn watch(_own: UnixStream, _done: &AtomicBool, _target: &Mutex<Option<WatchTarget>>) {}
}

/// The three descriptors a `RUN` sends after its frame: stdin, stdout, stderr.
///
/// Each arrives as one `SCM_RIGHTS` message with one byte of payload, and
/// `MSG_CMSG_CLOEXEC` is set at receipt, so none of them can leak into
/// anything else this supervisor spawns.
#[cfg(target_os = "linux")]
fn receive_stdio(raw: &UnixStream) -> std::io::Result<[std::os::fd::OwnedFd; 3]> {
    use std::os::fd::AsRawFd;
    let fd = raw.as_raw_fd();
    Ok([
        crate::net::linux::recv_fd(fd)?,
        crate::net::linux::recv_fd(fd)?,
        crate::net::linux::recv_fd(fd)?,
    ])
}

#[cfg(not(target_os = "linux"))]
fn receive_stdio(_raw: &UnixStream) -> std::io::Result<[std::os::fd::OwnedFd; 3]> {
    Err(std::io::Error::other("a one-shot sandbox is a Linux thing"))
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
    peer_verdict(current_uid(), peer_uid(stream))
}

/// The decision itself, separated from the syscall that feeds it.
///
/// Fails **closed**. `peer_uid` returns `None` when the credentials could not
/// be read at all, and the honest reading of that is "this connection's owner
/// is unknown" — which, for a check whose failure mode is arbitrary code
/// execution as this user, has to be a refusal. It read as `None` meaning
/// *allowed* until the code review (B-02), because the syscall and the
/// policy shared one `?`.
fn peer_verdict(ours: u32, peer: Option<u32>) -> Option<Response> {
    match peer {
        None => Some(Response::error(
            ControlError::Unauthorised,
            "cannot read the credentials of the process at the other end of this \
             connection, so it is refused"
                .to_string(),
        )),
        Some(peer) if peer != ours => Some(Response::error(
            ControlError::Unauthorised,
            format!("this supervisor belongs to uid {ours}, not uid {peer}"),
        )),
        Some(_) => None,
    }
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
            tenant: crate::spec::resolve::DEFAULT_TENANT.into(),
            image: String::new(),
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
            .exec(
                "resize",
                serde_json::Value::Null,
                Duration::from_secs(1),
                None,
            )
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
            tenant: crate::spec::resolve::DEFAULT_TENANT.into(),
            image: String::new(),
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
                tenant: None,
                key: None,
                stream: false,
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

    /// Two tenants, byte-identical scripts, one file.
    ///
    /// Deduplication is not a saving here, it is the property that makes a
    /// content-addressed store safe to share: a name derived from the bytes
    /// cannot be claimed, so the second tenant to register a script gets the
    /// first one's file *because it is the same file*, and neither can put
    /// different bytes under a digest the other is running. The tenants cannot
    /// reach each other through it either, and that is proved where it can be
    /// — in the agent, by `test_two_scripts_in_one_pool_cannot_see_each_other`.
    #[test]
    fn two_tenants_registering_the_same_script_get_one_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let mut greeted = true;
        let source = "def handler(event):\n    return {'ok': True}\n";

        let first = dispatch(
            &supervisor,
            Request::PutScript {
                source: source.into(),
                tenant: None,
            },
            &mut greeted,
        );
        let second = dispatch(
            &supervisor,
            Request::PutScript {
                source: source.into(),
                tenant: None,
            },
            &mut greeted,
        );

        let (digest, size) = match (&first, &second) {
            (
                Response::Script {
                    digest: a,
                    size,
                    existed: false,
                },
                Response::Script {
                    digest: b,
                    existed: true,
                    ..
                },
            ) => {
                assert_eq!(a, b, "the same bytes must have the same name");
                (a.clone(), *size)
            }
            other => panic!("{other:?}"),
        };
        assert_eq!(size as usize, source.len());
        assert_eq!(
            crate::scripts::ScriptStore::new(supervisor.paths())
                .list()
                .expect("list")
                .len(),
            1,
            "two registrations left two files"
        );

        // And it is there to be found, by name, by either of them.
        match dispatch(
            &supervisor,
            Request::GetScript {
                digest: digest.clone(),
            },
            &mut greeted,
        ) {
            Response::Script { digest: back, .. } => assert_eq!(back, digest),
            other => panic!("{other:?}"),
        }

        assert!(matches!(
            dispatch(
                &supervisor,
                Request::DeleteScript {
                    digest: digest.clone()
                },
                &mut greeted
            ),
            Response::Ok
        ));
        assert!(matches!(
            dispatch(&supervisor, Request::GetScript { digest }, &mut greeted),
            Response::Error {
                code: ControlError::NotFound,
                ..
            }
        ));
    }

    /// A tenant owns the scripts it registered, and deleting it takes them.
    #[test]
    fn deleting_a_tenant_takes_its_scripts_and_leaves_the_shared_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let mut greeted = true;
        let put = |source: &str, tenant: &str, greeted: &mut bool| -> String {
            match dispatch(
                &supervisor,
                Request::PutScript {
                    source: source.into(),
                    tenant: Some(tenant.into()),
                },
                greeted,
            ) {
                Response::Script { digest, .. } => digest,
                other => panic!("{other:?}"),
            }
        };

        dispatch(
            &supervisor,
            Request::CreateTenant { id: "a".into() },
            &mut greeted,
        );
        dispatch(
            &supervisor,
            Request::CreateTenant { id: "b".into() },
            &mut greeted,
        );
        let shared = put("shared = 1\n", "a", &mut greeted);
        assert_eq!(shared, put("shared = 1\n", "b", &mut greeted));
        let only_a = put("only_a = 1\n", "a", &mut greeted);

        match dispatch(
            &supervisor,
            Request::DeleteTenant { id: "a".into() },
            &mut greeted,
        ) {
            Response::Tenants {
                removed_scripts, ..
            } => assert_eq!(removed_scripts, vec![only_a.clone()]),
            other => panic!("{other:?}"),
        }

        let store = crate::scripts::ScriptStore::new(supervisor.paths());
        let parse = crate::scripts::ScriptDigest::parse;
        assert!(
            !store.contains(&parse(&only_a).unwrap()),
            "the script only the deleted tenant had is still on disk"
        );
        assert!(
            store.contains(&parse(&shared).unwrap()),
            "a script another tenant still refers to was deleted"
        );
    }

    /// A tenant may only reach the functions that are theirs.
    ///
    /// The other half of `a_tenant_cannot_run_another_tenants_script_by_digest`:
    /// a pool call is refused by digest, and a warm function is refused by
    /// name. Both answer `not_found`, because the difference between "no such
    /// function" and "not yours" is a fact about another customer.
    ///
    /// Registered cold, which is the part of the registry a test can build
    /// without a kernel: `owned_by` reads the same two maps a warm one is in.
    #[test]
    fn a_tenant_cannot_reach_another_tenants_function_by_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");

        let mut resolved = resolved_fn("resize");
        resolved.tenant = "acme".into();
        supervisor.cold.lock().expect("cold").insert(
            "resize".into(),
            Cold {
                resolved,
                secrets: BTreeMap::new(),
                sources: Sources::default(),
                last_status: cold_status("resize"),
                since: Instant::now(),
            },
        );

        // The operator reaches everything on their own host.
        assert!(supervisor.owned_by("resize", None).is_ok());
        assert!(supervisor.owned_by("resize", Some("acme")).is_ok());

        for stranger in ["globex", crate::spec::resolve::DEFAULT_TENANT] {
            let refused = supervisor
                .owned_by("resize", Some(stranger))
                .expect_err("another tenant's function");
            match refused {
                Response::Error { code, message } => {
                    assert_eq!(code, ControlError::NotFound);
                    assert!(
                        !message.contains("acme"),
                        "the refusal named the owner: {message}"
                    );
                }
                other => panic!("{other:?}"),
            }
        }

        // And a name nobody served is refused identically, which is what
        // makes the two indistinguishable from outside.
        let message = |response| match response {
            Response::Error { message, .. } => message,
            other => panic!("{other:?}"),
        };
        let missing = message(
            supervisor
                .owned_by("resize-2", Some("acme"))
                .expect_err("no such function"),
        );
        let stranger = message(
            supervisor
                .owned_by("resize", Some("globex"))
                .expect_err("somebody else's"),
        );
        assert_eq!(
            missing.replace("resize-2", "resize"),
            stranger,
            "the two refusals differ, so one can be told from the other"
        );
    }

    /// A token is minted once, resolves, and stops resolving when revoked.
    #[test]
    fn a_token_round_trips_through_the_control_protocol() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let mut greeted = true;

        let (id, secret) = match dispatch(
            &supervisor,
            Request::MintToken {
                tenant: Some("acme".into()),
            },
            &mut greeted,
        ) {
            Response::Tokens { tokens, secret } => (
                tokens.first().expect("a token").id.clone(),
                secret.expect("the secret is on the mint"),
            ),
            other => panic!("{other:?}"),
        };

        // Minting for a tenant registered it, so an embedder onboarding a
        // customer makes one call rather than two in the right order.
        match dispatch(
            &supervisor,
            Request::Tenants {
                id: Some("acme".into()),
            },
            &mut greeted,
        ) {
            Response::Tenants { tenants, .. } => assert_eq!(tenants.len(), 1),
            other => panic!("{other:?}"),
        }

        let store = crate::tokens::Tokens::new(supervisor.paths());
        assert_eq!(
            store
                .resolve(&secret)
                .expect("resolve")
                .and_then(|t| t.tenant().map(str::to_string)),
            Some("acme".into())
        );

        // A listing never carries a secret, whatever it carries.
        match dispatch(&supervisor, Request::Tokens, &mut greeted) {
            Response::Tokens { tokens, secret } => {
                assert_eq!(tokens.len(), 1);
                assert!(secret.is_none(), "a listing answered with a secret");
            }
            other => panic!("{other:?}"),
        }

        assert!(matches!(
            dispatch(
                &supervisor,
                Request::RevokeToken { id: id.clone() },
                &mut greeted
            ),
            Response::Tokens { .. }
        ));
        assert!(
            store.resolve(&secret).expect("resolve").is_none(),
            "a revoked token still resolves"
        );
        assert!(matches!(
            dispatch(&supervisor, Request::RevokeToken { id }, &mut greeted),
            Response::Tokens { .. }
        ));
    }

    /// Deleting a tenant takes their keys with their code.
    #[test]
    fn deleting_a_tenant_revokes_their_tokens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let mut greeted = true;
        let mint = |tenant: &str, greeted: &mut bool| match dispatch(
            &supervisor,
            Request::MintToken {
                tenant: Some(tenant.into()),
            },
            greeted,
        ) {
            Response::Tokens { secret, .. } => secret.expect("a secret"),
            other => panic!("{other:?}"),
        };
        let acme = mint("acme", &mut greeted);
        let globex = mint("globex", &mut greeted);

        dispatch(
            &supervisor,
            Request::DeleteTenant { id: "acme".into() },
            &mut greeted,
        );

        let store = crate::tokens::Tokens::new(supervisor.paths());
        assert!(
            store.resolve(&acme).expect("resolve").is_none(),
            "the deleted tenant's key still opens the door"
        );
        assert!(
            store.resolve(&globex).expect("resolve").is_some(),
            "another tenant's token went with it"
        );
    }

    /// A tenant may only name a script it registered.
    ///
    /// A digest is not a capability — anybody holding the bytes can compute
    /// one — so a tenant that learns another's digest must not be able to run
    /// it. The answer is the same one a digest nobody registered gets, which
    /// is deliberate: "exists but not yours" is a fact about another tenant.
    #[test]
    fn a_tenant_cannot_run_another_tenants_script_by_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let mut greeted = true;
        dispatch(
            &supervisor,
            Request::CreateTenant { id: "a".into() },
            &mut greeted,
        );
        dispatch(
            &supervisor,
            Request::CreateTenant { id: "b".into() },
            &mut greeted,
        );
        let digest = match dispatch(
            &supervisor,
            Request::PutScript {
                source: "secret = 1\n".into(),
                tenant: Some("a".into()),
            },
            &mut greeted,
        ) {
            Response::Script { digest, .. } => digest,
            other => panic!("{other:?}"),
        };

        // No pool is registered, so a request that got past the ownership
        // check would fail with `no runtime named` instead — which is exactly
        // how this test tells the two refusals apart.
        let by_stranger = dispatch(
            &supervisor,
            Request::ExecScript {
                runtime: "nowhere".into(),
                script: crate::protocol::Script {
                    path: None,
                    source: None,
                    digest: Some(digest.clone()),
                    entry_point: None,
                },
                event: serde_json::Value::Null,
                timeout_ms: 1_000,
                tenant: Some("b".into()),
                key: None,
                stream: false,
            },
            &mut greeted,
        );
        match by_stranger {
            Response::Error { code, message } => {
                assert_eq!(code, ControlError::NotFound);
                assert!(message.contains("no script"), "{message}");
            }
            other => panic!("{other:?}"),
        }

        // And the owner gets past the ownership check, to the missing pool.
        let by_owner = dispatch(
            &supervisor,
            Request::ExecScript {
                runtime: "nowhere".into(),
                script: crate::protocol::Script {
                    path: None,
                    source: None,
                    digest: Some(digest),
                    entry_point: None,
                },
                event: serde_json::Value::Null,
                timeout_ms: 1_000,
                tenant: Some("a".into()),
                key: None,
                stream: false,
            },
            &mut greeted,
        );
        match by_owner {
            Response::Error { message, .. } => {
                assert!(message.contains("no runtime"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    /// A digest becomes a path, so it is parsed before it is used as one.
    #[test]
    fn a_digest_that_is_not_one_is_refused_rather_than_looked_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
        let mut greeted = true;

        for bad in ["../../etc/passwd", "sha256:nope", ""] {
            let response = dispatch(
                &supervisor,
                Request::GetScript { digest: bad.into() },
                &mut greeted,
            );
            assert!(
                matches!(
                    response,
                    Response::Error {
                        code: ControlError::BadSpec,
                        ..
                    }
                ),
                "{bad:?} → {response:?}"
            );
        }
    }

    #[test]
    fn calling_a_function_that_was_never_served_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");

        let response = supervisor
            .exec(
                "nope",
                serde_json::Value::Null,
                Duration::from_secs(1),
                None,
            )
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

    /// A peer whose credentials cannot be read is refused, not admitted.
    ///
    /// The syscall is the input to this decision, so the decision is what is
    /// tested; a socket on which `SO_PEERCRED` genuinely fails cannot be
    /// conjured from inside a process that owns both ends. The bug this pins
    /// (B-02) was that the syscall and the policy shared one `?`, so "could
    /// not read the credentials" and "the credentials are ours" returned the
    /// same answer: allowed.
    #[test]
    fn a_peer_that_cannot_be_identified_is_refused() {
        let ours = 1000;

        let unknown = peer_verdict(ours, None).expect("an unidentifiable peer is refused");
        match unknown {
            Response::Error { code, message } => {
                assert_eq!(code, ControlError::Unauthorised);
                assert!(message.contains("cannot read the credentials"), "{message}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }

        let stranger = peer_verdict(ours, Some(1001)).expect("another user is refused");
        assert!(matches!(
            stranger,
            Response::Error {
                code: ControlError::Unauthorised,
                ..
            }
        ));

        // The positive case, so neither refusal above can be satisfied by a
        // check that refuses everything.
        assert!(peer_verdict(ours, Some(ours)).is_none());
    }
}
