// SPDX-License-Identifier: Apache-2.0
//! What this host says about itself: `/healthz`, `/metrics`, `/drain`.
//!
//! The routes a load balancer, a scraper and a deploy script use, none of
//! which is about one function. `/healthz` is the one unauthenticated
//! route and is cached for a second because of it; `/metrics` is scoped
//! by the token the way `GET /fn` is, so a series named after a function
//! is only shown to whoever may see that function.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use bytes::Bytes;
use hyper::{Response, StatusCode};
use zygo_core::supervisor::{Request as Control, Response as Reply};

use super::reply::{ApiBody, HttpError, json, reply_to_response, whole};
use super::routes_fn::mine;
use super::{Api, control};

/// `GET /healthz`: whether this host should be sent work.
///
/// Three answers, because a load balancer needs three:
///
/// * **`200 ok`** — every pool is at its floor.
/// * **`200 degraded`** — a pool is below `min_warm`, so requests will work
///   but the first of them pay a cold start. Still `200`: a host that can
///   serve should be served to, and a probe that took it out of rotation for
///   being slow would take every host out at once after a restart.
/// * **`503 stopping`** — the supervisor is draining. A balancer that keeps
///   sending here is the reason draining does not work, so this is the one
///   answer that is not `200`.
///
/// Cached for a second. The route is unauthenticated and a control round trip
/// per probe would be a way to make a host busy without a token.
pub(super) async fn healthz(api: &Arc<Api>) -> Result<Response<ApiBody>, HttpError> {
    const FRESH: std::time::Duration = std::time::Duration::from_secs(1);

    if let Some((at, body)) = api.health.lock().expect("health").clone()
        && at.elapsed() < FRESH
    {
        let stopping = body["status"] == "stopping";
        return Ok(json(
            if stopping {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            },
            &body,
        ));
    }

    let reply = control(api, |c| Ok(c.send(&Control::Runtimes)?)).await;
    let (status, body) = match reply {
        Ok(Reply::Runtimes { runtimes }) => {
            let below: Vec<&str> = runtimes
                .iter()
                .filter(|r| r.warm + r.paused < r.min_warm)
                .map(|r| r.name.as_str())
                .collect();
            if below.is_empty() {
                (
                    StatusCode::OK,
                    serde_json::json!({
                        "ok": true,
                        "status": "ok",
                        "uptime_s": api.started.elapsed().as_secs(),
                    }),
                )
            } else {
                (
                    StatusCode::OK,
                    serde_json::json!({
                        "ok": true,
                        "status": "degraded",
                        "below_min_warm": below,
                        "uptime_s": api.started.elapsed().as_secs(),
                    }),
                )
            }
        }
        // The supervisor is gone or going. Either way this host cannot take
        // work, which is the one thing this route exists to say.
        _ => (
            StatusCode::SERVICE_UNAVAILABLE,
            serde_json::json!({
                "ok": false,
                "status": "stopping",
                "uptime_s": api.started.elapsed().as_secs(),
            }),
        ),
    };

    *api.health.lock().expect("health") = Some((Instant::now(), body.clone()));
    Ok(json(status, &body))
}

/// `POST /drain`: stop admitting, finish what is running, then exit.
///
/// Answers *before* the process leaves, so a deploy script gets the count of
/// what was still in flight rather than a closed connection. `in_flight: 0` is
/// a clean drain; anything else is the grace running out, which is the
/// difference between "drained" and "gave up".
pub(super) async fn drain(api: &Arc<Api>, grace_ms: u64) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::Drain { grace_ms })?)).await?;
    let response = reply_to_response(reply);

    // The API goes too, once this answer is on the wire. A moment, not
    // immediately: `hyper` has to write the body first, and a process that
    // exited inside its own handler would answer nothing.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        std::process::exit(0);
    });
    Ok(response)
}

/// One reading of every number this process exports — shared by `/metrics`
/// and the OTLP push, so the two can never disagree.
pub(super) async fn snapshot(api: &Arc<Api>) -> anyhow::Result<crate::cmd::otlp::Snapshot> {
    let reply = control(api, |c| Ok(c.send(&Control::List)?)).await?;
    let functions = match reply {
        Reply::Functions { functions } => functions,
        Reply::Error { code, message } => anyhow::bail!("{}: {message}", code.as_str()),
        other => anyhow::bail!("unexpected answer to `list`: {other:?}"),
    };
    Ok(crate::cmd::otlp::Snapshot {
        api_requests: api.requests.load(Ordering::Relaxed),
        api_errors: api.errors.load(Ordering::Relaxed),
        functions,
        tenants: api.usage.lock().expect("usage").snapshot(),
    })
}

/// Prometheus text exposition, from what the supervisor reports.
///
/// Scoped the way `GET /fn` is: a tenant token gets the series for its own
/// functions, plus the two process-wide counters, which carry no names. The
/// operator gets the host. Until this was filtered, any valid token could
/// read every function name on the host here — a listing this API refuses
/// on `/fn`, handed out on `/metrics`.
pub(super) async fn metrics(
    api: &Arc<Api>,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let snapshot = snapshot(api).await?;
    let out = render_metrics(&snapshot, tenant.as_deref());
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
        .body(whole(Bytes::from(out)))
        .expect("a valid response"))
}

/// The exposition itself, from one reading, for one caller.
///
/// Separate from the handler so a test can render it without a supervisor:
/// what is worth testing is which series a token may see, and that is a
/// question about the token, not about the socket. The filter is [`mine`],
/// the same one the listing uses, so the two cannot disagree about whose a
/// function is.
fn render_metrics(snapshot: &crate::cmd::otlp::Snapshot, tenant: Option<&str>) -> String {
    let functions = mine(snapshot.functions.clone(), tenant);

    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "# HELP zygo_api_requests_total HTTP requests received."
    );
    let _ = writeln!(out, "# TYPE zygo_api_requests_total counter");
    let _ = writeln!(out, "zygo_api_requests_total {}", snapshot.api_requests);
    let _ = writeln!(
        out,
        "# HELP zygo_api_errors_total HTTP requests answered with an error."
    );
    let _ = writeln!(out, "# TYPE zygo_api_errors_total counter");
    let _ = writeln!(out, "zygo_api_errors_total {}", snapshot.api_errors);
    let _ = writeln!(
        out,
        "# HELP zygo_function_requests_total Requests served per function."
    );
    let _ = writeln!(out, "# TYPE zygo_function_requests_total counter");
    for f in &functions {
        let _ = writeln!(
            out,
            "zygo_function_requests_total{{fn=\"{}\"}} {}",
            f.name, f.requests
        );
    }
    let _ = writeln!(
        out,
        "# HELP zygo_function_failures_total Requests that failed per function."
    );
    let _ = writeln!(out, "# TYPE zygo_function_failures_total counter");
    for f in &functions {
        let _ = writeln!(
            out,
            "zygo_function_failures_total{{fn=\"{}\"}} {}",
            f.name, f.failures
        );
    }
    let _ = writeln!(
        out,
        "# HELP zygo_function_rss_bytes Resident memory of the warm zygote."
    );
    let _ = writeln!(out, "# TYPE zygo_function_rss_bytes gauge");
    for f in &functions {
        let _ = writeln!(
            out,
            "zygo_function_rss_bytes{{fn=\"{}\"}} {}",
            f.name,
            f.rss_kb * 1024
        );
    }
    let _ = writeln!(
        out,
        "# HELP zygo_function_state Current state, one series per function set to 1."
    );
    let _ = writeln!(out, "# TYPE zygo_function_state gauge");
    for f in &functions {
        let _ = writeln!(
            out,
            "zygo_function_state{{fn=\"{}\",state=\"{}\"}} 1",
            f.name,
            f.state.as_str()
        );
    }
    out
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

    /// `/metrics` is scoped the same way: a series is named after a
    /// function, and whose function that is, is a fact about a customer.
    #[test]
    fn metrics_show_one_tenant_their_own_series_only() {
        let snapshot = crate::cmd::otlp::Snapshot {
            api_requests: 7,
            api_errors: 2,
            functions: vec![
                status("resize", "acme"),
                status("resize-2", "globex"),
                status("internal", "default"),
            ],
            tenants: Vec::new(),
        };

        let operator = render_metrics(&snapshot, None);
        for name in ["resize", "resize-2", "internal"] {
            assert!(
                operator.contains(&format!(
                    "zygo_function_requests_total{{fn=\"{name}\"}} 3\n"
                )),
                "the operator sees the host: {operator}"
            );
        }

        let acme = render_metrics(&snapshot, Some("acme"));
        // The process-wide counters carry no names, so they are everybody's.
        assert!(acme.contains("zygo_api_requests_total 7\n"), "{acme}");
        assert!(acme.contains("zygo_api_errors_total 2\n"), "{acme}");
        // Their own, in every per-function family.
        assert!(acme.contains("zygo_function_requests_total{fn=\"resize\"} 3\n"));
        assert!(acme.contains("zygo_function_failures_total{fn=\"resize\"} 1\n"));
        assert!(acme.contains("zygo_function_rss_bytes{fn=\"resize\"} 0\n"));
        assert!(acme.contains("zygo_function_state{fn=\"resize\",state=\"warm\"} 1\n"));
        // And no other customer's name, nor the operator's own functions.
        assert!(
            !acme.contains("resize-2") && !acme.contains("internal"),
            "a tenant token must not learn another tenant's names: {acme}"
        );
        // The `HELP`/`TYPE` lines stay even when a family is empty, so a
        // scrape of a tenant with nothing warm still parses.
        assert!(acme.contains("# TYPE zygo_function_state gauge\n"));
    }
}
