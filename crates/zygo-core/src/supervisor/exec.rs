// SPDX-License-Identifier: Apache-2.0
//! One request to a warm function, from the socket to the sandbox.
//!
//! `exec_full` is the path: find the function, replace it if its agent
//! died, go through its gate, run it, and redirect once if the gate closed
//! under the request because somebody redeployed. `cancel` is the other
//! half — a request found by its id or its caller's key and killed from
//! outside. Runtime pools have their own copy of this shape in
//! [`runtime`](super::runtime).

use std::sync::Arc;
use std::time::Duration;

use super::registry::Entry;
use super::{ControlError, QUEUE_WAIT, Rejected, Response, Supervisor, next_log_request_id, usage};
use crate::pool::Call;

/// One pass at a request, from [`Supervisor::attempt`].
pub(super) enum Attempt {
    /// The request was admitted and ran, or was turned away with an answer.
    Done(std::result::Result<Response, Response>),
    /// The gate had been closed. The event comes back so it can be offered to
    /// whatever holds the name now.
    Closed(serde_json::Value),
}

impl Supervisor {
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
        event: serde_json::Value,
        timeout: Duration,
        key: Option<&str>,
        sink: Option<crate::pool::ChunkSink<'_>>,
    ) -> std::result::Result<Response, Response> {
        self.exec_full(name, event, timeout, key, sink, None)
    }

    /// The whole of what one request to a warm function can carry.
    pub fn exec_full(
        &self,
        name: &str,
        mut event: serde_json::Value,
        timeout: Duration,
        key: Option<&str>,
        sink: Option<crate::pool::ChunkSink<'_>>,
        workspace: Option<crate::supervisor::protocol::WorkspaceRequest>,
    ) -> std::result::Result<Response, Response> {
        let workspace = self.resolve_workspace(workspace)?;
        // A warm function belongs to one tenant, so its limits are that
        // tenant's — resolved once here rather than per attempt.
        let limits = self.limits_for(self.tenant_of(name).as_deref())?;
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

            let call = Call {
                key,
                sink,
                workspace: workspace.clone(),
                tenant_limits: limits.clone(),
                // A function's values were set when it was served.
                secrets: None,
                ..Call::new(event, timeout)
            };
            event = match self.attempt(&entry, name, call) {
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
    pub(super) fn attempt(&self, entry: &Entry, name: &str, call: Call<'_>) -> Attempt {
        let permit = match entry.gate.enter(QUEUE_WAIT) {
            Ok(permit) => permit,
            Err(Rejected::Closed) => return Attempt::Closed(call.event),
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
        // **After** the permit, and that ordering is the fix. It used
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
            .call_full(call)
            .map(|(outcome, _)| outcome)
            .map_err(|e| Response::error(ControlError::CallFailed, e));
        drop(permit);
        // After the call, not before: the clock should measure how long the
        // function has been idle, not how long ago a slow request started.
        entry.touch();

        if let Ok(outcome) = &outcome {
            usage(outcome);
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

    /// Turn a request's `workspace` into the bytes the pool will unpack.
    ///
    /// A `blob` is read from the store here, on the supervisor's side, rather
    /// than travelling again: that is the whole point of having sent it once.
    /// `inline` is decoded here for the same reason the store's `get` hashes —
    /// the sooner a bad input is refused, the less of the request has run.
    pub(super) fn resolve_workspace(
        &self,
        request: Option<crate::supervisor::protocol::WorkspaceRequest>,
    ) -> std::result::Result<Option<crate::pool::Workspace>, Response> {
        let Some(request) = request else {
            return Ok(None);
        };
        if request.inline.is_some() && request.blob.is_some() {
            return Err(Response::error(
                ControlError::BadSpec,
                "a workspace is `inline` or `blob`, not both",
            ));
        }

        let inbox = match (&request.inline, &request.blob) {
            (Some(encoded), _) => Some(
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
                    .map_err(|e| {
                        Response::error(
                            ControlError::BadSpec,
                            format!("`workspace.inline` is not base64: {e}"),
                        )
                    })?,
            ),
            (_, Some(digest)) => {
                let parsed = crate::scripts::ScriptDigest::parse(digest)
                    .map_err(|e| Response::error(ControlError::BadSpec, e))?;
                match crate::blobs::BlobStore::new(&self.paths)
                    .get(&parsed)
                    .map_err(|e| Response::error(ControlError::CallFailed, e))?
                {
                    Some(bytes) => Some(bytes),
                    None => {
                        return Err(Response::error(
                            ControlError::NotFound,
                            format!("no blob {digest}"),
                        ));
                    }
                }
            }
            (None, None) => None,
        };

        Ok(Some(crate::pool::Workspace {
            inbox,
            collect: request.collect,
        }))
    }
}
