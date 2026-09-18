//! The control protocol: CLI ↔ supervisor (design doc §3.4).
//!
//! Deliberately **not** the agent wire protocol. The agent runs tenant code, so
//! it must never be able to say "stop that other function" or "serve this spec";
//! keeping the two message sets disjoint makes that a type error rather than a
//! review item. Only the framing is shared ([`crate::protocol::frame`]).
//!
//! The CLI sends the *inputs* to resolution — a spec file's contents and the
//! layer its flags built — rather than a resolved function. The supervisor is
//! the authority on what a function is, so it does the resolving, and the
//! warnings come back over the wire. The cost is that the CLI has to make paths
//! absolute first: the supervisor's working directory is its own.

use serde::{Deserialize, Serialize};

use crate::pool::{Outcome, Status};
use crate::spec::{Layer, Spec};

/// Control protocol version, bumped on any incompatible change.
///
/// Separate from [`crate::protocol::PROTOCOL_VERSION`]: a third-party agent and
/// the CLI evolve independently, and tying them together would mean an agent
/// author had to care about `zygo ps`.
pub const CONTROL_VERSION: u32 = 1;

/// CLI → supervisor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "UPPERCASE")]
pub enum Request {
    /// First frame on every connection. Checked before anything else runs.
    Hello { control: u32, client: String },

    /// Warm a function up, replacing any function already under that name.
    Serve {
        name: String,
        /// Contents of the spec file, when one was given.
        spec: Option<Box<Spec>>,
        /// What the flags said.
        layer: Box<Layer>,
        /// The directory the client's relative paths are relative to.
        ///
        /// The supervisor has its own working directory and generally is not
        /// even in the same one, so `--mount ./data:/data` would otherwise
        /// resolve somewhere nobody meant. Carrying it explicitly keeps the
        /// client's view of the filesystem the authority on what it asked for.
        base_dir: std::path::PathBuf,
        /// The `--allow-*` flags: each removes a guarantee, so they travel as
        /// data rather than being inferred from the layer.
        allow_host_net: bool,
        allow_private_net: bool,
        allow_unlimited: bool,
    },

    /// Call a warm function.
    Exec {
        name: String,
        event: serde_json::Value,
        timeout_ms: u64,
    },

    /// Everything `zygo ps` shows.
    List,

    /// Shut a function down, or all of them.
    Stop { name: Option<String> },

    /// Ask the supervisor itself to exit once in-flight requests finish.
    Shutdown,

    /// Liveness probe, used by the client to decide whether an existing socket
    /// belongs to a supervisor that is actually running.
    Ping,
}

/// Supervisor → CLI. Exactly one per request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "UPPERCASE")]
pub enum Response {
    /// Answer to `Hello`.
    Welcome {
        control: u32,
        version: String,
        pid: u32,
    },

    /// A function is warm and ready.
    Served {
        name: String,
        runtime: String,
        rss_kb: u64,
        imports_ms: f64,
        /// How long the warm-up took, which is the cold start this saves later.
        warm_ms: f64,
        /// Non-fatal resolution notes. The CLI prints them.
        warnings: Vec<String>,
    },

    /// A request ran. `outcome.succeeded()` says whether the handler liked it.
    Executed { outcome: Box<Outcome> },

    Functions { functions: Vec<Status> },

    Stopped { names: Vec<String> },

    Pong,

    Ok,

    /// The tenant is at its concurrency limit and its queue is full.
    ///
    /// The design document's `429`: a distinct response rather than an error,
    /// because the caller's correct reaction is to retry rather than to give up,
    /// and a platform routing to a second machine needs to tell the two apart.
    Busy {
        name: String,
        in_flight: u32,
        queued: u32,
        limit: u32,
    },

    Error {
        code: ControlError,
        message: String,
    },
}

/// Stable machine-readable failure kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlError {
    /// The client speaks a control version this supervisor does not.
    VersionMismatch,
    /// No function registered under that name.
    NotFound,
    /// The spec did not resolve.
    BadSpec,
    /// Warming the sandbox failed.
    WarmFailed,
    /// The request reached the function but the call failed.
    CallFailed,
    /// A frame arrived out of order — anything before `Hello`, for instance.
    BadMessage,
    /// The connection is not from the user who owns the supervisor.
    Unauthorised,
}

impl ControlError {
    pub const fn as_str(self) -> &'static str {
        match self {
            ControlError::VersionMismatch => "version_mismatch",
            ControlError::NotFound => "not_found",
            ControlError::BadSpec => "bad_spec",
            ControlError::WarmFailed => "warm_failed",
            ControlError::CallFailed => "call_failed",
            ControlError::BadMessage => "bad_message",
            ControlError::Unauthorised => "unauthorised",
        }
    }
}

impl Response {
    /// Build an error response from anything printable.
    pub fn error(code: ControlError, message: impl std::fmt::Display) -> Response {
        Response::Error {
            code,
            message: message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_request(r: &Request) -> Request {
        let bytes = crate::protocol::frame::encode(r).expect("encode");
        // Skip the 4-byte length prefix the framing added.
        crate::protocol::frame::decode(&bytes[4..]).expect("decode")
    }

    fn roundtrip_response(r: &Response) -> Response {
        let bytes = crate::protocol::frame::encode(r).expect("encode");
        crate::protocol::frame::decode(&bytes[4..]).expect("decode")
    }

    #[test]
    fn every_request_survives_the_wire() {
        let requests = [
            Request::Hello {
                control: CONTROL_VERSION,
                client: "zygo 0.1.0".into(),
            },
            Request::Serve {
                name: "resize".into(),
                spec: None,
                layer: Box::new(Layer {
                    image: Some("python:3.12-slim".into()),
                    ..Default::default()
                }),
                base_dir: "/home/me/project".into(),
                allow_host_net: false,
                allow_private_net: true,
                allow_unlimited: false,
            },
            Request::Exec {
                name: "resize".into(),
                event: serde_json::json!({ "url": "https://example.com" }),
                timeout_ms: 30_000,
            },
            Request::List,
            Request::Stop {
                name: Some("resize".into()),
            },
            Request::Stop { name: None },
            Request::Shutdown,
            Request::Ping,
        ];
        for r in &requests {
            assert_eq!(&roundtrip_request(r), r, "{r:?}");
        }
    }

    #[test]
    fn every_response_survives_the_wire() {
        let responses = [
            Response::Welcome {
                control: CONTROL_VERSION,
                version: "0.1.0".into(),
                pid: 42,
            },
            Response::Served {
                name: "resize".into(),
                runtime: "python3.12".into(),
                rss_kb: 15_000,
                imports_ms: 120.5,
                warm_ms: 310.0,
                warnings: vec!["scratch is larger than memory".into()],
            },
            Response::Functions {
                functions: vec![Status {
                    name: "resize".into(),
                    state: crate::sandbox::SandboxState::Warm,
                    runtime: "python3.12".into(),
                    rss_kb: 15_000,
                    imports_ms: 120.5,
                    requests: 9,
                    failures: 1,
                }],
            },
            Response::Stopped {
                names: vec!["resize".into()],
            },
            Response::Busy {
                name: "resize".into(),
                in_flight: 4,
                queued: 16,
                limit: 4,
            },
            Response::Pong,
            Response::Ok,
            Response::error(ControlError::NotFound, "no function named `resize`"),
        ];
        for r in &responses {
            assert_eq!(&roundtrip_response(r), r, "{r:?}");
        }
    }

    #[test]
    fn an_outcome_crosses_the_control_socket_intact() {
        // `zygo exec` prints this, so a field lost in transit is a wrong answer
        // rather than an error.
        let outcome = Outcome {
            exit_code: 0,
            result: serde_json::json!({ "ok": true, "n": 3 }),
            stdout: "hello\n".into(),
            stderr: String::new(),
            error: None,
            metrics: crate::protocol::Metrics {
                wall_ms: 1.25,
                ..Default::default()
            },
        };
        let sent = Response::Executed {
            outcome: Box::new(outcome.clone()),
        };
        match roundtrip_response(&sent) {
            Response::Executed { outcome: got } => {
                assert_eq!(*got, outcome);
                assert!(got.succeeded());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_failed_outcome_keeps_its_error_across_the_wire() {
        let outcome = Outcome {
            exit_code: 1,
            result: serde_json::Value::Null,
            stdout: String::new(),
            stderr: "Traceback...\n".into(),
            error: Some("ZeroDivisionError: division by zero".into()),
            metrics: crate::protocol::Metrics::default(),
        };
        let sent = Response::Executed {
            outcome: Box::new(outcome),
        };
        match roundtrip_response(&sent) {
            Response::Executed { outcome } => {
                assert!(!outcome.succeeded());
                assert_eq!(
                    outcome.error.as_deref(),
                    Some("ZeroDivisionError: division by zero")
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_tag_names_are_the_stable_part_of_the_protocol() {
        // A third-party client reads these strings. Renaming a variant must not
        // silently rename the wire.
        let json = serde_json::to_value(Request::Ping).expect("json");
        assert_eq!(json["type"], "PING");

        let json = serde_json::to_value(Response::Busy {
            name: "x".into(),
            in_flight: 1,
            queued: 0,
            limit: 1,
        })
        .expect("json");
        assert_eq!(json["type"], "BUSY");

        let json = serde_json::to_value(Response::error(ControlError::NotFound, "x")).expect("json");
        assert_eq!(json["type"], "ERROR");
        assert_eq!(json["code"], "not_found");
    }

    #[test]
    fn error_codes_have_stable_names() {
        for (code, want) in [
            (ControlError::VersionMismatch, "version_mismatch"),
            (ControlError::NotFound, "not_found"),
            (ControlError::BadSpec, "bad_spec"),
            (ControlError::WarmFailed, "warm_failed"),
            (ControlError::CallFailed, "call_failed"),
            (ControlError::BadMessage, "bad_message"),
            (ControlError::Unauthorised, "unauthorised"),
        ] {
            assert_eq!(code.as_str(), want);
            assert_eq!(
                serde_json::to_value(code).expect("json"),
                serde_json::Value::String(want.into()),
                "`as_str` and serde must not drift apart"
            );
        }
    }

    #[test]
    fn an_unknown_message_type_is_rejected_rather_than_guessed() {
        let body = br#"{"type":"DESTROY_EVERYTHING"}"#;
        assert!(crate::protocol::frame::decode::<Request>(body).is_err());
    }

    #[test]
    fn an_agent_wire_message_is_not_a_control_message() {
        // The whole point of two protocols: tenant code that gets hold of the
        // agent socket still cannot say anything the supervisor will act on.
        let exec = crate::protocol::Message::Exec {
            id: "1".into(),
            event: serde_json::Value::Null,
            timeout_ms: 1000,
            env_overrides: Default::default(),
        };
        let bytes = crate::protocol::frame::encode(&exec).expect("encode");
        assert!(
            crate::protocol::frame::decode::<Request>(&bytes[4..]).is_err(),
            "an agent `EXEC` must not deserialise as a control request"
        );
    }
}
