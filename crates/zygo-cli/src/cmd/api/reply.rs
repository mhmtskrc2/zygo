// SPDX-License-Identifier: Apache-2.0
//! How a control reply becomes an HTTP answer.
//!
//! Every route ends here: a supervisor `Response` mapped to a status code
//! and a JSON body, an [`Outcome`] to the 200/408/499/500/504 a caller
//! reads, and [`HttpError`] for a problem this process found itself.
//! Nothing ever fails the connection — the caller is a webhook or a script,
//! and a dropped connection tells it nothing.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::HeaderValue;
use hyper::{Response, StatusCode};
use zygo_core::pool::Outcome;
use zygo_core::supervisor::{ControlError, Response as Reply};

use super::request::REQUEST_ID_HEADER;

/// Every answer this API sends.
///
/// Boxed rather than `Full<Bytes>` because one route streams: `?stream=1`
/// answers with newline-delimited JSON, a line at a time, while the request is
/// still running. Everything else is still one buffer — `ok()` wraps it — so
/// the cost of the box is one allocation per response and no change to how any
/// of them are built.
pub(super) type ApiBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

/// One whole body, as every non-streaming route answers.
pub(super) fn whole(bytes: Bytes) -> ApiBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

/// A failure that already knows its status code.
#[derive(Debug)]
pub(super) struct HttpError {
    pub(super) status: StatusCode,
    pub(super) body: serde_json::Value,
    /// End the connection with this answer.
    ///
    /// For a refusal decided **before the request body was read** — a bad
    /// token, a header that does not parse. HTTP/1.1 gives a server two
    /// choices there: read the body it is about to throw away, or close. What
    /// it may not do is leave the body on the wire and keep the connection,
    /// which is what this used to do: hyper closed it anyway, the client had
    /// already put it back in its pool, and that caller's *next* request —
    /// often an unrelated one — failed with a broken pipe. Intermittently,
    /// because it depended on which pooled connection came out next.
    close: bool,
}

impl HttpError {
    pub(super) fn new(status: StatusCode, message: impl std::fmt::Display) -> HttpError {
        HttpError {
            status,
            body: serde_json::json!({ "error": message.to_string() }),
            close: false,
        }
    }

    /// The same, for a refusal decided before the body was read.
    pub(super) fn closing(status: StatusCode, message: impl std::fmt::Display) -> HttpError {
        HttpError {
            close: true,
            ..HttpError::new(status, message)
        }
    }

    pub(super) fn into_response(self) -> Response<ApiBody> {
        let mut response = json(self.status, &self.body);
        if self.close {
            response
                .headers_mut()
                .insert(hyper::header::CONNECTION, HeaderValue::from_static("close"));
        }
        response
    }
}

impl From<anyhow::Error> for HttpError {
    fn from(e: anyhow::Error) -> HttpError {
        HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

pub(super) fn reply_to_json(reply: Reply) -> (StatusCode, serde_json::Value) {
    match reply {
        Reply::Executed { outcome } => outcome_to_json(*outcome),
        Reply::Busy {
            name,
            in_flight,
            queued,
            limit,
        } => (
            StatusCode::TOO_MANY_REQUESTS,
            serde_json::json!({
                "error": format!("`{name}` is at its concurrency limit"),
                "in_flight": in_flight, "queued": queued, "limit": limit,
            }),
        ),
        Reply::Warmed { name, state } => (
            StatusCode::OK,
            serde_json::json!({ "name": name, "state": state }),
        ),
        Reply::Functions { functions } => (
            StatusCode::OK,
            serde_json::json!({ "functions": functions }),
        ),
        Reply::Served {
            name,
            runtime,
            rss_kb,
            imports_ms,
            warm_ms,
            warnings,
            change,
        } => (
            StatusCode::OK,
            serde_json::json!({
                "name": name, "runtime": runtime, "rss_kb": rss_kb,
                "imports_ms": imports_ms, "warm_ms": warm_ms,
                "warnings": warnings, "change": change,
            }),
        ),
        Reply::Stopped { names } => (StatusCode::OK, serde_json::json!({ "stopped": names })),
        Reply::Drained {
            in_flight,
            grace_ms,
        } => (
            StatusCode::OK,
            serde_json::json!({
                "drained": in_flight == 0,
                "in_flight": in_flight,
                "grace_ms": grace_ms,
            }),
        ),
        Reply::Secrets { names } => (
            StatusCode::OK,
            // Names, and there is nowhere in this shape to put a value.
            serde_json::json!({ "secrets": names }),
        ),
        Reply::Cancelled { id, started } => (
            StatusCode::OK,
            serde_json::json!({
                "cancelled": true,
                "request_id": id,
                // `false` is the better outcome and worth saying: the child
                // was still parked waiting to be let go, so no handler code
                // ran at all.
                "started": started,
            }),
        ),
        Reply::Script {
            digest,
            size,
            existed,
        } => (
            StatusCode::OK,
            serde_json::json!({ "sha256": digest, "size": size, "existed": existed }),
        ),
        Reply::RuntimeServed {
            name,
            runtime,
            warm,
            rss_kb,
            imports_ms,
            warm_ms,
            warnings,
            change,
        } => (
            StatusCode::OK,
            serde_json::json!({
                "name": name, "runtime": runtime, "warm": warm, "rss_kb": rss_kb,
                "imports_ms": imports_ms, "warm_ms": warm_ms,
                "warnings": warnings, "change": change,
            }),
        ),
        Reply::Runtimes { runtimes } => {
            (StatusCode::OK, serde_json::json!({ "runtimes": runtimes }))
        }
        Reply::Logs {
            name,
            entries,
            next,
        } => (
            StatusCode::OK,
            serde_json::json!({ "name": name, "entries": entries, "next": next }),
        ),
        Reply::Error { code, message } => {
            let status = match code {
                ControlError::NotFound => StatusCode::NOT_FOUND,
                ControlError::BadSpec => StatusCode::BAD_REQUEST,
                ControlError::WarmFailed => StatusCode::SERVICE_UNAVAILABLE,
                ControlError::Unauthorised => StatusCode::FORBIDDEN,
                // Well formed, and the answer is "not that much".
                ControlError::AboveCeiling => StatusCode::UNPROCESSABLE_ENTITY,
                // Well formed, and the answer is "not yet". `reply_to_response`
                // puts a `Retry-After` on it: nothing about the request needs
                // changing, so the caller's correct move is to send it again.
                ControlError::DepsBuilding => StatusCode::SERVICE_UNAVAILABLE,
                ControlError::VersionMismatch
                | ControlError::CallFailed
                | ControlError::BadMessage => StatusCode::INTERNAL_SERVER_ERROR,
            };
            (
                status,
                serde_json::json!({ "error": message, "code": code.as_str() }),
            )
        }
        other => (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({ "error": format!("unexpected reply {other:?}") }),
        ),
    }
}

pub(super) fn reply_to_response(reply: Reply) -> Response<ApiBody> {
    let (status, body) = reply_to_json(reply);
    let mut response = json(status, &body);
    if status == StatusCode::TOO_MANY_REQUESTS {
        response
            .headers_mut()
            .insert("retry-after", hyper::header::HeaderValue::from_static("1"));
    }
    // A pool that named a dependency set still being built. Seconds rather
    // than the one second a `BUSY` gets: a build is minutes, and a caller
    // retrying every second for four minutes is a caller this host has to
    // answer four hundred times to say the same thing.
    if body.get("code") == Some(&serde_json::json!("deps_building")) {
        response
            .headers_mut()
            .insert("retry-after", hyper::header::HeaderValue::from_static("5"));
    }
    // In a header as well as the body, for the same reason `Retry-After` is:
    // the caller who needs it is often the one not reading the body.
    if let Some(id) = body.get("request_id").and_then(|v| v.as_str())
        && let Ok(value) = hyper::header::HeaderValue::from_str(id)
    {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

/// `POST /fn/<name>`'s answers: 200 with the result and
/// metrics; 408 when the deadline killed it; 500 when the handler raised.
pub(super) fn outcome_to_json(outcome: Outcome) -> (StatusCode, serde_json::Value) {
    let metrics = serde_json::json!({
        "wall_ms": outcome.metrics.wall_ms,
        "cpu_ms": outcome.metrics.cpu_ms,
        "peak_rss_kb": outcome.metrics.peak_rss_kb,
    });
    let id = outcome.id;
    // Before the timeout, because a cancelled request is one whose caller
    // already knows why it stopped, and telling them it "exceeded the
    // function's timeout" would be a lie about their own action. `499` is
    // nginx's for a client that went away, and the nearest thing to a
    // registered code for this.
    if outcome.cancelled {
        return (
            StatusCode::from_u16(499).expect("a valid status"),
            serde_json::json!({
                "error": "the request was cancelled",
                "cancelled": true,
                "request_id": id,
                "stdout": outcome.stdout,
                "stderr": outcome.stderr,
                "metrics": metrics,
            }),
        );
    }
    if outcome.timed_out {
        return (
            StatusCode::REQUEST_TIMEOUT,
            serde_json::json!({
                "error": "the request exceeded the function's timeout and was killed",
                "request_id": id,
                "stderr": outcome.stderr,
                "metrics": metrics,
            }),
        );
    }
    // Not a 408. A timeout says the work is too slow or the limit is too
    // tight, and both are about the caller's own numbers. This says the
    // sandbox went quiet with budget left — a `504`, because from the
    // caller's side the thing behind this API stopped answering.
    if outcome.stuck {
        return (
            StatusCode::GATEWAY_TIMEOUT,
            serde_json::json!({
                "error": "the sandbox stopped reporting this request and it was killed",
                "stuck": true,
                "request_id": id,
                "stdout": outcome.stdout,
                "stderr": outcome.stderr,
                "metrics": metrics,
            }),
        );
    }
    if let Some(error) = outcome.error {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            serde_json::json!({
                "error": error,
                "request_id": id,
                "stdout": outcome.stdout,
                "stderr": outcome.stderr,
                "exit_code": outcome.exit_code,
                "metrics": metrics,
            }),
        );
    }
    let mut body = serde_json::json!({
        "result": outcome.result,
        "request_id": id,
        "stdout": outcome.stdout,
        "stderr": outcome.stderr,
        "metrics": metrics,
    });
    // Only when `?out=1` asked. A caller that did not gets exactly the answer
    // it always got, down to the absent key.
    if let Some(tar) = outcome.workspace {
        body["workspace"] = tar.into();
    }
    (StatusCode::OK, body)
}

pub(super) fn json(status: StatusCode, body: &serde_json::Value) -> Response<ApiBody> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(whole(Bytes::from(
            serde_json::to_vec(body).unwrap_or_else(|_| b"{}".to_vec()),
        )))
        .expect("a valid response")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zygo_core::protocol::Metrics;

    fn outcome(error: Option<&str>, timed_out: bool) -> Outcome {
        Outcome {
            tenant: "default".into(),
            function: "resize".into(),
            script: None,
            id: "00000001".into(),
            cancelled: false,
            stuck: false,
            workspace: None,
            exit_code: if error.is_some() { 1 } else { 0 },
            result: serde_json::json!({ "ok": true }),
            stdout: "hi\n".into(),
            stderr: String::new(),
            error: error.map(str::to_string),
            metrics: Metrics {
                wall_ms: 1.5,
                ..Default::default()
            },
            timed_out,
        }
    }

    #[test]
    fn the_status_codes_are_the_design_documents() {
        // 200, 408, 429, 500 — and each distinguishable from the body.
        let (s, body) = outcome_to_json(outcome(None, false));
        assert_eq!(s, StatusCode::OK);
        assert_eq!(body["result"]["ok"], true);
        assert_eq!(body["metrics"]["wall_ms"], 1.5);

        let (s, body) = outcome_to_json(outcome(Some("ZeroDivisionError"), false));
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], "ZeroDivisionError");

        let (s, _) = outcome_to_json(outcome(None, true));
        assert_eq!(s, StatusCode::REQUEST_TIMEOUT);

        let (s, body) = reply_to_json(Reply::Busy {
            name: "f".into(),
            in_flight: 4,
            queued: 16,
            limit: 4,
        });
        assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["limit"], 4);
    }

    #[test]
    fn a_deadline_kill_is_a_408_not_a_500() {
        // Both arrive as exit 137; only `timed_out` tells them apart, which is
        // why the supervisor records it rather than the API guessing.
        let killed = Outcome {
            error: Some("killed by SIGKILL (out of memory, or the deadline expired)".into()),
            exit_code: 137,
            ..outcome(None, true)
        };
        assert_eq!(outcome_to_json(killed).0, StatusCode::REQUEST_TIMEOUT);

        let oom = Outcome {
            error: Some("killed by SIGKILL (out of memory, or the deadline expired)".into()),
            exit_code: 137,
            ..outcome(None, false)
        };
        assert_eq!(outcome_to_json(oom).0, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn control_errors_map_to_distinct_statuses() {
        let status = |code| reply_to_json(Reply::error(code, "x")).0;
        assert_eq!(status(ControlError::NotFound), StatusCode::NOT_FOUND);
        assert_eq!(status(ControlError::BadSpec), StatusCode::BAD_REQUEST);
        assert_eq!(
            status(ControlError::WarmFailed),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status(ControlError::CallFailed),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    /// The three replies the SDK routes added have to map to something, and
    /// the fallback for an unmapped one is a 500 — which would make `serve`
    /// look broken while working.
    #[test]
    fn the_new_replies_have_statuses_of_their_own() {
        let (status, body) = reply_to_json(Reply::Served {
            name: "resize".into(),
            runtime: "python3.12".into(),
            rss_kb: 2048,
            imports_ms: 40.0,
            warm_ms: 120.0,
            warnings: vec!["no timeout set".into()],
            change: zygo_core::supervisor::Change::Replaced,
        });
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["change"], "replaced");
        assert_eq!(body["warnings"][0], "no timeout set");

        let (status, body) = reply_to_json(Reply::Stopped {
            names: vec!["resize".into()],
        });
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["stopped"][0], "resize");

        let (status, body) = reply_to_json(Reply::Logs {
            name: "resize".into(),
            entries: Vec::new(),
            next: 7,
        });
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["next"], 7);
    }
}
