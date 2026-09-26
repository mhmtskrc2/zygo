// SPDX-License-Identifier: Apache-2.0
//! `zygo api` — the HTTP front door (`docs/book/17-api-sdk-mcp.md`).
//!
//! A client of the supervisor, not a second copy of it. Every route here turns
//! into one control request over the unix socket, which is the same boundary
//! the CLI crosses; "one RPC boundary on the request path" holds
//! because HTTP → supervisor replaces CLI → supervisor rather than adding to
//! it. The supervisor stays the only process that owns sandboxes, and this one
//! can be restarted, moved to a unix socket, or fronted by something else
//! without a warm function noticing.
//!
//! Security posture: bearer auth by default, `127.0.0.1` by default,
//! and an unauthenticated listener is refused on anything but a unix socket or
//! a loopback address. The token comes from `ZYGO_API_TOKEN` and nowhere else —
//! never a flag, because flags are in `ps`.
//!
//! ## The module
//!
//! This file is the server: [`run`] starts it, [`route`] is the table every
//! request goes through, and [`control`] is the pooled connection to the
//! supervisor that every route ends in. The rest lives beside it:
//!
//! - `listen`, `auth`, `request` — where it listens, who is asking, what.
//! - `routes_fn`, `routes_runtimes`, `routes_tenants`,
//!   `routes_scripts_blobs_deps`, `host` — one file per resource.
//! - `stream`, `reply`, `usage` — how an answer goes out, and what it cost.
//!
//! `route` is one flat `match`, on purpose: `cmd/openapi.rs` reads this
//! file and checks every arm against the OpenAPI document.

mod auth;
mod host;
mod listen;
mod reply;
mod request;
mod routes_fn;
mod routes_runtimes;
mod routes_scripts_blobs_deps;
mod routes_tenants;
mod stream;
mod usage;

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::Context;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use zygo_core::spec::{ApiAuth, Spec};
use zygo_core::supervisor::Request as Control;
use zygo_core::supervisor::client::Client;

use crate::cli::{ApiArgs, Cli};
use crate::output::Style;

use auth::authorise;
use host::{drain, healthz, metrics, snapshot};
use listen::{Listen, serve};
use reply::{ApiBody, HttpError, json};
use request::{CallParams, Query, parse_event, read_body, request_key, timeout_header};
use routes_fn::{batch, cancel, exec, list, logs, one_shot, serve_fn, stats, stop, warm};
use routes_runtimes::{call_runtime, runtimes, serve_runtime, stop_runtime, with_out};
use routes_scripts_blobs_deps::{
    delete_blob, delete_deps, delete_script, deps, get_blob, get_script, put_blob, put_deps,
    put_script,
};
use routes_tenants::{
    create_tenant, delete_secret, delete_tenant, list_tokens, mint_token, put_secret, revoke_token,
    secret_names, set_limits, tenants,
};
use stream::exec_streaming;
use usage::{Usage, deliver_usage};

/// Version of the HTTP surface itself, bumped on any incompatible change.
///
/// Separate from the crate version and from [`CONTROL_VERSION`]: an SDK pinned
/// to this number should keep working across Zygo releases that only change
/// what happens behind the routes. `GET /version` reports it, and that is what
/// a client checks rather than parsing a release string.
///
/// [`CONTROL_VERSION`]: zygo_core::supervisor::CONTROL_VERSION
pub const API_VERSION: u32 = 1;

/// Idle control connections kept for reuse.
///
/// Each one pins a thread in the supervisor, so the pool that only ever grew
/// turned a burst of concurrent requests into a permanent thread count.
/// Above this, a finished connection is closed instead of kept.
pub(super) const MAX_IDLE_CLIENTS: usize = 32;

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
    // The first time this was hit, the warning would have been the difference
    // between a request refused and the whole API gone: systemd's default
    // `OOMPolicy=stop` stops a unit when any process in it is OOM-killed, and
    // a sandbox over its memory limit is exactly that. Said at start, where
    // the person starting the unit is reading.
    let oom_policy = zygo_core::doctor::oom_policy_of_sandboxes();
    if let zygo_core::doctor::OomPolicy::Policy { unit, policy } = &oom_policy
        && oom_policy.stops_the_unit()
    {
        crate::output::warn(&format!(
            "{} has OOMPolicy={policy}: one sandbox over its memory limit will stop this \
             unit, the supervisor and every pool with it. Set OOMPolicy=continue (and \
             Delegate=yes) in the unit; `zygo doctor` shows the same",
            unit.name
        ));
    }
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
            // made a momentary burst permanent.
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
    // two errors, so the rate on a dashboard was twice the truth.
    if response.status().is_server_error() {
        api.errors.fetch_add(1, Ordering::Relaxed);
    }
    Ok(response)
}

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
        (&Method::GET, ["metrics"]) => metrics(api, tenant).await,
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
            let params = CallParams::parse(&req, tenant)?;
            let body = read_body(req).await?;
            call_runtime(api, name, &body, params).await
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
