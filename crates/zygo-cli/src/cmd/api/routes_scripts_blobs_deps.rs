// SPDX-License-Identifier: Apache-2.0
//! The routes about content: `/scripts`, `/blobs` and `/deps`.
//!
//! Bytes a caller registers once and names by digest afterwards. All
//! three are content-addressed and idempotent — `201` when the host did
//! not have it, `200` when it did — and registering runs nothing, which
//! is why a tenant token may. Forgetting one is the operator's, because
//! the stores are shared by digest.

use std::sync::Arc;

use hyper::{Response, StatusCode};
use zygo_core::supervisor::{Request as Control, Response as Reply};

use super::reply::{ApiBody, HttpError, json, reply_to_response};
use super::{Api, control};

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
pub(super) async fn put_deps(
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
pub(super) async fn deps(
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
pub(super) async fn delete_deps(
    api: &Arc<Api>,
    id: String,
) -> Result<Response<ApiBody>, HttpError> {
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

/// `PUT /blobs`: the body **is** the tar, and the answer is its name.
///
/// Not JSON around it, for the reason `PUT /scripts` gives: a blob is bytes,
/// the caller has them as bytes, and wrapping them to unwrap them again is a
/// transformation with no reader. It is the one route whose body is binary.
pub(super) async fn put_blob(api: &Arc<Api>, body: &[u8]) -> Result<Response<ApiBody>, HttpError> {
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

pub(super) async fn get_blob(
    api: &Arc<Api>,
    digest: String,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::GetBlob { digest })?)).await?;
    Ok(reply_to_response(reply))
}

pub(super) async fn delete_blob(
    api: &Arc<Api>,
    digest: String,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::DeleteBlob { digest })?)).await?;
    match reply {
        Reply::Ok => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "deleted": true }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

pub(super) async fn put_script(
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
pub(super) async fn get_script(
    api: &Arc<Api>,
    digest: String,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::GetScript { digest })?)).await?;
    match reply {
        Reply::Script { digest, size, .. } => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "sha256": digest, "size": size }),
        )),
        other => Ok(reply_to_response(other)),
    }
}

pub(super) async fn delete_script(
    api: &Arc<Api>,
    digest: String,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::DeleteScript { digest })?)).await?;
    match reply {
        Reply::Ok => Ok(json(
            StatusCode::OK,
            &serde_json::json!({ "deleted": true }),
        )),
        other => Ok(reply_to_response(other)),
    }
}
