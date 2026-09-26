// SPDX-License-Identifier: Apache-2.0
//! The routes about tenants: `/tenants`, their limits, secrets and tokens.
//!
//! A tenant is an embedder's customer, and every route here is the
//! operator's except reading one's own record and one's own secret
//! *names*. A secret's value is the body of a `PUT` and is never answered
//! back; a token's secret is answered exactly once, at minting.

use std::sync::Arc;

use hyper::{Response, StatusCode};
use zygo_core::supervisor::{Request as Control, Response as Reply};

use super::reply::{ApiBody, HttpError, json, reply_to_response};
use super::{Api, control};

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
pub(super) async fn create_tenant(
    api: &Arc<Api>,
    body: &[u8],
) -> Result<Response<ApiBody>, HttpError> {
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

pub(super) async fn tenants(
    api: &Arc<Api>,
    id: Option<String>,
) -> Result<Response<ApiBody>, HttpError> {
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

pub(super) async fn delete_tenant(
    api: &Arc<Api>,
    id: String,
) -> Result<Response<ApiBody>, HttpError> {
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
pub(super) async fn set_limits(
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
pub(super) async fn put_secret(
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
pub(super) async fn secret_names(
    api: &Arc<Api>,
    tenant: String,
) -> Result<Response<ApiBody>, HttpError> {
    let reply = control(api, move |c| Ok(c.send(&Control::Secrets { tenant })?)).await?;
    Ok(reply_to_response(reply))
}

pub(super) async fn delete_secret(
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
pub(super) async fn mint_token(
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
pub(super) async fn list_tokens(api: &Arc<Api>) -> Result<Response<ApiBody>, HttpError> {
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
pub(super) async fn revoke_token(
    api: &Arc<Api>,
    id: String,
) -> Result<Response<ApiBody>, HttpError> {
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
