// SPDX-License-Identifier: Apache-2.0
//! The routes about runtime pools: `/runtimes` and `/runtimes/<name>/call`.
//!
//! A pool is an interpreter and a dependency set with nobody's code in it;
//! the code arrives with each call, as a digest the store holds or as
//! source for a one-off. This is the surface an embedder builds on without
//! touching the Zygo host's disk, which is why `base_dir` is optional here
//! and required on `PUT /fn/<name>`.

use std::sync::Arc;

use hyper::{Response, StatusCode};
use zygo_core::supervisor::{Request as Control, Response as Reply};

use super::reply::{ApiBody, HttpError, json, reply_to_response};
use super::{Api, control};
use zygo_core::spec::Layer;
use zygo_core::supervisor::WorkspaceRequest;

use super::request::CallParams;
use super::stream::exec_streaming;
use super::usage::count_usage;

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

pub(super) async fn serve_runtime(
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

pub(super) async fn runtimes(
    api: &Arc<Api>,
    tenant: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
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

pub(super) async fn stop_runtime(
    api: &Arc<Api>,
    name: String,
) -> Result<Response<ApiBody>, HttpError> {
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
pub(super) fn with_out(workspace: Option<WorkspaceRequest>, out: bool) -> Option<WorkspaceRequest> {
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

pub(super) async fn call_runtime(
    api: &Arc<Api>,
    name: String,
    body: &[u8],
    params: CallParams,
) -> Result<Response<ApiBody>, HttpError> {
    let CallParams {
        timeout_ms,
        tenant,
        key,
        streaming,
        out,
    } = params;
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
