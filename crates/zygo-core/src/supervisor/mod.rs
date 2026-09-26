// SPDX-License-Identifier: Apache-2.0
//! The supervisor: the process that owns the warm pool between commands.
//!
//! Zygo is daemonless (rule P5 in `docs/book/08-principles.md`). That is a
//! statement about *whose* process
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
//! One RPC boundary on the request path, which is the whole point of that rule:
//! Docker's client → daemon → containerd → shim chain is deliberately absent.
//!
//! Recovery is by rewarming, not by state repair. If the
//! supervisor is restarted, its functions are gone and the next `serve` pays
//! the warm-up again — a few hundred milliseconds, against the standing risk of
//! reattaching to sandboxes whose state nobody can vouch for.
//!
//! ## The module
//!
//! This file is [`Supervisor`] itself — the tables, and how it is made.
//! What it does lives beside it, by what it is about:
//!
//! - `registry` — what is served under each name: serve, stop, look up.
//! - `exec` — one request to a warm function, gate and redirect included.
//! - `runtime` — runtime pools: anonymous zygotes, the script per request.
//! - `lifecycle` — the launcher thread, rewarm backoff, idle tiers, drain.
//! - `oneshot` — a sandbox run for a client with the client's streams.
//! - `tenants`, `stores`, `deps` — the routes about customers and content.
//! - `listener`, `dispatch` — the socket, and the table behind it.
//! - `protocol`, `client`, `gate` — the wire, its client, and admission.
//!
//! Every public item is re-exported here, so a caller names
//! `zygo_core::supervisor::Listener` and never a file.

pub mod client;
pub mod deps;
mod dispatch;
mod exec;
pub mod gate;
mod lifecycle;
mod listener;
mod oneshot;
pub mod protocol;
mod registry;
pub mod runtime;
mod stores;
mod tenants;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::paths::Paths;
use crate::pool::{Pool, PoolConfig};

pub use gate::{Gate, Rejected};
pub use lifecycle::{REWARM_BACKOFF_BASE, REWARM_BACKOFF_MAX, TIER_INTERVAL, Tiered};
#[cfg(unix)]
pub use lifecycle::{TermGuard, on_terminate};
pub use listener::Listener;
pub use protocol::{CONTROL_VERSION, Change, ControlError, Request, Response, WorkspaceRequest};
pub use registry::FunctionLoad;
pub use runtime::{RuntimeStatus, ScriptRequest};

use lifecycle::{Backoff, Launcher};
use registry::{Cold, Entry};

/// How long a request waits for a concurrency slot before it is told to retry.
///
/// Short on purpose: the caller is blocked on a socket, and a queue that holds
/// someone for longer than their own patience has converted backpressure into a
/// timeout. Everything past this gets a `BUSY` it can act on.
pub const QUEUE_WAIT: Duration = Duration::from_secs(5);

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
    /// The key per-tenant secrets are sealed with, if this host has one.
    ///
    /// Read once at start-up, not per request: a key that could change under a
    /// running supervisor would mean half the store readable and half not, and
    /// nothing would say which half. `None` is a host with no secrets, which
    /// is a working host — see [`Supervisor::secret_store`].
    secret_key: Option<Arc<crate::secrets::SecretKey>>,
    /// Rewarm history per name. See [`Backoff`] for why it is not in `Entry`.
    rewarms: Mutex<BTreeMap<String, Backoff>>,
    /// One lock per function name, held for the length of a rewarm.
    ///
    /// Without it, N requests that all find the same crashed function all
    /// rewarm it: N sandboxes built, N of them registered in turn, and N−1
    /// thrown away — on a function that crashes under load, which is when the
    /// host can least afford it. The first request through builds the
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
    /// Held for the length of one dependency-set build.
    ///
    /// One at a time: two `pip install`s at two CPUs each, on a host that is
    /// also serving requests, is a host that stops serving them. Builds queue
    /// here in the order their threads reach it. See [`deps`].
    deps_build: Arc<Mutex<()>>,
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

        // A build belongs to the supervisor that started it. If that process
        // is gone, nothing is going to finish this one, and a pool naming it
        // would wait for ever on a `503` telling it to try again.
        let interrupted = crate::deps::fail_interrupted(&paths);
        if interrupted > 0 {
            tracing::info!(
                count = interrupted,
                "dependency builds left unfinished by a previous supervisor were marked failed"
            );
        }

        Ok(Supervisor {
            pool,
            launcher: Launcher::new()?,
            paths,
            functions: Mutex::new(BTreeMap::new()),
            runtimes: Mutex::new(BTreeMap::new()),
            // A key that was configured and is unusable stops the supervisor
            // here, rather than at the first request that needed it: an
            // operator who typo'd the variable must not get a host that
            // quietly behaves as though they had set nothing.
            secret_key: crate::secrets::SecretKey::from_env(|k| std::env::var(k).ok())?
                .map(Arc::new),
            cold: Mutex::new(BTreeMap::new()),
            rewarms: Mutex::new(BTreeMap::new()),
            warming: Mutex::new(BTreeMap::new()),
            logs: Mutex::new(BTreeMap::new()),
            started: Instant::now(),
            stopping: AtomicBool::new(false),
            deps_build: Arc::new(Mutex::new(())),
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
}

/// Request ids in the log: short, unique per supervisor, and the same shape
/// the agent protocol uses so a line in the log can be matched to a trace.
fn next_log_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{:08x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// Record what one finished request cost.
///
/// A structured `tracing` event, at `info`, with one field per column an
/// embedder bills on. It goes out from the **supervisor**, which is the only
/// process that sees every request — `zygo exec` at a terminal, an HTTP call,
/// an MCP tool — so a host's usage is one stream rather than whatever each
/// front end happened to notice.
///
/// Deliberately a log record and not a channel. Every deployment already has
/// somewhere logs go; a second transport out of the supervisor would be a
/// second thing to configure, secure and lose events to. The API layers its
/// own delivery on top for the requests it made, which is where an embedder's
/// billing actually sits.
pub(super) fn usage(outcome: &crate::pool::Outcome) {
    let usage = crate::pool::Usage::from(outcome);
    tracing::info!(
        target: "zygo::usage",
        tenant = %usage.tenant,
        function = %usage.function,
        script = usage.script.as_deref().unwrap_or(""),
        request_id = %usage.request_id,
        wall_ms = usage.wall_ms,
        cpu_ms = usage.cpu_ms,
        peak_rss_kb = usage.peak_rss_kb,
        outcome = %usage.outcome,
        "request finished"
    );
}

#[cfg(test)]
mod tests;
