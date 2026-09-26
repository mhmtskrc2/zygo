// SPDX-License-Identifier: Apache-2.0
//! The registry: what is served under each name, and in which tier.
//!
//! A name is warm ([`Entry`]: a sandbox, a gate, a clock), or cold
//! ([`Cold`]: the spec and the last status, nothing running), or unknown.
//! `serve` puts a name in, `stop` takes it out, `lookup` finds it and wakes
//! it if it went cold, and `owned_by` says whose it is. [`Sources`] is
//! what makes `zygo up` idempotent: the bytes a function was built from,
//! so an edited handler counts as a change and an untouched one does not.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Change, ControlError, Gate, Response, Supervisor};
use crate::pool::{Function, Status};
use crate::spec::{Layer, ResolveOptions, ResolvedFn, Spec};

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
pub(super) struct Sources(pub(super) Vec<(PathBuf, Option<[u8; 32]>)>);

impl Sources {
    /// Hash the files `resolved` will load. A file that cannot be read hashes
    /// to `None`: the warm-up will report that properly, and until then the
    /// comparison only has to say "not what is registered", which it does.
    pub(super) fn of(resolved: &ResolvedFn) -> Sources {
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
pub(super) struct Entry {
    pub(super) resolved: ResolvedFn,
    /// Secret values, kept apart from `resolved` on purpose: the resolved spec
    /// is what `zygo spec explain` prints, and these must never be in it.
    pub(super) secrets: BTreeMap<String, String>,
    /// What was on disk when the sandbox started. See [`Sources`].
    pub(super) sources: Sources,
    pub(super) function: Function,
    pub(super) gate: Gate,
    pub(super) registered: Instant,
    /// When a request last finished. Drives the idle policy.
    pub(super) last_used: Mutex<Instant>,
}

impl Entry {
    pub(super) fn status(&self) -> Status {
        self.function.status()
    }

    pub(super) fn touch(&self) {
        *self.last_used.lock().expect("last_used") = Instant::now();
    }

    pub(super) fn idle_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(*self.last_used.lock().expect("last_used"))
    }
}

/// A function that has been tiered all the way down.
///
/// Only the spec is kept. That is the whole point: a cold function costs
/// nothing but a map entry, and the next request rebuilds it from exactly the
/// configuration it had. The last status is kept alongside so `zygo ps` does not
/// reset someone's request count just because their function went quiet.
pub(super) struct Cold {
    pub(super) resolved: ResolvedFn,
    pub(super) secrets: BTreeMap<String, String>,
    pub(super) sources: Sources,
    pub(super) last_status: Status,
    pub(super) since: Instant,
}

/// A function that was just warmed and registered.
pub(super) struct Warmed {
    pub(super) entry: Arc<Entry>,
    pub(super) status: Status,
    pub(super) warnings: Vec<String>,
    /// Whether something — warm, paused or cold — held the name before.
    pub(super) replaced: bool,
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

impl Supervisor {
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

        // Whatever the tenant has in the store fills in what the client did
        // not send. The client wins where both have a value: `zygo serve` at a
        // terminal is somebody saying what they want *now*, and a stored
        // secret is the standing arrangement.
        //
        // Two sources rather than one because they answer different needs. An
        // operator serving from a shell has the value in their environment; an
        // embedder's customer has it in the store and nobody to restart.
        let mut secrets = secrets;
        if let Some(store) = self.secret_store() {
            match store.values(&resolved.tenant) {
                Ok(stored) => {
                    for (name, value) in stored {
                        secrets.entry(name).or_insert(value);
                    }
                }
                Err(e) => return Err(Response::error(ControlError::BadSpec, e)),
            }
        }

        // Every secret the spec names has to have a value, and only the client
        // or the store could have supplied one — so a missing value is
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
                     set {} in the environment of the shell running `zygo serve`, \
                     or store {} for this tenant with \
                     `PUT /tenants/<id>/secrets/<name>`",
                    missing.join(", "),
                    if missing.len() == 1 { "it" } else { "them" },
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
    pub(super) fn is_registered_as(
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
    pub(super) fn warm_and_register(
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

    /// The log ring for `name`, created on first use.
    pub(super) fn logs_for(&self, name: &str) -> crate::pool::Logs {
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
    pub(super) fn ensure_warm(&self, name: &str) -> std::result::Result<Arc<Entry>, Response> {
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
    ///
    /// With `runtimes`, a pool under that name goes too, and with no name
    /// every pool: what `zygo stop` asks for. A pool is reported as
    /// `runtime.<name>`, the way `DELETE /tenants/<id>` reports one, so the
    /// output says which kind went. A name that is **both** a function and a
    /// pool stops both — `stop` means "forget this name", and the two
    /// registries were only ever separate so that `DELETE /fn/<name>`, which
    /// leaves `runtimes` off, cannot reach a pool that every tenant shares.
    pub fn stop(
        &self,
        name: Option<&str>,
        runtimes: bool,
    ) -> std::result::Result<Response, Response> {
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
                Some(name) => functions.remove(name).into_iter().collect(),
                None => std::mem::take(&mut *functions)
                    .into_values()
                    .collect::<Vec<_>>(),
            }
        };

        let pools: Vec<String> = if runtimes {
            let held: Vec<String> = {
                let pools = self.runtimes.lock().expect("runtimes");
                match name {
                    Some(name) => pools
                        .contains_key(name)
                        .then(|| name.to_string())
                        .into_iter()
                        .collect(),
                    None => pools.keys().cloned().collect(),
                }
            };
            held.into_iter()
                .filter(|pool| self.stop_runtime(pool).is_ok())
                .map(|pool| format!("runtime.{pool}"))
                .collect()
        } else {
            Vec::new()
        };

        if let Some(name) = name
            && removed.is_empty()
            && cold_names.is_empty()
            && pools.is_empty()
        {
            return Err(Response::error(
                ControlError::NotFound,
                if runtimes {
                    format!("no function or runtime pool named `{name}`")
                } else {
                    format!("no function named `{name}`")
                },
            ));
        }

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
        names.extend(pools);
        Ok(Response::Stopped { names })
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
    pub(super) fn owned_by(
        &self,
        name: &str,
        caller: Option<&str>,
    ) -> std::result::Result<(), Response> {
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

    pub(super) fn lookup(&self, name: &str) -> std::result::Result<Arc<Entry>, Response> {
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
}
