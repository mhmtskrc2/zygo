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

/// Environment variable through which the supervisor hands a runtime agent the
/// seccomp program its forked child must install before the handler runs:
/// base64 of the raw `struct sock_filter` array, in the host's byte order.
/// Absent when the function's profile does not tighten the child. The agent
/// contract is in `spec/protocol.md` §3.
pub const CHILD_SECCOMP_ENV: &str = "ZYGO_CHILD_SECCOMP";

/// The code one request runs, when the zygote does not already hold it.
///
/// **The child loads this, after the fork.** Not the agent, and not before
/// `GO`: a zygote that imported a tenant's script would be a zygote that
/// tenant's next request could observe, and a zygote shared between tenants
/// would be a place one could leave something for another. The whole point of
/// a runtime pool is that the warm process is anonymous — an interpreter and
/// its dependency set, and nothing of anybody's.
///
/// Two shapes, and the difference is who holds the bytes:
///
/// * `path` — the supervisor wrote the script into the sandbox before `GO`,
///   `0400`, in a directory that cannot be listed, and this names it. The
///   preferred shape, and the reason is which processes have the bytes: only
///   the child does. A `source` on the wire is read into the *zygote's*
///   address space to be forwarded, and in a runtime pool the zygote is shared
///   — so the next tenant's fork inherits a copy-on-write view of a heap that
///   held this tenant's code.
/// * `source` — the text itself, on the wire. Needs no writable path into the
///   sandbox, which is what makes it the fallback where there is none, and
///   what makes it right for a one-off.
///
/// At least one is set. Both being absent is the same as no `script` at all,
/// and is refused rather than guessed at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Script {
    /// Where the child can read it, inside the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The script itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// `sha256:…` of the contents, which the child checks before it loads them.
    ///
    /// Load-bearing for `path`, and the reason is that the file is *not*
    /// protected from the tenant. A sandbox has one uid: the child that is
    /// about to load `/run/script/<hash>` can unlink it and write its own in
    /// its place, for itself or for another request in flight on the same pool
    /// zygote. What it cannot do is reach this field, which arrives on the
    /// supervisor's connection — so a script whose bytes do not hash to it is
    /// refused rather than run. Under `source` the check is self-consistency
    /// and costs a hash; it is kept so that one rule covers both shapes.
    ///
    /// Also an identity: an agent may key a compiled-code cache *in the child*
    /// on it, and a log line that carries it says which version of a script
    /// failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// The entry point to call, when it is not `handler`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_point: Option<String>,
}

impl Script {
    /// A script the `EXEC` carries the bytes of.
    ///
    /// The one-off shape. The supervisor turns this into the `path` shape
    /// wherever it can write into the sandbox; see `pool::WarmFn::place_script`.
    pub fn inline(source: impl Into<String>) -> Script {
        Script {
            path: None,
            source: Some(source.into()),
            digest: None,
            entry_point: None,
        }
    }

    /// A script already in the sandbox, named and digested.
    pub fn at(path: impl Into<String>, digest: impl Into<String>) -> Script {
        Script {
            path: Some(path.into()),
            source: None,
            digest: Some(digest.into()),
            entry_point: None,
        }
    }

    /// Whether this names something the child could actually load.
    pub fn is_loadable(&self) -> bool {
        self.path.is_some() || self.source.is_some()
    }
}

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
        /// The code to run, when it did not come with the zygote (proto 1.1).
        ///
        /// Absent is the original shape and the fast path: the agent was
        /// started with a handler, imported it once, and every request is a
        /// fork of that. Present is the *runtime pool* shape: one zygote per
        /// image-and-dependency-set, and the script arrives with the request.
        ///
        /// An embedder has ten thousand scripts and cannot hold ten thousand
        /// zygotes — measured at 9.98 MB of PSS each, which is 97 GiB at that
        /// count (`docs/bench-embed.md`). This field is how one zygote serves
        /// all of them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script: Option<Script>,
        /// Send output as it is produced, in `CHUNK` frames (proto 1.3).
        ///
        /// Off by default and asked for per request, not per function: a
        /// `CHUNK` per `print()` is a syscall per `print()`, and the warm path
        /// is measured in milliseconds. A caller that wants to watch a long
        /// request pays for it; everybody else keeps the path that was
        /// measured.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stream: bool,
        /// This request's own directory inside the sandbox (proto 1.5).
        ///
        /// Where the caller's files were unpacked and where the handler leaves
        /// whatever it wants back. The agent hands the path to the child in
        /// `ZYGO_WORKSPACE` and makes it the child's working directory.
        ///
        /// **Not `/work`**, and it could not be: making one path mean a
        /// different directory to each request needs a mount namespace per
        /// request, and a forked child has no capability to create one —
        /// measured, see `crate::workspace`. The name is 128 random bits and
        /// `/work` cannot be listed, so a neighbour in the same sandbox can
        /// neither find it nor guess it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },

    /// Agent → supervisor: the child exists but has not started work.
    #[serde(rename = "FORKED")]
    Forked { id: String, pid: u32 },

    /// Supervisor → agent: the child is in its cgroup, it may proceed.
    #[serde(rename = "GO")]
    Go { id: String },

    /// Supervisor → agent: stop this request (proto 1.2).
    ///
    /// **Not** how the request is killed. Zygo kills it by writing
    /// `cgroup.kill` on the request's own cgroup, from outside the sandbox,
    /// because trusting the agent to stop tenant code is trusting the blast
    /// radius to contain itself — the same reason `timeout_ms` is a courtesy
    /// rather than a control.
    ///
    /// What this frame is for is the *answer*: an agent that has been told
    /// sets `cancelled` on the `DONE` it synthesises, so the caller learns
    /// that their own cancel is why the request stopped rather than reading a
    /// 137 and guessing between a deadline and an out-of-memory kill. An
    /// agent that ignores it is still conforming, and the supervisor fills in
    /// `cancelled` itself — it knows, because it is the one that asked.
    #[serde(rename = "CANCEL")]
    Cancel { id: String },

    /// Child → agent → supervisor: output, as it is produced (proto 1.3).
    ///
    /// Only sent when the `EXEC` asked for it. Without `stream`, output is
    /// captured in the child and arrives once, in `RESULT` — which is the
    /// cheaper path and stays the default, because a `CHUNK` per `print()` is
    /// a syscall per `print()` on a request measured in milliseconds.
    ///
    /// The agent **forwards without buffering**: the value of a stream is that
    /// a caller sees the first line before the last one exists, and an agent
    /// that accumulated would deliver the same bytes at the same time as
    /// `RESULT` does.
    ///
    /// `RESULT` still carries the whole of `stdout` and `stderr` afterwards,
    /// bounded as always. A caller that streamed has seen it; one that did not
    /// gets it the usual way; and neither has to reassemble anything to know
    /// what the request printed.
    #[serde(rename = "CHUNK")]
    Chunk {
        id: String,
        stream: Stream,
        /// The text itself. Not framed by line: a handler that writes half a
        /// line and then blocks should have that half line delivered.
        data: String,
    },

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
        /// A `CANCEL` for this id is why it stopped (proto 1.2).
        ///
        /// Optional, and the supervisor does not depend on it: it knows
        /// whether it cancelled this request. It is here so that an agent
        /// which *does* track cancellation can say so, and so a third-party
        /// supervisor reading this protocol has the fact on the wire.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cancelled: bool,
        #[serde(flatten)]
        metrics: Metrics,
    },

    /// Liveness, both ways.
    ///
    /// Without `id` it is the supervisor asking whether the *agent* is alive,
    /// which is what it has always been.
    ///
    /// With one it is the agent saying that a *request* is alive (proto 1.4):
    /// a heartbeat on behalf of a child that is still running. It exists
    /// because raising the timeout ceiling to hours made the deadline a poor
    /// backstop — a request wedged in the first minute of a six-hour budget
    /// holds its slot for the rest of it. A supervisor that has heard nothing
    /// for its grace period kills the request as **stuck**, which is a
    /// different fact from "too slow" and gets a different answer.
    #[serde(rename = "PING")]
    Ping {
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },

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

/// Which of a request's streams a `CHUNK` belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
    /// The handler's own progress reports, which are not output.
    ///
    /// A long request has two things to say — what it printed, and how far it
    /// has got — and conflating them means a caller has to parse the first to
    /// find the second. This is the second, and an agent exposes it to the
    /// handler as a call rather than as a stream to write to.
    Progress,
}

impl Stream {
    pub const fn as_str(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
            Stream::Progress => "progress",
        }
    }
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
            | Message::Cancel { id }
            | Message::Chunk { id, .. }
            | Message::Result { id, .. }
            | Message::Done { id, .. } => Some(id),
            Message::Error { id, .. } => id.as_deref(),
            // A heartbeat *for a request* belongs to that request, which is
            // what gets it routed to the thread waiting on it. A bare `PING`
            // belongs to nobody, as it always has.
            Message::Ping { id, .. } => id.as_deref(),
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
            Message::Cancel { .. } => "CANCEL",
            Message::Chunk { .. } => "CHUNK",
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
                // A child's `RESULT` is a request that finished on its own.
                // Cancellation is the agent's to add, or the supervisor's.
                cancelled: false,
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

    /// The fixtures both suites read, so the Rust and Python implementations
    /// are held to one set of bytes rather than to each other.
    fn fixtures() -> serde_json::Value {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../spec/fixtures/protocol-v1.json"
        );
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture file")).expect("json")
    }

    #[test]
    fn every_fixture_message_decodes_and_canonical_ones_round_trip_exactly() {
        let f = fixtures();
        assert_eq!(f["proto"], PROTOCOL_VERSION);
        let mut seen = std::collections::BTreeSet::new();
        for case in f["messages"].as_array().expect("messages") {
            let name = case["name"].as_str().unwrap();
            let raw = &case["message"];
            let message: Message = serde_json::from_value(raw.clone())
                .unwrap_or_else(|e| panic!("fixture `{name}` does not decode: {e}"));
            assert_eq!(message.kind(), raw["type"].as_str().unwrap(), "{name}");
            seen.insert(message.kind());
            if case["canonical"].as_bool().unwrap_or(false) {
                let back = serde_json::to_value(&message).unwrap();
                assert_eq!(
                    back, *raw,
                    "fixture `{name}` did not re-serialise to itself"
                );
            }
        }
        // Every message type has a fixture, so an addition to the enum is an
        // addition here too — and therefore to the Python suite.
        for kind in [
            "READY", "EXEC", "FORKED", "GO", "RESULT", "DONE", "PING", "PONG", "SHUTDOWN", "ERROR",
        ] {
            assert!(seen.contains(kind), "no fixture for {kind}");
        }
    }

    #[test]
    fn frame_fixtures_are_the_exact_bytes_in_both_directions() {
        let f = fixtures();
        for case in f["frames"].as_array().expect("frames") {
            let name = case["name"].as_str().unwrap();
            let bytes: Vec<u8> = hex::decode(case["hex"].as_str().unwrap()).unwrap();
            let message: Message = serde_json::from_value(case["message"].clone()).unwrap();
            assert_eq!(frame::encode(&message).unwrap(), bytes, "{name}: encode");
            let decoded: Message = frame::decode(&bytes[frame::HEADER_BYTES..]).unwrap();
            assert_eq!(decoded, message, "{name}: decode");
            let announced = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
            assert_eq!(
                announced,
                bytes.len() - frame::HEADER_BYTES,
                "{name}: length prefix"
            );
        }
    }

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
            script: None,
            id: "01f3".into(),
            event: json!({"url": "https://example.com"}),
            timeout_ms: 30_000,
            env_overrides: BTreeMap::new(),
            stream: false,
            workspace: None,
        };
        let text = serde_json::to_string(&m).unwrap();
        assert!(!text.contains("env_overrides"), "{text}");
        // And an `EXEC` that did not ask to stream does not say so: an agent
        // that predates 1.3 sees exactly the bytes it always did.
        assert!(!text.contains("stream"), "{text}");
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
        assert_eq!(Message::Ping { seq: 1, id: None }.request_id(), None);
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
