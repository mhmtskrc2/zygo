// SPDX-License-Identifier: Apache-2.0
//! One warm function: a sandbox with an agent in it, waiting.
//!
//! The request path the crate exists for. `EXEC` makes the agent fork;
//! `FORKED` names the child, which is then admitted to its own cgroup and
//! given its secrets while it is parked; `GO` lets it run; `DONE` is the
//! answer. [`WarmFn::call_streaming`] is that path in full, deadline and
//! heartbeat included, and every other `call_*` narrows it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use super::conn::{Conn, ReplyError};
use super::request::{
    Call, KILL_GRACE, RequestLease, Requests, Workspace, WorkspaceLease, admit, innermost_ns_pid,
    kill_request, next_request_id, resident_kb,
};
use super::scripts::{SCRIPT_DIR_IN_SANDBOX, ScriptLease, Scripts, place_script};
use super::secrets::{Secrets, SecretsAt, SecretsLease, place_secrets};
use super::timing::{CallTiming, CpuAccounting};
use super::{Counters, Outcome, Status};
use crate::error::{Error, IoContext, Result};
use crate::protocol::{Message, Metrics, ProtocolError};
use crate::sandbox::SandboxState;

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

/// One warm function: a sandbox with an agent in it, waiting.
pub struct WarmFn {
    pub(super) name: String,
    /// See [`Status::tenant`].
    pub(super) tenant: String,
    /// See [`Status::image`].
    pub(super) image: String,
    /// The supervisor's end of the connection to the agent.
    pub(super) conn: std::sync::Arc<Conn>,
    /// The thread routing replies. Joined on drop, after the socket is shut
    /// down so it is guaranteed to be on its way out.
    pub(super) replies: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Kept so the connection can be closed deliberately rather than only when
    /// the last descriptor happens to go.
    pub(super) socket: std::os::unix::net::UnixStream,
    /// Announced by the agent in its `READY`.
    pub(super) runtime: String,
    pub(super) rss_kb: u64,
    pub(super) imports_ms: f64,
    pub(super) counters: Mutex<Counters>,
    /// Requests sent and not yet answered, so a cancel on another thread can
    /// find one. See [`InFlight`](super::InFlight).
    pub(super) in_flight: Requests,
    /// `Warm` until something goes wrong. Behind a lock because a request that
    /// finds the agent wedged has to record that for every later caller.
    pub(super) state: Mutex<SandboxState>,
    /// The tenant's cgroup: where its limits are enforced and its CPU is
    /// accounted. Shared with the sandbox this one replaced, for as long as
    /// that one is still being torn down.
    pub(super) tenant_cgroup: Option<PathBuf>,
    /// This sandbox's own generation under the tenant, where per-request
    /// cgroups are created when they are. Retiring the previous generation
    /// kills that one's cgroup, not this one.
    pub(super) generation_cgroup: Option<PathBuf>,
    pub(super) per_request_cgroup: bool,
    /// The function's own wall-clock budget, from its resolved spec.
    ///
    /// This is the limit; a caller's timeout only decides how long *it* waits.
    /// The spec's limits are mandatory, so a client asking for longer cannot
    /// get it.
    pub(super) timeout: std::time::Duration,
    /// Everything this function was declared with, kept whole.
    ///
    /// The ceiling a tenant's own limits are narrowed against. A pool's
    /// requests come from different tenants and the sandbox is shared, so the
    /// narrowing cannot happen at warm time — it happens per request, on the
    /// request's own cgroup.
    pub(super) limits: crate::sandbox::limits::Limits,
    /// The agent's pid as the *host* sees it.
    ///
    /// The agent lives in its own pid namespace, so the pid it reports in
    /// `FORKED` is meaningless here; this is the anchor used to translate it.
    /// See [`WarmFn::host_pid_of`]. It is also the way into the sandbox's
    /// filesystem from outside, via `/proc/<pid>/root`, which is how secrets
    /// get in without ever crossing the agent's connection.
    pub(super) agent_host_pid: u32,
    /// Secret values by name, and how many requests currently need them.
    ///
    /// Behind one lock, because the count decides when the files exist: the
    /// first request in flight writes them, the last one out removes them, and
    /// two requests racing on that boundary must not leave a caller reading a
    /// file the other just deleted.
    pub(super) secrets: Mutex<Secrets>,
    /// Scripts written into the sandbox for the requests running them, and how
    /// many requests each still has. See [`Scripts`] and `place_script` in
    /// `pool/scripts.rs`.
    pub(super) scripts: Mutex<Scripts>,
    /// The host side of `/run/script`: this sandbox's alone, removed with it.
    pub(super) script_dir: PathBuf,
    /// Kept alive: dropping it kills the sandbox.
    pub(super) _sandbox: Box<dyn crate::backend::Sandbox>,
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
    /// The three phases are the ones worth telling apart: fork, cgroup setup,
    /// and the handler itself.
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

    /// The same, under a name the caller chose. See `InFlight::key`.
    pub fn call_script_keyed(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        caller: Option<&str>,
        key: Option<&str>,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_streaming(Call {
            script,
            caller,
            key,
            ..Call::new(event, timeout)
        })
    }

    /// The same, with everything a request can carry (proto 1.3 and up).
    ///
    /// `sink` is called for every `CHUNK` the agent forwards. Passing `None`
    /// is the ordinary path and is what everything that is not streaming
    /// does: the `EXEC` then does not ask for chunks, the child captures its
    /// output the way it always has, and not one extra frame crosses the
    /// socket. See [`Call`] for the rest.
    pub fn call_streaming(&self, call: Call<'_>) -> Result<(Outcome, CallTiming)> {
        let Call {
            event,
            script,
            timeout,
            caller,
            key,
            sink,
            workspace,
            tenant_limits,
            secrets,
        } = call;
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
        // This used to be resolved inside `admit`, which returned `None`
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
            narrowed.as_ref().unwrap_or(&self.limits),
        );
        request.running_at(request_cgroup.as_deref(), host_pid);
        let _secrets = match self.place_secrets(secrets) {
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
        // than the spec allows is asking past a mandatory limit.
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
    /// This is the middle tier of the idle policy: a
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

    fn place_secrets(&self, own: Option<BTreeMap<String, String>>) -> Result<SecretsLease<'_>> {
        place_secrets(&self.secrets, SecretsAt::Proc(self.agent_host_pid), own)
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
