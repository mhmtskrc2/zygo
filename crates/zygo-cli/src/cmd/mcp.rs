//! `zygo mcp` — Zygo's tools, over the Model Context Protocol.
//!
//! An agent host (Claude Code, Claude Desktop, Cursor, and anything else that
//! speaks MCP) starts this as a child process and talks JSON-RPC 2.0 to it over
//! standard input and output, one message per line. There is no port, no
//! token and no network: the transport is a pipe between two processes running
//! as the same user.
//!
//! # Why this and not the HTTP API
//!
//! The HTTP API is for programs somebody wrote. This is for a model, and the
//! difference is what may be varied per call.
//!
//! A model reads untrusted text — a web page, a file, an error message — and
//! that text can ask it for things. So the tools here expose a *program* and
//! nothing else: no mounts, no network mode, no limits, no image. Those are
//! set once, on this command line, by the person who installed the server, and
//! a model cannot widen them by asking. That is principle P6 applied to a
//! caller that can be talked into anything.
//!
//! What it does not restrict is the code itself, because running code is the
//! entire point — and running it is safe in the way Zygo is: no capabilities,
//! a read-only root, a seccomp allowlist, mandatory limits, and no network
//! unless this command line said otherwise.
//!
//! Anything needing more — a dependency set, an egress allowlist, a secret —
//! is declared as a function in `sandbox.toml`, brought up with `zygo up`, and
//! called here by name. The boundary is then in a file somebody reviewed,
//! which is where it belongs.
//!
//! # On macOS
//!
//! Forwarded into the Linux VM like every other sandbox command, and stdio
//! passes through, so this works unchanged. The VM hop is paid once when the
//! host starts the server rather than once per tool call, which is the case
//! the shim's own documentation says the ~1 ms warm path is reachable in.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use zygo_core::spec::{Layer, Mount, MountMode, Spec};
use zygo_core::supervisor::client::Client;
use zygo_core::supervisor::{Request as Control, Response as Reply};

use crate::cli::{Cli, McpArgs};

/// Protocol revisions this server knows how to speak.
///
/// The client names one in `initialize` and the server answers with a version
/// it will actually use. Echoing back a version we know keeps a client on its
/// own dialect; anything else gets [`PREFERRED`], and the client decides
/// whether it can live with that — which is what the specification asks for
/// and is better than agreeing to a revision whose shape is unknown here.
const SPOKEN: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// What this server answers with when the client asks for something unknown.
const PREFERRED: &str = "2025-06-18";

/// Where the program being run is mounted, read-only.
const CODE_DIR: &str = "/zygo";

/// Where the workspace is mounted, writable.
const WORK_DIR: &str = "/work";

/// A language the `run_code` tool accepts, and how to run it.
struct Language {
    name: &'static str,
    /// File extension for the program, which is what makes an interpreter's
    /// error messages and tracebacks name something recognisable.
    extension: &'static str,
    /// Argument vector, with `{}` standing for the program's path inside the
    /// sandbox.
    argv: &'static [&'static str],
}

const LANGUAGES: &[Language] = &[
    Language {
        name: "python",
        extension: "py",
        argv: &["python3", "{}"],
    },
    Language {
        name: "node",
        extension: "js",
        argv: &["node", "{}"],
    },
    Language {
        name: "sh",
        extension: "sh",
        argv: &["/bin/sh", "{}"],
    },
];

/// The server's own state, shared by every request in flight.
struct Server {
    exe: PathBuf,
    paths: zygo_core::Paths,
    /// The sandbox every `run_code` call gets: the flags, and the resolver's
    /// defaults where a flag was absent.
    layer: Layer,
    images: Images,
    /// Where `/work` comes from on the host.
    workspace: PathBuf,
    /// Kept alive so a scratch workspace outlives the calls that use it and
    /// is removed when the server exits. `None` when `--workspace` named a
    /// real directory, which is the caller's to keep.
    _scratch: Option<tempfile::TempDir>,
    /// One line at a time, so two answers written from two threads cannot
    /// interleave into something no JSON parser will accept.
    out: Mutex<std::io::Stdout>,
}

struct Images {
    python: String,
    node: String,
    sh: String,
}

impl Images {
    fn for_language(&self, name: &str) -> Option<&str> {
        match name {
            "python" => Some(&self.python),
            "node" => Some(&self.node),
            "sh" => Some(&self.sh),
            _ => None,
        }
    }
}

pub fn run(cli: &Cli, args: &McpArgs) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    paths.ensure()?;
    let exe = std::env::current_exe().context("cannot find this binary")?;

    // A spec file, when there is one, supplies the `[defaults]` every sandbox
    // starts from — so `mem`, `network` and the rest are set in the same place
    // for the agent's ad-hoc code as for the functions it can call.
    let spec = Spec::discover(args.spec_file.path())?.unwrap_or_default();
    let layer = spec.defaults.merge(&args.to_layer()?);

    let (workspace, scratch) = match &args.workspace {
        Some(dir) => {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
            (absolute(dir)?, None)
        }
        None => {
            let dir = tempfile::Builder::new()
                .prefix("zygo-mcp-")
                .tempdir()
                .context("cannot create a workspace")?;
            (absolute(dir.path())?, Some(dir))
        }
    };

    let server = Arc::new(Server {
        exe,
        paths,
        layer,
        images: Images {
            python: args.python_image.clone(),
            node: args.node_image.clone(),
            sh: args.sh_image.clone(),
        },
        workspace,
        _scratch: scratch,
        out: Mutex::new(std::io::stdout()),
    });

    // Standard output belongs to the protocol, so everything a human reads
    // goes to standard error — which is where the host shows a server's log.
    eprintln!(
        "zygo mcp — workspace {} mounted at {WORK_DIR}",
        server.workspace.display()
    );

    let stdin = std::io::stdin();
    let mut line = String::new();
    let mut workers = Vec::new();
    loop {
        line.clear();
        // Read on this thread and answer on another: a `run_code` call takes
        // as long as the code does, and a host is entitled to send a `ping` or
        // a second tool call while it runs.
        match stdin.lock().read_line(&mut line) {
            // The host closed the pipe: it has finished with this server.
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return Err(e).context("cannot read from standard input"),
        }
        let text = line.trim().to_string();
        if text.is_empty() {
            continue;
        }
        let server = Arc::clone(&server);
        workers.push(std::thread::spawn(move || server.handle_line(&text)));
        // Threads that have finished are reaped here rather than accumulating
        // for the life of the session.
        workers.retain(|w| !w.is_finished());
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(0)
}

fn absolute(path: &Path) -> anyhow::Result<PathBuf> {
    std::fs::canonicalize(path).with_context(|| format!("cannot resolve {}", path.display()))
}

impl Server {
    fn handle_line(&self, text: &str) {
        let message: serde_json::Value = match serde_json::from_str(text) {
            Ok(message) => message,
            // No id to answer against, so there is nothing to reply to; the
            // specification says a parse error takes a null id.
            Err(e) => {
                self.write(&error_response(
                    serde_json::Value::Null,
                    -32700,
                    &format!("parse error: {e}"),
                ));
                return;
            }
        };

        let id = message.get("id").cloned();
        let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = message
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // A message without an id is a notification: acknowledged by doing the
        // work and saying nothing, and answering one is a protocol error.
        let Some(id) = id else {
            return;
        };

        let response = match self.dispatch(method, &params) {
            Ok(result) => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(Refusal::Method) => error_response(id, -32601, &format!("no method `{method}`")),
            Err(Refusal::Params(message)) => error_response(id, -32602, &message),
        };
        self.write(&response);
    }

    fn dispatch(
        &self,
        method: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value, Refusal> {
        match method {
            "initialize" => Ok(self.initialize(params)),
            "ping" => Ok(serde_json::json!({})),
            "tools/list" => Ok(serde_json::json!({ "tools": tools() })),
            "tools/call" => self.call(params),
            // Declared in neither direction, and a host that asks anyway gets
            // the honest answer rather than an empty list it would then show.
            _ => Err(Refusal::Method),
        }
    }

    fn initialize(&self, params: &serde_json::Value) -> serde_json::Value {
        let asked = params.get("protocolVersion").and_then(|v| v.as_str());
        let version = match asked {
            Some(asked) if SPOKEN.contains(&asked) => asked,
            _ => PREFERRED,
        };
        serde_json::json!({
            "protocolVersion": version,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "zygo", "version": env!("CARGO_PKG_VERSION") },
            "instructions": format!(
                "Zygo runs code in an isolated sandbox: no capabilities, a read-only root \
                 filesystem, a seccomp allowlist and enforced memory, CPU and process limits. \
                 `run_code` runs a program you write; `{WORK_DIR}` is a directory that persists \
                 between calls, and nothing else you write survives the call. The sandbox's \
                 limits, network and mounts are fixed by whoever started this server and cannot \
                 be changed from here. `list_functions` shows the functions declared in the \
                 project's sandbox.toml — those are warm and cost about a millisecond, so prefer \
                 one over `run_code` when it does the job."
            ),
        })
    }

    fn call(&self, params: &serde_json::Value) -> Result<serde_json::Value, Refusal> {
        let name = params
            .get("name")
            .and_then(|n| n.as_str())
            .ok_or_else(|| Refusal::Params("`name` is required".into()))?;
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));

        // A tool that fails is a *result* saying so, not a JSON-RPC error: the
        // model is supposed to read the failure and try something else, and an
        // error at the protocol layer is handled by the host instead and never
        // reaches it.
        let outcome = match name {
            "run_code" => self.run_code(&arguments),
            "list_functions" => self.list_functions(),
            "call_function" => self.call_function(&arguments),
            "function_logs" => self.function_logs(&arguments),
            other => return Err(Refusal::Params(format!("no tool named `{other}`"))),
        };
        Ok(match outcome {
            Ok(text) => serde_json::json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false,
            }),
            Err(e) => serde_json::json!({
                "content": [{ "type": "text", "text": format!("{e:#}") }],
                "isError": true,
            }),
        })
    }

    fn run_code(&self, arguments: &serde_json::Value) -> anyhow::Result<String> {
        let language = arguments
            .get("language")
            .and_then(|v| v.as_str())
            .context("`language` is required")?;
        let code = arguments
            .get("code")
            .and_then(|v| v.as_str())
            .context("`code` is required")?;
        let stdin = arguments
            .get("stdin")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let spec = LANGUAGES
            .iter()
            .find(|l| l.name == language)
            .with_context(|| {
                let known: Vec<&str> = LANGUAGES.iter().map(|l| l.name).collect();
                format!(
                    "no language `{language}`; this server runs {}",
                    known.join(", ")
                )
            })?;
        let image = self
            .images
            .for_language(language)
            .expect("every language has an image")
            .to_string();

        // The program goes in a directory of its own, mounted read-only, so it
        // is not in the writable workspace the code can reach — a program that
        // rewrites itself mid-run is a debugging story nobody needs.
        let dir = tempfile::Builder::new()
            .prefix("zygo-code-")
            .tempdir()
            .context("cannot stage the program")?;
        let file = format!("main.{}", spec.extension);
        std::fs::write(dir.path().join(&file), code).context("cannot write the program")?;
        let inside = format!("{CODE_DIR}/{file}");

        let mut layer = self.layer.clone();
        let mut mounts = layer.mounts.clone().unwrap_or_default();
        mounts.push(Mount {
            source: absolute(dir.path())?,
            target: PathBuf::from(CODE_DIR),
            mode: MountMode::Ro,
        });
        mounts.push(Mount {
            source: self.workspace.clone(),
            target: PathBuf::from(WORK_DIR),
            mode: MountMode::Rw,
        });
        layer.mounts = Some(mounts);
        // So a relative path in the model's code means the workspace, which is
        // the only place it can write anything that outlives the call.
        layer.workdir = Some(PathBuf::from(WORK_DIR));

        let argv: Vec<String> = spec
            .argv
            .iter()
            .map(|a| {
                if *a == "{}" {
                    inside.clone()
                } else {
                    (*a).to_string()
                }
            })
            .collect();
        let deadline = layer
            .timeout
            .map(|t| t.0)
            .unwrap_or(std::time::Duration::from_secs(30))
            // The launcher enforces the sandbox's own timeout against the whole
            // process tree; this is the outer bound for a run that never gets
            // that far, and the image pull on a first run lives inside it.
            .saturating_add(std::time::Duration::from_secs(300));

        let captured =
            super::oneshot::run(&self.exe, layer, &image, &argv, stdin.as_bytes(), deadline)?;
        Ok(render(&captured))
    }

    fn list_functions(&self) -> anyhow::Result<String> {
        let functions = match self.control(Control::List)? {
            Reply::Functions { functions } => functions,
            other => anyhow::bail!("unexpected answer: {other:?}"),
        };
        if functions.is_empty() {
            return Ok(
                "No warm functions. Declare them in sandbox.toml and run `zygo up`, \
                       or use run_code for one-off work."
                    .into(),
            );
        }
        let mut out = String::new();
        for f in &functions {
            out.push_str(&format!(
                "{} — {} on {}, {} request(s), {} failed, {} MB resident\n",
                f.name,
                f.state.as_str(),
                f.image,
                f.requests,
                f.failures,
                f.rss_kb / 1024
            ));
        }
        Ok(out)
    }

    fn call_function(&self, arguments: &serde_json::Value) -> anyhow::Result<String> {
        let name = arguments
            .get("name")
            .and_then(|v| v.as_str())
            .context("`name` is required")?
            .to_string();
        let event = arguments
            .get("event")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let timeout_ms = 60_000;
        match self.control(Control::Exec {
            name: name.clone(),
            event,
            timeout_ms,
            tenant: None,
        })? {
            Reply::Executed { outcome } => {
                let mut out = String::new();
                if let Some(error) = &outcome.error {
                    out.push_str(&format!("The handler raised: {error}\n"));
                } else {
                    out.push_str(&format!(
                        "{}\n",
                        serde_json::to_string_pretty(&outcome.result)
                            .unwrap_or_else(|_| outcome.result.to_string())
                    ));
                }
                append_stream(&mut out, "stdout", &outcome.stdout);
                append_stream(&mut out, "stderr", &outcome.stderr);
                if outcome.timed_out {
                    out.push_str("The request was killed for exceeding its timeout.\n");
                }
                Ok(out)
            }
            Reply::Busy {
                in_flight, limit, ..
            } => anyhow::bail!(
                "`{name}` is at its concurrency limit ({in_flight} of {limit} in flight); \
                 try again shortly"
            ),
            Reply::Error { code, message } => {
                anyhow::bail!("{}: {message}", code.as_str())
            }
            other => anyhow::bail!("unexpected answer: {other:?}"),
        }
    }

    fn function_logs(&self, arguments: &serde_json::Value) -> anyhow::Result<String> {
        let name = arguments
            .get("name")
            .and_then(|v| v.as_str())
            .context("`name` is required")?
            .to_string();
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(20)
            .clamp(1, 200) as u32;
        let failed = arguments
            .get("failed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        match self.control(Control::Logs {
            name,
            after: 0,
            limit,
            failed,
            tenant: None,
        })? {
            Reply::Logs { entries, .. } if entries.is_empty() => Ok("No log entries.".into()),
            Reply::Logs { entries, .. } => {
                let mut out = String::new();
                for entry in &entries {
                    out.push_str(&format!("[{}] {}\n", entry.seq, entry.text));
                }
                Ok(out)
            }
            Reply::Error { code, message } => anyhow::bail!("{}: {message}", code.as_str()),
            other => anyhow::bail!("unexpected answer: {other:?}"),
        }
    }

    /// One control request, on a connection of its own.
    ///
    /// `connect` rather than `connect_or_start`: these three tools read and
    /// call warm functions, and there is no such thing to read or call unless
    /// somebody has already served one. Starting a supervisor here would
    /// answer "no functions" from a process that did not exist a moment ago,
    /// which is a slower way of saying the same thing.
    fn control(&self, request: Control) -> anyhow::Result<Reply> {
        let mut client = Client::connect(&self.paths).map_err(|_| {
            anyhow::anyhow!(
                "no warm functions are running. Declare them in sandbox.toml and run \
                 `zygo up`, or use run_code for one-off work."
            )
        })?;
        Ok(client.send(&request)?)
    }

    fn write(&self, message: &serde_json::Value) {
        let mut out = self.out.lock().expect("stdout");
        let _ = writeln!(out, "{message}");
        let _ = out.flush();
    }
}

/// Why a JSON-RPC request could not be answered at all.
///
/// Distinct from a tool that ran and failed, which is a successful result with
/// `isError` set — see [`Server::call`].
#[derive(Debug)]
enum Refusal {
    Method,
    Params(String),
}

fn error_response(id: serde_json::Value, code: i32, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

fn append_stream(out: &mut String, name: &str, text: &str) {
    if !text.trim().is_empty() {
        out.push_str(&format!("\n--- {name} ---\n{text}"));
    }
}

/// What a model reads after a one-shot run.
///
/// Output first and labels only where they are needed: a successful run that
/// printed one line should read as that line, not as a report about it.
fn render(captured: &super::oneshot::Captured) -> String {
    let mut out = String::new();
    if !captured.stdout.is_empty() {
        out.push_str(&captured.stdout);
        if !captured.stdout.ends_with('\n') {
            out.push('\n');
        }
    }
    append_stream(&mut out, "stderr", &captured.stderr);
    // Why it ended, in words, because the exit status cannot say: a deadline
    // kill and an out-of-memory kill are both 137, and a model that reads
    // "exit 137" will not know which of its two problems to fix.
    if captured.oom_killed {
        out.push_str(
            "\nThe sandbox ran out of memory and the kernel stopped it. \
             Use less memory, or ask whoever runs this server to raise the limit.\n",
        );
    } else if captured.timed_out {
        out.push_str("\nThe sandbox was killed for exceeding its time limit.\n");
    } else if captured.exit_code != 0 {
        out.push_str(&format!("\nExit code {}.\n", captured.exit_code));
    }
    if out.trim().is_empty() {
        out.push_str("(the program produced no output and exited successfully)\n");
    }
    out
}

/// The tools, and their schemas.
///
/// Every field a model may set describes the *program*. There is deliberately
/// no image, no mount, no network and no limit here: see this module's own
/// documentation for why a caller that reads untrusted text does not get to
/// move the boundary it runs inside.
fn tools() -> serde_json::Value {
    let languages: Vec<&str> = LANGUAGES.iter().map(|l| l.name).collect();
    serde_json::json!([
        {
            "name": "run_code",
            "description": format!(
                "Run a program in an isolated sandbox and return its output. The sandbox has no \
                 capabilities, a read-only root filesystem and enforced memory, CPU and process \
                 limits. `{WORK_DIR}` is a writable directory that persists between calls and is \
                 the working directory; everything else written is discarded when the call ends. \
                 The image, limits, network access and mounts are fixed by whoever started this \
                 server and cannot be set here."
            ),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "language": {
                        "type": "string",
                        "enum": languages,
                        "description": "Which runtime to use."
                    },
                    "code": {
                        "type": "string",
                        "description": "The program. Written to a file and run, so it may be of any length."
                    },
                    "stdin": {
                        "type": "string",
                        "description": "Fed to the program's standard input, which is then closed."
                    }
                },
                "required": ["language", "code"]
            }
        },
        {
            "name": "list_functions",
            "description":
                "List the warm functions this project declares. A warm function costs about a \
                 millisecond to call, against tens of milliseconds for run_code, and it carries \
                 its own dependencies, network allowlist and secrets — so prefer one whenever it \
                 does the job.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "call_function",
            "description":
                "Call a warm function by name with a JSON event, and return what its handler \
                 returned. Use list_functions to see what is available.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The function's name." },
                    "event": {
                        "type": "object",
                        "description": "The JSON event passed to the handler."
                    }
                },
                "required": ["name"]
            }
        },
        {
            "name": "function_logs",
            "description":
                "Recent log entries for a warm function: the zygote's own output and one entry \
                 per request. Use it to see why a call failed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The function's name." },
                    "limit": {
                        "type": "integer",
                        "description": "How many recent entries to return (default 20)."
                    },
                    "failed": {
                        "type": "boolean",
                        "description": "Only requests that failed."
                    }
                },
                "required": ["name"]
            }
        }
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool the dispatcher accepts is advertised, and every tool
    /// advertised is accepted.
    ///
    /// The failure this catches is a renamed tool: the list and the `match`
    /// are two places, and a model can only call what the list names.
    #[test]
    fn the_advertised_tools_are_the_implemented_ones() {
        let advertised: Vec<String> = tools()
            .as_array()
            .expect("an array")
            .iter()
            .map(|t| t["name"].as_str().expect("a name").to_string())
            .collect();
        let implemented = [
            "run_code",
            "list_functions",
            "call_function",
            "function_logs",
        ];
        assert_eq!(advertised, implemented);
    }

    /// No tool lets a model choose an image, a mount, a network mode or a
    /// limit.
    ///
    /// This is the module's security claim, and it is one line away from being
    /// false at any time — adding a convenient `image` parameter is exactly
    /// the sort of thing that looks harmless in isolation.
    #[test]
    fn no_tool_can_widen_the_sandbox() {
        let forbidden = [
            "image",
            "mount",
            "mounts",
            "network",
            "net",
            "allow",
            "mem",
            "cpu",
            "pids",
            "timeout",
            "isolation",
            "seccomp",
            "env",
            "user",
            "secrets",
        ];
        for tool in tools().as_array().expect("an array") {
            let name = tool["name"].as_str().expect("a name");
            let properties = &tool["inputSchema"]["properties"];
            for key in properties.as_object().into_iter().flatten().map(|(k, _)| k) {
                assert!(
                    !forbidden.contains(&key.as_str()),
                    "`{name}` lets the model set `{key}`, which moves the sandbox boundary"
                );
            }
        }
    }

    /// Each language names an image the server knows, and its argument vector
    /// has exactly one placeholder for the program.
    #[test]
    fn every_language_can_actually_be_run() {
        let images = Images {
            python: "p".into(),
            node: "n".into(),
            sh: "s".into(),
        };
        for language in LANGUAGES {
            assert!(
                images.for_language(language.name).is_some(),
                "`{}` has no image",
                language.name
            );
            let holes = language.argv.iter().filter(|a| **a == "{}").count();
            assert_eq!(holes, 1, "`{}` has {holes} program slots", language.name);
        }
    }

    /// `initialize` answers with a revision the client asked for when it is
    /// one this server knows, and with its own when it is not.
    #[test]
    fn the_protocol_revision_is_negotiated_rather_than_assumed() {
        let server = test_server();
        let answer = server.initialize(&serde_json::json!({ "protocolVersion": "2024-11-05" }));
        assert_eq!(answer["protocolVersion"], "2024-11-05");

        let answer = server.initialize(&serde_json::json!({ "protocolVersion": "1999-01-01" }));
        assert_eq!(answer["protocolVersion"], PREFERRED);

        let answer = server.initialize(&serde_json::json!({}));
        assert_eq!(answer["protocolVersion"], PREFERRED);
        assert_eq!(answer["serverInfo"]["name"], "zygo");
    }

    /// A notification — a message with no `id` — is answered with silence, and
    /// a request is not.
    ///
    /// `notifications/initialized` arrives from every host immediately after
    /// `initialize`, and replying to it is a protocol error that some hosts
    /// report as a broken server.
    #[test]
    fn a_notification_gets_no_answer_and_a_request_does() {
        let server = test_server();
        assert!(
            server
                .dispatch("notifications/initialized", &serde_json::Value::Null)
                .is_err(),
            "the method itself is unknown, which is what makes silence the only correct answer"
        );
        assert!(server.dispatch("ping", &serde_json::Value::Null).is_ok());
    }

    /// An unknown tool is a protocol error, but a tool that *ran* and failed
    /// is a result the model can read.
    #[test]
    fn a_failing_tool_answers_the_model_rather_than_the_host() {
        let server = test_server();
        let refused = server.call(&serde_json::json!({ "name": "no_such_tool" }));
        assert!(matches!(refused, Err(Refusal::Params(_))));

        // `code` is missing, so the tool runs and fails. That has to come back
        // as a result with `isError`, because a JSON-RPC error is handled by
        // the host and never reaches the model that could fix it.
        let answered = server
            .call(&serde_json::json!({
                "name": "run_code",
                "arguments": { "language": "python" }
            }))
            .expect("a result, not an error");
        assert_eq!(answered["isError"], true);
        assert!(
            answered["content"][0]["text"]
                .as_str()
                .expect("text")
                .contains("`code` is required")
        );
    }

    /// A run that printed nothing and succeeded still says something, because
    /// an empty tool result reads to a model as a broken tool.
    #[test]
    fn output_is_rendered_for_a_reader() {
        let quiet = super::super::oneshot::Captured {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            abandoned: false,
            oom_killed: false,
            peak_rss_kb: 0,
            wall_ms: 1.0,
        };
        assert!(render(&quiet).contains("no output"));

        let failed = super::super::oneshot::Captured {
            exit_code: 1,
            stdout: "partial\n".into(),
            stderr: "Traceback…\n".into(),
            timed_out: false,
            abandoned: false,
            oom_killed: false,
            peak_rss_kb: 0,
            wall_ms: 1.0,
        };
        let text = render(&failed);
        assert!(text.starts_with("partial\n"), "{text}");
        assert!(text.contains("Traceback"), "{text}");
        assert!(text.contains("Exit code 1"), "{text}");

        let killed = super::super::oneshot::Captured {
            exit_code: 137,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
            abandoned: false,
            oom_killed: false,
            peak_rss_kb: 0,
            wall_ms: 1.0,
        };
        assert!(render(&killed).contains("time limit"));

        // Both kills are exit 137, so the two have to read differently — a
        // model told "exit 137" cannot know which of its two problems to fix.
        let starved = super::super::oneshot::Captured {
            exit_code: 137,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: false,
            abandoned: false,
            oom_killed: true,
            peak_rss_kb: 65_536,
            wall_ms: 1.0,
        };
        let text = render(&starved);
        assert!(text.contains("out of memory"), "{text}");
        assert!(!text.contains("time limit"), "{text}");
    }

    fn test_server() -> Server {
        Server {
            exe: PathBuf::from("/nonexistent/zygo"),
            paths: zygo_core::Paths::rooted(std::env::temp_dir().join("zygo-mcp-test")),
            layer: Layer::default(),
            images: Images {
                python: "python:3.12-slim".into(),
                node: "node:22-slim".into(),
                sh: "alpine:3".into(),
            },
            workspace: std::env::temp_dir(),
            _scratch: None,
            out: Mutex::new(std::io::stdout()),
        }
    }
}
