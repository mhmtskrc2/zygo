//! The warm execution wire protocol (design doc §3.4).
//!
//! Deliberately small and language independent (ADR-009). Anything that speaks
//! it — the built-in Python agent, a Node agent, thirty lines of bash — gets
//! limits, timeouts, tiering, metrics and `vm` transport for free.
//!
//! Transport is a unix socket for `ns` and `gvisor`, vsock for `vm`. Framing is
//! in [`frame`]; the messages are here.
//!
//! ## Mandatory agent behaviours
//!
//! 1. Every request runs in its own process — or at minimum a pid that can be
//!    moved into its own cgroup.
//! 2. The child pid is reported with [`Message::Forked`], and the child does
//!    not start work until the supervisor has moved it into the request cgroup
//!    and sent [`Message::Go`].
//! 3. Results are JSON, with stdout and stderr in separate fields, plus exit
//!    code and resource measurements.
//! 4. The agent never handles a request in its own process, so its memory stays
//!    in the "just after a clean import" state.

pub mod frame;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use frame::{FrameError, FrameReader, FrameWriter, MAX_FRAME_BYTES, decode, encode};

/// Wire protocol version, announced in [`Message::Ready`]. A supervisor that
/// sees an unknown version refuses the agent rather than guessing.
pub const PROTOCOL_VERSION: u32 = 1;

/// A protocol message.
///
/// Serialised as a JSON object with a `type` discriminator, e.g.
/// `{"type":"READY","proto":1,"pid":42,...}`. A tagged representation keeps
/// hand-written agents simple: they can switch on one field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Message {
    /// Agent → supervisor, once, after imports finish.
    #[serde(rename = "READY")]
    Ready {
        proto: u32,
        pid: u32,
        /// How long the agent's one-time warm-up took.
        imports_ms: f64,
        rss_kb: u64,
        /// Free-form runtime identifier, e.g. `python/3.12.4`.
        runtime: String,
    },

    /// Supervisor → agent: run this event.
    #[serde(rename = "EXEC")]
    Exec {
        id: String,
        event: serde_json::Value,
        timeout_ms: u64,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        env_overrides: BTreeMap<String, String>,
    },

    /// Agent → supervisor: the child exists but has not started work.
    #[serde(rename = "FORKED")]
    Forked { id: String, pid: u32 },

    /// Supervisor → agent: the child is in its cgroup, it may proceed.
    #[serde(rename = "GO")]
    Go { id: String },

    /// Child → agent: the outcome.
    #[serde(rename = "RESULT")]
    Result {
        id: String,
        exit_code: i32,
        #[serde(default)]
        result: serde_json::Value,
        #[serde(default)]
        stdout: String,
        #[serde(default)]
        stderr: String,
        /// Set when the handler raised; `result` is then meaningless.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(flatten)]
        metrics: Metrics,
    },

    /// Agent → supervisor: same payload as `RESULT`, forwarded upwards.
    #[serde(rename = "DONE")]
    Done {
        id: String,
        exit_code: i32,
        #[serde(default)]
        result: serde_json::Value,
        #[serde(default)]
        stdout: String,
        #[serde(default)]
        stderr: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(flatten)]
        metrics: Metrics,
    },

    #[serde(rename = "PING")]
    Ping { seq: u64 },

    #[serde(rename = "PONG")]
    Pong { seq: u64 },

    /// Supervisor → agent: finish in-flight requests, then exit.
    #[serde(rename = "SHUTDOWN")]
    Shutdown {
        #[serde(default)]
        grace_ms: u64,
    },

    /// Either direction: a protocol-level failure, not a handler failure.
    #[serde(rename = "ERROR")]
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        code: ErrorCode,
        message: String,
    },
}

/// Per-request measurements. Flattened into `RESULT`/`DONE`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Metrics {
    #[serde(default)]
    pub peak_rss_kb: u64,
    #[serde(default)]
    pub wall_ms: f64,
    #[serde(default)]
    pub cpu_ms: f64,
}

/// Protocol-level error codes. A closed set, so a supervisor can map them onto
/// HTTP status codes without string matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The message could not be parsed or was not expected here.
    BadMessage,
    /// The wire protocol version is not supported.
    UnsupportedVersion,
    /// The handler could not be imported; the agent is unusable.
    HandlerLoad,
    /// `fork()` or the spawn fallback failed.
    SpawnFailed,
    /// The request exceeded `timeout_ms`.
    Timeout,
    /// Too many in-flight requests for this agent's `concurrency`.
    Overloaded,
    /// The result was not JSON-serialisable.
    BadResult,
    /// Anything else. The message carries the detail.
    Internal,
}

impl Message {
    /// Correlation id, where the message has one.
    pub fn request_id(&self) -> Option<&str> {
        match self {
            Message::Exec { id, .. }
            | Message::Forked { id, .. }
            | Message::Go { id }
            | Message::Result { id, .. }
            | Message::Done { id, .. } => Some(id),
            Message::Error { id, .. } => id.as_deref(),
            _ => None,
        }
    }

    /// Discriminator, for logs and metrics labels.
    pub const fn kind(&self) -> &'static str {
        match self {
            Message::Ready { .. } => "READY",
            Message::Exec { .. } => "EXEC",
            Message::Forked { .. } => "FORKED",
            Message::Go { .. } => "GO",
            Message::Result { .. } => "RESULT",
            Message::Done { .. } => "DONE",
            Message::Ping { .. } => "PING",
            Message::Pong { .. } => "PONG",
            Message::Shutdown { .. } => "SHUTDOWN",
            Message::Error { .. } => "ERROR",
        }
    }

    /// Turn a child's `RESULT` into the `DONE` the supervisor sees. The agent
    /// is a pass-through here; keeping it a single call means an agent cannot
    /// accidentally drop a field on the way up.
    pub fn result_into_done(self) -> Option<Message> {
        match self {
            Message::Result {
                id,
                exit_code,
                result,
                stdout,
                stderr,
                error,
                metrics,
            } => Some(Message::Done {
                id,
                exit_code,
                result,
                stdout,
                stderr,
                error,
                metrics,
            }),
            _ => None,
        }
    }
}

/// Errors raised while speaking the protocol.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("protocol framing: {0}")]
    Frame(#[from] FrameError),

    #[error("agent speaks protocol version {found}, this build speaks {expected}")]
    VersionMismatch { found: u32, expected: u32 },

    #[error("expected {expected} from the agent, got {found}")]
    Unexpected {
        expected: &'static str,
        found: &'static str,
    },

    #[error("agent error [{code:?}]: {message}")]
    Agent { code: ErrorCode, message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ready_roundtrips() {
        let m = Message::Ready {
            proto: PROTOCOL_VERSION,
            pid: 42,
            imports_ms: 312.5,
            rss_kb: 41200,
            runtime: "python/3.12.4".into(),
        };
        let text = serde_json::to_string(&m).unwrap();
        assert!(text.contains(r#""type":"READY""#), "{text}");
        assert_eq!(serde_json::from_str::<Message>(&text).unwrap(), m);
    }

    #[test]
    fn exec_omits_empty_env_overrides() {
        let m = Message::Exec {
            id: "01f3".into(),
            event: json!({"url": "https://example.com"}),
            timeout_ms: 30_000,
            env_overrides: BTreeMap::new(),
        };
        let text = serde_json::to_string(&m).unwrap();
        assert!(!text.contains("env_overrides"), "{text}");
        assert_eq!(serde_json::from_str::<Message>(&text).unwrap(), m);
    }

    #[test]
    fn metrics_are_flattened_onto_result() {
        let m = Message::Result {
            id: "a".into(),
            exit_code: 0,
            result: json!({"status": 200}),
            stdout: "hi\n".into(),
            stderr: String::new(),
            error: None,
            metrics: Metrics {
                peak_rss_kb: 41200,
                wall_ms: 12.3,
                cpu_ms: 9.1,
            },
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        // Flattened, not nested: an agent writes `wall_ms` at the top level.
        assert_eq!(v["wall_ms"], 12.3);
        assert_eq!(v["peak_rss_kb"], 41200);
        assert!(v.get("metrics").is_none());
    }

    /// A hand-written agent should be able to emit the minimum and be understood.
    #[test]
    fn minimal_agent_output_is_accepted() {
        let m: Message =
            serde_json::from_str(r#"{"type":"RESULT","id":"a","exit_code":0}"#).unwrap();
        match m {
            Message::Result {
                result,
                stdout,
                metrics,
                error,
                ..
            } => {
                assert!(result.is_null());
                assert_eq!(stdout, "");
                assert_eq!(metrics, Metrics::default());
                assert!(error.is_none());
            }
            other => panic!("expected RESULT, got {other:?}"),
        }
    }

    #[test]
    fn result_converts_to_done_without_losing_fields() {
        let r = Message::Result {
            id: "a".into(),
            exit_code: 1,
            result: json!(null),
            stdout: "out".into(),
            stderr: "err".into(),
            error: Some("ValueError: boom".into()),
            metrics: Metrics {
                peak_rss_kb: 1,
                wall_ms: 2.0,
                cpu_ms: 3.0,
            },
        };
        match r.result_into_done().unwrap() {
            Message::Done {
                id,
                exit_code,
                stdout,
                stderr,
                error,
                metrics,
                ..
            } => {
                assert_eq!(id, "a");
                assert_eq!(exit_code, 1);
                assert_eq!(stdout, "out");
                assert_eq!(stderr, "err");
                assert_eq!(error.as_deref(), Some("ValueError: boom"));
                assert_eq!(metrics.cpu_ms, 3.0);
            }
            other => panic!("expected DONE, got {other:?}"),
        }
    }

    #[test]
    fn request_ids_are_exposed_for_correlation() {
        assert_eq!(Message::Go { id: "x".into() }.request_id(), Some("x"));
        assert_eq!(Message::Ping { seq: 1 }.request_id(), None);
        assert_eq!(
            Message::Error {
                id: None,
                code: ErrorCode::BadMessage,
                message: "nope".into()
            }
            .request_id(),
            None
        );
    }

    #[test]
    fn error_codes_use_stable_snake_case_wire_names() {
        let text = serde_json::to_string(&ErrorCode::UnsupportedVersion).unwrap();
        assert_eq!(text, r#""unsupported_version""#);
    }
}
