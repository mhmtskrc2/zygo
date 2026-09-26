// SPDX-License-Identifier: Apache-2.0
//! Reading a request: its body, its event, its headers, its query string.
//!
//! Every limit a caller can push against is here — the body size, the
//! timeout ceiling, the shape of a request key — and every refusal names
//! the limit, because a caller that asked for a day and was given an hour
//! would report the wrong thing when the wait ended.

use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use hyper::{Request, StatusCode};
use zygo_core::supervisor::WorkspaceRequest;

use super::reply::HttpError;

/// Largest request body accepted. An event bigger than this belongs in a
/// scratch file, and the supervisor's own frame limit is the same order.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// The client's wait when it did not say. The function's own `timeout` is the
/// real limit and the supervisor enforces it; this only bounds the HTTP hold.
pub(super) const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Longest `X-Zygo-Timeout-Ms` a caller may ask for.
///
/// The function's own `timeout` is the real limit and the supervisor enforces
/// it; this header only says how long the caller will wait. Unbounded, it is a
/// way to hold a connection and a supervisor thread for as long as you like.
///
/// A day rather than the hour it was. An embedder's long jobs — a render, a
/// migration, a model run — are hours, and an hour was a ceiling they hit for
/// no reason this API had: the *function's* `timeout` was always the limit
/// that mattered. What made an hour safe to raise is the heartbeat
/// (`pool::HEARTBEAT_GRACE`): a wedged request is now killed in a minute
/// whatever its budget says, so the ceiling no longer doubles as the only
/// backstop against holding a slot for ever.
pub(super) const MAX_TIMEOUT_MS: u64 = 24 * 3_600_000;

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
pub(super) const REQUEST_KEY_HEADER: &str = "x-zygo-request-key";

/// The header a request's own id comes back in.
///
/// What `DELETE /requests/<id>` names. Of no use to the caller of *this* call,
/// which has already finished by the time a header arrives — that is what
/// `X-Zygo-Request-Key` is for. It is here for anything joining a log line
/// back to the request it describes, and for a proxy that tees the answer.
pub(super) const REQUEST_ID_HEADER: &str = "x-zygo-request-id";

/// The caller's own name for this request, if they gave one.
///
/// Bounded and restricted to printable ASCII, because it is compared against
/// request ids and appears in log lines: a key with a newline in it would be a
/// caller writing into the supervisor's log.
pub(super) fn request_key(req: &Request<Incoming>) -> Result<Option<String>, HttpError> {
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

pub(super) fn timeout_header(req: &Request<Incoming>) -> Result<u64, HttpError> {
    timeout_header_from(req.headers())
}

/// The headers alone, so this is reachable from a test.
///
/// `Request<Incoming>` cannot be built outside a server, which is why the
/// ceiling below went untested until there was a ceiling to test.
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

pub(super) async fn read_body(req: Request<Incoming>) -> Result<Bytes, HttpError> {
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
pub(super) fn parse_event(body: &[u8]) -> Result<serde_json::Value, HttpError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(body)
        .map_err(|e| HttpError::new(StatusCode::BAD_REQUEST, format!("body is not JSON: {e}")))
}

/// A query string, parsed once. Percent-decoding is deliberately absent: every
/// parameter here is a number or a boolean, and a `%` in one is a mistake
/// worth reporting rather than decoding.
pub(super) struct Query<'a>(Vec<(&'a str, &'a str)>);

impl<'a> Query<'a> {
    pub(super) fn parse(query: &'a str) -> Query<'a> {
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

    pub(super) fn number(&self, key: &str) -> Result<Option<u64>, HttpError> {
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
    pub(super) fn blob(&self, key: &str) -> Result<Option<WorkspaceRequest>, HttpError> {
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

    pub(super) fn flag(&self, key: &str) -> Result<bool, HttpError> {
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

/// What a call route reads before its body: how the request is to be run.
///
/// The headers and query parameters `POST /runtimes/<name>/call` reads, in
/// the order it reads them, so a caller who got two of them wrong is told
/// about the same one every time. One struct rather than five arguments
/// handed down the route, so the handler's signature says what a call is.
pub(super) struct CallParams {
    /// `X-Zygo-Timeout-Ms`, or the default. How long the caller waits.
    pub(super) timeout_ms: u64,
    /// Whose request this is, as the token settled it.
    pub(super) tenant: Option<String>,
    /// `X-Zygo-Request-Key`: the caller's own name for the request.
    pub(super) key: Option<String>,
    /// `?stream=1`: answer as output is produced.
    pub(super) streaming: bool,
    /// `?out=1`: send the request's workspace back.
    pub(super) out: bool,
}

impl CallParams {
    pub(super) fn parse(
        req: &Request<Incoming>,
        tenant: Option<String>,
    ) -> Result<CallParams, HttpError> {
        let timeout_ms = timeout_header(req)?;
        let key = request_key(req)?;
        let query = Query::parse(req.uri().query().unwrap_or(""));
        let streaming = query.flag("stream")?;
        let out = query.flag("out")?;
        Ok(CallParams {
            timeout_ms,
            tenant,
            key,
            streaming,
            out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::MAX_IDLE_CLIENTS;
    use super::super::routes_fn::{BATCH_IN_FLIGHT, MAX_BATCH};
    use super::*;

    #[test]
    fn an_empty_body_is_a_null_event() {
        assert_eq!(parse_event(b"").unwrap(), serde_json::Value::Null);
        assert_eq!(parse_event(b"  \n").unwrap(), serde_json::Value::Null);
        assert_eq!(parse_event(br#"{"n":1}"#).unwrap()["n"], 1);
        assert!(parse_event(b"{nope").is_err());
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

    /// The ceilings that stop one caller from spending the host's resources.
    ///
    /// Each is a number that was absent until the code review, and each
    /// absence had the same shape: a request that costs the *server* more the
    /// larger the caller makes it.
    #[test]
    fn a_caller_cannot_ask_for_an_unbounded_amount_of_work() {
        // The wait a caller may ask for. Refused rather than clamped —
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

        // The two ceilings that bound supervisor connections.
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
