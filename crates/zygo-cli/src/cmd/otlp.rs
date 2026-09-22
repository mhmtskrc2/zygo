//! OTLP metrics export from `zygo api` (design doc §3.12).
//!
//! OTLP/HTTP with the protocol's JSON encoding, pushed to a collector on an
//! interval. JSON rather than protobuf because the protocol defines both as
//! stable and the JSON one costs nothing this binary does not already carry:
//! no `prost`, no `tonic`, no code generation — `serde_json` and the HTTP
//! client the registry pull already links. Every collector that speaks
//! OTLP/HTTP accepts it on the same port as the protobuf form (`4318`).
//!
//! What is exported is exactly what `/metrics` exposes, from the same
//! snapshot, so the two views cannot disagree. Metrics only: the request
//! spans the design also names need a trace context on the request path,
//! which is a protocol change and is not made here.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use zygo_core::pool::Status;

/// The path the metrics service lives at under an OTLP/HTTP endpoint.
const METRICS_PATH: &str = "/v1/metrics";

/// `AGGREGATION_TEMPORALITY_CUMULATIVE`: every data point is a total since
/// `startTimeUnixNano`, which is what the counters here are.
const CUMULATIVE: u32 = 2;

/// Where and how often to push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exporter {
    /// The full metrics URL, `…/v1/metrics`.
    pub url: String,
    /// Extra request headers — an auth token, usually.
    pub headers: Vec<(String, String)>,
    pub interval: Duration,
}

impl Exporter {
    /// From the collector's base URL (the OTLP convention: `/v1/metrics` is
    /// appended unless it is already there) and the interval.
    pub fn new(endpoint: &str, interval: Duration) -> anyhow::Result<Exporter> {
        let trimmed = endpoint.trim().trim_end_matches('/');
        anyhow::ensure!(
            trimmed.starts_with("http://") || trimmed.starts_with("https://"),
            "the OTLP endpoint must be an http:// or https:// URL, not `{endpoint}`"
        );
        anyhow::ensure!(
            interval >= Duration::from_secs(1),
            "the OTLP export interval must be at least one second"
        );
        let url = if trimmed.ends_with(METRICS_PATH) {
            trimmed.to_string()
        } else {
            format!("{trimmed}{METRICS_PATH}")
        };
        Ok(Exporter {
            url,
            headers: headers_from_env(std::env::var("OTEL_EXPORTER_OTLP_HEADERS").ok()),
            interval,
        })
    }
}

/// `OTEL_EXPORTER_OTLP_HEADERS`: `key=value` pairs separated by commas, as the
/// OpenTelemetry environment specification defines it.
pub fn headers_from_env(value: Option<String>) -> Vec<(String, String)> {
    value
        .unwrap_or_default()
        .split(',')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            let (k, v) = (k.trim(), v.trim());
            (!k.is_empty()).then(|| (k.to_string(), v.to_string()))
        })
        .collect()
}

/// One reading of everything the exporter reports — the same numbers
/// `/metrics` renders as Prometheus text.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub api_requests: u64,
    pub api_errors: u64,
    pub functions: Vec<Status>,
}

/// The `ExportMetricsServiceRequest`, in OTLP's JSON mapping.
///
/// The mapping is protobuf's canonical JSON: 64-bit integers are strings,
/// enums may be numbers, field names are lowerCamelCase. `started` is when
/// this API process began, which is where every cumulative counter starts.
pub fn payload(snapshot: &Snapshot, started: SystemTime, now: SystemTime) -> serde_json::Value {
    let nanos = |t: SystemTime| {
        t.duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_string()
    };
    let start = nanos(started);
    let time = nanos(now);
    let attr = |key: &str, value: &str| serde_json::json!({ "key": key, "value": { "stringValue": value } });

    let sum_point = |value: u64, attributes: Vec<serde_json::Value>| {
        serde_json::json!({
            "startTimeUnixNano": start,
            "timeUnixNano": time,
            "asInt": value.to_string(),
            "attributes": attributes,
        })
    };
    let gauge_point = |value: u64, attributes: Vec<serde_json::Value>| {
        serde_json::json!({
            "timeUnixNano": time,
            "asInt": value.to_string(),
            "attributes": attributes,
        })
    };
    let sum = |name: &str, description: &str, unit: &str, points: Vec<serde_json::Value>| {
        serde_json::json!({
            "name": name,
            "description": description,
            "unit": unit,
            "sum": {
                "aggregationTemporality": CUMULATIVE,
                "isMonotonic": true,
                "dataPoints": points,
            },
        })
    };
    let gauge = |name: &str, description: &str, unit: &str, points: Vec<serde_json::Value>| {
        serde_json::json!({
            "name": name,
            "description": description,
            "unit": unit,
            "gauge": { "dataPoints": points },
        })
    };

    let per_fn = |f: &Status| vec![attr("fn", &f.name)];
    let functions = &snapshot.functions;
    let metrics = vec![
        sum(
            "zygo.api.requests",
            "HTTP requests received.",
            "{request}",
            vec![sum_point(snapshot.api_requests, vec![])],
        ),
        sum(
            "zygo.api.errors",
            "HTTP requests answered with an error.",
            "{request}",
            vec![sum_point(snapshot.api_errors, vec![])],
        ),
        sum(
            "zygo.function.requests",
            "Requests served per function.",
            "{request}",
            functions
                .iter()
                .map(|f| sum_point(f.requests, per_fn(f)))
                .collect(),
        ),
        sum(
            "zygo.function.failures",
            "Requests that failed per function.",
            "{request}",
            functions
                .iter()
                .map(|f| sum_point(f.failures, per_fn(f)))
                .collect(),
        ),
        gauge(
            "zygo.function.rss",
            "Resident memory of the warm zygote.",
            "By",
            functions
                .iter()
                .map(|f| gauge_point(f.rss_kb * 1024, per_fn(f)))
                .collect(),
        ),
        gauge(
            "zygo.function.state",
            "Current state, one point per function set to 1.",
            "1",
            functions
                .iter()
                .map(|f| {
                    let mut attributes = per_fn(f);
                    attributes.push(attr("state", f.state.as_str()));
                    gauge_point(1, attributes)
                })
                .collect(),
        ),
    ];

    serde_json::json!({
        "resourceMetrics": [{
            "resource": {
                "attributes": [
                    attr("service.name", "zygo"),
                    attr("service.version", env!("CARGO_PKG_VERSION")),
                ],
            },
            "scopeMetrics": [{
                "scope": { "name": "zygo", "version": env!("CARGO_PKG_VERSION") },
                "metrics": metrics,
            }],
        }],
    })
}

/// Send one payload. A non-2xx answer is an error carrying the status and
/// the start of the body, which is where a collector says what it disliked.
pub async fn post(
    client: &reqwest::Client,
    exporter: &Exporter,
    body: &serde_json::Value,
) -> anyhow::Result<()> {
    let mut request = client
        .post(&exporter.url)
        .header("content-type", "application/json")
        .json(body);
    for (k, v) in &exporter.headers {
        request = request.header(k.as_str(), v.as_str());
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("cannot reach the OTLP collector at {}", exporter.url))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        let text: String = text.chars().take(200).collect();
        anyhow::bail!("the OTLP collector answered {status}: {text}");
    }
    Ok(())
}

/// Push on the interval until the process ends.
///
/// A collector that is down is a warning once, and a recovery is a line
/// once: the API's own log must not fill with the collector's absence.
pub async fn run<F, Fut>(exporter: Exporter, started: SystemTime, snapshot: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Snapshot>>,
{
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("otlp: cannot build an HTTP client: {e}");
            return;
        }
    };
    let failing = AtomicBool::new(false);
    let mut ticker = tokio::time::interval(exporter.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires at once; skip it so the first export carries a
    // whole interval of data rather than an empty start.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let result = match snapshot().await {
            Ok(s) => post(&client, &exporter, &payload(&s, started, SystemTime::now())).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => {
                if failing.swap(false, Ordering::Relaxed) {
                    tracing::info!("otlp: export to {} recovered", exporter.url);
                }
            }
            Err(e) => {
                if !failing.swap(true, Ordering::Relaxed) {
                    tracing::warn!("otlp: export failed, will keep trying: {e:#}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zygo_core::sandbox::SandboxState;

    fn snapshot() -> Snapshot {
        Snapshot {
            api_requests: 12,
            api_errors: 1,
            functions: vec![Status {
                tenant: "default".into(),
                name: "resize".into(),
                image: String::new(),
                state: SandboxState::Warm,
                runtime: "python/3.12".into(),
                rss_kb: 15_000,
                imports_ms: 120.0,
                requests: 9,
                failures: 1,
            }],
        }
    }

    #[test]
    fn the_endpoint_gets_the_metrics_path_exactly_once() {
        let at = |e: &str| Exporter::new(e, Duration::from_secs(60)).unwrap().url;
        assert_eq!(
            at("http://localhost:4318"),
            "http://localhost:4318/v1/metrics"
        );
        assert_eq!(at("http://c:4318/"), "http://c:4318/v1/metrics");
        assert_eq!(at("https://c/v1/metrics"), "https://c/v1/metrics");
        assert!(Exporter::new("c:4318", Duration::from_secs(60)).is_err());
        assert!(Exporter::new("http://c", Duration::from_millis(10)).is_err());
    }

    #[test]
    fn headers_follow_the_opentelemetry_environment_convention() {
        assert_eq!(
            headers_from_env(Some("authorization=Bearer x, x-tenant=a".into())),
            vec![
                ("authorization".to_string(), "Bearer x".to_string()),
                ("x-tenant".to_string(), "a".to_string()),
            ]
        );
        assert!(headers_from_env(None).is_empty());
        assert!(headers_from_env(Some("nonsense".into())).is_empty());
    }

    /// The JSON mapping's traps, each one a payload a collector rejects
    /// silently or with a 400: 64-bit integers as strings, cumulative sums
    /// that say so, a start time on every sum point.
    #[test]
    fn the_payload_is_otlp_json() {
        let started = UNIX_EPOCH + Duration::from_secs(1_000);
        let now = UNIX_EPOCH + Duration::from_secs(1_060);
        let p = payload(&snapshot(), started, now);

        let scope = &p["resourceMetrics"][0]["scopeMetrics"][0];
        assert_eq!(scope["scope"]["name"], "zygo");
        let resource = &p["resourceMetrics"][0]["resource"]["attributes"];
        assert_eq!(resource[0]["key"], "service.name");
        assert_eq!(resource[0]["value"]["stringValue"], "zygo");

        let metrics = scope["metrics"].as_array().unwrap();
        let names: Vec<&str> = metrics
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "zygo.api.requests",
                "zygo.api.errors",
                "zygo.function.requests",
                "zygo.function.failures",
                "zygo.function.rss",
                "zygo.function.state",
            ]
        );

        let requests = &metrics[0]["sum"];
        assert_eq!(requests["aggregationTemporality"], CUMULATIVE);
        assert_eq!(requests["isMonotonic"], true);
        let point = &requests["dataPoints"][0];
        assert_eq!(
            point["asInt"], "12",
            "int64 is a string in the JSON mapping"
        );
        assert_eq!(point["startTimeUnixNano"], "1000000000000");
        assert_eq!(point["timeUnixNano"], "1060000000000");

        let per_fn = &metrics[2]["sum"]["dataPoints"][0];
        assert_eq!(per_fn["asInt"], "9");
        assert_eq!(per_fn["attributes"][0]["key"], "fn");
        assert_eq!(per_fn["attributes"][0]["value"]["stringValue"], "resize");

        let rss = &metrics[4]["gauge"]["dataPoints"][0];
        assert_eq!(rss["asInt"], (15_000u64 * 1024).to_string());
        assert!(
            rss.get("startTimeUnixNano").is_none(),
            "a gauge has no start"
        );
        assert_eq!(metrics[4]["unit"], "By");

        let state = &metrics[5]["gauge"]["dataPoints"][0];
        assert_eq!(state["attributes"][1]["key"], "state");
        assert_eq!(state["attributes"][1]["value"]["stringValue"], "warm");
    }

    /// A payload that is never sent exports nothing. This receives the real
    /// request on a real socket and checks what arrived.
    #[test]
    fn the_exporter_actually_posts_to_the_collector() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let received = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            // Read until the headers are complete, then the announced body.
            let body_start = loop {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0, "the client hung up before sending a request");
                raw.extend_from_slice(&buf[..n]);
                if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i + 4;
                }
            };
            let head = String::from_utf8_lossy(&raw[..body_start]).to_string();
            let length: usize = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse().unwrap())
                })
                .expect("a content-length");
            while raw.len() < body_start + length {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0);
                raw.extend_from_slice(&buf[..n]);
            }
            let body = raw[body_start..body_start + length].to_vec();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .unwrap();
            (head, body)
        });

        let exporter = Exporter {
            url: format!("http://{addr}/v1/metrics"),
            headers: vec![("x-token".into(), "secret".into())],
            interval: Duration::from_secs(60),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(async {
                let client = reqwest::Client::new();
                let body = payload(&snapshot(), UNIX_EPOCH, SystemTime::now());
                post(&client, &exporter, &body).await
            })
            .expect("the post succeeds against a 200");

        let (head, body) = received.join().unwrap();
        let request_line = head.lines().next().unwrap();
        assert_eq!(request_line, "POST /v1/metrics HTTP/1.1");
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("content-type: application/json"), "{head}");
        assert!(lower.contains("x-token: secret"), "{head}");
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            parsed["resourceMetrics"][0]["scopeMetrics"][0]["metrics"][0]["name"],
            "zygo.api.requests"
        );
    }

    /// The failure mode a collector shows first: a status code and a reason.
    #[test]
    fn a_rejected_payload_is_an_error_that_quotes_the_collector() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 65536];
            let _ = stream.read(&mut buf);
            stream
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\ncontent-length: 17\r\nconnection: close\r\n\r\nunknown field foo",
                )
                .unwrap();
        });
        let exporter = Exporter {
            url: format!("http://{addr}/v1/metrics"),
            headers: vec![],
            interval: Duration::from_secs(60),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = runtime
            .block_on(async {
                post(
                    &reqwest::Client::new(),
                    &exporter,
                    &payload(&Snapshot::default(), UNIX_EPOCH, UNIX_EPOCH),
                )
                .await
            })
            .expect_err("a 400 is a failure");
        let text = format!("{err:#}");
        assert!(text.contains("400"), "{text}");
        assert!(text.contains("unknown field foo"), "{text}");
    }
}
