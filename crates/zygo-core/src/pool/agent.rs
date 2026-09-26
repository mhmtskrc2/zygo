// SPDX-License-Identifier: Apache-2.0
//! The agents Zygo carries inside the binary, and where they land.
//!
//! A warm function is an interpreter with an agent in it, waiting for
//! `EXEC`. This is everything about that agent that the pool has to know
//! before a sandbox exists: the source it is written from, the paths it and
//! the handler are mounted at, the descriptor it talks on, the argv that
//! starts it, and the `READY` handshake that says it is up. Nothing here
//! runs a request; see [`super::WarmFn`] for that.

use std::path::PathBuf;

use crate::error::{Error, Result};
use crate::protocol::{ErrorCode, Message, PROTOCOL_VERSION, ProtocolError};
use crate::spec::ResolvedFn;

/// The reference Python agent, carried inside the binary.
///
/// Zygo ships as a single static binary with no runtime dependencies, so
/// the agent cannot be a file the user is expected to have. It is written into
/// the data directory on first use and bind-mounted into the sandbox.
///
/// `../../agents` is `crates/zygo-core/agents`, a symlink to the repository's
/// `agents/` directory. It has to be reached from inside the crate rather
/// than as `../../../../agents`: `cargo package` includes only what is under the
/// package root, so a path that climbs out of it compiles in a checkout and
/// fails for everyone who runs `cargo install zygo-cli`.
pub const PYTHON_AGENT: &str = include_str!("../../agents/python/zygo_agent.py");

/// The reference Node agent, carried the same way.
///
/// Node has no `fork()`, so it keeps a pool of pre-loaded workers instead of
/// forking a zygote. Everything above that — the wire, the cgroup window,
/// secrets, the `strict` child filter — is identical, which is the point of
/// having a protocol rather than an interface.
pub const NODE_AGENT: &str = include_str!("../../agents/node/zygo_agent.js");

/// One of the agents Zygo carries inside the binary.
///
/// Each is a file in the data directory, a mount inside the sandbox and an
/// argv; nothing else about a runtime reaches the supervisor, which is what
/// keeps adding one small.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinAgent {
    Python,
    Node,
}

impl BuiltinAgent {
    /// The agent Zygo ships for this runtime, or `None` for one it does not.
    pub fn for_runtime(runtime: &crate::spec::Runtime) -> Option<BuiltinAgent> {
        use crate::spec::{BuiltinRuntime, Runtime};
        match runtime {
            Runtime::Builtin(BuiltinRuntime::Python) => Some(BuiltinAgent::Python),
            Runtime::Builtin(BuiltinRuntime::Node) => Some(BuiltinAgent::Node),
            _ => None,
        }
    }

    pub fn source(self) -> &'static str {
        match self {
            BuiltinAgent::Python => PYTHON_AGENT,
            BuiltinAgent::Node => NODE_AGENT,
        }
    }

    /// Where it is written on the host, relative to the data directory.
    pub fn host_relative(self) -> &'static str {
        match self {
            BuiltinAgent::Python => "agents/python/zygo_agent.py",
            BuiltinAgent::Node => "agents/node/zygo_agent.js",
        }
    }

    /// Where it is bind-mounted inside every sandbox.
    ///
    /// The extension matters: `require` and `import` both decide what a file
    /// is by its name.
    pub fn agent_in_sandbox(self) -> &'static str {
        match self {
            BuiltinAgent::Python => AGENT_IN_SANDBOX,
            BuiltinAgent::Node => "/zygo/agent.js",
        }
    }

    /// Where the tenant's handler is bind-mounted inside every sandbox.
    pub fn handler_in_sandbox(self) -> &'static str {
        match self {
            BuiltinAgent::Python => HANDLER_IN_SANDBOX,
            BuiltinAgent::Node => "/zygo/handler.js",
        }
    }

    /// Argv that starts this agent inside a sandbox.
    ///
    /// `--fd` rather than a socket path: nothing has to exist in the
    /// sandbox's filesystem, so no bind mount and no assumption about the
    /// image.
    ///
    /// Without a handler this is a **runtime pool**: the agent is told no
    /// tenant code at all and every request brings its own `script`. The
    /// argument is absent rather than empty, because the agent decides which
    /// shape it is by whether it was given one (`spec/protocol.md` §2).
    pub fn argv(self, mode: &str, handler: bool) -> Vec<String> {
        let interpreter = match self {
            BuiltinAgent::Python => "python3",
            BuiltinAgent::Node => "node",
        };
        let mut argv = vec![
            interpreter.to_string(),
            self.agent_in_sandbox().to_string(),
            "--fd".to_string(),
            AGENT_FD.to_string(),
        ];
        if handler {
            argv.push(self.handler_in_sandbox().to_string());
            argv.push(mode.to_string());
        }
        argv
    }
}

/// Where the agent's socket lands in the sandbox's file descriptor table.
///
/// Passing a connected descriptor rather than a socket path means nothing has
/// to exist in the sandbox's filesystem for the agent to reach the supervisor —
/// no bind mount, no path that the image must happen to have, and nothing on
/// disk for a second sandbox to find.
pub const AGENT_FD: std::os::fd::RawFd = 3;

/// How long an agent has to announce itself before the warm-up is failed.
///
/// Generous: the interpreter starts and the handler's imports run under it,
/// and `numpy` on a small board is seconds. Finite, because the launcher
/// serves one warm-up at a time — a wait with no end here is a supervisor
/// that never serves anything again, which is what happened on a Pi.
pub const AGENT_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether an error is a socket read timeout rather than a protocol fault.
pub(super) fn is_timeout(e: &Error) -> bool {
    let Error::Protocol(ProtocolError::Frame(crate::protocol::frame::FrameError::Io(io))) = e
    else {
        return false;
    };
    matches!(
        io.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// Read the agent's `READY` and check that it speaks a protocol this build
/// understands.
pub fn read_ready(
    reader: &mut crate::protocol::FrameReader<std::os::unix::net::UnixStream>,
) -> Result<(String, u64, f64)> {
    match reader.read().map_err(ProtocolError::from)? {
        Some(Message::Ready {
            proto,
            imports_ms,
            rss_kb,
            runtime,
            ..
        }) => {
            if proto != PROTOCOL_VERSION {
                return Err(ProtocolError::VersionMismatch {
                    found: proto,
                    expected: PROTOCOL_VERSION,
                }
                .into());
            }
            Ok((runtime, rss_kb, imports_ms))
        }
        Some(Message::Error { code, message, .. }) => {
            Err(ProtocolError::Agent { code, message }.into())
        }
        other => Err(ProtocolError::Unexpected {
            expected: "READY",
            found: other.map(|m| m.kind()).unwrap_or("end of stream"),
        }
        .into()),
    }
}

/// Where the Python agent and its handler are bind-mounted inside every
/// sandbox. [`BuiltinAgent`] has the same pair for every runtime.
pub const AGENT_IN_SANDBOX: &str = "/zygo/agent.py";
pub const HANDLER_IN_SANDBOX: &str = "/zygo/handler.py";

/// The mounts a warm agent-backed function needs on top of its spec's own.
pub fn agent_mounts(
    agent: BuiltinAgent,
    agent_host: &std::path::Path,
    f: &ResolvedFn,
) -> Vec<crate::spec::Mount> {
    use crate::spec::{Mount, MountMode};

    let mut mounts = f.mounts.clone();
    mounts.push(Mount {
        source: agent_host.to_path_buf(),
        target: PathBuf::from(agent.agent_in_sandbox()),
        mode: MountMode::Ro,
    });
    if let Some(entry) = &f.entry {
        mounts.push(Mount {
            source: entry.clone(),
            target: PathBuf::from(agent.handler_in_sandbox()),
            mode: MountMode::Ro,
        });
    }
    mounts
}

/// Turn an agent-side failure into the error a caller should see.
pub fn agent_failure(code: ErrorCode, message: String) -> Error {
    ProtocolError::Agent { code, message }.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Layer, ResolveOptions, resolve_standalone};

    fn resolved(entry: Option<&str>) -> ResolvedFn {
        let mut layer = Layer {
            image: Some("python:3.12-slim".into()),
            ..Default::default()
        };
        match entry {
            Some(e) => layer.entry = Some(PathBuf::from(e)),
            None => layer.cmd = Some(vec!["/bin/true".into()]),
        }
        resolve_standalone("demo", &layer, &ResolveOptions::default()).unwrap()
    }

    /// A runtime pool: an image and an agent, and nothing of anybody's.
    fn resolved_pool() -> ResolvedFn {
        let layer = Layer {
            image: Some("python:3.12-slim".into()),
            runtime: Some(crate::spec::Runtime::Builtin(
                crate::spec::BuiltinRuntime::Python,
            )),
            ..Default::default()
        };
        resolve_standalone(
            "demo-pool",
            &layer,
            &ResolveOptions {
                pool: true,
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn the_embedded_agent_is_the_real_one() {
        assert!(
            PYTHON_AGENT.contains("PROTOCOL_VERSION = 1"),
            "the embedded agent is not the reference agent"
        );
        assert!(
            PYTHON_AGENT.contains("gc.freeze()"),
            "the embedded agent lost the copy-on-write protection"
        );
        assert!(
            PYTHON_AGENT.contains("--fd"),
            "the embedded agent cannot take an inherited socket"
        );
    }

    #[test]
    fn the_embedded_node_agent_is_the_real_one() {
        assert!(
            NODE_AGENT.contains("const PROTOCOL_VERSION = 1"),
            "the embedded Node agent is not the reference agent"
        );
        assert!(
            NODE_AGENT.contains("--fd"),
            "the embedded Node agent cannot take an inherited socket"
        );
        // The one thing the Node agent is here to do that the example did
        // not: honour the supervisor's child filter, one way or the other.
        assert!(
            NODE_AGENT.contains(crate::protocol::CHILD_SECCOMP_ENV),
            "the embedded Node agent ignores the strict child filter"
        );
    }

    #[test]
    fn the_agent_is_started_on_an_inherited_descriptor() {
        for (agent, interpreter) in [
            (BuiltinAgent::Python, "python3"),
            (BuiltinAgent::Node, "node"),
        ] {
            let argv = agent.argv("function", true);
            assert_eq!(argv[0], interpreter);
            assert_eq!(argv[1], agent.agent_in_sandbox());
            assert_eq!(argv[2], "--fd");
            assert_eq!(argv[3], AGENT_FD.to_string());
            assert_eq!(argv[4], agent.handler_in_sandbox());
            assert!(
                !argv.iter().any(|a| a.contains(".sock")),
                "a socket path would have to exist inside the sandbox: {argv:?}"
            );
        }
    }

    /// A pool zygote is started with no handler *argument*, not with an empty
    /// one: the agent decides which shape it is by whether it was given one,
    /// and an empty string is a path it would try to open.
    #[test]
    fn a_pool_zygote_is_started_without_a_handler_at_all() {
        for agent in [BuiltinAgent::Python, BuiltinAgent::Node] {
            let argv = agent.argv("function", false);
            assert_eq!(argv.len(), 4, "{argv:?}");
            assert_eq!(argv[3], AGENT_FD.to_string());
            assert!(
                !argv.iter().any(|a| a.contains("handler")),
                "a pool zygote must not be told about a handler: {argv:?}"
            );
        }
    }

    /// And nothing of the tenant's is mounted into one either. The pool's
    /// whole isolation claim is that its zygote is anonymous.
    #[test]
    fn a_pool_zygote_mounts_the_agent_and_nothing_else() {
        let pool = resolved_pool();
        let mounts = agent_mounts(
            BuiltinAgent::Python,
            std::path::Path::new("/data/agents/python/zygo_agent.py"),
            &pool,
        );
        assert_eq!(mounts.len(), 1, "{mounts:?}");
        assert_eq!(
            mounts[0].target,
            PathBuf::from(BuiltinAgent::Python.agent_in_sandbox())
        );
    }

    /// The extension is not decoration: `require` and `import` both decide
    /// what a file is by its name, so a Node handler mounted at `.py` is a
    /// handler Node will not load.
    #[test]
    fn each_agent_keeps_its_own_paths_in_the_sandbox() {
        let python = BuiltinAgent::Python;
        let node = BuiltinAgent::Node;
        assert!(python.agent_in_sandbox().ends_with(".py"));
        assert!(python.handler_in_sandbox().ends_with(".py"));
        assert!(node.agent_in_sandbox().ends_with(".js"));
        assert!(node.handler_in_sandbox().ends_with(".js"));
        assert_ne!(python.host_relative(), node.host_relative());
    }

    #[test]
    fn the_agent_and_handler_are_mounted_read_only_alongside_the_specs_mounts() {
        use crate::spec::MountMode;

        let mut f = resolved(Some("/host/handler.py"));
        f.mounts = vec!["/host/cache:/cache:rw".parse().unwrap()];

        let mounts = agent_mounts(
            BuiltinAgent::Python,
            std::path::Path::new("/data/agents/zygo_agent.py"),
            &f,
        );
        let find = |target: &str| {
            mounts
                .iter()
                .find(|m| m.target == std::path::Path::new(target))
        };

        let agent = find(AGENT_IN_SANDBOX).expect("the agent must be mounted");
        assert_eq!(agent.mode, MountMode::Ro);
        assert_eq!(agent.source, PathBuf::from("/data/agents/zygo_agent.py"));

        let handler = find(HANDLER_IN_SANDBOX).expect("the handler must be mounted");
        assert_eq!(handler.mode, MountMode::Ro, "tenant code is not writable");

        // And the spec's own mounts survive.
        assert!(find("/cache").is_some(), "the spec's mounts were dropped");
    }

    #[test]
    fn a_warm_exec_function_needs_no_handler_mount() {
        let f = resolved(None);
        let mounts = agent_mounts(
            BuiltinAgent::Python,
            std::path::Path::new("/data/agent.py"),
            &f,
        );
        assert!(
            !mounts
                .iter()
                .any(|m| m.target == std::path::Path::new(HANDLER_IN_SANDBOX)),
            "warm-exec has no handler to mount"
        );
    }
}
