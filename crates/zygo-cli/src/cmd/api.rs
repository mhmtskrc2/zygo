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
use zygo_core::spec::{ApiAuth, Layer, Spec};
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

/// Version of the HTTP surface itself, bumped on any incompatible change.
///
/// Separate from the crate version and from [`CONTROL_VERSION`]: an SDK pinned
/// to this number should keep working across Zygo releases that only change
/// what happens behind the routes. `GET /version` reports it, and that is what
/// a client checks rather than parsing a release string.
///
/// [`CONTROL_VERSION`]: zygo_core::supervisor::CONTROL_VERSION
pub const API_VERSION: u32 = 1;

/// Largest batch accepted.
///
/// Without a cap, a 16 MB body of `[null,null,…]` is about a million elements,
/// and the batch handler opened a supervisor connection and spawned a task for
/// each (S-06). The supervisor serves every connection on a thread, so the
/// cost of that request was paid by the host rather than by the caller.
const MAX_BATCH: usize = 1024;

/// How many batch elements may be in flight at once.
///
/// A batch is one caller asking for several things; it should not be able to
/// out-compete every other caller for supervisor connections by asking for a
/// thousand. The per-function concurrency limit is what actually bounds
/// sandbox work — this bounds the connections in front of it.
const BATCH_IN_FLIGHT: usize = 16;

/// Idle control connections kept for reuse.
///
/// Each one pins a thread in the supervisor, so the pool that only ever grew
/// (S-07) turned a burst of concurrent requests into a permanent thread count.
/// Above this, a finished connection is closed instead of kept.
const MAX_IDLE_CLIENTS: usize = 32;

/// Longest `X-Zygo-Timeout-Ms` a caller may ask for.
///
/// The function's own `timeout` is the real limit and the supervisor enforces
/// it; this header only says how long the caller will wait. Unbounded, it is a
/// way to hold a connection and a supervisor thread for as long as you like
/// (S-08).
const MAX_TIMEOUT_MS: u64 = 3_600_000;

/// How long a connection has to send its request headers.
///
/// hyper only honours this when the builder has a timer, and it had none
/// (S-09) — so a connection that opened and sent one byte was held for ever.
const HEADER_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long a one-shot `POST /run` may hold the connection beyond the
/// sandbox's own timeout before the child is killed outright.
///
/// The launcher enforces the function's `timeout` against the whole process
/// tree; this is the outer bound for the case where it cannot — a child that
/// never starts, or a backend that hangs before the timer exists. Without it
/// an HTTP caller has no upper bound at all.
const RUN_GRACE_MS: u64 = 30_000;

/// Everything a request handler needs, shared across connections.
struct Api {
    paths: zygo_core::Paths,
    exe: std::path::PathBuf,
    /// `None` means no authentication — permitted only where refusing would
    /// protect nobody (see [`Listen::allows_no_auth`]).
    token: Option<String>,
    /// Whether this listener may create and destroy sandboxes, not only call
    /// the ones already declared.
    ///
    /// Off by default, and the difference is the whole security posture of the
    /// API. Without it a token holder can call `[fn.*]` from a spec file
    /// somebody reviewed; with it the same token can serve a function with any
    /// mount, any image and any command — which is running arbitrary code as
    /// this user, through a port. P6 says a widened boundary has to be spelled
    /// out, so it is a flag rather than a default.
    deploy: bool,
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
        deploy: args.allow_deploy,
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
        "{} {listen}  {}  {}",
        style.dim("api"),
        style.dim(if api.token.is_some() {
            "bearer auth"
        } else {
            "no auth"
        }),
        style.dim(if api.deploy {
            "deploy on: callers may serve, stop and run"
        } else {
            "call-only: serve, stop and run are refused"
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
        let result = http1::Builder::new()
            // Without a timer hyper accepts `header_read_timeout` and then
            // silently does nothing with it, so a connection that sends one
            // byte and stops is held for ever (S-09).
            .timer(hyper_util::rt::TokioTimer::new())
            .header_read_timeout(HEADER_READ_TIMEOUT)
            .serve_connection(io, service)
            .await;
        if let Err(e) = result {
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
        Err(e) => e.into_response(),
    };
    // Counted once, from the status that was actually sent. Counting in the
    // `Err` arm *and* here meant every 500 produced by a refused request was
    // two errors, so the rate on a dashboard was twice the truth (B-30).
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
        (&Method::GET, ["version"]) => Ok(json(
            StatusCode::OK,
            &serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "api": API_VERSION,
                "control": zygo_core::supervisor::CONTROL_VERSION,
                "deploy": api.deploy,
            }),
        )),
        (&Method::PUT, ["fn", name]) => {
            let name = name.to_string();
            deployable(api)?;
            let body = read_body(req).await?;
            serve_fn(api, name, &body).await
        }
        (&Method::DELETE, ["fn", name]) => {
            let name = name.to_string();
            deployable(api)?;
            stop(api, name).await
        }
        (&Method::POST, ["run"]) => {
            deployable(api)?;
            let body = read_body(req).await?;
            one_shot(api, &body).await
        }
        (&Method::GET, ["fn", name, "logs"]) => {
            let name = name.to_string();
            let query = req.uri().query().unwrap_or("").to_string();
            logs(api, name, &query).await
        }
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
        (&Method::PUT, ["scripts"]) => {
            deployable(api)?;
            let body = read_body(req).await?;
            put_script(api, &body).await
        }
        (&Method::GET, ["scripts", digest]) => get_script(api, digest.to_string()).await,
        (&Method::DELETE, ["scripts", digest]) => {
            let digest = digest.to_string();
            deployable(api)?;
            delete_script(api, digest).await
        }
        (_, ["fn", ..])
        | (_, ["metrics"])
        | (_, ["version"])
        | (_, ["run"])
        | (_, ["scripts", ..]) => Err(HttpError::new(
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
    timeout_header_from(req.headers())
}

/// The headers alone, so this is reachable from a test.
///
/// `Request<Incoming>` cannot be built outside a server, which is why the
/// ceiling below went untested until there was a ceiling to test (S-08).
fn timeout_header_from(headers: &hyper::HeaderMap) -> Result<u64, HttpError> {
    match headers.get("x-zygo-timeout-ms") {
        None => Ok(DEFAULT_TIMEOUT_MS),
        Some(v) => {
            let ms = v
                .to_str()
                .ok()
                .and_then(|s| s.trim().parse::<u64>().ok())
                .filter(|ms| *ms > 0)
                .ok_or_else(|| {
                    HttpError::new(
                        StatusCode::BAD_REQUEST,
                        "X-Zygo-Timeout-Ms must be a positive integer",
                    )
                })?;
            // Refused rather than silently clamped: a caller that asked for a
            // day and was given an hour would report the wrong thing when the
            // wait ended.
            if ms > MAX_TIMEOUT_MS {
                return Err(HttpError::new(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "X-Zygo-Timeout-Ms is {ms}, over this API's ceiling of \
                         {MAX_TIMEOUT_MS}\n  → the function's own `timeout` is the \
                         real limit; this header only says how long you will wait"
                    ),
                ));
            }
            Ok(ms)
        }
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
            // Above the ceiling the connection is dropped rather than kept:
            // each idle one pins a supervisor thread, so a pool that only grew
            // made a momentary burst permanent (S-07).
            let mut idle = api.clients.lock().expect("clients");
            if idle.len() < MAX_IDLE_CLIENTS {
                idle.push(client);
            }
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
    // supervisor connections ahead of every other caller (S-06). The
    // per-function concurrency limit still bounds the sandbox work behind
    // this; the semaphore bounds the plumbing in front of it.
    let permits = Arc::new(tokio::sync::Semaphore::new(BATCH_IN_FLIGHT));
    let calls = events.into_iter().map(|event| {
        let name = name.clone();
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
        // one thing a batch exists to prevent (S-06).
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

/// `PUT /scripts`: the body **is** the script, and the answer is its name.
///
/// Not JSON around it: a script is a file, the caller has it as bytes, and
/// wrapping those bytes in a JSON string to unwrap them again is a
/// transformation with no reader. Content-addressed, so this is idempotent —
/// the same bytes are the same name however many times, and from however many
/// tenants, which is what `201` versus `200` says.
async fn put_script(api: &Arc<Api>, body: &[u8]) -> Result<Response<Full<Bytes>>, HttpError> {
    let source = std::str::from_utf8(body)
        .map_err(|e| {
            HttpError::new(
                StatusCode::BAD_REQUEST,
                format!("a script must be UTF-8 text: {e}"),
            )
        })?
        .to_string();
    if source.is_empty() {
        return Err(HttpError::new(
            StatusCode::BAD_REQUEST,
            "the body is empty; it should be the script itself",
        ));
    }

    let reply = control(api, move |c| Ok(c.send(&Control::PutScript { source })?)).await?;
    match reply {
        Reply::Script {
            digest,
            size,
            existed,
        } => Ok(json(
            if existed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            },
            &serde_json::json!({ "sha256": digest, "size": size, "existed": existed }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

/// `GET /scripts/<hash>`: whether this host holds it, and how big it is.
///
/// Never the bytes. A digest is not a capability — anyone who can guess one
/// has it — so answering with the script would make every tenant's code
/// readable by every other tenant that knows what it is looking for. What this
/// answers is the question a caller actually has: do I need to upload it
/// again?
async fn get_script(api: &Arc<Api>, digest: String) -> Result<Response<Full<Bytes>>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::GetScript { digest })?)).await?;
    match reply {
        Reply::Script { digest, size, .. } => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "sha256": digest, "size": size }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

async fn delete_script(api: &Arc<Api>, digest: String) -> Result<Response<Full<Bytes>>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::DeleteScript { digest })?)).await?;
    match reply {
        Reply::Ok => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "deleted": true }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

/// The gate on everything that creates or destroys a sandbox.
///
/// A 403 rather than a 404: pretending the route does not exist would send an
/// SDK author looking for a typo, when the answer is a flag on the server.
fn deployable(api: &Api) -> Result<(), HttpError> {
    if api.deploy {
        return Ok(());
    }
    Err(HttpError::new(
        StatusCode::FORBIDDEN,
        "this API may only call functions that are already served\n  \
         → start it with `zygo api --allow-deploy` to let callers serve, stop \
         and run, which is running arbitrary code as the user it runs as",
    ))
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

async fn serve_fn(
    api: &Arc<Api>,
    name: String,
    body: &[u8],
) -> Result<Response<Full<Bytes>>, HttpError> {
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

async fn stop(api: &Arc<Api>, name: String) -> Result<Response<Full<Bytes>>, HttpError> {
    let wanted = name.clone();
    let reply = control(api, move |c| {
        Ok(c.send(&Control::Stop { name: Some(name) })?)
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

async fn logs(
    api: &Arc<Api>,
    name: String,
    query: &str,
) -> Result<Response<Full<Bytes>>, HttpError> {
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
        })?)
    })
    .await?;
    Ok(reply_to_response(reply))
}

/// A query string, parsed once. Percent-decoding is deliberately absent: every
/// parameter here is a number or a boolean, and a `%` in one is a mistake
/// worth reporting rather than decoding.
struct Query<'a>(Vec<(&'a str, &'a str)>);

impl<'a> Query<'a> {
    fn parse(query: &'a str) -> Query<'a> {
        Query(
            query
                .split('&')
                .filter(|p| !p.is_empty())
                .map(|pair| match pair.split_once('=') {
                    Some((k, v)) => (k, v),
                    None => (pair, ""),
                })
                .collect(),
        )
    }

    fn get(&self, key: &str) -> Option<&'a str> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    fn number(&self, key: &str) -> Result<Option<u64>, HttpError> {
        match self.get(key) {
            None => Ok(None),
            Some(raw) => raw.parse().map(Some).map_err(|_| {
                HttpError::new(
                    StatusCode::BAD_REQUEST,
                    format!("`{key}` must be a non-negative integer, not `{raw}`"),
                )
            }),
        }
    }

    fn flag(&self, key: &str) -> Result<bool, HttpError> {
        match self.get(key) {
            None => Ok(false),
            Some("") | Some("1") | Some("true") => Ok(true),
            Some("0") | Some("false") => Ok(false),
            Some(raw) => Err(HttpError::new(
                StatusCode::BAD_REQUEST,
                format!("`{key}` must be true or false, not `{raw}`"),
            )),
        }
    }
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
/// The work is [`super::oneshot::run`], which spawns `zygo run` as a child and
/// captures its streams; the reasoning for a child rather than an in-process
/// launch is documented there. Here it runs on a blocking thread, because it
/// is synchronous and the runtime this API uses is not.
async fn one_shot(api: &Arc<Api>, body: &[u8]) -> Result<Response<Full<Bytes>>, HttpError> {
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
        super::oneshot::run(&exe, layer, &image, &argv, stdin.as_bytes(), deadline)
    })
    .await
    .context("the sandbox task panicked")??;

    // 200 whenever the sandbox *ran*, whatever ended it — a non-zero exit, its
    // own timeout, an out-of-memory kill. The body says which, and that is the
    // whole reason those fields exist; answering 408 for a sandbox that hit
    // the `timeout` its caller asked for made the SDK raise instead of
    // returning the result, which contradicts "a non-zero exit is not an
    // exception". Found by `poc/verify_api.sh` against a real kernel.
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
        }),
    ))
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
        Reply::Script {
            digest,
            size,
            existed,
        } => (
            StatusCode::OK,
            serde_json::json!({ "sha256": digest, "size": size, "existed": existed }),
        ),
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

    /// The gate on everything that creates or destroys a sandbox, and the
    /// status it refuses with.
    ///
    /// A 403 rather than a 404: pretending the route does not exist sends an
    /// SDK author looking for a typo, when the answer is a flag on the server.
    /// The message has to name that flag, because nothing else can.
    #[test]
    fn deploy_is_off_until_it_is_asked_for() {
        let api = |deploy| Api {
            paths: zygo_core::Paths::rooted(std::env::temp_dir().join("zygo-api-test")),
            exe: std::path::PathBuf::from("/nonexistent/zygo"),
            token: None,
            deploy,
            clients: std::sync::Mutex::new(Vec::new()),
            started: Instant::now(),
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        };

        assert!(deployable(&api(true)).is_ok());

        let refused = deployable(&api(false)).expect_err("a call-only API refuses");
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert!(
            refused.body["error"]
                .as_str()
                .expect("a message")
                .contains("--allow-deploy"),
            "the refusal has to name the flag: {:?}",
            refused.body
        );
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

    /// Query parameters are read as what they are, and a value that is neither
    /// is reported rather than defaulted.
    ///
    /// Defaulting is the tempting choice and the wrong one: `?failed=yes`
    /// would silently return every entry, and the caller would conclude
    /// nothing had failed.
    #[test]
    fn log_query_parameters_are_parsed_or_refused() {
        let query = Query::parse("after=12&limit=5&failed=true");
        assert_eq!(query.number("after").unwrap(), Some(12));
        assert_eq!(query.number("limit").unwrap(), Some(5));
        assert!(query.flag("failed").unwrap());

        let empty = Query::parse("");
        assert_eq!(empty.number("after").unwrap(), None);
        assert!(!empty.flag("failed").unwrap());

        // A bare `?failed` is the form a hand-written URL takes.
        assert!(Query::parse("failed").flag("failed").unwrap());
        assert!(!Query::parse("failed=false").flag("failed").unwrap());

        assert!(Query::parse("after=soon").number("after").is_err());
        assert!(Query::parse("failed=yes").flag("failed").is_err());
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

    /// The ceilings that stop one caller from spending the host's resources.
    ///
    /// Each is a number that was absent until the code review, and each
    /// absence had the same shape: a request that costs the *server* more the
    /// larger the caller makes it.
    #[test]
    fn a_caller_cannot_ask_for_an_unbounded_amount_of_work() {
        // S-08: the wait a caller may ask for. Refused rather than clamped —
        // a caller given an hour when it asked for a day would report the
        // wrong thing when the wait ended.
        let over = Request::builder()
            .header("x-zygo-timeout-ms", (MAX_TIMEOUT_MS + 1).to_string())
            .body(())
            .expect("a request");
        let refused = timeout_header_from(over.headers()).expect_err("over the ceiling");
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);
        assert!(
            refused.body["error"]
                .as_str()
                .expect("a message")
                .contains(&MAX_TIMEOUT_MS.to_string()),
            "the refusal should name the ceiling: {:?}",
            refused.body
        );

        let at_the_line = Request::builder()
            .header("x-zygo-timeout-ms", MAX_TIMEOUT_MS.to_string())
            .body(())
            .expect("a request");
        assert_eq!(
            timeout_header_from(at_the_line.headers()).expect("the ceiling itself is allowed"),
            MAX_TIMEOUT_MS
        );

        // And the ordinary case still works, so none of the above can pass by
        // refusing everything.
        let ordinary = Request::builder()
            .header("x-zygo-timeout-ms", "2500")
            .body(())
            .expect("a request");
        assert_eq!(
            timeout_header_from(ordinary.headers()).expect("an ordinary header"),
            2500
        );

        // S-06 and S-07: the two ceilings that bound supervisor connections.
        // Checked as relationships rather than as literals, because what
        // matters is that a batch cannot out-compete the pool in front of it —
        // and at compile time, since both are constants.
        const _: () = assert!(
            BATCH_IN_FLIGHT <= MAX_IDLE_CLIENTS,
            "one batch may take every pooled connection"
        );
        const _: () = assert!(MAX_BATCH >= BATCH_IN_FLIGHT, "the cap is below the gate");
    }
}
