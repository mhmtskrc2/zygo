//! Runtime pools: warm the runtime, not the function.
//!
//! A `[fn.<name>]` is one script imported into one zygote. That is the right
//! shape for a function that is called often, and the wrong shape for an
//! embedder with ten thousand of them: a warm zygote costs about 10 MB of
//! proportional memory, which is 97 GiB at that count
//! (`docs/bench-embed.md`). A `[runtime.<name>]` is the other shape — a few
//! anonymous zygotes, an interpreter and a dependency set each, with the
//! script arriving in the request and loaded in the forked child.
//!
//! ```text
//!   EXEC_SCRIPT(runtime, script) ──► pool ──pick a zygote──► fork ──► child
//!                                                   loads the script here
//! ```
//!
//! What has to be true for several tenants to share one zygote is exactly one
//! thing: **the zygote holds nothing of anybody's**. The script never reaches
//! it — the supervisor writes the file into the sandbox and the `EXEC` names
//! a path (`pool::place_script`) — and the child that loads it exits with the
//! request. `Pool::serve_with_logs` refuses to warm a pool that was given a
//! handler, so the claim is checked rather than intended.
//!
//! Scaling is deliberately plain: `min_warm` zygotes always, one more when the
//! gate says every warm one is full, and the existing idle tiering to give
//! them back. A pool that grows on queue depth and shrinks on idleness needs
//! no scheduler, and the two numbers an operator sets are the two they think
//! in — "always have this many" and "never have more than this".

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{ControlError, Gate, QUEUE_WAIT, Rejected, Response, Supervisor, next_log_request_id};
use crate::pool::Function;
use crate::protocol::Script;
use crate::spec::ResolvedFn;

/// One zygote in a pool.
///
/// The count is what routing is decided on and what tiering respects: a
/// zygote with a request in flight is neither the one to pause nor, usually,
/// the one to send the next request to.
pub(super) struct Zygote {
    function: Function,
    in_flight: AtomicU32,
    last_used: Mutex<Instant>,
}

impl Zygote {
    fn new(function: Function) -> Zygote {
        Zygote {
            function,
            in_flight: AtomicU32::new(0),
            last_used: Mutex::new(Instant::now()),
        }
    }

    fn in_flight(&self) -> u32 {
        self.in_flight.load(Ordering::SeqCst)
    }

    fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(*self.last_used.lock().expect("last_used"))
    }

    fn touch(&self) {
        *self.last_used.lock().expect("last_used") = Instant::now();
    }
}

/// A request in flight on one zygote. Dropping it is what frees the slot, so
/// a panicking request path does not leave a zygote looking permanently busy.
struct Busy<'a>(&'a Zygote);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}

/// A registered runtime pool.
pub(super) struct RuntimePool {
    /// The pool's shape: image, dependency set, agent, limits. No code.
    pub(super) resolved: ResolvedFn,
    /// Admission for the pool as a whole. Its ceiling is every zygote's
    /// `concurrency` at `max_warm`, so a caller is told `BUSY` only when the
    /// pool cannot grow its way out of the load.
    pub(super) gate: Gate,
    pub(super) zygotes: Mutex<Vec<Arc<Zygote>>>,
    pub(super) registered: Instant,
}

impl RuntimePool {
    fn warm_count(&self) -> u32 {
        self.zygotes.lock().expect("zygotes").len() as u32
    }
}

/// What `zygo top` and `zygo stats` show for one pool.
///
/// Counted rather than summarised: warm, paused and the room left between
/// them and `max_warm` are three different things to an operator deciding
/// whether a host is out of memory or out of pool.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeStatus {
    pub name: String,
    /// Whose pool this is. See [`crate::pool::Status::tenant`].
    #[serde(default)]
    pub tenant: String,
    pub image: String,
    /// What the agent announced, e.g. `python/3.12.4`. Empty until one is warm.
    pub runtime: String,
    /// Zygotes that can take a request now.
    pub warm: u32,
    /// Zygotes frozen by the idle policy. One write brings each back.
    pub paused: u32,
    /// Room between the zygotes that exist and `max_warm`.
    pub cold: u32,
    pub min_warm: u32,
    pub max_warm: u32,
    pub in_flight: u32,
    pub queued: u32,
    pub requests: u64,
    pub failures: u64,
    /// Resident memory across the pool's zygotes, as each reported it.
    pub rss_kb: u64,
    /// How long this pool has been registered.
    pub uptime_s: u64,
}

impl Supervisor {
    /// Register a runtime pool and bring `min_warm` zygotes up.
    ///
    /// Replaces a pool already under that name, for the reason `serve` gives
    /// for functions: the operator has just said what this runtime should be,
    /// and a name that kept serving the old image would be a debugging trap.
    /// The zygotes hold nothing, so there is no blue/green to do — the old
    /// ones are dropped once the new ones are up.
    pub fn serve_runtime(
        &self,
        name: &str,
        spec: Option<&crate::spec::Spec>,
        layer: &crate::spec::Layer,
        options: &crate::spec::ResolveOptions,
    ) -> std::result::Result<Response, Response> {
        let owned;
        let spec = match spec {
            Some(s) => s,
            None => {
                owned = crate::spec::Spec::default();
                &owned
            }
        };
        let resolved = spec
            .resolve_runtime(name, layer, options)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;

        let started = Instant::now();
        let mut zygotes = Vec::new();
        for _ in 0..resolved.min_warm {
            zygotes.push(Arc::new(Zygote::new(self.warm_zygote(&resolved)?)));
        }
        let status = zygotes
            .first()
            .map(|z| z.function.status())
            .unwrap_or_else(|| crate::pool::Status {
                name: name.to_string(),
                tenant: resolved.tenant.clone(),
                image: resolved.image.clone(),
                state: crate::sandbox::SandboxState::Cold,
                runtime: String::new(),
                rss_kb: 0,
                imports_ms: 0.0,
                requests: 0,
                failures: 0,
            });
        let warnings = resolved.warnings.clone();
        let warm = zygotes.len() as u32;

        let pool = Arc::new(RuntimePool {
            gate: Gate::named(
                &format!("runtime.{name}"),
                resolved.concurrency.saturating_mul(resolved.max_warm),
                Gate::default_queue_limit(resolved.concurrency.saturating_mul(resolved.max_warm)),
            ),
            zygotes: Mutex::new(zygotes),
            resolved,
            registered: Instant::now(),
        });

        // The previous pool is dropped outside the lock: dropping a zygote
        // tears a sandbox down, and doing that under the registry lock would
        // block every other connection on it.
        let previous = self
            .runtimes
            .lock()
            .expect("runtimes")
            .insert(name.to_string(), Arc::clone(&pool));
        let replaced = previous.is_some();
        drop(previous);

        Ok(Response::RuntimeServed {
            name: name.to_string(),
            runtime: status.runtime,
            warm,
            rss_kb: status.rss_kb,
            imports_ms: status.imports_ms,
            warm_ms: started.elapsed().as_secs_f64() * 1000.0,
            warnings,
            change: if replaced {
                super::Change::Replaced
            } else {
                super::Change::Started
            },
        })
    }

    /// Start one zygote for a pool, on the launcher thread.
    fn warm_zygote(&self, resolved: &ResolvedFn) -> std::result::Result<Function, Response> {
        let pool = Arc::clone(&self.pool);
        let spec = resolved.clone();
        let logs = self.logs_for(&resolved.name);
        self.launcher
            .run(move || pool.serve_with_logs(&spec, logs))
            .map_err(|e| Response::error(ControlError::WarmFailed, e))?
            .map_err(|e| Response::error(ControlError::WarmFailed, e))
    }

    fn runtime_named(&self, name: &str) -> std::result::Result<Arc<RuntimePool>, Response> {
        self.runtimes
            .lock()
            .expect("runtimes")
            .get(name)
            .cloned()
            .ok_or_else(|| {
                Response::error(
                    ControlError::NotFound,
                    format!("no runtime named `{name}`; register one with `POST /runtimes`"),
                )
            })
    }

    /// Run one script in a pool.
    ///
    /// The script is the request's, not the runtime's: it reaches the forked
    /// child and dies with it, which is what lets two tenants share the
    /// zygote it was forked from.
    pub fn exec_script(
        &self,
        name: &str,
        script: Script,
        event: serde_json::Value,
        timeout: Duration,
        tenant: Option<&str>,
        key: Option<&str>,
    ) -> std::result::Result<Response, Response> {
        self.exec_script_streaming(name, script, event, timeout, tenant, key, None)
    }

    /// The same, with output delivered as it is produced (v8).
    #[allow(clippy::too_many_arguments)]
    pub fn exec_script_streaming(
        &self,
        name: &str,
        script: Script,
        event: serde_json::Value,
        timeout: Duration,
        tenant: Option<&str>,
        key: Option<&str>,
        sink: Option<crate::pool::ChunkSink<'_>>,
    ) -> std::result::Result<Response, Response> {
        self.exec_script_full(name, script, event, timeout, tenant, key, sink, None)
    }

    /// The whole of what one request to a pool can carry.
    #[allow(clippy::too_many_arguments)]
    pub fn exec_script_full(
        &self,
        name: &str,
        script: Script,
        event: serde_json::Value,
        timeout: Duration,
        tenant: Option<&str>,
        key: Option<&str>,
        sink: Option<crate::pool::ChunkSink<'_>>,
        workspace: Option<crate::supervisor::protocol::WorkspaceRequest>,
    ) -> std::result::Result<Response, Response> {
        let workspace = self.resolve_workspace(workspace)?;
        let tenant_limits = self.limits_for(tenant)?;
        let script = self.script_for_request(script, tenant)?;
        let pool = self.runtime_named(name)?;

        let permit = match pool.gate.enter(QUEUE_WAIT) {
            Ok(permit) => permit,
            Err(Rejected::Closed) => {
                return Err(Response::error(
                    ControlError::NotFound,
                    format!("`{name}` is shutting down"),
                ));
            }
            Err(rejected) => {
                let (in_flight, queued, limit) = rejected.load().unwrap_or_default();
                return Ok(Response::Busy {
                    name: name.to_string(),
                    in_flight,
                    queued,
                    limit,
                });
            }
        };

        let zygote = self.pick_zygote(name, &pool)?;
        // Counted before the call and released on every exit from it, so a
        // request that fails does not leave a zygote looking busy for ever.
        zygote.in_flight.fetch_add(1, Ordering::SeqCst);
        let _busy = Busy(&zygote);

        // Thawing is one write, so a paused zygote answers at warm speed plus
        // that. After the count, for the reason the function path gives: a
        // zygote that is visibly busy is one `tier_idle` will not freeze.
        if let Err(e) = zygote.function.resume() {
            return Err(Response::error(ControlError::CallFailed, e));
        }

        // `tenant`, not the pool's: a pool is shared, so the request's own
        // owner is the only answer to "who may cancel this?".
        let outcome = zygote
            .function
            .call_full(
                event,
                Some(script),
                timeout,
                tenant,
                key,
                sink,
                workspace,
                // Whose limits apply: the request's tenant, not the pool's.
                // A pool is shared, so the pool's own tenant cannot answer
                // this — the same argument cancellation made.
                tenant_limits,
            )
            .map(|(outcome, _)| outcome)
            .map_err(|e| Response::error(ControlError::CallFailed, e));
        drop(permit);

        if let Ok(outcome) = &outcome {
            super::usage(outcome);
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

        outcome.map(|outcome| Response::Executed {
            outcome: Box::new(outcome),
        })
    }

    /// The bytes a request's script names.
    ///
    /// Three shapes reach here and only one of them travels further. A
    /// `digest` on its own is the embedder's usual case — register once, call
    /// ten thousand times — and it is read out of the store here, which is
    /// also where the store's own check that a file hashes to its name
    /// happens. A `source` is the one-off. A `path` is somebody who has
    /// arranged delivery themselves and is taken at their word.
    fn script_for_request(
        &self,
        mut script: Script,
        tenant: Option<&str>,
    ) -> std::result::Result<Script, Response> {
        if script.is_loadable() {
            return Ok(script);
        }
        let Some(digest) = script.digest.clone() else {
            return Err(Response::error(
                ControlError::BadSpec,
                "a script must carry `source`, or a `digest` this host holds",
            ));
        };
        let digest = crate::scripts::ScriptDigest::parse(&digest)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;

        // A tenant may only name a script it registered. The digest is not a
        // capability — anybody who has the bytes can compute it — so without
        // this a tenant who guessed, or who was told, another tenant's digest
        // could run their code and read whatever it returns.
        if let Some(tenant) = tenant {
            let owned = crate::tenants::Tenants::new(&self.paths)
                .get(tenant)
                .map_err(|e| Response::error(ControlError::BadSpec, e))?
                .is_some_and(|t| t.scripts.contains(digest.as_str()));
            if !owned {
                // The same answer as a digest nobody registered, deliberately:
                // telling a tenant that a script exists but is not theirs is
                // telling them something about another tenant.
                return Err(Response::error(
                    ControlError::NotFound,
                    format!("no script {digest}; register it with `PUT /scripts` first"),
                ));
            }
        }

        let store = crate::scripts::ScriptStore::new(&self.paths);
        match store.get(&digest) {
            Ok(Some(source)) => {
                script.source = Some(source);
                Ok(script)
            }
            Ok(None) => Err(Response::error(
                ControlError::NotFound,
                format!("no script {digest}; register it with `PUT /scripts` first"),
            )),
            Err(e) => Err(Response::error(ControlError::CallFailed, e)),
        }
    }

    /// The zygote this request should run on.
    ///
    /// The least busy healthy one. With several identical zygotes and a
    /// `concurrency` each, that is the whole of the scheduling — there is
    /// nothing to be clever about beyond spreading the load, because every
    /// zygote is the same image with the same dependencies and none of them
    /// holds anything a particular request needs.
    ///
    /// Dead ones are *stepped over* here and cleared up by
    /// [`scale_runtimes`], which owns the pool's size. A request that
    /// happened to arrive after a crash should not pay for the rewarm when
    /// there is a working zygote beside it. Only a pool with nothing healthy
    /// left warms one on the request's own time, because then there is no
    /// alternative to offer it.
    ///
    /// [`scale_runtimes`]: Supervisor::scale_runtimes
    fn pick_zygote(
        &self,
        name: &str,
        pool: &Arc<RuntimePool>,
    ) -> std::result::Result<Arc<Zygote>, Response> {
        let healthy = {
            let zygotes = pool.zygotes.lock().expect("zygotes");
            zygotes
                .iter()
                .filter(|z| z.function.is_healthy())
                .min_by_key(|z| z.in_flight())
                .map(Arc::clone)
        };
        if let Some(zygote) = healthy {
            return Ok(zygote);
        }
        tracing::warn!(
            runtime = name,
            "no healthy zygote left in the pool; warming one now"
        );
        self.prune_zygotes(pool);
        self.add_zygote(name, pool)
    }

    /// Drop the zygotes whose agents have died, so the pool's size is the
    /// number that can actually serve a request.
    fn prune_zygotes(&self, pool: &Arc<RuntimePool>) -> usize {
        let mut zygotes = pool.zygotes.lock().expect("zygotes");
        let before = zygotes.len();
        // A zygote with a request in flight is kept whatever its state: the
        // request holds an `Arc` and is still being answered on it.
        zygotes.retain(|z| z.function.is_healthy() || z.in_flight() > 0);
        before - zygotes.len()
    }

    /// Add one zygote to a pool and return it.
    fn add_zygote(
        &self,
        name: &str,
        pool: &Arc<RuntimePool>,
    ) -> std::result::Result<Arc<Zygote>, Response> {
        let zygote = Arc::new(Zygote::new(self.warm_zygote(&pool.resolved)?));
        pool.zygotes
            .lock()
            .expect("zygotes")
            .push(Arc::clone(&zygote));
        tracing::info!(runtime = name, warm = pool.warm_count(), "pool grew");
        Ok(zygote)
    }

    /// Grow every pool that is full and has room, by one zygote.
    ///
    /// Called from the idle thread, once a second, beside [`tier_idle`] —
    /// which is also where shrinking happens, so one pass owns both
    /// directions and they cannot fight. Not on the request path, and that is
    /// the decision worth spelling out: warming costs a few hundred
    /// milliseconds, so a request that waited for a new zygote would be
    /// slower than one that queued for the zygote already there. The queue is
    /// bounded (`QUEUE_WAIT`) and a burst that outlasts it is told `BUSY`; a
    /// burst that lasts more than a second gets another zygote.
    ///
    /// The signal is the gate's own numbers — in flight plus queued against
    /// what the warm zygotes can take — because that is what callers are
    /// actually experiencing rather than a rate this would have to smooth.
    ///
    /// [`tier_idle`]: Supervisor::tier_idle
    /// Stop a request running in one of the pools.
    ///
    /// A pool's requests are spread across its zygotes, so this asks each in
    /// turn. `None` means no pool holds the id. See [`Supervisor::cancel`],
    /// which tries the functions first.
    pub(super) fn cancel_in_pools(&self, id: &str, caller: Option<&str>) -> Option<bool> {
        let pools: Vec<(String, Arc<RuntimePool>)> = self
            .runtimes
            .lock()
            .expect("runtimes")
            .iter()
            .map(|(name, pool)| (name.clone(), Arc::clone(pool)))
            .collect();

        for (name, pool) in pools {
            // A pool belongs to whoever declared it, but its *requests* belong
            // to whoever sent them — and a pool is shared, which is the point
            // of one. So the check cannot be on the pool's tenant: each
            // request carries its own owner, and `Function::cancel` answers
            // `None` for one that is not the caller's.
            let zygotes: Vec<Arc<Zygote>> = pool.zygotes.lock().expect("zygotes").to_vec();
            for zygote in zygotes {
                if let Some(started) = zygote.function.cancel(id, caller) {
                    tracing::info!(runtime = %name, request = %id, started, "cancelled");
                    return Some(started);
                }
            }
        }
        None
    }

    pub fn scale_runtimes(&self) -> Vec<String> {
        let pools: Vec<(String, Arc<RuntimePool>)> = self
            .runtimes
            .lock()
            .expect("runtimes")
            .iter()
            .map(|(name, pool)| (name.clone(), Arc::clone(pool)))
            .collect();

        let mut grown = Vec::new();
        for (name, pool) in pools {
            // Dead zygotes first, so the count below is of zygotes that can
            // actually serve a request. This is also what brings a pool back
            // after a crash: the floor rule then sees it is short.
            let dropped = self.prune_zygotes(&pool);
            if dropped > 0 {
                tracing::warn!(runtime = %name, dropped, "dropped zygotes whose agents had died");
            }

            let (in_flight, queued) = pool.gate.load();
            let warm = pool.warm_count();
            let capacity = warm.saturating_mul(pool.resolved.concurrency);
            let below_floor = warm < pool.resolved.min_warm;
            let loaded = warm < pool.resolved.max_warm && in_flight + queued > capacity;
            // One at a time, deliberately: a spike that needs four more
            // zygotes gets them over four seconds, and one that was over in a
            // moment costs a single sandbox rather than a pool's worth.
            if below_floor || loaded {
                match self.add_zygote(&name, &pool) {
                    Ok(_) => grown.push(name),
                    Err(response) => {
                        tracing::warn!(runtime = %name, "could not grow the pool: {response:?}");
                    }
                }
            }
        }
        grown
    }

    /// Stop a runtime pool and drop every zygote in it.
    pub fn stop_runtime(&self, name: &str) -> std::result::Result<Response, Response> {
        let pool = self.runtimes.lock().expect("runtimes").remove(name);
        let Some(pool) = pool else {
            return Err(Response::error(
                ControlError::NotFound,
                format!("no runtime named `{name}`"),
            ));
        };
        // Stop admitting, then let the drop take the sandboxes: requests
        // already admitted keep their zygote alive through their own `Arc`.
        pool.gate.close();
        for zygote in pool.zygotes.lock().expect("zygotes").iter() {
            let _ = zygote.function.shutdown();
        }
        Ok(Response::Stopped {
            names: vec![name.to_string()],
        })
    }

    /// Every pool, for `zygo top` and `GET /runtimes`.
    pub fn runtimes(&self) -> Vec<RuntimeStatus> {
        let pools: Vec<(String, Arc<RuntimePool>)> = self
            .runtimes
            .lock()
            .expect("runtimes")
            .iter()
            .map(|(name, pool)| (name.clone(), Arc::clone(pool)))
            .collect();

        pools
            .into_iter()
            .map(|(name, pool)| {
                let zygotes = pool.zygotes.lock().expect("zygotes");
                let mut status = RuntimeStatus {
                    name,
                    tenant: pool.resolved.tenant.clone(),
                    image: pool.resolved.image.clone(),
                    runtime: String::new(),
                    warm: 0,
                    paused: 0,
                    cold: 0,
                    min_warm: pool.resolved.min_warm,
                    max_warm: pool.resolved.max_warm,
                    in_flight: 0,
                    queued: 0,
                    requests: 0,
                    failures: 0,
                    rss_kb: 0,
                    uptime_s: pool.registered.elapsed().as_secs(),
                };
                for zygote in zygotes.iter() {
                    let s = zygote.function.status();
                    match s.state {
                        crate::sandbox::SandboxState::Paused => status.paused += 1,
                        _ => status.warm += 1,
                    }
                    status.requests += s.requests;
                    status.failures += s.failures;
                    status.rss_kb += s.rss_kb;
                    if status.runtime.is_empty() {
                        status.runtime = s.runtime;
                    }
                }
                status.cold = pool
                    .resolved
                    .max_warm
                    .saturating_sub(status.warm + status.paused);
                let (in_flight, queued) = pool.gate.load();
                status.in_flight = in_flight;
                status.queued = queued;
                status
            })
            .collect()
    }

    /// Apply the idle policy to the pools, once.
    ///
    /// The zygotes above `min_warm` are the ones that go: they exist because
    /// of load that is now over, and giving them back is the point of having
    /// grown them. The floor is frozen when it goes quiet — one write brings
    /// it back — and never dropped, because `min_warm` is a promise about how
    /// fast the *next* request is served.
    pub(super) fn tier_idle_runtimes(&self, tiered: &mut super::Tiered) {
        let now = Instant::now();
        let pools: Vec<(String, Arc<RuntimePool>)> = self
            .runtimes
            .lock()
            .expect("runtimes")
            .iter()
            .map(|(name, pool)| (name.clone(), Arc::clone(pool)))
            .collect();

        for (name, pool) in pools {
            let mut zygotes = pool.zygotes.lock().expect("zygotes");
            let mut index = 0;
            while index < zygotes.len() {
                let zygote = Arc::clone(&zygotes[index]);
                // A request in flight means this zygote is in use whatever
                // the clock says.
                if zygote.in_flight() > 0 {
                    index += 1;
                    continue;
                }
                let idle = zygote.idle_for(now);
                let above_floor = zygotes.len() as u32 > pool.resolved.min_warm;

                if above_floor && idle >= pool.resolved.cold_after.get() {
                    zygotes.remove(index);
                    tiered.cooled.push(format!("runtime.{name}"));
                    continue;
                }
                if idle >= pool.resolved.idle_timeout.get()
                    && zygote.function.state() == crate::sandbox::SandboxState::Warm
                    && zygote.function.pause().is_ok()
                {
                    tiered.paused.push(format!("runtime.{name}"));
                }
                index += 1;
            }
        }
    }
}
