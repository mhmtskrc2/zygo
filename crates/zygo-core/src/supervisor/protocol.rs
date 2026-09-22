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
///
/// A supervisor that does not know a request drops the connection, which a
/// client sees as a reset with no explanation; the version is what turns that
/// into "stop the running supervisor". It is checked for equality, so a
/// supervisor left running across an upgrade answers every command with the
/// mismatch — except `zygo run`, which only *asks* whether a supervisor will
/// take the sandbox and, told no, builds one itself.
///
/// - v2: `RUN`, `STARTED` and `RAN` — a one-shot sandbox started by the
///   supervisor on the client's streams.
/// - v3: `PUT_SCRIPT`, `GET_SCRIPT`, `DELETE_SCRIPT` and `SCRIPT` — the
///   content-addressed script store behind `PUT /scripts`.
pub const CONTROL_VERSION: u32 = 3;

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
        /// Secret values by name, read from the *client's* environment.
        ///
        /// The client's, not the supervisor's: the supervisor was started by
        /// whichever command needed one first and inherited that environment,
        /// which is nobody's idea of where `STRIPE_KEY` lives. The spec names
        /// the secrets; the shell that runs `zygo serve` supplies them.
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        secrets: std::collections::BTreeMap<String, String>,
        /// Leave the function alone if what is registered under this name is
        /// already exactly this: the same resolved spec, the same secret
        /// values, the same handler and requirements bytes on disk.
        ///
        /// `zygo up` sets this, so running it twice is not two deploys. A bare
        /// `zygo serve` leaves it off: the user just said what they want, and
        /// "already running" is not an answer to that.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        if_changed: bool,
    },

    /// Call a warm function.
    Exec {
        name: String,
        event: serde_json::Value,
        timeout_ms: u64,
    },

    /// Run a one-shot sandbox here, on the client's behalf.
    ///
    /// Why a one-shot goes through a supervisor at all: on an ordinary systemd
    /// session `zygo run` cannot build a cgroup where it starts, so it
    /// re-executes itself in a transient scope — about 34 ms of a 45 ms run,
    /// and unavoidable from that process, because cgroup delegation
    /// containment forbids it moving anywhere better (`zygo-cli/src/scope.rs`
    /// has the measurements and the proof). The supervisor is the one process
    /// on the machine already sitting in a delegated, built `zygo.slice`. It
    /// forks the sandbox there instead, with the client's own standard
    /// streams: the client sends its three descriptors over the connection
    /// with `SCM_RIGHTS` immediately after this frame, in order — stdin,
    /// stdout, stderr.
    ///
    /// The image must already be in the store, like `SERVE`; pulling is the
    /// client's job and its output.
    ///
    /// Answered twice: `STARTED` with the sandbox's pid as soon as it exists,
    /// so the client can forward the terminal's signals to it, and `RAN`
    /// when it has exited.
    Run {
        spec: Option<Box<Spec>>,
        layer: Box<Layer>,
        base_dir: std::path::PathBuf,
        allow_host_net: bool,
        allow_private_net: bool,
        allow_unlimited: bool,
        /// The three descriptors that follow are one terminal, not three
        /// streams: the child adopts it as its controlling terminal.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        tty: bool,
        /// The signals the client ignores, bit `n - 1` for signal `n`: the
        /// program gets these as `SIG_IGN` and every other signal as its
        /// default, whatever the supervisor's own dispositions are. What
        /// `nohup zygo run …` means, kept; what a supervisor started with
        /// `&` from a script would otherwise pass on — `SIGINT` ignored, so
        /// that Ctrl-C did nothing in any run it started — dropped.
        #[serde(default)]
        ignored_signals: u64,
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

    /// Where to enter a function's sandbox, for `zygo shell`.
    ///
    /// The supervisor answers with a pid and stops there. It does not run the
    /// shell: a terminal would then have to be proxied over this socket, and
    /// the client can do the whole thing itself — it runs as the same user, so
    /// entering the user namespace Zygo created grants it the same capability
    /// set inside that namespace as the supervisor would have had.
    Shell { name: String },

    /// A function's recent log: the zygote's own output and one entry per
    /// request.
    ///
    /// `after` is the sequence number to start from — zero for "the last
    /// `limit`", anything else for "everything since", which is how
    /// `zygo logs -f` follows without a stream.
    Logs {
        name: String,
        #[serde(default)]
        after: u64,
        #[serde(default = "default_log_limit")]
        limit: u32,
        #[serde(default)]
        failed: bool,
    },

    /// Make a registered function warm now, without calling it.
    ///
    /// For the moment after a deploy: a function that went cold, or one that
    /// is paused, is brought back so the first real request does not pay for
    /// it. A function that was never served is `not_found` — this warms, it
    /// does not register.
    Warm { name: String },

    /// Register a script, and get back the name the store gave it.
    ///
    /// Content-addressed, so this is idempotent in the strongest sense: the
    /// same bytes are the same name and the same file, however many tenants
    /// send them and however many times. The answer says whether this call
    /// was the one that wrote it.
    ///
    /// The script is not run, and naming it does not make it runnable: a
    /// request has to name a function or (from Phase 1.2) a runtime as well.
    PutScript { source: String },

    /// Whether the store holds this digest, and how big it is.
    GetScript { digest: String },

    /// Forget a script. `not_found` if it was never registered.
    DeleteScript { digest: String },
}

fn default_log_limit() -> u32 {
    50
}

/// What a `SERVE` did to the name it served.
///
/// A deploy tool reads this to say "3 replaced, 7 unchanged" rather than
/// listing ten green ticks that hide which functions actually restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    /// Nothing held the name; a sandbox was started.
    #[default]
    Started,
    /// Something held the name and this is its replacement: the new sandbox
    /// was warm before the old one stopped taking requests, and requests the
    /// old one had accepted finish on it.
    Replaced,
    /// The function already registered was identical, so it was kept —
    /// counters, resident pages and all. Only with `if_changed`.
    Unchanged,
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
        /// What serving did to whatever held the name before.
        #[serde(default)]
        change: Change,
    },

    /// A request ran. `outcome.succeeded()` says whether the handler liked it.
    Executed {
        outcome: Box<Outcome>,
    },

    /// A `RUN` sandbox exists and is about to `execve`.
    ///
    /// Sent before the sandbox runs so the client can forward its terminal's
    /// signals to the right pid; a Ctrl-C at the client would otherwise reach
    /// nothing, because the sandbox is the supervisor's child, not the
    /// client's. The pid is init's, as the client sees it; init leads its own
    /// process group, so `kill(-pid)` reaches everything it forked.
    Started {
        pid: u32,
    },

    /// A `RUN` sandbox has exited. The same fields `zygo run --outcome`
    /// writes, because they are the same facts.
    Ran {
        exit_code: i32,
        /// The supervisor's deadline killed it.
        timed_out: bool,
        /// The kernel killed something in it for running out of memory.
        oom_killed: bool,
        peak_rss_kb: u64,
        wall_ms: f64,
    },

    Functions {
        functions: Vec<Status>,
    },

    Stopped {
        names: Vec<String>,
    },

    /// Answer to `Shell`: the sandbox's first process, on the host.
    ///
    /// `/proc/<pid>/ns/*` is every namespace the sandbox is made of. The pid is
    /// only useful to a client on the same host and as the same user, which is
    /// the only kind this socket has.
    Sandbox {
        name: String,
        pid: u32,
        /// The function's `workdir`, so the shell starts where a request does.
        workdir: std::path::PathBuf,
    },

    /// Answer to `Logs`. `next` is what to pass as `after` to continue.
    Logs {
        name: String,
        entries: Vec<crate::pool::LogEntry>,
        next: u64,
    },

    /// Answer to `Warm`: the function's state afterwards.
    Warmed {
        name: String,
        state: crate::sandbox::SandboxState,
    },

    /// Answer to `PutScript` and `GetScript`.
    Script {
        /// `sha256:…`, which is the script's name everywhere else.
        digest: String,
        size: u64,
        /// The store already held these bytes.
        ///
        /// The observable half of deduplication: two tenants that register
        /// byte-identical scripts get one file, and the second is told so
        /// rather than being left to assume it. Always true for a `GetScript`,
        /// which answers `not_found` otherwise.
        existed: bool,
    },

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
                secrets: std::collections::BTreeMap::from([(
                    "STRIPE_KEY".to_string(),
                    "sk_test_123".to_string(),
                )]),
                if_changed: true,
            },
            Request::Exec {
                name: "resize".into(),
                event: serde_json::json!({ "url": "https://example.com" }),
                timeout_ms: 30_000,
            },
            Request::List,
            Request::Run {
                spec: None,
                layer: Box::default(),
                base_dir: "/work".into(),
                allow_host_net: false,
                allow_private_net: true,
                allow_unlimited: false,
                tty: false,
                ignored_signals: 1 << (libc::SIGHUP - 1),
            },
            Request::Stop {
                name: Some("resize".into()),
            },
            Request::Stop { name: None },
            Request::Shutdown,
            Request::Ping,
            Request::Warm {
                name: "resize".into(),
            },
            Request::Shell {
                name: "resize".into(),
            },
            Request::Logs {
                name: "resize".into(),
                after: 17,
                limit: 20,
                failed: true,
            },
            Request::PutScript {
                source: "def handler(event):\n    return event\n".into(),
            },
            Request::GetScript {
                digest: "sha256:abc".into(),
            },
            Request::DeleteScript {
                digest: "sha256:abc".into(),
            },
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
                change: Change::Replaced,
            },
            Response::Functions {
                functions: vec![Status {
                    name: "resize".into(),
                    image: String::new(),
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
            Response::Warmed {
                name: "resize".into(),
                state: crate::sandbox::SandboxState::Warm,
            },
            Response::Script {
                digest: "sha256:abc".into(),
                size: 41,
                existed: true,
            },
            Response::Sandbox {
                name: "resize".into(),
                pid: 4242,
                workdir: "/zygo".into(),
            },
            Response::Logs {
                name: "resize".into(),
                entries: vec![
                    crate::pool::LogEntry {
                        seq: 3,
                        at_ms: 1_700_000_000_000,
                        kind: crate::pool::LogKind::Zygote,
                        text: "warming up".into(),
                    },
                    crate::pool::LogEntry {
                        seq: 4,
                        at_ms: 1_700_000_000_500,
                        kind: crate::pool::LogKind::Request {
                            id: "01f3".into(),
                            exit_code: 1,
                            timed_out: false,
                            wall_ms: 12.5,
                            error: Some("ZeroDivisionError".into()),
                            stderr: "Traceback".into(),
                        },
                        text: String::new(),
                    },
                ],
                next: 5,
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
            timed_out: false,
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
            timed_out: false,
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

        let json =
            serde_json::to_value(Response::error(ControlError::NotFound, "x")).expect("json");
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
    fn a_serve_with_no_secrets_does_not_put_an_empty_map_on_the_wire() {
        // Keeps the common frame small, and keeps a client that predates the
        // field able to read the JSON a newer one produces.
        let request = Request::Serve {
            name: "f".into(),
            spec: None,
            layer: Box::default(),
            base_dir: "/p".into(),
            allow_host_net: false,
            allow_private_net: false,
            allow_unlimited: false,
            secrets: Default::default(),
            if_changed: false,
        };
        // The *top-level* key, not the word: the layer inside has a `secrets`
        // field of its own (the names), and that one is allowed to be there.
        let json = serde_json::to_value(&request).expect("json");
        assert!(json.get("secrets").is_none(), "{json}");
        assert!(json.get("if_changed").is_none(), "{json}");
        assert_eq!(roundtrip_request(&request), request);
    }

    #[test]
    fn a_frame_from_before_blue_green_still_parses() {
        // Both sides default the fields blue/green added, so a client and a
        // supervisor from either side of that change keep understanding each
        // other without a control-version bump.
        let serve = br#"{"type":"SERVE","name":"f","spec":null,"layer":{},"base_dir":"/p",
            "allow_host_net":false,"allow_private_net":false,"allow_unlimited":false}"#;
        match crate::protocol::frame::decode::<Request>(serve).expect("decode") {
            Request::Serve { if_changed, .. } => assert!(!if_changed),
            other => panic!("{other:?}"),
        }

        let served = br#"{"type":"SERVED","name":"f","runtime":"exec","rss_kb":1,
            "imports_ms":0.0,"warm_ms":2.0,"warnings":[]}"#;
        match crate::protocol::frame::decode::<Response>(served).expect("decode") {
            Response::Served { change, .. } => assert_eq!(change, Change::Started),
            other => panic!("{other:?}"),
        }

        // And the names a deploy script will grep for.
        for (change, want) in [
            (Change::Started, "started"),
            (Change::Replaced, "replaced"),
            (Change::Unchanged, "unchanged"),
        ] {
            assert_eq!(
                serde_json::to_value(change).expect("json"),
                serde_json::Value::String(want.into())
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
            script: None,
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
