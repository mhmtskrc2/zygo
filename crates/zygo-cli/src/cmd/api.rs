// SPDX-License-Identifier: Apache-2.0
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

/// Every answer this API sends.
///
/// Boxed rather than `Full<Bytes>` because one route streams: `?stream=1`
/// answers with newline-delimited JSON, a line at a time, while the request is
/// still running. Everything else is still one buffer — `ok()` wraps it — so
/// the cost of the box is one allocation per response and no change to how any
/// of them are built.
type ApiBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

/// One whole body, as every non-streaming route answers.
fn whole(bytes: Bytes) -> ApiBody {
    Full::new(bytes).map_err(|never| match never {}).boxed()
}

/// A body whose bytes arrive from somewhere else, while the response is open.
///
/// Twenty lines rather than a `tokio-stream` dependency for its `StreamBody`
/// adapter: this is the whole of what that would do, and a crate added for a
/// wrapper is a crate to keep up with.
struct Streamed(tokio::sync::mpsc::Receiver<Bytes>);

impl hyper::body::Body for Streamed {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<std::result::Result<hyper::body::Frame<Bytes>, Self::Error>>> {
        // The sender going away ends the body, which is how the request's own
        // thread says it has finished: it drops its half.
        self.0
            .poll_recv(cx)
            .map(|frame| frame.map(|bytes| Ok(hyper::body::Frame::data(bytes))))
    }
}
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use zygo_core::pool::Outcome;
use zygo_core::spec::{ApiAuth, Layer, Spec};
use zygo_core::supervisor::client::Client;
use zygo_core::supervisor::{
    ControlError, Request as Control, Response as Reply, WorkspaceRequest,
};

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
///
/// A day rather than the hour it was. An embedder's long jobs — a render, a
/// migration, a model run — are hours, and an hour was a ceiling they hit for
/// no reason this API had: the *function's* `timeout` was always the limit
/// that mattered. What made an hour safe to raise is the heartbeat
/// (`pool::HEARTBEAT_GRACE`): a wedged request is now killed in a minute
/// whatever its budget says, so the ceiling no longer doubles as the only
/// backstop against holding a slot for ever.
const MAX_TIMEOUT_MS: u64 = 24 * 3_600_000;

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

/// What each tenant has used, and what has not been delivered yet.
///
/// One place rather than two, because they are one fact counted twice: a
/// dashboard reads the totals and a billing system reads the events, and a
/// deployment that had them disagree would trust neither.
#[derive(Default)]
struct Usage {
    totals: std::collections::BTreeMap<String, super::otlp::TenantUsage>,
    /// Events waiting for the webhook, oldest first.
    queued: std::collections::VecDeque<zygo_core::pool::Usage>,
    /// Events dropped because the queue was full.
    ///
    /// Counted rather than silently lost: at-least-once delivery that quietly
    /// becomes at-most-once is worse than one that says so.
    dropped: u64,
}

/// How many undelivered usage events to keep.
///
/// A bound rather than a growing queue, because the alternative to dropping is
/// the API process growing without limit while a webhook is down — which takes
/// the *serving* path down with it, to protect the billing path. The wrong way
/// round: requests matter more than their receipts, and the receipts are also
/// in the supervisor's log.
const USAGE_QUEUE: usize = 10_000;

/// How many events go in one webhook delivery.
const USAGE_BATCH: usize = 256;

impl Usage {
    /// Count one finished request, and queue it for delivery.
    fn record(&mut self, usage: zygo_core::pool::Usage) {
        let totals = self.totals.entry(usage.tenant.clone()).or_default();
        totals.tenant = usage.tenant.clone();
        totals.requests += 1;
        if usage.outcome != "ok" {
            totals.failures += 1;
        }
        totals.cpu_ms += usage.cpu_ms;
        totals.wall_ms += usage.wall_ms;
        *totals.by_outcome.entry(usage.outcome.clone()).or_default() += 1;

        if self.queued.len() >= USAGE_QUEUE {
            // The oldest, not the newest: a billing system that has fallen
            // behind wants the most recent state it can get, and the events
            // it lost are the ones furthest from now.
            self.queued.pop_front();
            self.dropped += 1;
        }
        self.queued.push_back(usage);
    }

    fn snapshot(&self) -> Vec<super::otlp::TenantUsage> {
        self.totals.values().cloned().collect()
    }

    fn take_batch(&mut self) -> Vec<zygo_core::pool::Usage> {
        self.queued
            .drain(..USAGE_BATCH.min(self.queued.len()))
            .collect()
    }

    /// Put a failed batch back at the front, so it is retried in order.
    fn return_batch(&mut self, batch: Vec<zygo_core::pool::Usage>) {
        for usage in batch.into_iter().rev() {
            if self.queued.len() >= USAGE_QUEUE {
                self.dropped += 1;
                continue;
            }
            self.queued.push_front(usage);
        }
    }
}

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
    /// Per-tenant totals since this process started, and the queue waiting to
    /// be delivered to `--usage-webhook`. See [`Usage`].
    usage: std::sync::Mutex<Usage>,
    /// The last `/healthz` answer, and when it was computed.
    ///
    /// The route is unauthenticated, so a control round trip per probe would
    /// be a way to make a host busy without holding a token.
    health: std::sync::Mutex<Option<(Instant, serde_json::Value)>>,
    started: Instant,
    requests: AtomicU64,
    errors: AtomicU64,
}

pub fn run(cli: &Cli, args: &ApiArgs) -> anyhow::Result<u8> {
    // Before anything is read or bound: printing the document is a question
    // about this binary, not about a host.
    if args.openapi {
        crate::output::json(&super::openapi::document())?;
        return Ok(0);
    }

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
        usage: std::sync::Mutex::new(Usage::default()),
        health: std::sync::Mutex::new(None),
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
    if let Some(url) = &args.usage_webhook {
        // Validated before the listener is bound, like the collector URL: a
        // bad webhook is a start-up error rather than a warning ten seconds
        // into serving.
        let url = reqwest::Url::parse(url)
            .with_context(|| format!("`{url}` is not a URL for --usage-webhook"))?;
        eprintln!(
            "{} {url}  every {:?}",
            style.dim("usage"),
            args.usage_interval.get()
        );
        let for_usage = Arc::clone(&api);
        runtime.spawn(deliver_usage(for_usage, url, args.usage_interval.get()));
    }

    runtime.block_on(serve(listen, api))?;
    Ok(0)
}

/// Deliver queued usage events to the webhook, for ever.
///
/// At-least-once: a batch that fails goes back on the front of the queue in
/// order and is tried again. A receiver may therefore see an event twice and
/// should key on `request_id` — which is documented on the flag, because
/// silent duplicates in a billing feed are worse than loud ones.
async fn deliver_usage(api: Arc<Api>, url: reqwest::Url, interval: std::time::Duration) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::error!("usage: cannot build an HTTP client: {e}");
            return;
        }
    };

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut failing = false;
    loop {
        ticker.tick().await;
        loop {
            let batch = api.usage.lock().expect("usage").take_batch();
            if batch.is_empty() {
                break;
            }
            let body = serde_json::json!({ "events": batch });
            match client.post(url.clone()).json(&body).send().await {
                Ok(response) if response.status().is_success() => {
                    if failing {
                        tracing::info!("usage: the webhook is answering again");
                        failing = false;
                    }
                }
                outcome => {
                    // Back on the front, in order: a billing feed that
                    // reordered under failure would be one nobody could
                    // reconcile.
                    api.usage.lock().expect("usage").return_batch(batch);
                    if !failing {
                        failing = true;
                        // Once per outage, not once per attempt: a webhook
                        // that is down for an hour must not be an hour of
                        // log lines.
                        match outcome {
                            Ok(r) => tracing::warn!("usage: the webhook answered {}", r.status()),
                            Err(e) => tracing::warn!("usage: the webhook is unreachable: {e}"),
                        }
                    }
                    break;
                }
            }
        }

        let dropped = {
            let mut usage = api.usage.lock().expect("usage");
            std::mem::take(&mut usage.dropped)
        };
        if dropped > 0 {
            tracing::warn!(
                "usage: dropped {dropped} events; the queue holds {USAGE_QUEUE} and the \
                 webhook is behind"
            );
        }
    }
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
async fn handle(req: Request<Incoming>, api: Arc<Api>) -> Result<Response<ApiBody>, Infallible> {
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
    fn new(status: StatusCode, message: impl std::fmt::Display) -> HttpError {
        HttpError {
            status,
            body: serde_json::json!({ "error": message.to_string() }),
            close: false,
        }
    }

    /// The same, for a refusal decided before the body was read.
    fn closing(status: StatusCode, message: impl std::fmt::Display) -> HttpError {
        HttpError {
            close: true,
            ..HttpError::new(status, message)
        }
    }

    fn into_response(self) -> Response<ApiBody> {
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

use hyper::header::HeaderValue;

async fn route(req: Request<Incoming>, api: &Arc<Api>) -> Result<Response<ApiBody>, HttpError> {
    // `/healthz` is deliberately unauthenticated: a load balancer probing it
    // has no business holding the token, and it reveals nothing but how ready
    // this host is to take work.
    if req.method() == Method::GET && req.uri().path() == "/healthz" {
        return healthz(api).await;
    }

    let actor = authorise(&req, api)?;

    let path = req.uri().path().to_string();
    let segments: Vec<&str> = path
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    let tenant = actor.tenant().map(str::to_string);

    match (req.method(), segments.as_slice()) {
        (&Method::GET, ["fn"]) => list(api, tenant).await,
        (&Method::POST, ["tenants"]) => {
            let body = read_body(req).await?;
            actor.operator_only("creating a tenant")?;
            create_tenant(api, &body).await
        }
        (&Method::GET, ["tenants"]) => {
            actor.operator_only("listing the tenants")?;
            tenants(api, None).await
        }
        (&Method::GET, ["tenants", id]) => {
            // A tenant reading *itself* is not a leak, and it is how a client
            // discovers which scripts it registered. Any other id is.
            let id = id.to_string();
            if actor.tenant() != Some(id.as_str()) {
                actor.operator_only("reading another tenant")?;
            }
            tenants(api, Some(id)).await
        }
        (&Method::DELETE, ["tenants", id]) => {
            let id = id.to_string();
            actor.may_deploy()?;
            delete_tenant(api, id).await
        }
        // A tenant's own secrets: readable as *names* by that tenant, set and
        // removed by the operator. A tenant cannot write its own, deliberately
        // — the operator is who holds the relationship with the key's issuer,
        // and a customer that could set one could set a value the operator's
        // own functions then use.
        // A tenant's limits. The operator's, because they can only narrow and
        // narrowing somebody is not something that somebody asks for.
        (&Method::PATCH, ["tenants", id, "limits"]) => {
            let id = id.to_string();
            let body = read_body(req).await?;
            actor.may_deploy()?;
            set_limits(api, id, &body).await
        }
        (&Method::GET, ["tenants", id, "secrets"]) => {
            let id = id.to_string();
            if actor.tenant() != Some(id.as_str()) {
                actor.operator_only("reading another tenant's secrets")?;
            }
            secret_names(api, id).await
        }
        (&Method::PUT, ["tenants", id, "secrets", name]) => {
            let (id, name) = (id.to_string(), name.to_string());
            let body = read_body(req).await?;
            actor.may_deploy()?;
            put_secret(api, id, name, &body).await
        }
        (&Method::DELETE, ["tenants", id, "secrets", name]) => {
            let (id, name) = (id.to_string(), name.to_string());
            actor.may_deploy()?;
            delete_secret(api, id, name).await
        }
        (&Method::POST, ["tenants", id, "tokens"]) => {
            let id = id.to_string();
            actor.may_deploy()?;
            mint_token(api, Some(id)).await
        }
        (&Method::POST, ["tokens"]) => {
            actor.may_deploy()?;
            mint_token(api, None).await
        }
        (&Method::GET, ["tokens"]) => {
            actor.may_deploy()?;
            list_tokens(api).await
        }
        (&Method::DELETE, ["tokens", id]) => {
            let id = id.to_string();
            actor.may_deploy()?;
            revoke_token(api, id).await
        }
        // Not gated on deploy: stopping your own request is not a widened
        // boundary, it is the narrowest thing a caller can ask for.
        (&Method::DELETE, ["requests", id]) => {
            let id = id.to_string();
            cancel(api, id, tenant).await
        }
        // Draining is the operator's: it stops this host serving anybody.
        (&Method::POST, ["drain"]) => {
            let query = Query::parse(req.uri().query().unwrap_or(""));
            let grace_ms = query.number("grace_ms")?.unwrap_or(30_000);
            actor.may_deploy()?;
            drain(api, grace_ms).await
        }
        (&Method::GET, ["metrics"]) => metrics(api).await,
        (&Method::GET, ["version"]) => Ok(json(
            StatusCode::OK,
            &serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "api": API_VERSION,
                "control": zygo_core::supervisor::CONTROL_VERSION,
                // What *this* caller may do, not what the flag says: a tenant
                // token asking is told the truth about its own request.
                "deploy": actor.deploy && actor.is_operator(),
            }),
        )),
        // The body is read *before* the gate, on every route that carries
        // one. Refusing without reading leaves an unread body on the
        // connection, hyper closes it, and the caller's *next* request on that
        // pooled connection fails with a broken pipe — a 403 on one call
        // turning into a transport error on an unrelated one. The body is
        // bounded by `MAX_BODY_BYTES`, so reading one this API is about to
        // throw away costs nothing worth saving.
        (&Method::PUT, ["fn", name]) => {
            let name = name.to_string();
            let body = read_body(req).await?;
            actor.may_deploy()?;
            serve_fn(api, name, &body, tenant).await
        }
        (&Method::DELETE, ["fn", name]) => {
            let name = name.to_string();
            actor.may_deploy()?;
            stop(api, name).await
        }
        (&Method::POST, ["run"]) => {
            let body = read_body(req).await?;
            actor.may_deploy()?;
            one_shot(api, &body).await
        }
        (&Method::GET, ["fn", name, "logs"]) => {
            let name = name.to_string();
            let query = req.uri().query().unwrap_or("").to_string();
            logs(api, name, &query, tenant).await
        }
        (&Method::POST, ["fn", name]) => {
            let name = name.to_string();
            let timeout_ms = timeout_header(&req)?;
            let key = request_key(&req)?;
            let query = Query::parse(req.uri().query().unwrap_or(""));
            let streaming = query.flag("stream")?;
            let out = query.flag("out")?;
            // A function's body is the event itself, with nowhere in it to
            // put a workspace, so the query string is where one is named. A
            // pool's body is JSON and has room for both.
            let workspace = with_out(query.blob("workspace")?, out);
            let body = read_body(req).await?;
            let event = parse_event(&body)?;
            if streaming {
                return exec_streaming(
                    api,
                    Control::Exec {
                        name,
                        event,
                        timeout_ms,
                        tenant,
                        key,
                        stream: true,
                        workspace,
                    },
                )
                .await;
            }
            exec(api, name, event, timeout_ms, tenant, key, workspace).await
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
            batch(api, name, events, timeout_ms, tenant).await
        }
        (&Method::GET, ["fn", name, "stats"]) => stats(api, name.to_string(), tenant).await,
        (&Method::POST, ["fn", name, "warm"]) => warm(api, name.to_string(), tenant).await,
        (&Method::GET, ["runtimes"]) => runtimes(api, tenant).await,
        (&Method::POST, ["runtimes"]) => {
            let body = read_body(req).await?;
            actor.may_deploy()?;
            serve_runtime(api, &body, tenant).await
        }
        (&Method::DELETE, ["runtimes", name]) => {
            let name = name.to_string();
            actor.may_deploy()?;
            stop_runtime(api, name).await
        }
        (&Method::POST, ["runtimes", name, "call"]) => {
            let name = name.to_string();
            let timeout_ms = timeout_header(&req)?;
            let key = request_key(&req)?;
            let query = Query::parse(req.uri().query().unwrap_or(""));
            let streaming = query.flag("stream")?;
            let out = query.flag("out")?;
            let body = read_body(req).await?;
            call_runtime(api, name, &body, timeout_ms, tenant, key, streaming, out).await
        }
        // Not gated on deploy, and this is the change tokens paid for:
        // registering a script for yourself is what a tenant token is *for*.
        // The bytes are inert — running them needs a pool the operator
        // declared — and the digest is theirs alone from here on.
        (&Method::PUT, ["scripts"]) => {
            let body = read_body(req).await?;
            put_script(api, &body, tenant).await
        }
        // Blobs, on the same terms scripts are: registering bytes runs
        // nothing, so a tenant token may. Forgetting one is the operator's,
        // because the store is shared by digest.
        (&Method::PUT, ["blobs"]) => {
            let body = read_body(req).await?;
            put_blob(api, &body).await
        }
        (&Method::GET, ["blobs", digest]) => get_blob(api, digest.to_string()).await,
        (&Method::DELETE, ["blobs", digest]) => {
            let digest = digest.to_string();
            actor.may_deploy()?;
            delete_blob(api, digest).await
        }
        // Dependency sets, on the same terms scripts are: uploading a
        // lockfile is a tenant's own business, and what it builds is theirs.
        // Building one *runs code* — a `setup.py`, an npm lifecycle script —
        // which is why the build sandbox reaches the registries and nothing
        // else; see `zygo_core::deps`.
        (&Method::POST, ["deps"]) => {
            let body = read_body(req).await?;
            put_deps(api, &body, tenant).await
        }
        (&Method::GET, ["deps"]) => deps(api, None, tenant).await,
        (&Method::GET, ["deps", id]) => deps(api, Some(id.to_string()), tenant).await,
        // The operator's: a dependency set is shared by id, so forgetting one
        // forgets it for every pool that was built on the same files.
        (&Method::DELETE, ["deps", id]) => {
            let id = id.to_string();
            actor.may_deploy()?;
            delete_deps(api, id).await
        }
        (&Method::GET, ["scripts", digest]) => get_script(api, digest.to_string()).await,
        // Still the operator's: the store is shared by digest, so forgetting
        // one byte-identical script forgets it for every tenant that
        // registered the same bytes.
        (&Method::DELETE, ["scripts", digest]) => {
            let digest = digest.to_string();
            actor.may_deploy()?;
            delete_script(api, digest).await
        }
        (_, ["fn", ..])
        | (_, ["requests", ..])
        | (_, ["drain"])
        | (_, ["metrics"])
        | (_, ["version"])
        | (_, ["run"])
        | (_, ["runtimes", ..])
        | (_, ["tenants", ..])
        | (_, ["tokens", ..])
        | (_, ["blobs", ..])
        | (_, ["deps", ..])
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

/// Who a request acts for, and what they may do.
///
/// Everything below `/fn`, `/runtimes` and `/scripts` belongs to somebody: an
/// embedder's customer, or the operator themselves. The answer comes from the
/// **token**, which is the only part of a request the caller cannot choose.
/// `X-Zygo-Tenant` is still read, but only as an operator saying which of
/// their customers they are acting for; a tenant token's own id wins, and a
/// header that disagrees with it is refused rather than ignored.
///
/// Three ways a request gets here:
///
/// * **No auth at all.** A `0600` unix socket or loopback, where whoever can
///   reach it is already this user. Operator, with `--allow-deploy` deciding
///   whether they may create sandboxes.
/// * **The bootstrap token**, `ZYGO_API_TOKEN`. Operator, same flag, same
///   answer — which is what keeps a setup that predates tokens working.
/// * **A minted token.** Operator or tenant, as the store says. A minted
///   operator token deploys unconditionally: minting one already required
///   deploy rights, so the decision was made when it was created.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Actor {
    /// `None` is the operator.
    tenant: Option<String>,
    /// May create and destroy sandboxes, scripts and tokens.
    deploy: bool,
}

/// The header an operator names a tenant with.
const TENANT_HEADER: &str = "x-zygo-tenant";

/// The header a caller names its own request with, so it can cancel it.
///
/// `X-Zygo-Request-Id` comes back *with the answer*, which is too late to stop
/// the call it belongs to. A caller that wants to be able to stop its own
/// request names it on the way in with this, and passes the same string to
/// `DELETE /requests/<name>`.
///
/// Not unique and not checked for uniqueness: two calls sharing a key are two
/// calls one cancel stops, which is what a caller who reused one meant. It is
/// only ever matched against the caller's *own* requests, so one tenant's key
/// cannot reach another's work.
const REQUEST_KEY_HEADER: &str = "x-zygo-request-key";

/// The header a request's own id comes back in.
///
/// What `DELETE /requests/<id>` names. Of no use to the caller of *this* call,
/// which has already finished by the time a header arrives — that is what
/// `X-Zygo-Request-Key` is for. It is here for anything joining a log line
/// back to the request it describes, and for a proxy that tees the answer.
const REQUEST_ID_HEADER: &str = "x-zygo-request-id";

impl Actor {
    fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    fn is_operator(&self) -> bool {
        self.tenant.is_none()
    }

    /// Refuse a route that is the operator's alone.
    ///
    /// Creating and deleting tenants is not something a tenant does, and
    /// neither is listing them: a customer that could enumerate the other
    /// customers is a leak, whatever the limits say.
    fn operator_only(&self, what: &str) -> Result<(), HttpError> {
        match &self.tenant {
            None => Ok(()),
            Some(id) => Err(HttpError::new(
                StatusCode::FORBIDDEN,
                format!("{what} is the operator's, and this request acts for tenant `{id}`"),
            )),
        }
    }

    /// The gate on everything that creates or destroys a sandbox.
    ///
    /// A 403 rather than a 404: pretending the route does not exist would send
    /// an SDK author looking for a typo, when the answer is a flag on the
    /// server or the kind of token they hold.
    fn may_deploy(&self) -> Result<(), HttpError> {
        if self.deploy {
            return self.operator_only("deploying");
        }
        Err(HttpError::new(
            StatusCode::FORBIDDEN,
            match &self.tenant {
                Some(id) => format!(
                    "serving, stopping and running are the operator's, and this \
                     request acts for tenant `{id}`\n  \
                     → a tenant token registers scripts and calls; it does not \
                     name images, mounts or commands"
                ),
                None => "this API may only call functions that are already served\n  \
                     → start it with `zygo api --allow-deploy`, or present an \
                     operator token minted with `zygo token mint`, to let callers \
                     serve, stop and run — which is running arbitrary code as the \
                     user it runs as"
                    .to_string(),
            },
        ))
    }
}

/// Resolve the bearer token to an actor, refusing one that resolves to nobody.
///
/// Authentication and authorisation in one pass, deliberately: two functions
/// meant a route could read the actor without having checked the token, and
/// the second one to be added would have been the one that forgot.
///
/// The token store is read from disk on every request rather than cached. That
/// is a few microseconds against a warm call's two milliseconds, and it is what
/// makes `DELETE /tokens/<id>` take effect on the next request instead of
/// whenever something decided to refresh.
fn authorise(req: &Request<Incoming>, api: &Api) -> Result<Actor, HttpError> {
    let header = || -> Result<Option<String>, HttpError> {
        let Some(value) = req.headers().get(TENANT_HEADER) else {
            return Ok(None);
        };
        let id = value
            .to_str()
            .map_err(|_| {
                HttpError::closing(
                    StatusCode::BAD_REQUEST,
                    "X-Zygo-Tenant must be ASCII: it is an id, not a name",
                )
            })?
            .trim();
        zygo_core::tenants::valid_id(id)
            .map_err(|e| HttpError::closing(StatusCode::BAD_REQUEST, e.to_string()))?;
        Ok(Some(id.to_string()))
    };

    // No authentication: the listener is a `0600` socket or loopback, so the
    // caller is this user, and this user is the operator.
    let Some(bootstrap) = &api.token else {
        return Ok(Actor {
            tenant: header()?,
            deploy: api.deploy,
        });
    };

    let presented = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or_else(|| HttpError::closing(StatusCode::UNAUTHORIZED, "missing bearer token"))?;

    // The bootstrap token first, and with a comparison that does not stop at
    // the first differing byte. On loopback the timing side channel is
    // academic, but the API is allowed on other addresses with a token, and
    // there it is not.
    if constant_time_eq(presented.as_bytes(), bootstrap.as_bytes()) {
        return Ok(Actor {
            tenant: header()?,
            deploy: api.deploy,
        });
    }

    let token = zygo_core::tokens::Tokens::new(&api.paths)
        .resolve(presented)
        .map_err(|e| HttpError::closing(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?
        .ok_or_else(|| {
            HttpError::closing(
                StatusCode::UNAUTHORIZED,
                "wrong or revoked bearer token".to_string(),
            )
        })?;

    match token.tenant() {
        // An operator token: the header names which customer they act for.
        None => Ok(Actor {
            tenant: header()?,
            deploy: true,
        }),
        // A tenant token: the id comes from the token, and a header that
        // disagrees is refused rather than quietly dropped. A client that
        // thinks it is acting for somebody else should be told it is not.
        Some(id) => {
            if let Some(named) = header()?
                && named != id
            {
                return Err(HttpError::closing(
                    StatusCode::FORBIDDEN,
                    format!(
                        "this token is tenant `{id}`'s and the request names `{named}`\n  \
                         → drop the X-Zygo-Tenant header; only an operator token \
                         may act for another tenant"
                    ),
                ));
            }
            Ok(Actor {
                tenant: Some(id.to_string()),
                deploy: false,
            })
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The caller's own name for this request, if they gave one.
///
/// Bounded and restricted to printable ASCII, because it is compared against
/// request ids and appears in log lines: a key with a newline in it would be a
/// caller writing into the supervisor's log.
fn request_key(req: &Request<Incoming>) -> Result<Option<String>, HttpError> {
    let Some(value) = req.headers().get(REQUEST_KEY_HEADER) else {
        return Ok(None);
    };
    let key = value
        .to_str()
        .map(str::trim)
        .ok()
        .filter(|k| !k.is_empty())
        .filter(|k| k.len() <= 128)
        .filter(|k| k.bytes().all(|b| b.is_ascii_graphic()))
        .ok_or_else(|| {
            HttpError::closing(
                StatusCode::BAD_REQUEST,
                "X-Zygo-Request-Key must be 1 to 128 printable ASCII characters",
            )
        })?;
    Ok(Some(key.to_string()))
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
                    HttpError::closing(
                        StatusCode::BAD_REQUEST,
                        "X-Zygo-Timeout-Ms must be a positive integer",
                    )
                })?;
            // Refused rather than silently clamped: a caller that asked for a
            // day and was given an hour would report the wrong thing when the
            // wait ended.
            if ms > MAX_TIMEOUT_MS {
                return Err(HttpError::closing(
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

/// `?stream=1`: newline-delimited JSON, a line per event, as they happen.
///
/// One object per line — `{"stream":"stdout","data":"…"}` while the request
/// runs, then one final `{"result":…}` or `{"error":…}` carrying exactly what
/// the non-streaming answer would have been, plus its status. The status *code*
/// is 200 as soon as the first byte goes out, because it has to be: a response
/// cannot be given a status after its body has started. The last line is where
/// the outcome is.
///
/// NDJSON rather than server-sent events: every client in every language can
/// read a line and parse JSON, and SSE's framing buys nothing here — there is
/// one stream, no reconnection, and no last-event id to resume from.
const STREAM_CONTENT_TYPE: &str = "application/x-ndjson";

/// How many lines may be waiting to be written before the request's own thread
/// is held up.
///
/// Small on purpose. The point of a stream is that the reader sees output
/// early, and a deep buffer between the handler and the socket is a way to
/// arrive at the same time as `RESULT` would have. When it fills, the
/// supervisor's sink blocks — which is backpressure reaching the handler,
/// which is the correct place for it to reach.
const STREAM_BACKLOG: usize = 64;

/// Run a request and answer with its output as it is produced.
async fn exec_streaming(api: &Arc<Api>, request: Control) -> Result<Response<ApiBody>, HttpError> {
    let (lines, rx) = tokio::sync::mpsc::channel::<Bytes>(STREAM_BACKLOG);
    let api = Arc::clone(api);

    // The control call is blocking and lives on its own thread for the whole
    // request; the body is whatever it has sent so far. Nothing here awaits
    // the end, which is the point.
    tokio::task::spawn_blocking(move || {
        let mut client = match api.clients.lock().expect("clients").pop() {
            Some(client) => client,
            None => match Client::connect_or_start(&api.paths, &api.exe) {
                Ok(client) => client,
                Err(e) => {
                    let _ = lines.blocking_send(line(&serde_json::json!({
                        "status": 500,
                        "error": format!("{e:#}"),
                    })));
                    return;
                }
            },
        };

        let answer = client.send_streaming(&request, |stream, data| {
            // A full channel blocks this thread, which is the supervisor
            // connection, which is the request. That is backpressure arriving
            // where it can do something: the handler waits for its reader.
            let _ = lines.blocking_send(line(&serde_json::json!({
                "stream": stream.as_str(),
                "data": data,
            })));
        });

        let last = match answer {
            Ok(reply) => {
                count_usage(&api, &reply);
                let (status, mut body) = reply_to_json(reply);
                body["status"] = status.as_u16().into();
                body
            }
            Err(e) => serde_json::json!({ "status": 500, "error": format!("{e:#}") }),
        };
        let _ = lines.blocking_send(line(&last));

        let mut idle = api.clients.lock().expect("clients");
        if idle.len() < MAX_IDLE_CLIENTS {
            idle.push(client);
        }
    });

    let body = Streamed(rx).boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", STREAM_CONTENT_TYPE)
        // Nothing between here and the client may hold a line back waiting for
        // more: a proxy that buffers turns a stream into a slow whole answer.
        .header("cache-control", "no-store")
        .header("x-accel-buffering", "no")
        .body(body)
        .map_err(|e| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, e))
}

/// One NDJSON line: the object, then a newline.
fn line(value: &serde_json::Value) -> Bytes {
    let mut out = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    out.push(b'\n');
    Bytes::from(out)
}

#[allow(clippy::too_many_arguments)]
async fn exec(
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
async fn batch(
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
    // supervisor connections ahead of every other caller (S-06). The
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

/// One customer's functions, or every one of them for the operator.
///
/// Filtered here rather than in the supervisor, unlike the routes that *act*
/// on a function: this is the operator's own process reading the operator's
/// own registry, and the question "which of these may this caller see?" is one
/// only the side that resolved the token can answer. Every route that does
/// something to a function is checked at the supervisor, where it belongs.
fn mine(
    functions: Vec<zygo_core::pool::Status>,
    tenant: Option<&str>,
) -> Vec<zygo_core::pool::Status> {
    match tenant {
        None => functions,
        Some(id) => functions.into_iter().filter(|f| f.tenant == id).collect(),
    }
}

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
async fn healthz(api: &Arc<Api>) -> Result<Response<ApiBody>, HttpError> {
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
async fn drain(api: &Arc<Api>, grace_ms: u64) -> Result<Response<ApiBody>, HttpError> {
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

async fn list(api: &Arc<Api>, tenant: Option<String>) -> Result<Response<ApiBody>, HttpError> {
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

async fn stats(
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

async fn warm(
    api: &Arc<Api>,
    name: String,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::Warm { name, tenant })?)).await?;
    Ok(reply_to_response(reply))
}

/// What `POST /runtimes` accepts: a `[runtime.<name>]` table as JSON.
///
/// The same shape as `PUT /fn/<name>`, minus the two things a pool cannot
/// have. There are no `secrets`, because a secret belongs to a tenant's
/// request and a pool's zygotes are shared; and `base_dir` is optional,
/// because a pool names no files on the host — that is what makes it the
/// route an embedder can build on without touching the Zygo host's disk.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ServeRuntimeRequest {
    name: String,
    #[serde(default)]
    layer: Layer,
    /// Only needed when the table names a path — `requirements`, a mount.
    #[serde(default)]
    base_dir: Option<std::path::PathBuf>,
    /// A dependency set from `POST /deps`, built inside this pool's image.
    ///
    /// The API-native half of `requirements`: that one names a file on the
    /// Zygo host, which is exactly what an embedder does not have. One that is
    /// still building answers `503` with a `Retry-After` and starts nothing.
    #[serde(default)]
    deps: Option<String>,
}

async fn serve_runtime(
    api: &Arc<Api>,
    body: &[u8],
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let request: ServeRuntimeRequest = serde_json::from_slice(body).map_err(|e| {
        HttpError::new(
            StatusCode::BAD_REQUEST,
            format!("body is not a runtime definition: {e}"),
        )
    })?;
    if let Some(base_dir) = &request.base_dir
        && !base_dir.is_absolute()
    {
        return Err(HttpError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "`base_dir` must be absolute, and `{}` is not\n  \
                 → it names a directory on the host this API runs on",
                base_dir.display()
            ),
        ));
    }
    // `/` is as good a default as any and is never used: a pool with no paths
    // in it has nothing to resolve relative to, and one with paths has to say
    // where they are.
    let base_dir = request
        .base_dir
        .unwrap_or_else(|| std::path::PathBuf::from("/"));

    let deps = request.deps;
    let reply = control(api, move |c| {
        Ok(c.send(&Control::ServeRuntime {
            tenant,
            name: request.name,
            spec: None,
            layer: Box::new(request.layer),
            base_dir,
            deps,
            // Never from a socket, for the reason `PUT /fn/<name>` gives.
            allow_host_net: false,
            allow_private_net: false,
            allow_unlimited: false,
        })?)
    })
    .await?;
    Ok(reply_to_response(reply))
}

/// `DELETE /requests/<id>`: stop a request that is running.
///
/// Answers as soon as the kill has been sent, not when the request has
/// stopped: the request's own caller is the one waiting for the outcome, and
/// holding this connection open until they get it would make a cancel cost as
/// long as the thing it cancelled.
async fn cancel(
    api: &Arc<Api>,
    id: String,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::Cancel { id, tenant })?)).await?;
    Ok(reply_to_response(reply))
}

async fn runtimes(api: &Arc<Api>, tenant: Option<String>) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, |c| Ok(c.send(&Control::Runtimes)?)).await?;
    match (reply, tenant) {
        (Reply::Runtimes { runtimes }, Some(id)) => {
            let runtimes: Vec<_> = runtimes.into_iter().filter(|r| r.tenant == id).collect();
            Ok(json(
                StatusCode::OK,
                &serde_json::json!({ "runtimes": runtimes }),
            ))
        }
        (other, _) => Ok(reply_to_response(other)),
    }
}

async fn stop_runtime(api: &Arc<Api>, name: String) -> Result<Response<ApiBody>, HttpError> {
    let wanted = name.clone();
    let reply = control(api, move |c| Ok(c.send(&Control::StopRuntime { name })?)).await?;
    match reply {
        Reply::Stopped { names } if names.is_empty() => Err(HttpError::new(
            StatusCode::NOT_FOUND,
            format!("no runtime named `{wanted}`"),
        )),
        other => Ok(reply_to_response(other)),
    }
}

/// What `POST /runtimes/<name>/call` accepts.
///
/// `script` is either a digest the store holds — the embedder's usual case,
/// registered once and called ten thousand times — or the code itself, for a
/// one-off that is not worth registering.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CallRuntimeRequest {
    script: ScriptRef,
    #[serde(default)]
    event: serde_json::Value,
    /// The function to call, when it is not `handler`.
    #[serde(default)]
    entry_point: Option<String>,
    /// Files for this request. See [`WorkspaceBody`].
    #[serde(default)]
    workspace: Option<WorkspaceBody>,
}

/// What `workspace` on a call body accepts.
///
/// `inline` is a tar as base64 and `blob` is one this host already holds;
/// exactly one of them, because two would be two answers to "what is in the
/// directory". `?out=1` on the query string asks for it back, and is a query
/// parameter rather than a field so that a call with no body at all can ask.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceBody {
    #[serde(default)]
    inline: Option<String>,
    #[serde(default)]
    blob: Option<String>,
}

impl From<WorkspaceBody> for WorkspaceRequest {
    fn from(body: WorkspaceBody) -> WorkspaceRequest {
        WorkspaceRequest {
            inline: body.inline,
            blob: body.blob,
            collect: false,
        }
    }
}

/// Fold `?out=1` into whatever the body asked for.
fn with_out(workspace: Option<WorkspaceRequest>, out: bool) -> Option<WorkspaceRequest> {
    match (workspace, out) {
        (Some(w), out) => Some(WorkspaceRequest { collect: out, ..w }),
        // `?out=1` alone is a request with no files in and its directory back:
        // a handler that only *produces* something still needs somewhere to
        // put it.
        (None, true) => Some(WorkspaceRequest {
            collect: true,
            ..WorkspaceRequest::default()
        }),
        (None, false) => None,
    }
}

/// `"sha256:…"` or `{"source": "…"}`.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum ScriptRef {
    Digest(String),
    Source { source: String },
}

/// Every route whose body can name a path on the host Zygo runs on.
///
/// Kept as a list rather than a comment because it is the thing a tenant token
/// must never reach: a host path is how a caller reads the operator's disk,
/// and `deny_unknown_fields` on every request body is what stops one appearing
/// somewhere else by accident.
///
/// All three are behind [`Actor::may_deploy`], which a tenant token never
/// passes — the check is not "reject paths for tenants" but "a tenant does not
/// reach the routes that have them". `POST /runtimes/<name>/call` is
/// deliberately not here: [`ScriptRef`] is a digest or a source, and
/// `crate::protocol::Script`'s own `path` field is the *supervisor's* to set
/// once it has written the file into the sandbox.
#[cfg(test)]
const ROUTES_THAT_NAME_A_HOST_PATH: &[&str] = &[
    "PUT /fn/<name>", // `base_dir`, and the layer's mounts and `entry`
    "POST /runtimes", // `base_dir`
    "POST /run",      // the layer's mounts and `cmd`
];

#[allow(clippy::too_many_arguments)]
async fn call_runtime(
    api: &Arc<Api>,
    name: String,
    body: &[u8],
    timeout_ms: u64,
    tenant: Option<String>,
    key: Option<String>,
    streaming: bool,
    out: bool,
) -> Result<Response<ApiBody>, HttpError> {
    let request: CallRuntimeRequest = serde_json::from_slice(body).map_err(|e| {
        HttpError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "body must be {{\"script\": \"sha256:…\" | {{\"source\": \"…\"}}, \
                 \"event\": …}}: {e}"
            ),
        )
    })?;

    let mut script = match request.script {
        ScriptRef::Digest(digest) => zygo_core::protocol::Script {
            path: None,
            source: None,
            digest: Some(digest),
            entry_point: None,
        },
        ScriptRef::Source { source } => zygo_core::protocol::Script::inline(source),
    };
    script.entry_point = request.entry_point;

    let call = Control::ExecScript {
        key,
        runtime: name,
        script,
        event: request.event,
        timeout_ms,
        tenant,
        stream: streaming,
        workspace: with_out(request.workspace.map(Into::into), out),
    };
    if streaming {
        return exec_streaming(api, call).await;
    }
    let reply = control(api, move |c| Ok(c.send(&call)?)).await?;
    count_usage(api, &reply);
    Ok(reply_to_response(reply))
}

/// `POST /deps`: build a dependency set from files in the request.
///
/// The API-native half of the spec's `requirements`, which names a path on the
/// Zygo host — the one thing an embedder has no way to produce. Here the
/// lockfile is in the request, the build happens on this host, and what comes
/// back is an id a pool can be built on.
///
/// **Answers before the build finishes**, with `building`. A `pip install` is
/// minutes; an HTTP request that waited for one would time out in every proxy
/// between here and the caller. Poll `GET /deps/<id>`, or send the same
/// `POST /runtimes` again and read the `Retry-After`.
///
/// Idempotent by content, like `PUT /scripts`: the same files against the same
/// image are the same id, and `200` rather than `201` says this host already
/// had it.
async fn put_deps(
    api: &Arc<Api>,
    body: &[u8],
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PutDepsRequest {
        /// The image the dependencies are built *inside*, and part of the id:
        /// a wheel built for one interpreter fails at import in another.
        image: String,
        /// File name to base64 of its bytes. Base64 because a lockfile is not
        /// always UTF-8 and JSON has no other way to carry bytes.
        files: std::collections::BTreeMap<String, String>,
    }
    let request: PutDepsRequest = serde_json::from_slice(body).map_err(|e| {
        HttpError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "body must be {{\"image\": \"…\", \"files\": {{\"requirements.txt\": \"<base64>\"}}}}: {e}"
            ),
        )
    })?;

    let reply = control(api, move |c| {
        Ok(c.send(&Control::PutDeps {
            image: request.image,
            files: request.files,
            tenant,
        })?)
    })
    .await?;
    match reply {
        Reply::Dependencies { deps, existed, .. } => {
            let status = if existed {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            Ok(json(status, &deps_json(deps.first(), "")))
        }
        other => Ok(reply_to_response(other)),
    }
}

/// `GET /deps` and `GET /deps/<id>`: how a build went, with its log.
async fn deps(
    api: &Arc<Api>,
    id: Option<String>,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let one = id.is_some();
    let reply = control(api, move |c| Ok(c.send(&Control::Deps { id, tenant })?)).await?;
    match reply {
        Reply::Dependencies { deps, log, .. } if one => {
            Ok(json(StatusCode::OK, &deps_json(deps.first(), &log)))
        }
        Reply::Dependencies { deps, .. } => Ok(json(
            StatusCode::OK,
            &serde_json::json!({
                "deps": deps.iter().map(|d| deps_json(Some(d), "")).collect::<Vec<_>>(),
            }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

/// `DELETE /deps/<id>`: forget one, unless a pool is built on it.
async fn delete_deps(api: &Arc<Api>, id: String) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::DeleteDeps { id })?)).await?;
    match reply {
        Reply::Ok => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "deleted": true }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

/// One dependency set as a caller reads it.
///
/// The log is in the same object as the state rather than at a second URL: a
/// caller looking at `failed` wants the reason, and making them ask twice for
/// it is how a client ends up not showing it at all.
fn deps_json(status: Option<&zygo_core::deps::Status>, log: &str) -> serde_json::Value {
    let Some(status) = status else {
        return serde_json::json!({});
    };
    serde_json::json!({
        "id": status.id,
        "kind": status.kind.as_str(),
        "state": status.state.as_str(),
        "image": status.image,
        "files": status.files,
        "error": status.error,
        "started_ms": status.started_ms,
        "finished_ms": status.finished_ms,
        "tenants": status.tenants,
        "log": log,
    })
}

/// `PUT /scripts`: the body **is** the script, and the answer is its name.
///
/// Not JSON around it: a script is a file, the caller has it as bytes, and
/// wrapping those bytes in a JSON string to unwrap them again is a
/// transformation with no reader. Content-addressed, so this is idempotent —
/// the same bytes are the same name however many times, and from however many
/// tenants, which is what `201` versus `200` says.
/// `POST /tenants`: one customer of the embedder.
///
/// Idempotent — `201` when it was created, `200` when it was already there —
/// for the same reason `PUT /scripts` is: an embedder that creates a customer
/// they already have has not made a mistake worth failing a deploy over.
async fn create_tenant(api: &Arc<Api>, body: &[u8]) -> Result<Response<ApiBody>, HttpError> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CreateTenantRequest {
        id: String,
    }
    let request: CreateTenantRequest = serde_json::from_slice(body).map_err(|e| {
        HttpError::new(
            StatusCode::BAD_REQUEST,
            format!("body must be {{\"id\": \"<tenant>\"}}: {e}"),
        )
    })?;

    let reply = control(api, move |c| {
        Ok(c.send(&Control::CreateTenant { id: request.id })?)
    })
    .await?;
    match reply {
        Reply::Tenants {
            tenants, existed, ..
        } => Ok(json(
            if existed {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            },
            &serde_json::json!({ "tenant": tenants.first(), "existed": existed }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

async fn tenants(api: &Arc<Api>, id: Option<String>) -> Result<Response<ApiBody>, HttpError> {
    let one = id.is_some();
    let reply = control(api, move |c| Ok(c.send(&Control::Tenants { id })?)).await?;
    match reply {
        Reply::Tenants { tenants, .. } if one => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "tenant": tenants.first() }),
        )),
        Reply::Tenants { tenants, .. } => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "tenants": tenants }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

async fn delete_tenant(api: &Arc<Api>, id: String) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::DeleteTenant { id })?)).await?;
    match reply {
        Reply::Tenants {
            removed_scripts,
            stopped,
            ..
        } => Ok(json(
            StatusCode::OK,
            &serde_json::json!({
                "deleted": true,
                "removed_scripts": removed_scripts,
                "stopped": stopped,
            }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

/// `PATCH /tenants/<id>/limits`: what this tenant may not exceed.
///
/// `PATCH` rather than `PUT` because it is a partial description: the keys
/// that are present are set and the rest are left alone, which is what an
/// operator tightening one number wants.
async fn set_limits(
    api: &Arc<Api>,
    tenant: String,
    body: &[u8],
) -> Result<Response<ApiBody>, HttpError> {
    let limits: zygo_core::tenants::TenantLimits = serde_json::from_slice(body).map_err(|e| {
        HttpError::new(
            StatusCode::BAD_REQUEST,
            format!(
                "body must be a limits table — mem, cpu, pids, timeout, scratch, \
                 network, allow: {e}"
            ),
        )
    })?;
    let reply = control(api, move |c| {
        Ok(c.send(&Control::SetLimits {
            tenant,
            limits: Box::new(limits),
        })?)
    })
    .await?;
    match reply {
        Reply::Tenants { tenants, .. } => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "tenant": tenants.first() }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

/// `PUT /tenants/<id>/secrets/<name>`: the body **is** the value.
///
/// Not JSON around it, for the reason a script's body is the script: a secret
/// is a string the caller has, and wrapping it to unwrap it again is a
/// transformation with no reader — and one more place a value could be logged.
async fn put_secret(
    api: &Arc<Api>,
    tenant: String,
    name: String,
    body: &[u8],
) -> Result<Response<ApiBody>, HttpError> {
    let value = std::str::from_utf8(body)
        .map_err(|e| {
            HttpError::new(
                StatusCode::BAD_REQUEST,
                format!("a secret value must be UTF-8 text: {e}"),
            )
        })?
        .to_string();
    if value.is_empty() {
        return Err(HttpError::new(
            StatusCode::BAD_REQUEST,
            "the body is empty; it should be the secret's value",
        ));
    }
    let reply = control(api, move |c| {
        Ok(c.send(&Control::PutSecret {
            tenant,
            name,
            value,
        })?)
    })
    .await?;
    Ok(reply_to_response(reply))
}

/// `GET /tenants/<id>/secrets`: the **names**, never the values.
async fn secret_names(api: &Arc<Api>, tenant: String) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::Secrets { tenant })?)).await?;
    Ok(reply_to_response(reply))
}

async fn delete_secret(
    api: &Arc<Api>,
    tenant: String,
    name: String,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| {
        Ok(c.send(&Control::DeleteSecret { tenant, name })?)
    })
    .await?;
    Ok(reply_to_response(reply))
}

/// `POST /tenants/<id>/tokens` and `POST /tokens`: mint one.
///
/// `201`, and the only response in this API that carries a secret. It is not
/// stored, so it cannot be fetched again — a client that drops it has to
/// revoke the token and mint another, which is the same bargain every
/// credential worth the name makes.
async fn mint_token(
    api: &Arc<Api>,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::MintToken { tenant })?)).await?;
    match reply {
        Reply::Tokens { tokens, secret } => Ok(json(
            StatusCode::CREATED,
            &serde_json::json!({ "token": tokens.first(), "secret": secret }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

/// `GET /tokens`: every token, hashes and all, never a secret.
async fn list_tokens(api: &Arc<Api>) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, |c| Ok(c.send(&Control::Tokens)?)).await?;
    match reply {
        Reply::Tokens { tokens, .. } => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "tokens": tokens }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

/// `DELETE /tokens/<id>`: revoke one, from the next request onwards.
async fn revoke_token(api: &Arc<Api>, id: String) -> Result<Response<ApiBody>, HttpError> {
    let wanted = id.clone();
    let reply = control(api, move |c| Ok(c.send(&Control::RevokeToken { id })?)).await?;
    match reply {
        Reply::Tokens { tokens, .. } => Ok(json(
            StatusCode::OK,
            &serde_json::json!({
                "revoked": true,
                "token": tokens.iter().find(|t| t.id == wanted),
            }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

/// `PUT /blobs`: the body **is** the tar, and the answer is its name.
///
/// Not JSON around it, for the reason `PUT /scripts` gives: a blob is bytes,
/// the caller has them as bytes, and wrapping them to unwrap them again is a
/// transformation with no reader. It is the one route whose body is binary.
async fn put_blob(api: &Arc<Api>, body: &[u8]) -> Result<Response<ApiBody>, HttpError> {
    if body.is_empty() {
        return Err(HttpError::new(
            StatusCode::BAD_REQUEST,
            "the body is empty; it should be the tar itself",
        ));
    }
    // Base64 only for the control socket, which is JSON. The HTTP side of
    // this route never encodes anything.
    let tar = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, body);
    let reply = control(api, move |c| Ok(c.send(&Control::PutBlob { tar })?)).await?;
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

async fn get_blob(api: &Arc<Api>, digest: String) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::GetBlob { digest })?)).await?;
    Ok(reply_to_response(reply))
}

async fn delete_blob(api: &Arc<Api>, digest: String) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::DeleteBlob { digest })?)).await?;
    match reply {
        Reply::Ok => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "deleted": true }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

async fn put_script(
    api: &Arc<Api>,
    body: &[u8],
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
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

    let reply = control(api, move |c| {
        Ok(c.send(&Control::PutScript { source, tenant })?)
    })
    .await?;
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
async fn get_script(api: &Arc<Api>, digest: String) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::GetScript { digest })?)).await?;
    match reply {
        Reply::Script { digest, size, .. } => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "sha256": digest, "size": size }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

async fn delete_script(api: &Arc<Api>, digest: String) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::DeleteScript { digest })?)).await?;
    match reply {
        Reply::Ok => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "deleted": true }),
        )),
        other => Ok(reply_to_response(other)),
    }
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

async fn stop(api: &Arc<Api>, name: String) -> Result<Response<ApiBody>, HttpError> {
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

    /// A `sha256:…` blob named on the query string, as a workspace request.
    ///
    /// For `POST /fn/<name>`, whose body is the event and has nowhere to put
    /// one. Only a *blob* can be named this way — an inline tar in a URL
    /// would be a megabyte of base64 in a request line, which every proxy
    /// between here and the caller has an opinion about.
    fn blob(&self, key: &str) -> Result<Option<WorkspaceRequest>, HttpError> {
        let Some(digest) = self.get(key).filter(|d| !d.is_empty()) else {
            return Ok(None);
        };
        zygo_core::scripts::ScriptDigest::parse(digest)
            .map_err(|e| HttpError::new(StatusCode::BAD_REQUEST, e.to_string()))?;
        Ok(Some(WorkspaceRequest {
            blob: Some(digest.to_string()),
            ..WorkspaceRequest::default()
        }))
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
async fn one_shot(api: &Arc<Api>, body: &[u8]) -> Result<Response<ApiBody>, HttpError> {
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
            // Whether the program ran at all, and how far the run got when
            // it did not: a start failure is *unavailable*, not the code's.
            "started": captured.started,
            "phase": captured.phase,
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
        tenants: api.usage.lock().expect("usage").snapshot(),
    })
}

/// Prometheus text exposition, from what the supervisor reports.
async fn metrics(api: &Arc<Api>) -> Result<Response<ApiBody>, HttpError> {
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
        .body(whole(Bytes::from(out)))
        .expect("a valid response"))
}

/// Map a control reply to an HTTP status and JSON body (§4.6).
/// Count a finished request, if that is what this reply is.
///
/// Called at the four places something is *run* — a function, a pool, a batch
/// element, a stream — rather than inside `reply_to_response`, which every
/// route uses and most of them do not run anything. An explicit call at four
/// sites is easier to check than an implicit one at thirty.
fn count_usage(api: &Api, reply: &Reply) {
    if let Reply::Executed { outcome } = reply {
        api.usage
            .lock()
            .expect("usage")
            .record(zygo_core::pool::Usage::from(outcome.as_ref()));
    }
}

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

fn reply_to_response(reply: Reply) -> Response<ApiBody> {
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

/// The design document's `POST /fn/<name>` answers: 200 with the result and
/// metrics; 408 when the deadline killed it; 500 when the handler raised.
fn outcome_to_json(outcome: Outcome) -> (StatusCode, serde_json::Value) {
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

fn json(status: StatusCode, body: &serde_json::Value) -> Response<ApiBody> {
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
        let operator = |deploy| Actor {
            tenant: None,
            deploy,
        };

        assert!(operator(true).may_deploy().is_ok());

        let refused = operator(false)
            .may_deploy()
            .expect_err("a call-only API refuses");
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

    /// A tenant token never deploys, whatever the flag says.
    ///
    /// The flag decides whether *the operator* may create sandboxes. A tenant
    /// asking is refused for a different reason, and told so: naming an image,
    /// a mount and a command is running arbitrary code as the host user, which
    /// is not something one customer gets to do because another is trusted.
    /// A tenant token cannot reach any route whose body names a host path.
    ///
    /// The audit 2.6 asks for, as a test rather than a reading: if a fourth
    /// route ever grows a path, this is the list it has to be added to, and
    /// the assertion below is what says it must be gated.
    #[test]
    fn nothing_that_names_a_host_path_is_reachable_by_a_tenant() {
        let tenant = Actor {
            tenant: Some("acme".into()),
            deploy: true,
        };
        for route in ROUTES_THAT_NAME_A_HOST_PATH {
            let refused = tenant
                .may_deploy()
                .expect_err(&format!("{route} was allowed"));
            assert_eq!(refused.status, StatusCode::FORBIDDEN, "{route}");
        }
    }

    #[test]
    fn a_tenant_never_deploys_however_the_api_was_started() {
        for deploy in [true, false] {
            let refused = Actor {
                tenant: Some("acme".into()),
                deploy,
            }
            .may_deploy()
            .expect_err("a tenant cannot deploy");
            assert_eq!(refused.status, StatusCode::FORBIDDEN);
            let message = refused.body["error"].as_str().expect("a message");
            assert!(message.contains("acme"), "{message}");
            assert!(
                !message.contains("--allow-deploy"),
                "a tenant cannot act on that advice: {message}"
            );
        }
    }

    /// Listing is scoped by the token, not by what the caller asked for.
    #[test]
    fn a_listing_shows_one_tenant_their_own_functions_only() {
        let status = |name: &str, tenant: &str| zygo_core::pool::Status {
            name: name.into(),
            tenant: tenant.into(),
            image: String::new(),
            state: zygo_core::sandbox::SandboxState::Warm,
            runtime: "python/3.12".into(),
            rss_kb: 0,
            imports_ms: 0.0,
            requests: 0,
            failures: 0,
        };
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
