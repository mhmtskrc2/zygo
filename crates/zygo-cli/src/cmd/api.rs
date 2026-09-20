//! `zygo api` — the HTTP front door (design doc §4.6, requirement F9).
//!
//! A client of the supervisor, not a second copy of it. Every route here turns
//! into one control request over the unix socket, which is the same boundary
//! the CLI crosses; ADR-005's "one RPC boundary on the request path" holds
//! because HTTP → supervisor replaces CLI → supervisor rather than adding to
//! it. The supervisor stays the only process that owns sandboxes, and this one
//! can be restarted, moved to a unix socket, or fronted by something else
//! without a warm function noticing.
//!
//! Security posture (§3.10): bearer auth by default, `127.0.0.1` by default,
//! and an unauthenticated listener is refused on anything but a unix socket or
//! a loopback address. The token comes from `ZYGO_API_TOKEN` and nowhere else —
//! never a flag, because flags are in `ps`.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Context;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use zygo_core::pool::Outcome;
use zygo_core::spec::{ApiAuth, Spec};
use zygo_core::supervisor::client::Client;
use zygo_core::supervisor::{ControlError, Request as Control, Response as Reply};

use crate::cli::{ApiArgs, Cli};
use crate::output::Style;

/// Largest request body accepted. An event bigger than this belongs in a
/// scratch file, and the supervisor's own frame limit is the same order.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// The client's wait when it did not say. The function's own `timeout` is the
/// real limit and the supervisor enforces it; this only bounds the HTTP hold.
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Everything a request handler needs, shared across connections.
struct Api {
    paths: zygo_core::Paths,
    exe: std::path::PathBuf,
    /// `None` means no authentication — permitted only where refusing would
    /// protect nobody (see [`Listen::allows_no_auth`]).
    token: Option<String>,
    /// Idle control connections, one per request in flight at peak. The
    /// supervisor serves each on its own thread, so this is what turns
    /// concurrent HTTP requests into concurrent sandbox requests.
    clients: std::sync::Mutex<Vec<Client>>,
    started: Instant,
    requests: AtomicU64,
    errors: AtomicU64,
}

pub fn run(cli: &Cli, args: &ApiArgs) -> anyhow::Result<u8> {
    let spec = Spec::discover(args.spec_file.path())?.unwrap_or_default();
    // No `[api]` section means the documented defaults: loopback, bearer.
    let api_spec = spec.api.clone().unwrap_or_default();
    let listen = Listen::parse(args.listen.as_deref().unwrap_or(&api_spec.listen))?;

    let auth = if args.no_auth {
        ApiAuth::None
    } else {
        api_spec.auth
    };
    let token = match auth {
        ApiAuth::Bearer => Some(
            std::env::var("ZYGO_API_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
                .context(
                    "bearer auth is on and ZYGO_API_TOKEN is not set\n  \
                     → export ZYGO_API_TOKEN=<a long random string>, \
                     or pass --no-auth for a unix socket or loopback listener",
                )?,
        ),
        ApiAuth::None => {
            anyhow::ensure!(
                listen.allows_no_auth(),
                "refusing to serve without authentication on {listen}\n  \
                 → an unauthenticated API on a reachable address lets anyone on the \
                 network run code as you; listen on 127.0.0.1 or a unix socket, \
                 or set ZYGO_API_TOKEN and use bearer auth"
            );
            None
        }
    };

    let paths = super::paths(cli);
    let exe = std::env::current_exe().context("cannot find this binary to start a supervisor")?;
    // Reach the supervisor once up front, so "no supervisor and none could be
    // started" is reported before the listener is bound rather than on the
    // first request.
    let first = Client::connect_or_start(&paths, &exe)?;

    let api = Arc::new(Api {
        paths,
        exe,
        token,
        clients: std::sync::Mutex::new(vec![first]),
        started: Instant::now(),
        requests: AtomicU64::new(0),
        errors: AtomicU64::new(0),
    });

    // Validated before the listener is bound: a bad collector URL is a
    // start-up error, not the first warning sixty seconds in.
    let exporter = match &args.otlp_endpoint {
        Some(endpoint) => Some(super::otlp::Exporter::new(endpoint, args.otlp_interval.0)?),
        None => None,
    };

    let style = Style::stderr();
    eprintln!(
        "{} {listen}  {}",
        style.dim("api"),
        style.dim(if api.token.is_some() {
            "bearer auth"
        } else {
            "no auth"
        })
    );
    if let Some(exporter) = &exporter {
        eprintln!(
            "{} {}  every {:?}",
            style.dim("otlp"),
            exporter.url,
            exporter.interval
        );
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if let Some(exporter) = exporter {
        let started = std::time::SystemTime::now();
        let for_export = Arc::clone(&api);
        runtime.spawn(super::otlp::run(exporter, started, move || {
            let api = Arc::clone(&for_export);
            async move { snapshot(&api).await }
        }));
    }
    runtime.block_on(serve(listen, api))?;
    Ok(0)
}

/// Where to listen, parsed from `HOST:PORT` or `unix://PATH`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Listen {
    Tcp(std::net::SocketAddr),
    Unix(std::path::PathBuf),
}

impl Listen {
    fn parse(text: &str) -> anyhow::Result<Listen> {
        if let Some(path) = text.strip_prefix("unix://") {
            anyhow::ensure!(!path.is_empty(), "unix:// needs a path");
            return Ok(Listen::Unix(path.into()));
        }
        let addr = text
            .parse()
            .with_context(|| format!("`{text}` is not HOST:PORT or unix://PATH"))?;
        Ok(Listen::Tcp(addr))
    }

    /// Whether serving unauthenticated here exposes anything.
    ///
    /// A unix socket is `0600` in a `0700` directory — the same protection the
    /// control socket has — and loopback is reachable only from this host.
    /// Anything else is a network interface, and "no auth" there means anyone
    /// who can route to it can run code as this user.
    fn allows_no_auth(&self) -> bool {
        match self {
            Listen::Unix(_) => true,
            Listen::Tcp(addr) => addr.ip().is_loopback(),
        }
    }
}

impl std::fmt::Display for Listen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Listen::Tcp(addr) => write!(f, "http://{addr}"),
            Listen::Unix(path) => write!(f, "unix://{}", path.display()),
        }
    }
}

async fn serve(listen: Listen, api: Arc<Api>) -> anyhow::Result<()> {
    match listen {
        Listen::Tcp(addr) => {
            let listener = tokio::net::TcpListener::bind(addr)
                .await
                .with_context(|| format!("cannot listen on {addr}"))?;
            loop {
                let (stream, _) = listener.accept().await?;
                spawn_connection(stream, Arc::clone(&api));
            }
        }
        Listen::Unix(path) => {
            // A path left by a previous run is a file nobody is listening on;
            // the socket that matters is the one about to be bound.
            let _ = std::fs::remove_file(&path);
            let listener = tokio::net::UnixListener::bind(&path)
                .with_context(|| format!("cannot listen on {}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            }
            loop {
                let (stream, _) = listener.accept().await?;
                spawn_connection(stream, Arc::clone(&api));
            }
        }
    }
}

fn spawn_connection<S>(stream: S, api: Arc<Api>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let io = TokioIo::new(stream);
        let service = service_fn(move |req| handle(req, Arc::clone(&api)));
        if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
            tracing::debug!("connection ended: {e}");
        }
    });
}

/// One HTTP request → one answer. Never fails the connection: every problem is
/// a status code with a JSON body, because the caller is a webhook or a script
/// and a dropped connection tells it nothing.
async fn handle(
    req: Request<Incoming>,
    api: Arc<Api>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    api.requests.fetch_add(1, Ordering::Relaxed);
    let response = match route(req, &api).await {
        Ok(response) => response,
        Err(e) => {
            api.errors.fetch_add(1, Ordering::Relaxed);
            e.into_response()
        }
    };
    if response.status().is_server_error() {
        api.errors.fetch_add(1, Ordering::Relaxed);
    }
    Ok(response)
}

/// A failure that already knows its status code.
#[derive(Debug)]
struct HttpError {
    status: StatusCode,
    body: serde_json::Value,
}

impl HttpError {
    fn new(status: StatusCode, message: impl std::fmt::Display) -> HttpError {
        HttpError {
            status,
            body: serde_json::json!({ "error": message.to_string() }),
        }
    }

    fn into_response(self) -> Response<Full<Bytes>> {
        json(self.status, &self.body)
    }
}

impl From<anyhow::Error> for HttpError {
    fn from(e: anyhow::Error) -> HttpError {
        HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

async fn route(req: Request<Incoming>, api: &Arc<Api>) -> Result<Response<Full<Bytes>>, HttpError> {
    // `/healthz` is deliberately unauthenticated: a load balancer probing it
    // has no business holding the token, and it reveals nothing but "up".
    if req.method() == Method::GET && req.uri().path() == "/healthz" {
        return Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "ok": true, "uptime_s": api.started.elapsed().as_secs() }),
        ));
    }

    authorise(&req, api)?;

    let path = req.uri().path().to_string();
    let segments: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    match (req.method(), segments.as_slice()) {
        (&Method::GET, ["fn"]) => list(api).await,
        (&Method::GET, ["metrics"]) => metrics(api).await,
        (&Method::POST, ["fn", name]) => {
            let name = name.to_string();
            let timeout_ms = timeout_header(&req)?;
            let body = read_body(req).await?;
            let event = parse_event(&body)?;
            exec(api, name, event, timeout_ms).await
        }
        (&Method::POST, ["fn", name, "batch"]) => {
            let name = name.to_string();
            let timeout_ms = timeout_header(&req)?;
            let body = read_body(req).await?;
            let events: Vec<serde_json::Value> = serde_json::from_slice(&body).map_err(|e| {
                HttpError::new(
                    StatusCode::BAD_REQUEST,
                    format!("body must be a JSON array of events: {e}"),
                )
            })?;
            batch(api, name, events, timeout_ms).await
        }
        (&Method::GET, ["fn", name, "stats"]) => stats(api, name.to_string()).await,
        (&Method::POST, ["fn", name, "warm"]) => warm(api, name.to_string()).await,
        (_, ["fn", ..]) | (_, ["metrics"]) => Err(HttpError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            format!("{} {}", req.method(), path),
        )),
        _ => Err(HttpError::new(
            StatusCode::NOT_FOUND,
            format!("no route {path}"),
        )),
    }
}

/// Bearer auth, when it is on.
///
/// The comparison does not stop at the first differing byte. On loopback the
/// timing side channel is academic, but the API is allowed on other addresses
/// with a token, and there it is not.
fn authorise(req: &Request<Incoming>, api: &Api) -> Result<(), HttpError> {
    let Some(expected) = &api.token else {
        return Ok(());
    };
    let presented = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    match presented {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => Ok(()),
        _ => Err(HttpError::new(
            StatusCode::UNAUTHORIZED,
            "missing or wrong bearer token",
        )),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn timeout_header(req: &Request<Incoming>) -> Result<u64, HttpError> {
    match req.headers().get("x-zygo-timeout-ms") {
        None => Ok(DEFAULT_TIMEOUT_MS),
        Some(v) => v
            .to_str()
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .ok_or_else(|| {
                HttpError::new(
                    StatusCode::BAD_REQUEST,
                    "X-Zygo-Timeout-Ms must be a positive integer",
                )
            }),
    }
}

async fn read_body(req: Request<Incoming>) -> Result<Bytes, HttpError> {
    Limited::new(req.into_body(), MAX_BODY_BYTES)
        .collect()
        .await
        .map(|c| c.to_bytes())
        .map_err(|e| {
            HttpError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("body over {MAX_BODY_BYTES} bytes or unreadable: {e}"),
            )
        })
}

/// An empty body is `null`, as `zygo exec` treats an empty argument: how you
/// call a handler that takes nothing.
fn parse_event(body: &[u8]) -> Result<serde_json::Value, HttpError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(body)
        .map_err(|e| HttpError::new(StatusCode::BAD_REQUEST, format!("body is not JSON: {e}")))
}

/// Run one control request on a pooled connection, off the async runtime.
///
/// The client is blocking on purpose — it is the same code the CLI uses — so
/// it runs on a blocking thread. A connection is returned to the pool only
/// after a successful exchange: one that errored may be mid-frame, and the
/// next request must not inherit that.
async fn control<T, F>(api: &Arc<Api>, f: F) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&mut Client) -> anyhow::Result<T> + Send + 'static,
{
    let api = Arc::clone(api);
    tokio::task::spawn_blocking(move || {
        let mut client = match api.clients.lock().expect("clients").pop() {
            Some(client) => client,
            None => Client::connect_or_start(&api.paths, &api.exe)?,
        };
        let result = f(&mut client);
        if result.is_ok() {
            api.clients.lock().expect("clients").push(client);
        }
        result
    })
    .await
    .context("the control request panicked")?
}

async fn exec(
    api: &Arc<Api>,
    name: String,
    event: serde_json::Value,
    timeout_ms: u64,
) -> Result<Response<Full<Bytes>>, HttpError> {
    let reply = control(api, move |c| {
        Ok(c.send(&Control::Exec {
            name,
            event,
            timeout_ms,
        })?)
    })
    .await?;
    Ok(reply_to_response(reply))
}

/// `POST /fn/<name>/batch`: every event at once, answers in order.
///
/// Each element carries its own status, because one event being refused with
/// a 429 or failing in its handler must not hide the answers to the others —
/// and must not be hidden by them.
async fn batch(
    api: &Arc<Api>,
    name: String,
    events: Vec<serde_json::Value>,
    timeout_ms: u64,
) -> Result<Response<Full<Bytes>>, HttpError> {
    let calls = events.into_iter().map(|event| {
        let name = name.clone();
        let api = Arc::clone(api);
        async move {
            let reply = control(&api, move |c| {
                Ok(c.send(&Control::Exec {
                    name,
                    event,
                    timeout_ms,
                })?)
            })
            .await;
            match reply {
                Ok(reply) => {
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

/// `join_all` without pulling in `futures`: the batch is small and ordered.
async fn futures_join_all<F>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futures.into_iter().map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for h in handles {
        out.push(h.await.expect("a batch element panicked"));
    }
    out
}

async fn list(api: &Arc<Api>) -> Result<Response<Full<Bytes>>, HttpError> {
    let reply = control(api, |c| Ok(c.send(&Control::List)?)).await?;
    match reply {
        Reply::Functions { functions } => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "functions": functions }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

async fn stats(api: &Arc<Api>, name: String) -> Result<Response<Full<Bytes>>, HttpError> {
    let reply = control(api, |c| Ok(c.send(&Control::List)?)).await?;
    match reply {
        Reply::Functions { functions } => match functions.into_iter().find(|f| f.name == name) {
            Some(f) => Ok(json(
                StatusCode::OK,
                &serde_json::to_value(f).unwrap_or_default(),
            )),
            None => Err(HttpError::new(
                StatusCode::NOT_FOUND,
                format!("no function named `{name}`"),
            )),
        },
        other => Ok(reply_to_response(other)),
    }
}

async fn warm(api: &Arc<Api>, name: String) -> Result<Response<Full<Bytes>>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::Warm { name })?)).await?;
    Ok(reply_to_response(reply))
}

/// One reading of every number this process exports — shared by `/metrics`
/// and the OTLP push, so the two can never disagree.
async fn snapshot(api: &Arc<Api>) -> anyhow::Result<super::otlp::Snapshot> {
    let reply = control(api, |c| Ok(c.send(&Control::List)?)).await?;
    let functions = match reply {
        Reply::Functions { functions } => functions,
        Reply::Error { code, message } => anyhow::bail!("{}: {message}", code.as_str()),
        other => anyhow::bail!("unexpected answer to `list`: {other:?}"),
    };
    Ok(super::otlp::Snapshot {
        api_requests: api.requests.load(Ordering::Relaxed),
        api_errors: api.errors.load(Ordering::Relaxed),
        functions,
    })
}

/// Prometheus text exposition, from what the supervisor reports.
async fn metrics(api: &Arc<Api>) -> Result<Response<Full<Bytes>>, HttpError> {
    let snapshot = snapshot(api).await?;
    let functions = &snapshot.functions;

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
    for f in functions {
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
    for f in functions {
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
    for f in functions {
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
    for f in functions {
        let _ = writeln!(
            out,
            "zygo_function_state{{fn=\"{}\",state=\"{}\"}} 1",
            f.name,
            f.state.as_str()
        );
    }

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/plain; version=0.0.4; charset=utf-8")
        .body(Full::new(Bytes::from(out)))
        .expect("a valid response"))
}

/// Map a control reply to an HTTP status and JSON body (§4.6).
fn reply_to_json(reply: Reply) -> (StatusCode, serde_json::Value) {
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
        Reply::Error { code, message } => {
            let status = match code {
                ControlError::NotFound => StatusCode::NOT_FOUND,
                ControlError::BadSpec => StatusCode::BAD_REQUEST,
                ControlError::WarmFailed => StatusCode::SERVICE_UNAVAILABLE,
                ControlError::Unauthorised => StatusCode::FORBIDDEN,
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

fn reply_to_response(reply: Reply) -> Response<Full<Bytes>> {
    let (status, body) = reply_to_json(reply);
    let mut response = json(status, &body);
    if status == StatusCode::TOO_MANY_REQUESTS {
        response
            .headers_mut()
            .insert("retry-after", hyper::header::HeaderValue::from_static("1"));
    }
    response
}

/// The design document's `POST /fn/<name>` answers: 200 with the result and
/// metrics; 408 when the deadline killed it; 500 when the handler raised.
fn outcome_to_json(outcome: Outcome) -> (StatusCode, serde_json::Value) {
    let metrics = serde_json::json!({
        "wall_ms": outcome.metrics.wall_ms,
        "cpu_ms": outcome.metrics.cpu_ms,
        "peak_rss_kb": outcome.metrics.peak_rss_kb,
    });
    if outcome.timed_out {
        return (
            StatusCode::REQUEST_TIMEOUT,
            serde_json::json!({
                "error": "the request exceeded the function's timeout and was killed",
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
                "stdout": outcome.stdout,
                "stderr": outcome.stderr,
                "exit_code": outcome.exit_code,
                "metrics": metrics,
            }),
        );
    }
    (
        StatusCode::OK,
        serde_json::json!({
            "result": outcome.result,
            "stdout": outcome.stdout,
            "stderr": outcome.stderr,
            "metrics": metrics,
        }),
    )
}

fn json(status: StatusCode, body: &serde_json::Value) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(
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
        // §4.6: 200, 408, 429, 500 — and each distinguishable from the body.
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

    #[test]
    fn listen_addresses_parse_both_forms() {
        assert_eq!(
            Listen::parse("127.0.0.1:7700").unwrap(),
            Listen::Tcp("127.0.0.1:7700".parse().unwrap())
        );
        assert_eq!(
            Listen::parse("unix:///tmp/api.sock").unwrap(),
            Listen::Unix("/tmp/api.sock".into())
        );
        assert!(Listen::parse("unix://").is_err());
        assert!(Listen::parse("not an address").is_err());
    }

    #[test]
    fn no_auth_is_only_allowed_where_it_exposes_nothing() {
        // P6: an unauthenticated API on a reachable address is code execution
        // for anyone on the network. Loopback and a 0600 unix socket are the
        // only places refusing would protect nobody.
        assert!(Listen::parse("127.0.0.1:7700").unwrap().allows_no_auth());
        assert!(Listen::parse("[::1]:7700").unwrap().allows_no_auth());
        assert!(
            Listen::parse("unix:///tmp/api.sock")
                .unwrap()
                .allows_no_auth()
        );
        assert!(!Listen::parse("0.0.0.0:7700").unwrap().allows_no_auth());
        assert!(!Listen::parse("10.0.0.5:7700").unwrap().allows_no_auth());
        assert!(!Listen::parse("[::]:7700").unwrap().allows_no_auth());
    }

    #[test]
    fn token_comparison_does_not_depend_on_where_the_difference_is() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrex"));
        assert!(!constant_time_eq(b"secret", b"xecret"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn an_empty_body_is_a_null_event() {
        assert_eq!(parse_event(b"").unwrap(), serde_json::Value::Null);
        assert_eq!(parse_event(b"  \n").unwrap(), serde_json::Value::Null);
        assert_eq!(parse_event(br#"{"n":1}"#).unwrap()["n"], 1);
        assert!(parse_event(b"{nope").is_err());
    }
}
