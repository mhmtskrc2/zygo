// SPDX-License-Identifier: Apache-2.0
//! How the supervisor starts, recovers, idles, drains and stops.
//!
//! Everything about the process's own life rather than about one request:
//! the [`Launcher`] thread every sandbox is created on, the [`Backoff`]
//! that keeps a crashing function from becoming a busy host, the idle
//! policy that freezes and then forgets a quiet function, and the two ways
//! out — `DRAIN` over the socket, and `SIGTERM`, which drains too.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::registry::{Cold, Entry};
use super::{ControlError, Response, Supervisor, runtime};
use crate::error::{Error, Result};

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
/// Starts are serialised as a consequence, so what runs here is kept to the
/// start itself: the `clone3`, the handshake with the child, and the wait for
/// an agent's `READY`, each with a deadline of its own. Everything a start
/// needs that can be made beforehand — a venv, a derived layer, the bytecode
/// layer, a resolved allowlist, a rootfs view — is made on the caller's thread
/// first ([`crate::pool::Pool::prepare`]). A `pip install` used to run here,
/// and every `zygo run` on the host waited behind it.
///
/// The queue is bounded in both directions. A job waits at most the budget
/// its caller gives ([`WARM_QUEUE_WAIT`], [`RUN_QUEUE_WAIT`]) for its turn,
/// and no more than [`LAUNCH_QUEUE_LIMIT`] jobs may be waiting at once; past
/// either, the caller is told the launcher is busy, what it is doing and for
/// how long, and the job never runs. Without the bound, one start that had
/// hung inside the kernel held every later one for ever, the supervisor
/// answered `ping`, and nothing said why.
pub(super) struct Launcher {
    jobs: std::sync::mpsc::Sender<Job>,
    thread: Option<std::thread::JoinHandle<()>>,
    load: Arc<Load>,
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// How many starts may wait for the launcher at once. One more is refused
/// at the door, with no wait: a queue that deep means the launcher is stuck
/// or the host is far past what it can warm, and either way the honest
/// answer is now.
pub const LAUNCH_QUEUE_LIMIT: usize = 32;

/// How long a warm-up may wait for its turn on the launcher.
///
/// Generous: a `POST /runtimes` with `min_warm = 8` on a small board queues
/// eight starts of a few seconds each behind one another, and they are all
/// legitimate. A start that has not even begun after this is behind
/// something that is not going to finish.
pub const WARM_QUEUE_WAIT: Duration = Duration::from_secs(60);

/// How long a one-shot run may wait for its turn.
///
/// Under the thirty seconds a client gives `RUN` to say `STARTED`
/// ([`super::client::CONTROL_TIMEOUT`]), so the refusal reaches the client
/// as an answer it can print rather than as a timeout it has to guess at.
pub const RUN_QUEUE_WAIT: Duration = Duration::from_secs(20);

/// A start that has held the launcher this long is not going to finish.
///
/// What the launcher does for one start — the `clone3`, the mounts, the
/// handshake to `execve` — has a 60 s deadline in the `ns` backend, and the
/// kill-and-reap after a failed one has 10 s more. Twice that and a bit: a
/// start past this is parked inside the kernel, in a wait no deadline in
/// user space can end, and the honest thing is to say so.
pub const LAUNCH_STUCK_AFTER: Duration = Duration::from_secs(180);

/// What the launcher is doing, for the refusal message and the log.
#[derive(Default)]
struct Load {
    /// Jobs sent and not yet taken off the channel.
    queued: std::sync::atomic::AtomicUsize,
    /// The job running now.
    running: Mutex<Option<RunningJob>>,
}

struct RunningJob {
    what: String,
    since: Instant,
    /// Whether the log has already been told this one is stuck.
    warned: bool,
}

/// What the launcher is doing, as `RUNTIMES` reports it (v15) and
/// `GET /healthz` repeats it.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct LauncherLoad {
    /// Starts waiting for their turn.
    pub queued: u32,
    /// The start the launcher is on now, if any — `warm \`resize\``,
    /// `run \`python:3.12-slim\``.
    pub running: Option<String>,
    /// For how long, in seconds.
    pub running_s: u64,
    /// Whether that is longer than any start can legitimately take
    /// ([`LAUNCH_STUCK_AFTER`]): the launcher is parked inside the kernel
    /// and will not come back on its own.
    pub stuck: bool,
}

/// A queued job's state: the hand-off between a caller that may give up and
/// the thread that may pick the job up, decided by one compare-and-swap so
/// that exactly one of them wins.
const QUEUED: u8 = 0;
const RUNNING: u8 = 1;
const ABANDONED: u8 = 2;

impl Launcher {
    pub(super) fn new() -> Result<Launcher> {
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
                    // the supervisor looked hung rather than broken.
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
            load: Arc::new(Load::default()),
        })
    }

    /// Run `f` on the launcher thread and wait for its result.
    ///
    /// `wait` bounds the time in the queue, not the work: once `f` has
    /// started, the caller waits for it to finish, however long its own
    /// deadlines allow. A job still queued when the budget runs out is
    /// abandoned — it never runs — and the caller gets an error naming what
    /// the launcher was doing instead. `what` is the job's name in that
    /// message and in the log.
    pub(super) fn run_within<T: Send + 'static>(
        &self,
        what: impl Into<String>,
        wait: Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T> {
        use std::sync::atomic::{AtomicU8, Ordering};
        use std::sync::mpsc::RecvTimeoutError;

        let what = what.into();
        // Refused before anything is sent: a queue this deep is not going to
        // reach this job inside any budget, and queueing it anyway would only
        // make the one after it wait longer to be told the same thing.
        if self.load.queued.load(Ordering::SeqCst) >= LAUNCH_QUEUE_LIMIT {
            return Err(self.busy(&what, None));
        }

        let (done, result) = std::sync::mpsc::channel();
        let ticket = Arc::new(AtomicU8::new(QUEUED));
        let job: Job = {
            let ticket = Arc::clone(&ticket);
            let load = Arc::clone(&self.load);
            let what = what.clone();
            Box::new(move || {
                load.queued.fetch_sub(1, Ordering::SeqCst);
                // The caller gave up while this sat in the queue. Nobody is
                // waiting for the result, and a sandbox started for nobody
                // would be a leak, so the work is not done at all.
                if ticket
                    .compare_exchange(QUEUED, RUNNING, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
                {
                    return;
                }
                let _running = Running::start(&load, what);
                // A receiver that has gone away means the caller was killed;
                // the work still ran, and dropping the result is the right
                // thing.
                let _ = done.send(f());
            })
        };
        self.load.queued.fetch_add(1, Ordering::SeqCst);
        if self.jobs.send(job).is_err() {
            self.load.queued.fetch_sub(1, Ordering::SeqCst);
            return Err(launcher_gone());
        }

        match result.recv_timeout(wait) {
            Ok(value) => Ok(value),
            Err(RecvTimeoutError::Disconnected) => Err(launcher_gone()),
            Err(RecvTimeoutError::Timeout) => {
                // Still queued: take it back, and it never runs. Already
                // running: the sandbox is being made for this caller, so the
                // caller stays for it — the start has deadlines of its own.
                if ticket
                    .compare_exchange(QUEUED, ABANDONED, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
                {
                    Err(self.busy(&what, Some(wait)))
                } else {
                    result.recv().map_err(|_| launcher_gone())
                }
            }
        }
    }

    /// How many jobs are waiting for their turn.
    #[cfg(test)]
    pub(super) fn queued(&self) -> usize {
        self.load.queued.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// What the launcher is doing right now.
    pub(super) fn load(&self) -> LauncherLoad {
        self.load_at(Instant::now(), LAUNCH_STUCK_AFTER)
    }

    /// [`Launcher::load`] with the clock and the threshold as arguments, so
    /// "stuck" can be tested without waiting three minutes for it.
    pub(super) fn load_at(&self, now: Instant, stuck_after: Duration) -> LauncherLoad {
        let queued = self.load.queued.load(std::sync::atomic::Ordering::SeqCst) as u32;
        let running = self.load.running.lock().unwrap_or_else(|e| e.into_inner());
        match running.as_ref() {
            Some(job) => {
                let age = now.saturating_duration_since(job.since);
                LauncherLoad {
                    queued,
                    running: Some(job.what.clone()),
                    running_s: age.as_secs(),
                    stuck: age >= stuck_after,
                }
            }
            None => LauncherLoad {
                queued,
                ..LauncherLoad::default()
            },
        }
    }

    /// Say in the log, once per job, that the running start is stuck.
    ///
    /// Called from the idle thread once a second, so the log names the
    /// stuck start without anybody having to ask — the refusal message
    /// names it too, but only to the callers that were refused.
    pub(super) fn warn_if_stuck(&self) {
        let mut running = self.load.running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = running.as_mut()
            && !job.warned
            && job.since.elapsed() >= LAUNCH_STUCK_AFTER
        {
            job.warned = true;
            tracing::warn!(
                job = job.what,
                running_s = job.since.elapsed().as_secs(),
                queued = self.load.queued.load(std::sync::atomic::Ordering::SeqCst),
                "launcher stuck: this start is past every deadline a start has, so it is \
                 parked inside the kernel; every later start is refused until the \
                 supervisor is restarted"
            );
        }
    }

    /// The error a refused job gets, and the same line in the log.
    fn busy(&self, what: &str, waited: Option<Duration>) -> Error {
        let queued = self.load.queued.load(std::sync::atomic::Ordering::SeqCst);
        let doing = match self
            .load
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(job) => format!(
                "`{}` has been running for {} s",
                job.what,
                job.since.elapsed().as_secs()
            ),
            None => "nothing is running".to_string(),
        };
        let reason = match waited {
            Some(wait) => format!(
                "`{what}` waited {} s for the launcher and did not get a turn: \
                 {queued} starts are queued and {doing}",
                wait.as_secs()
            ),
            None => format!(
                "the launcher's queue is full, so `{what}` was not queued: \
                 {queued} starts are waiting and {doing}"
            ),
        };
        tracing::warn!(job = what, queued, "launcher busy: {doing}");
        Error::BackendUnavailable {
            backend: "supervisor",
            reason,
            remedy: "try again shortly; if it does not clear, `supervisor.log` in the data \
                     folder says what the launcher is doing, and `zygo supervisor stop` \
                     ends it"
                .into(),
        }
    }
}

/// Marks the job the launcher is on, and unmarks it however the job ends —
/// a panic included, or the refusal message would name a job that was long
/// gone.
struct Running<'a>(&'a Load);

impl<'a> Running<'a> {
    fn start(load: &'a Load, what: String) -> Self {
        *load.running.lock().unwrap_or_else(|e| e.into_inner()) = Some(RunningJob {
            what,
            since: Instant::now(),
            warned: false,
        });
        Running(load)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        *self.0.running.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
        remedy: "`supervisor.log` in the data folder has the panic; a single panicking job \
                 no longer stops the ones after it"
            .into(),
    }
}

/// How soon after a crash the first rewarm may be attempted.
///
/// Immediately: the target is that a function is back within 500 ms of its
/// agent dying, and a warm-up is ~300 ms, so there is
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
pub(super) struct Backoff {
    pub(super) failures: u32,
    pub(super) last_attempt: Option<Instant>,
}

impl Backoff {
    /// How long to wait after `failures` consecutive failures.
    pub(super) fn delay(failures: u32) -> Duration {
        if failures == 0 {
            return Duration::ZERO;
        }
        REWARM_BACKOFF_BASE
            .saturating_mul(1u32.checked_shl(failures - 1).unwrap_or(u32::MAX))
            .min(REWARM_BACKOFF_MAX)
    }

    /// Whether another attempt is allowed yet, and how long is left if not.
    pub(super) fn ready(&self, now: Instant) -> std::result::Result<(), Duration> {
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

impl Supervisor {
    /// Bring a crashed function back, respecting its backoff.
    ///
    /// Recovery is by rewarming rather than repair: a sandbox whose agent
    /// died has no state worth recovering, and the spec that built it is still
    /// in the registry. The backoff is what stops a function that cannot start
    /// at all — a handler that segfaults on import — from becoming a rewarm loop
    /// that costs more than the function ever would.
    pub(super) fn rewarm(
        &self,
        name: &str,
        broken: &Arc<Entry>,
    ) -> std::result::Result<Arc<Entry>, Response> {
        // One rewarm at a time per function. Everything below — the
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

    /// Apply the idle policy once.
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

    /// Ask the accept loop to stop taking connections.
    #[allow(clippy::missing_const_for_fn)]
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    /// Whether this supervisor is on its way out.
    ///
    /// What `GET /healthz` reports as a `503`: a load balancer that keeps
    /// sending to a draining host is the reason draining does not work.
    pub fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// What the launcher is doing: the `launcher` of a `RUNTIMES` answer.
    pub fn launcher_load(&self) -> LauncherLoad {
        self.launcher.load()
    }

    /// The idle thread's once-a-second look at the launcher: a start past
    /// every deadline a start has is named in the log, once.
    pub fn check_launcher(&self) {
        self.launcher.warn_if_stuck();
    }

    /// Stop admitting, let what is running finish, and say what was waited for.
    ///
    /// Three steps, in this order, and the order is the whole of it:
    ///
    /// 1. **Stop admitting.** Every gate closes, so a request that has not
    ///    started is refused now rather than accepted into a process that is
    ///    leaving. A caller sees the refusal immediately and can go elsewhere.
    /// 2. **Wait.** In-flight requests run to their own ends. A drain that
    ///    killed them would be a restart with extra steps.
    /// 3. **Give up on the stragglers.** Past `grace` the wait stops and the
    ///    answer says how many were still running, because a deploy script
    ///    needs to know whether it drained or timed out — and a `grace` that
    ///    silently became "for ever" is how a rolling restart hangs.
    ///
    /// The supervisor exits after answering, which is why the answer is sent
    /// before the accept loop is asked to stop.
    pub fn drain(&self, grace: Duration) -> Response {
        // The entries themselves, not their gates: a `Gate` is not `Clone`
        // and cloning one would be the wrong thing anyway — the gate a
        // request is waiting on has to be *the* gate, not a copy of its
        // numbers.
        let functions: Vec<Arc<Entry>> = self
            .functions
            .lock()
            .expect("registry")
            .values()
            .map(Arc::clone)
            .collect();
        let pools: Vec<Arc<runtime::RuntimePool>> = self
            .runtimes
            .lock()
            .expect("runtimes")
            .values()
            .map(Arc::clone)
            .collect();

        for entry in &functions {
            entry.gate.close();
        }
        for pool in &pools {
            pool.gate.close();
        }

        let running = || {
            functions.iter().map(|e| e.gate.load().0).sum::<u32>()
                + pools.iter().map(|p| p.gate.load().0).sum::<u32>()
        };
        let until = Instant::now() + grace;
        let mut in_flight = running();
        while in_flight > 0 && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
            in_flight = running();
        }

        tracing::info!(in_flight, "drained");
        Response::Drained {
            in_flight,
            grace_ms: grace.as_millis() as u64,
        }
    }

    /// Close a function to new requests and let it go.
    ///
    /// The gate is closed first so anything queued is turned away rather than
    /// admitted to a sandbox that is about to disappear. The `Arc` may still be
    /// held by a request in flight; the sandbox dies when the last one lets go,
    /// which is the behaviour a caller mid-request wants.
    pub(super) fn retire(&self, entry: Arc<Entry>) {
        tracing::debug!(function = %entry.resolved.name, "retire: closing the gate");
        entry.gate.close();
        let _ = entry.function.shutdown();
        tracing::debug!(function = %entry.resolved.name, "retire: shut down");
    }
}

/// Set by the `SIGTERM` handler, read by the accept loop.
///
/// A plain flag because a signal handler may not allocate, lock, or log: the
/// most it can honestly do is set this and nudge the loop awake.
pub(super) static TERMINATING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// How long `SIGTERM` waits for in-flight requests before giving up.
///
/// Twenty-five seconds, chosen against the thirty systemd and Docker give a
/// process by default: a drain that outlived its own `SIGKILL` would be a
/// drain that never finished.
pub(super) const DEFAULT_TERM_GRACE: Duration = Duration::from_secs(25);

/// Run `on_signal` when this process is asked to terminate.
///
/// Returns a guard that restores the previous disposition; dropping it is how
/// a test puts the process back as it found it.
#[cfg(unix)]
pub fn on_terminate(on_signal: impl Fn() + Send + Sync + 'static) -> TermGuard {
    static HANDLER: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>> = std::sync::OnceLock::new();
    let _ = HANDLER.set(Box::new(on_signal));

    extern "C" fn trampoline(_signal: libc::c_int) {
        // Nothing here allocates or locks. The closure this reaches sets an
        // atomic and connects to a unix socket, both of which are safe from a
        // handler; anything more would be a deadlock waiting for the right
        // moment.
        if let Some(handler) = HANDLER.get() {
            handler();
        }
    }

    // SAFETY: installing a handler for SIGTERM with a function that does no
    // allocation. `SIG_DFL` is restored by the guard.
    unsafe {
        libc::signal(libc::SIGTERM, trampoline as *const () as libc::sighandler_t);
    }
    TermGuard
}

/// Restores `SIGTERM` to its default when dropped.
#[cfg(unix)]
pub struct TermGuard;

#[cfg(unix)]
impl Drop for TermGuard {
    fn drop(&mut self) {
        // SAFETY: restoring the default disposition.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_DFL);
        }
    }
}
