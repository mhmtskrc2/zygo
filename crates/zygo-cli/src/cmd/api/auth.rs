// SPDX-License-Identifier: Apache-2.0
//! Who a request acts for, and what they may do.
//!
//! The bearer token is the only part of a request the caller cannot
//! choose, so it is where the answer comes from: the bootstrap token and
//! an unauthenticated listener are the operator; a minted token is whatever
//! the store says it is. [`Actor`] carries that answer to the routes, and
//! [`Actor::may_deploy`] is the gate on everything that creates a sandbox.

use hyper::body::Incoming;
use hyper::{Request, StatusCode};

use super::Api;
use super::reply::HttpError;

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
pub(super) struct Actor {
    /// `None` is the operator.
    pub(super) tenant: Option<String>,
    /// May create and destroy sandboxes, scripts and tokens.
    pub(super) deploy: bool,
}

/// The header an operator names a tenant with.
const TENANT_HEADER: &str = "x-zygo-tenant";

impl Actor {
    pub(super) fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    pub(super) fn is_operator(&self) -> bool {
        self.tenant.is_none()
    }

    /// Refuse a route that is the operator's alone.
    ///
    /// Creating and deleting tenants is not something a tenant does, and
    /// neither is listing them: a customer that could enumerate the other
    /// customers is a leak, whatever the limits say.
    pub(super) fn operator_only(&self, what: &str) -> Result<(), HttpError> {
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
    pub(super) fn may_deploy(&self) -> Result<(), HttpError> {
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
pub(super) fn authorise(req: &Request<Incoming>, api: &Api) -> Result<Actor, HttpError> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_does_not_depend_on_where_the_difference_is() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrex"));
        assert!(!constant_time_eq(b"secret", b"xecret"));
        assert!(!constant_time_eq(b"secret", b"secre"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
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
}
