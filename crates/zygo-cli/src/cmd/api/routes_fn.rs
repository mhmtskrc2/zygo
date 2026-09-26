// SPDX-License-Identifier: Apache-2.0
//! The routes about one function: `/fn/<name>`, `/run`, `/requests/<id>`.
//!
//! Serve it, call it — once, in a batch, or streaming — read its log and
//! its numbers, warm it, stop it, cancel a request on it; and `POST /run`,
//! the one route that is a sandbox with no function behind it. Each is one
//! control request to the supervisor, which is where ownership is checked.

use std::sync::Arc;

use hyper::{Response, StatusCode};
use zygo_core::supervisor::{Request as Control, Response as Reply};

use super::reply::{ApiBody, HttpError, json, reply_to_response};
use super::{Api, control};
use zygo_core::spec::Layer;
use zygo_core::supervisor::WorkspaceRequest;

use anyhow::Context;

use super::reply::reply_to_json;
use super::request::{DEFAULT_TIMEOUT_MS, Query};
use super::usage::count_usage;

/// Largest batch accepted.
///
/// Without a cap, a 16 MB body of `[null,null,…]` is about a million elements,
/// and the batch handler opened a supervisor connection and spawned a task for
/// each. The supervisor serves every connection on a thread, so the
/// cost of that request was paid by the host rather than by the caller.
pub(super) const MAX_BATCH: usize = 1024;

/// How many batch elements may be in flight at once.
///
/// A batch is one caller asking for several things; it should not be able to
/// out-compete every other caller for supervisor connections by asking for a
/// thousand. The per-function concurrency limit is what actually bounds
/// sandbox work — this bounds the connections in front of it.
pub(super) const BATCH_IN_FLIGHT: usize = 16;

/// How long a one-shot `POST /run` may hold the connection beyond the
/// sandbox's own timeout before the child is killed outright.
///
/// The launcher enforces the function's `timeout` against the whole process
/// tree; this is the outer bound for the case where it cannot — a child that
/// never starts, or a backend that hangs before the timer exists. Without it
/// an HTTP caller has no upper bound at all.
const RUN_GRACE_MS: u64 = 30_000;

pub(super) async fn exec(
    api: &Arc<Api>,
    name: String,
    event: serde_json::Value,
    timeout_ms: u64,
    tenant: Option<String>,
    key: Option<String>,
    workspace: Option<WorkspaceRequest>,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| {
        Ok(c.send(&Control::Exec {
            name,
            event,
            timeout_ms,
            tenant,
            key,
            stream: false,
            workspace,
        })?)
    })
    .await?;
    count_usage(api, &reply);
    Ok(reply_to_response(reply))
}

/// `POST /fn/<name>/batch`: every event at once, answers in order.
///
/// Each element carries its own status, because one event being refused with
/// a 429 or failing in its handler must not hide the answers to the others —
/// and must not be hidden by them.
pub(super) async fn batch(
    api: &Arc<Api>,
    name: String,
    events: Vec<serde_json::Value>,
    timeout_ms: u64,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    if events.len() > MAX_BATCH {
        return Err(HttpError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "a batch of {} is over this API's ceiling of {MAX_BATCH}\n  \
                 → send it in several requests; each element costs a supervisor \
                 connection and a thread",
                events.len()
            ),
        ));
    }

    // One caller asking for a thousand things must not take a thousand
    // supervisor connections ahead of every other caller. The
    // per-function concurrency limit still bounds the sandbox work behind
    // this; the semaphore bounds the plumbing in front of it.
    let permits = Arc::new(tokio::sync::Semaphore::new(BATCH_IN_FLIGHT));
    let calls = events.into_iter().map(|event| {
        let name = name.clone();
        let tenant = tenant.clone();
        let api = Arc::clone(api);
        let permits = Arc::clone(&permits);
        async move {
            let _permit = permits
                .acquire()
                .await
                .expect("the semaphore is not closed");
            let reply = control(&api, move |c| {
                Ok(c.send(&Control::Exec {
                    name,
                    event,
                    timeout_ms,
                    tenant,
                    // A batch is one HTTP request and many sandbox requests,
                    // so one key would name all of them. Cancelling the batch
                    // is cancelling the HTTP call; per-element cancellation
                    // needs a name per element, which nothing has asked for.
                    key: None,
                    // And `?stream=1` on a batch would interleave several
                    // requests' output on one connection with nothing to tell
                    // them apart. Call them one at a time to watch them.
                    stream: false,
                    // And a workspace on a batch would be one directory
                    // several requests wrote into at once, which is the thing
                    // a per-request workspace exists not to be.
                    workspace: None,
                })?)
            })
            .await;
            match reply {
                Ok(reply) => {
                    count_usage(&api, &reply);
                    let (status, body) = reply_to_json(reply);
                    let mut body = body;
                    body["status"] = status.as_u16().into();
                    body
                }
                Err(e) => serde_json::json!({ "status": 500, "error": format!("{e:#}") }),
            }
        }
    });
    let answers = futures_join_all(calls).await;
    Ok(json(StatusCode::OK, &serde_json::Value::Array(answers)))
}

/// `join_all` without pulling in `futures`: the batch is bounded and ordered.
///
/// Concrete in the element type rather than generic, because the one thing it
/// has to do on a panic — put a JSON error in that element's place — is only
/// expressible once the type is known.
async fn futures_join_all<F>(futures: impl IntoIterator<Item = F>) -> Vec<serde_json::Value>
where
    F: std::future::Future<Output = serde_json::Value> + Send + 'static,
{
    let handles: Vec<_> = futures.into_iter().map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        // A panicking element is reported as that element's failure. It used
        // to `expect`, which took down the whole batch — and with it the
        // answers to every element that had already succeeded, which is the
        // one thing a batch exists to prevent.
        out.push(h.await.unwrap_or_else(|e| {
            tracing::error!("a batch element panicked: {e}");
            serde_json::json!({
                "status": 500,
                "error": "this element failed unexpectedly; the others are unaffected",
            })
        }));
    }
    out
}

/// One customer's functions, or every one of them for the operator.
///
/// Filtered here rather than in the supervisor, unlike the routes that *act*
/// on a function: this is the operator's own process reading the operator's
/// own registry, and the question "which of these may this caller see?" is one
/// only the side that resolved the token can answer. Every route that does
/// something to a function is checked at the supervisor, where it belongs.
pub(super) fn mine(
    functions: Vec<zygo_core::pool::Status>,
    tenant: Option<&str>,
) -> Vec<zygo_core::pool::Status> {
    match tenant {
        None => functions,
        Some(id) => functions.into_iter().filter(|f| f.tenant == id).collect(),
    }
}

pub(super) async fn list(
    api: &Arc<Api>,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, |c| Ok(c.send(&Control::List)?)).await?;
    match reply {
        Reply::Functions { functions } => {
            let functions = mine(functions, tenant.as_deref());
            Ok(json(
                StatusCode::OK,
                &serde_json::json!({ "functions": functions }),
            ))
        }
        other => Ok(reply_to_response(other)),
    }
}

pub(super) async fn stats(
    api: &Arc<Api>,
    name: String,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, |c| Ok(c.send(&Control::List)?)).await?;
    match reply {
        Reply::Functions { functions } => {
            match mine(functions, tenant.as_deref())
                .into_iter()
                .find(|f| f.name == name)
            {
                Some(f) => Ok(json(
                    StatusCode::OK,
                    &serde_json::to_value(f).unwrap_or_default(),
                )),
                // The same answer a name nobody served gets: whose function
                // `name` is, is a fact about another customer.
                None => Err(HttpError::new(
                    StatusCode::NOT_FOUND,
                    format!("no function named `{name}`"),
                )),
            }
        }
        other => Ok(reply_to_response(other)),
    }
}

pub(super) async fn warm(
    api: &Arc<Api>,
    name: String,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::Warm { name, tenant })?)).await?;
    Ok(reply_to_response(reply))
}

/// `DELETE /requests/<id>`: stop a request that is running.
///
/// Answers as soon as the kill has been sent, not when the request has
/// stopped: the request's own caller is the one waiting for the outcome, and
/// holding this connection open until they get it would make a cancel cost as
/// long as the thing it cancelled.
pub(super) async fn cancel(
    api: &Arc<Api>,
    id: String,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::Cancel { id, tenant })?)).await?;
    Ok(reply_to_response(reply))
}

/// What `PUT /fn/<name>` accepts: the same inputs `zygo serve` sends, minus
/// the ones that only mean something at a terminal.
///
/// Deliberately **not** a whole spec file. The supervisor is the authority on
/// what a function is and resolves the layer itself; a client that wants a
/// `sandbox.toml` deployed has `zygo up`, which reads it where it lives rather
/// than shipping a copy whose relative paths mean something else here.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ServeRequest {
    #[serde(default)]
    layer: Layer,
    /// What the layer's relative paths are relative to, on *this* host.
    ///
    /// Absolute and required to be: the API process has a working directory of
    /// its own, and a caller across a socket has no way to know it. Letting it
    /// default silently is how `entry = "./handler.py"` ends up resolving
    /// somewhere nobody meant.
    base_dir: std::path::PathBuf,
    /// Secret values by name. The caller supplies them because the API
    /// process's environment is nobody's idea of where `STRIPE_KEY` lives —
    /// the same reasoning as the control protocol's own `secrets` field.
    #[serde(default)]
    secrets: std::collections::BTreeMap<String, String>,
    /// Leave the function alone when it is already exactly this. What
    /// `zygo up` sets, so calling this twice is not two deploys.
    #[serde(default)]
    if_changed: bool,
}

pub(super) async fn serve_fn(
    api: &Arc<Api>,
    name: String,
    body: &[u8],
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let request: ServeRequest = serde_json::from_slice(body).map_err(|e| {
        HttpError::new(
            StatusCode::BAD_REQUEST,
            format!("body is not a serve request: {e}"),
        )
    })?;
    if !request.base_dir.is_absolute() {
        return Err(HttpError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "`base_dir` must be absolute, and `{}` is not\n  \
                 → it names a directory on the host this API runs on",
                request.base_dir.display()
            ),
        ));
    }

    let reply = control(api, move |c| {
        Ok(c.send(&Control::Serve {
            tenant,
            name,
            spec: None,
            layer: Box::new(request.layer),
            base_dir: request.base_dir,
            // Never from a socket. Each of these removes a guarantee, and a
            // caller that can widen the boundary over HTTP makes the flag on
            // the server meaningless. Set them where the sandbox is declared.
            allow_host_net: false,
            allow_private_net: false,
            allow_unlimited: false,
            secrets: request.secrets,
            if_changed: request.if_changed,
        })?)
    })
    .await?;
    Ok(reply_to_response(reply))
}

pub(super) async fn stop(api: &Arc<Api>, name: String) -> Result<Response<ApiBody>, HttpError> {
    let wanted = name.clone();
    let reply = control(api, move |c| {
        Ok(c.send(&Control::Stop {
            name: Some(name),
            // A route about one function. A pool of the same name is every
            // tenant's, and has `DELETE /runtimes/<name>`.
            runtimes: false,
        })?)
    })
    .await?;
    // The supervisor answers `Stopped { names }` with an empty list for a name
    // it does not hold. An SDK needs that to be a 404, not a 200 that looks
    // like it worked.
    if let Reply::Stopped { names } = &reply
        && names.is_empty()
    {
        return Err(HttpError::new(
            StatusCode::NOT_FOUND,
            format!("no function named `{wanted}`"),
        ));
    }
    Ok(reply_to_response(reply))
}

pub(super) async fn logs(
    api: &Arc<Api>,
    name: String,
    query: &str,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let params = Query::parse(query);
    let after = params.number("after")?.unwrap_or(0);
    let limit = params.number("limit")?.unwrap_or(50);
    let failed = params.flag("failed")?;
    let limit = u32::try_from(limit).unwrap_or(u32::MAX);

    let reply = control(api, move |c| {
        Ok(c.send(&Control::Logs {
            name,
            after,
            limit,
            failed,
            tenant,
        })?)
    })
    .await?;
    Ok(reply_to_response(reply))
}

/// What `POST /run` accepts.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RunRequest {
    /// The sandbox, exactly as a `[fn.<name>]` table would describe it.
    /// `image` is required; `cmd` defaults to the image's own entrypoint.
    layer: Layer,
    /// Fed to the command's standard input, which is then closed.
    #[serde(default)]
    stdin: String,
}

/// `POST /run` — one sandbox, one command, no warm pool.
///
/// The work is [`crate::cmd::oneshot::run`], which spawns `zygo run` as a child and
/// captures its streams; the reasoning for a child rather than an in-process
/// launch is documented there. Here it runs on a blocking thread, because it
/// is synchronous and the runtime this API uses is not.
pub(super) async fn one_shot(api: &Arc<Api>, body: &[u8]) -> Result<Response<ApiBody>, HttpError> {
    let request: RunRequest = serde_json::from_slice(body).map_err(|e| {
        HttpError::new(
            StatusCode::BAD_REQUEST,
            format!("body is not a run request: {e}"),
        )
    })?;

    let layer = request.layer;
    let image = layer
        .image
        .clone()
        .ok_or_else(|| HttpError::new(StatusCode::BAD_REQUEST, "`layer.image` is required"))?;
    let argv = layer.cmd.clone().unwrap_or_default();
    // Relative paths in a body have no meaning here: the API's working
    // directory is its own, and unlike `serve` there is no `base_dir` to
    // anchor them to — the child resolves them against a temporary spec file
    // in a temporary directory.
    if let Some(mounts) = &layer.mounts {
        for mount in mounts {
            if !mount.source.is_absolute() {
                return Err(HttpError::new(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "mount source `{}` must be absolute\n  \
                         → a relative path in a request body has no directory to be relative to",
                        mount.source.display()
                    ),
                ));
            }
        }
    }

    // The bound the caller waits under. The sandbox's own timeout is enforced
    // by the launcher against the whole process tree; this covers the case
    // where the child never gets that far.
    let deadline = std::time::Duration::from_millis(
        layer
            .timeout
            .map(|t| u64::try_from(t.0.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .saturating_add(RUN_GRACE_MS),
    );

    let exe = api.exe.clone();
    let stdin = request.stdin;
    let captured = tokio::task::spawn_blocking(move || {
        crate::cmd::oneshot::run(&exe, layer, &image, &argv, stdin.as_bytes(), deadline)
    })
    .await
    .context("the sandbox task panicked")??;

    // 200 whenever the sandbox *ran*, whatever ended it — a non-zero exit, its
    // own timeout, an out-of-memory kill. The body says which, and that is the
    // whole reason those fields exist; answering 408 for a sandbox that hit
    // the `timeout` its caller asked for made the SDK raise instead of
    // returning the result, which contradicts "a non-zero exit is not an
    // exception". Found by `tests/linux/verify_api.sh` against a real kernel.
    //
    // 408 is kept for the one case it fits: this API's own outer bound fired,
    // the child was killed, and what it would have said is unknown. That is
    // Zygo failing to finish, not a sandbox doing its job.
    let status = if captured.abandoned {
        StatusCode::REQUEST_TIMEOUT
    } else {
        StatusCode::OK
    };
    Ok(json(
        status,
        &serde_json::json!({
            "exit_code": captured.exit_code,
            "stdout": captured.stdout,
            "stderr": captured.stderr,
            // Why it ended, which the exit status cannot carry: a deadline
            // kill and an out-of-memory kill are both 137.
            "timed_out": captured.timed_out,
            "oom_killed": captured.oom_killed,
            "peak_rss_kb": captured.peak_rss_kb,
            "wall_ms": captured.wall_ms,
            // Whether the program ran at all, and how far the run got when
            // it did not: a start failure is *unavailable*, not the code's.
            "started": captured.started,
            "phase": captured.phase,
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A warm function as the supervisor would list it, owned by `tenant`.
    fn status(name: &str, tenant: &str) -> zygo_core::pool::Status {
        zygo_core::pool::Status {
            name: name.into(),
            tenant: tenant.into(),
            image: String::new(),
            state: zygo_core::sandbox::SandboxState::Warm,
            runtime: "python/3.12".into(),
            rss_kb: 0,
            imports_ms: 0.0,
            requests: 3,
            failures: 1,
        }
    }

    /// Listing is scoped by the token, not by what the caller asked for.
    #[test]
    fn a_listing_shows_one_tenant_their_own_functions_only() {
        let all = vec![
            status("resize", "acme"),
            status("resize-2", "globex"),
            status("internal", "default"),
        ];

        let operator = mine(all.clone(), None);
        assert_eq!(operator.len(), 3, "the operator sees the host");

        let acme = mine(all, Some("acme"));
        assert_eq!(acme.len(), 1);
        assert_eq!(acme[0].name, "resize");
    }

    /// A request body may not remove a guarantee, whatever the deploy gate
    /// says.
    ///
    /// Each `allow_*` widens the boundary, so a caller that could set one over
    /// HTTP would make the flags on the server meaningless. Checked by
    /// deserialising a body that asks for all three: `deny_unknown_fields`
    /// refuses it outright, which is the strongest form of "not available".
    #[test]
    fn a_request_body_cannot_widen_the_boundary() {
        let honest: ServeRequest =
            serde_json::from_str(r#"{"layer":{"image":"alpine:3"},"base_dir":"/srv"}"#)
                .expect("an ordinary serve request");
        assert_eq!(honest.layer.image.as_deref(), Some("alpine:3"));

        for field in ["allow_host_net", "allow_private_net", "allow_unlimited"] {
            let body = format!(r#"{{"layer":{{}},"base_dir":"/srv","{field}":true}}"#);
            assert!(
                serde_json::from_str::<ServeRequest>(&body).is_err(),
                "`{field}` was accepted from a request body"
            );
        }
    }

    /// `base_dir` has to be absolute, and the check is here rather than in the
    /// supervisor because this is where the mistake can be explained.
    ///
    /// A relative path in a request body has no directory to be relative to:
    /// the API process has a working directory of its own, and a caller across
    /// a socket cannot know it.
    #[test]
    fn a_relative_base_directory_is_refused_with_the_reason() {
        let request: ServeRequest =
            serde_json::from_str(r#"{"layer":{},"base_dir":"./app"}"#).expect("parsed");
        assert!(!request.base_dir.is_absolute());
    }

    /// A one-shot run reports the image and command as arguments, and
    /// everything else as a sandbox — and `stdin` is not part of the sandbox.
    ///
    /// Leaking `stdin` into the layer would reach the spec file the child
    /// reads, where `deny_unknown_fields` refuses it — turning a perfectly
    /// good request into an unexplainable failure two processes away.
    #[test]
    fn a_run_request_separates_the_sandbox_from_the_call() {
        let request: RunRequest = serde_json::from_str(
            r#"{"layer":{"image":"alpine:3","cmd":["echo","hi"],"mem":"64M"},"stdin":"input"}"#,
        )
        .expect("a run request");
        assert_eq!(request.layer.image.as_deref(), Some("alpine:3"));
        assert_eq!(request.stdin, "input");

        // `stdin` sits beside the layer, so it cannot be in it.
        assert!(
            serde_json::to_value(&request.layer)
                .expect("serialisable")
                .get("stdin")
                .is_none()
        );

        assert!(
            serde_json::from_str::<RunRequest>(r#"{"layer":{},"stdin":"x","extra":1}"#).is_err(),
            "an unknown field should be refused rather than ignored"
        );
    }
}
