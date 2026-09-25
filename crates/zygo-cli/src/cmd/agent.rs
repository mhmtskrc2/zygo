// SPDX-License-Identifier: Apache-2.0
//! `zygo agent test <binary> [args…]` — the protocol conformance suite.
//!
//! The design's claim is that the warm protocol is language independent
//! (ADR-009): anything that speaks it gets limits, timeouts, idle tiering and
//! metrics for free. A claim like that is worth exactly as much as the tool
//! that checks it, so this is that tool — the same checks `spec/protocol.md`
//! §3 lists, run against a real process.
//!
//! The agent under test runs **on this host**, not in a sandbox. What is being
//! checked is the conversation, and a sandbox would add failure modes that
//! belong to Zygo rather than to the agent. The socket arrives at
//! [`zygo_core::pool::AGENT_FD`], which is where a sandboxed agent finds it
//! too, so the same binary works in both places.
//!
//! The handler the agent is started with has to satisfy a small contract, or
//! there is nothing to assert about the answers:
//!
//! * return the event it was given, unchanged;
//! * if `event.stdout` is a string, write it to stdout;
//! * if `event.stderr` is a string, write it to stderr;
//! * if `event.spawn` is a string, start a *program* that prints it — which is
//!   what the `strict` child filter takes away, and so what the suite has to
//!   be able to attempt.
//!
//! `examples/agents/` has one of these per agent.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use zygo_core::pool::AGENT_FD;
use zygo_core::protocol::{ErrorCode, Message, PROTOCOL_VERSION, frame};

use crate::cli::Cli;
use crate::output::{self, Style};

/// How long any single message may take to arrive.
///
/// This is a conformance check, not a benchmark: nothing here is measuring
/// how fast an agent answers, only that it does. So the number should be
/// large enough that slow hardware never enters into it, and small enough
/// that a hung agent fails instead of hanging the suite.
///
/// It was ten seconds, which was chosen for a cold interpreter on a loaded CI
/// runner and turned out to be a measurement after all: the `sh` reference
/// agent, which shells out to `jq` for every frame, exceeded it on a
/// Raspberry Pi whenever the rest of the suite was running — reporting a
/// conformance failure against an agent that conforms.
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

/// The words every "it never answered" failure carries.
///
/// Once an agent misses a deadline the conversation is out of step: its late
/// reply arrives during the *next* exchange, and every check after that is
/// answering the wrong question. A run on a loaded Raspberry Pi reported the
/// `sh` agent as non-conforming twice for that reason, when the real finding
/// was one slow reply. So the runner stops at the first one, and this phrase
/// is how it knows.
const STALLED: &str = "stopped answering";

/// Why the checks after a missed deadline are not asked.
const OUT_OF_STEP: &str = "not asked: the agent is no longer in step with this suite";

/// How long to wait, after `FORKED`, for a `DONE` that must *not* arrive.
///
/// The child is required to do nothing until `GO`. Long enough that a child
/// which ignored the rule has finished the echo handler many times over,
/// short enough not to dominate the suite.
const GO_GRACE: Duration = Duration::from_millis(300);

/// What a check produced.
///
/// A check that could not be asked is neither a pass nor a failure: the
/// `strict` child filter has nothing to say on a kernel that has no seccomp,
/// and a suite that counted that as either would be lying about a platform.
enum Outcome {
    Pass(String),
    Skipped(String),
    /// The agent does part of what the check asked and not the rest.
    ///
    /// Distinct from both, because both would be misleading. An agent that
    /// loads a script from `source` but not from `path` is not "functions
    /// only" — it advertises 1.1 — and it is not passing either: the
    /// supervisor sends `path` whenever it can write into the sandbox, which
    /// is the usual case, so those requests fail in production. Reported, not
    /// fatal: the conformance run is about the protocol, and this one says
    /// which half of a feature is there.
    Partial(String),
}

/// One conformance check.
struct Report {
    style: Style,
    passed: u32,
    failed: u32,
    json: bool,
    /// Set once the agent has missed a deadline. Everything after that is
    /// answering the previous question, so the run stops.
    stalled: bool,
    results: Vec<(String, bool, String)>,
    skips: Vec<(String, String)>,
    /// Checks the agent answered in part. See [`Outcome::Partial`].
    partials: Vec<(String, String)>,
}

impl Report {
    fn new(json: bool) -> Self {
        Self {
            style: Style::stdout(),
            passed: 0,
            failed: 0,
            json,
            stalled: false,
            results: Vec::new(),
            skips: Vec::new(),
            partials: Vec::new(),
        }
    }

    fn ok(&mut self, what: impl Into<String>, detail: impl Into<String>) {
        let (what, detail) = (what.into(), detail.into());
        self.passed += 1;
        if !self.json {
            if detail.is_empty() {
                println!("  {} {what}", self.style.green("PASS"));
            } else {
                println!(
                    "  {} {what} — {}",
                    self.style.green("PASS"),
                    self.style.dim(&detail)
                );
            }
        }
        self.results.push((what, true, detail));
    }

    fn bad(&mut self, what: impl Into<String>, detail: impl Into<String>) {
        let (what, detail) = (what.into(), detail.into());
        self.stalled |= detail.contains(STALLED);
        self.failed += 1;
        if !self.json {
            println!("  {} {what} — {detail}", self.style.red("FAIL"));
        }
        self.results.push((what, false, detail));
    }

    /// A check that was not run, and why.
    ///
    /// Counted as neither passed nor failed: it is not a result. A suite that
    /// scored these as failures would report an agent as non-conforming on
    /// the strength of one slow reply.
    fn skipped(&mut self, what: &str, why: &str) {
        if !self.json {
            println!(
                "  {} {what} — {}",
                self.style.yellow("SKIP"),
                self.style.dim(why)
            );
        }
        self.skips.push((what.to_string(), why.to_string()));
    }

    /// Record one check from a `Result`, so a failed step reads the same as a
    /// failed assertion rather than aborting the suite.
    fn check(&mut self, what: &str, outcome: anyhow::Result<String>) -> bool {
        self.record(what, outcome.map(Outcome::Pass))
    }

    /// A check the agent answered in part. Neither passed nor failed, and
    /// named in the summary so it cannot be read as a pass.
    fn partial(&mut self, what: &str, detail: &str) {
        if !self.json {
            println!("  {} {what} — {detail}", self.style.yellow("PART"));
        }
        self.partials.push((what.to_string(), detail.to_string()));
    }

    /// The same, for a check that is entitled to decline.
    fn record(&mut self, what: &str, outcome: anyhow::Result<Outcome>) -> bool {
        match outcome {
            Ok(Outcome::Pass(detail)) => {
                self.ok(what, detail);
                true
            }
            Ok(Outcome::Skipped(why)) => {
                self.skipped(what, &why);
                true
            }
            Ok(Outcome::Partial(detail)) => {
                self.partial(what, &detail);
                true
            }
            Err(e) => {
                self.bad(what, format!("{e:#}"));
                false
            }
        }
    }
}

/// The connection to the agent under test.
struct Agent {
    stream: UnixStream,
    reader: frame::FrameReader<UnixStream>,
    child: std::process::Child,
    /// Pid the agent announced in `READY`, for the "one process per request"
    /// check.
    announced_pid: u32,
    /// The handler to send with every request, when the agent holds none.
    ///
    /// See `--pool-script`. `None` is the ordinary shape: the agent was warmed
    /// with a handler and the suite sends events alone.
    pool: Option<zygo_core::protocol::Script>,
}

impl Agent {
    /// Start the agent with a connected socket at [`AGENT_FD`].
    fn start(binary: &Path, args: &[String]) -> anyhow::Result<Agent> {
        Agent::start_with_env(binary, args, &[])
    }

    /// The same, with extra environment — how the supervisor hands over the
    /// `strict` child filter.
    fn start_with_env(
        binary: &Path,
        args: &[String],
        env: &[(&str, String)],
    ) -> anyhow::Result<Agent> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        let (ours, theirs) = UnixStream::pair().context("could not create a socket pair")?;
        let their_fd = theirs.as_raw_fd();

        let mut command = std::process::Command::new(binary);
        command.args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        // SAFETY: between `fork` and `execve`. `dup2` and `fcntl` are
        // async-signal-safe, and nothing here allocates.
        unsafe {
            command.pre_exec(move || {
                // Its own process group, so that tearing the agent down takes
                // *everything it started* with it. An agent that leaks a
                // worker — a stray subshell, a parked child — leaves that
                // process holding this harness's inherited stdout, and any
                // caller reading the harness's output through a pipe then
                // waits for ever. The sh example agent did exactly that, and
                // the suite that tests it hung rather than failing.
                if libc::setpgid(0, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // `dup2` clears CLOEXEC on the new descriptor, which is what
                // lets the agent inherit it through `execve`. The source may
                // already be at the target, in which case `dup2` is a no-op
                // and the flag has to be cleared by hand.
                if their_fd != AGENT_FD {
                    if libc::dup2(their_fd, AGENT_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::fcntl(AGENT_FD, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = command
            .spawn()
            .with_context(|| format!("could not start the agent `{}`", binary.display()))?;
        drop(theirs);

        ours.set_read_timeout(Some(REPLY_TIMEOUT))
            .context("could not set a read timeout")?;
        let reader =
            frame::FrameReader::new(ours.try_clone().context("could not clone the socket")?);

        Ok(Agent {
            stream: ours,
            reader,
            child,
            announced_pid: 0,
            pool: None,
        })
    }

    /// An `EXEC` for this agent, carrying the pool handler when there is one.
    ///
    /// Every check below asks its question through here rather than building
    /// the message itself, so the same checks run against both shapes: an
    /// agent warmed with a handler, and a runtime pool that holds no tenant
    /// code and is sent one per request.
    fn exec(&self, id: &str, event: serde_json::Value) -> Message {
        Message::Exec {
            script: self.pool.clone(),
            id: id.to_string(),
            event,
            timeout_ms: 30_000,
            env_overrides: Default::default(),
            stream: false,
            workspace: None,
        }
    }

    fn send(&mut self, message: &Message) -> anyhow::Result<()> {
        let bytes = frame::encode(message)?;
        self.stream.write_all(&bytes)?;
        self.stream.flush()?;
        Ok(())
    }

    /// Write a frame whose body is not a message at all.
    fn send_raw(&mut self, body: &[u8]) -> anyhow::Result<()> {
        let mut out = (body.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(body);
        self.stream.write_all(&out)?;
        self.stream.flush()?;
        Ok(())
    }

    fn recv(&mut self) -> anyhow::Result<Message> {
        match self.reader.read() {
            Ok(Some(message)) => Ok(message),
            Ok(None) => anyhow::bail!("the agent closed the connection"),
            // The read timeout expiring is the most common failure a new
            // agent produces, and `Resource temporarily unavailable` is the
            // least helpful way to say it. Name what did not arrive.
            Err(frame::FrameError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                anyhow::bail!("the agent {STALLED}: nothing at all for {REPLY_TIMEOUT:?}")
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Read until a message satisfies `want`, discarding the rest.
    ///
    /// An agent is allowed to interleave: a `PONG` may arrive between a
    /// `FORKED` and its `DONE`, and a conformance suite that insisted on
    /// strict ordering would be testing something the protocol does not say.
    fn recv_matching(
        &mut self,
        what: &str,
        mut want: impl FnMut(&Message) -> bool,
    ) -> anyhow::Result<Message> {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            let message = self.recv()?;
            if want(&message) {
                return Ok(message);
            }
            anyhow::ensure!(
                Instant::now() < deadline,
                "the agent {STALLED}: no {what} within {REPLY_TIMEOUT:?}"
            );
        }
    }

    /// Whether any message arrives inside `window`. Used for the checks that
    /// require *silence*.
    fn quiet_for(&mut self, window: Duration) -> anyhow::Result<Option<Message>> {
        self.stream.set_read_timeout(Some(window))?;
        let outcome = match self.reader.read() {
            Ok(Some(m)) => Ok(Some(m)),
            Ok(None) => Err(anyhow::anyhow!("the agent closed the connection")),
            Err(frame::FrameError::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        };
        self.stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
        outcome
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // The group, not just the child: see the `setpgid` in `start`. The
        // agent is its own group leader, so its pid is the group id, and a
        // negative pid signals every member. Sent before `kill` so that an
        // agent which has already exited still has its leftovers collected.
        //
        // SAFETY: `kill` on a group this process created; the worst case is
        // ESRCH, which is ignored.
        unsafe {
            libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The handler every request carries, for an agent that holds none.
///
/// A runtime pool is started with an interpreter and a dependency set and no
/// tenant code, so the contract the suite relies on — echo the event, honour
/// `stdout`/`stderr`, start a program for `spawn` — has to arrive with each
/// request instead of being loaded once. It is the same file: the conformance
/// handler, sent as a script.
///
/// `source` rather than `path`, because this suite runs the agent as a plain
/// process on this host and has no sandbox to write into. The digest is over
/// the bytes as read, so the check the child does is a real one.
fn pool_handler(file: &Path) -> anyhow::Result<zygo_core::protocol::Script> {
    let source = std::fs::read_to_string(file)
        .with_context(|| format!("could not read the pool handler {}", file.display()))?;
    let digest = zygo_core::scripts::ScriptDigest::of(&source).to_string();
    Ok(zygo_core::protocol::Script {
        digest: Some(digest),
        ..zygo_core::protocol::Script::inline(source)
    })
}

/// `zygo agent test <binary> [args…]`.
pub fn test(
    cli: &Cli,
    binary: &Path,
    script: Option<&Path>,
    spawning: Option<&Path>,
    pool: Option<&Path>,
    args: &[String],
) -> anyhow::Result<u8> {
    let mut report = Report::new(cli.json);
    let pool = pool.map(pool_handler).transpose()?;
    if !cli.json {
        println!(
            "protocol conformance: {} (proto {PROTOCOL_VERSION})",
            binary.display()
        );
        println!(
            "  {}",
            Report::new(false).style.dim(
                "the handler must echo the event, write `stdout`/`stderr` when present, \
                 and start a program for `spawn`"
            )
        );
        if pool.is_some() {
            println!(
                "  {}",
                Report::new(false)
                    .style
                    .dim("runtime pool: the agent holds no handler, so every request carries one")
            );
        }
        println!();
    }

    let mut agent = Agent::start(binary, args)?;
    agent.pool = pool;

    // 1. READY, before anything else and before any request.
    let ready = report.check(
        "the agent announces itself with READY",
        ready_check(&mut agent),
    );
    if !ready {
        // Nothing below is meaningful without a warmed agent, and a suite that
        // reported nine more failures would bury the one that matters.
        return finish(cli, report);
    }

    // Stop at the first missed deadline. `?` would be wrong here — the checks
    // already run are a result and should be printed — so each step asks
    // whether the conversation is still in step before adding to a tally that
    // would otherwise count the same slow reply several times.
    macro_rules! step {
        ($what:expr, $check:expr) => {
            if report.stalled {
                report.skipped($what, OUT_OF_STEP);
            } else {
                report.check($what, $check);
            }
        };
    }
    macro_rules! step_or_skip {
        ($what:expr, $check:expr) => {
            if report.stalled {
                report.skipped($what, OUT_OF_STEP);
            } else {
                report.record($what, $check);
            }
        };
    }

    // 2. Liveness.
    step!(
        "PING is answered by PONG with the same seq",
        ping_check(&mut agent)
    );

    // 3–6. The request handshake, which is most of the protocol.
    step!(
        "EXEC is answered by FORKED naming a process that is not the agent",
        forked_check(&mut agent)
    );
    step!("the child does nothing until GO", go_check(&mut agent));
    step!(
        "the event reaches the handler and its result comes back",
        result_check(&mut agent)
    );
    step!(
        "stdout and stderr come back in separate fields",
        streams_check(&mut agent)
    );

    // 7. No silent loss, with more than one request outstanding.
    step!(
        "two requests in flight are both answered, with their own ids",
        concurrency_check(&mut agent)
    );

    // 8. A protocol error is reported rather than fatal.
    step!(
        "a frame that is not a message is an ERROR, not a crash",
        bad_message_check(&mut agent)
    );

    // 9. Protocol 1.1: a script that arrives with the request. Optional —
    // an agent that only serves the handler it was warmed with is still
    // conforming, and is reported as such rather than failed.
    step_or_skip!(
        "a script in EXEC is loaded by the child (proto 1.1)",
        script_in_exec_check(&mut agent, script)
    );

    // 10. The `strict` child filter. Runs a second copy of the agent, because
    // `ZYGO_CHILD_SECCOMP` is read at start-up and the conversation above
    // must not be held under it.
    step_or_skip!(
        "ZYGO_CHILD_SECCOMP is installed in the child, or the request is refused",
        child_seccomp_check(&mut agent, binary, args)
    );

    // 11. And it is installed before the *script* runs, not merely before the
    // handler is called. A script's module body is request code.
    step_or_skip!(
        "the child filter is installed before the script's first line (proto 1.1)",
        script_before_filter_check(&mut agent, binary, args, spawning)
    );

    // 12. Protocol 1.2: a request that is cancelled says so. Optional — an
    // agent that does not know `CANCEL` answers `bad_message` and carries on,
    // and the supervisor fills `cancelled` in itself.
    step_or_skip!(
        "a cancelled request comes back as DONE{cancelled} (proto 1.2)",
        cancel_check(&mut agent)
    );

    // 13. Protocol 1.3: output as it is produced. Optional — an agent that
    // ignores `stream` answers the same `DONE` it always did, which is
    // conforming and is what every 1.2 agent does.
    step_or_skip!(
        "output arrives in CHUNKs before the DONE (proto 1.3)",
        stream_check(&mut agent)
    );

    // 14. Protocol 1.4: a long request is reported alive while it runs.
    // Optional — an agent that sends no heartbeat is bounded by the request's
    // own deadline, which is what bounded it before this existed.
    step_or_skip!(
        "a long request is reported alive with PING{id} (proto 1.4)",
        heartbeat_check(&mut agent)
    );

    // 15. Shutdown. Last, because it ends the agent.
    step!("SHUTDOWN makes the agent exit", shutdown_check(&mut agent));

    finish(cli, report)
}

fn finish(cli: &Cli, report: Report) -> anyhow::Result<u8> {
    if cli.json {
        output::json(&serde_json::json!({
            "passed": report.passed,
            "failed": report.failed,
            "skipped": report.skips.len(),
            "checks": report
                .results
                .iter()
                .map(|(what, ok, detail)| serde_json::json!({
                    "check": what,
                    "passed": ok,
                    "detail": detail,
                }))
                .collect::<Vec<_>>(),
            "skips": report
                .skips
                .iter()
                .map(|(what, why)| serde_json::json!({ "check": what, "reason": why }))
                .collect::<Vec<_>>(),
            "partial": report
                .partials
                .iter()
                .map(|(what, detail)| serde_json::json!({ "check": what, "detail": detail }))
                .collect::<Vec<_>>(),
        }))?;
    } else {
        println!();
        let skipped = match report.skips.len() {
            0 => String::new(),
            n => format!(", {n} not asked"),
        };
        let partial = match report.partials.len() {
            0 => String::new(),
            n => format!(", {n} partial"),
        };
        if report.failed == 0 && !report.partials.is_empty() {
            // Not "conforms": it does the protocol, and not all of a feature
            // it advertises. Saying so here is the whole point of the bucket.
            println!(
                "{} {} checks passed{skipped}{partial} — conforms to protocol \
                 {PROTOCOL_VERSION}, with gaps named above",
                report.style.yellow("~"),
                report.passed
            );
        } else if report.failed == 0 {
            println!(
                "{} {} checks passed{skipped} — this agent conforms to protocol {PROTOCOL_VERSION}",
                report.style.green("✓"),
                report.passed
            );
        } else {
            println!(
                "{} {} passed, {} failed{skipped}{partial}",
                report.style.red("✗"),
                report.passed,
                report.failed
            );
        }
    }
    Ok(u8::from(report.failed > 0))
}

fn ready_check(agent: &mut Agent) -> anyhow::Result<String> {
    match agent.recv()? {
        Message::Ready {
            proto,
            pid,
            runtime,
            rss_kb,
            imports_ms,
        } => {
            anyhow::ensure!(
                proto == PROTOCOL_VERSION,
                "announced proto {proto}, this supervisor speaks {PROTOCOL_VERSION}"
            );
            anyhow::ensure!(pid > 0, "READY carries no pid");
            anyhow::ensure!(!runtime.trim().is_empty(), "READY carries no runtime name");
            agent.announced_pid = pid;
            Ok(format!(
                "{runtime}, pid {pid}, {} MB resident after {imports_ms:.0} ms",
                rss_kb / 1024
            ))
        }
        other => anyhow::bail!("the first message was {} rather than READY", other.kind()),
    }
}

fn ping_check(agent: &mut Agent) -> anyhow::Result<String> {
    agent.send(&Message::Ping { seq: 7, id: None })?;
    let reply = agent.recv_matching("PONG", |m| matches!(m, Message::Pong { .. }))?;
    match reply {
        Message::Pong { seq } => {
            anyhow::ensure!(seq == 7, "PONG carried seq {seq}, not the 7 that was sent");
            Ok("seq 7".into())
        }
        other => anyhow::bail!("expected PONG, got {}", other.kind()),
    }
}

/// `EXEC` → `FORKED`. Leaves the request unanswered on purpose: the next check
/// is the one that must see silence.
fn forked_check(agent: &mut Agent) -> anyhow::Result<String> {
    let request = agent.exec("c1", serde_json::json!({ "n": 1 }));
    agent.send(&request)?;
    let forked = agent.recv_matching("FORKED", |m| matches!(m, Message::Forked { .. }))?;
    match forked {
        Message::Forked { id, pid } => {
            anyhow::ensure!(id == "c1", "FORKED carried id `{id}`, not `c1`");
            anyhow::ensure!(pid > 0, "FORKED carries no pid");
            anyhow::ensure!(
                pid != agent.announced_pid,
                "the request runs in the agent's own process ({pid}); \
                 a request must have a pid of its own to be put in a cgroup"
            );
            Ok(format!("child pid {pid}"))
        }
        other => anyhow::bail!("expected FORKED, got {}", other.kind()),
    }
}

/// The child must not start work before `GO`: until then it is still in the
/// agent's cgroup, so anything it allocates is billed to the wrong tenant and
/// escapes the request's limits.
fn go_check(agent: &mut Agent) -> anyhow::Result<String> {
    if let Some(early) = agent.quiet_for(GO_GRACE)? {
        anyhow::bail!(
            "{} arrived {GO_GRACE:?} after FORKED and before GO; the child \
             started work outside its cgroup",
            early.kind()
        );
    }
    agent.send(&Message::Go { id: "c1".into() })?;
    let done = agent.recv_matching("DONE", |m| matches!(m, Message::Done { .. }))?;
    match done {
        Message::Done { id, .. } => {
            anyhow::ensure!(id == "c1", "DONE carried id `{id}`, not `c1`");
            Ok(format!("silent for {GO_GRACE:?}, then answered"))
        }
        other => anyhow::bail!("expected DONE, got {}", other.kind()),
    }
}

/// One full request, checking that the event arrives and the result returns.
fn result_check(agent: &mut Agent) -> anyhow::Result<String> {
    let event = serde_json::json!({ "n": 42, "s": "hello" });
    let done = one_request(agent, "c2", event.clone())?;
    match done {
        Message::Done {
            exit_code,
            result,
            error,
            ..
        } => {
            anyhow::ensure!(
                exit_code == 0,
                "the echo handler exited {exit_code}{}",
                error.map(|e| format!(": {e}")).unwrap_or_default()
            );
            anyhow::ensure!(
                result == event,
                "the handler was given {event} and the result was {result}"
            );
            Ok("the event came back unchanged".into())
        }
        other => anyhow::bail!("expected DONE, got {}", other.kind()),
    }
}

fn streams_check(agent: &mut Agent) -> anyhow::Result<String> {
    let event = serde_json::json!({ "stdout": "to-stdout", "stderr": "to-stderr" });
    let done = one_request(agent, "c3", event)?;
    match done {
        Message::Done { stdout, stderr, .. } => {
            anyhow::ensure!(
                stdout.contains("to-stdout"),
                "what the handler printed is not in `stdout`: {stdout:?}"
            );
            anyhow::ensure!(
                stderr.contains("to-stderr"),
                "what the handler wrote to stderr is not in `stderr`: {stderr:?}"
            );
            anyhow::ensure!(
                !stdout.contains("to-stderr"),
                "the two streams are mixed: stdout holds {stdout:?}"
            );
            Ok("kept apart".into())
        }
        other => anyhow::bail!("expected DONE, got {}", other.kind()),
    }
}

/// Two requests outstanding at once. An agent may serve them one at a time —
/// the protocol does not require concurrency — but it must answer both, and it
/// must not cross their ids.
fn concurrency_check(agent: &mut Agent) -> anyhow::Result<String> {
    let first = agent.exec("a", serde_json::json!({ "which": "a" }));
    let second = agent.exec("b", serde_json::json!({ "which": "b" }));
    agent.send(&first)?;
    agent.send(&second)?;

    // `None` records "answered by refusing", which a serial agent is entitled
    // to do: the protocol requires an answer, not concurrency.
    let mut answered: std::collections::BTreeMap<String, Option<serde_json::Value>> =
        Default::default();
    let mut refused = 0;
    let deadline = Instant::now() + REPLY_TIMEOUT;
    while answered.len() < 2 {
        anyhow::ensure!(
            Instant::now() < deadline,
            "the agent {STALLED}: {} of 2 requests answered within {REPLY_TIMEOUT:?}",
            answered.len()
        );
        match agent.recv()? {
            // Every fork still has to be released.
            Message::Forked { id, .. } => agent.send(&Message::Go { id })?,
            Message::Done { id, result, .. } => {
                anyhow::ensure!(
                    answered.insert(id.clone(), Some(result)).is_none(),
                    "`{id}` was answered twice"
                );
            }
            Message::Error {
                id: Some(id),
                code: ErrorCode::Overloaded,
                ..
            } => {
                refused += 1;
                anyhow::ensure!(
                    answered.insert(id.clone(), None).is_none(),
                    "`{id}` was answered twice"
                );
            }
            Message::Error { id, code, message } => {
                anyhow::bail!("{id:?} failed with {code:?}: {message}")
            }
            _ => {}
        }
    }

    for id in ["a", "b"] {
        let entry = answered
            .get(id)
            .with_context(|| format!("`{id}` was never answered"))?;
        if let Some(result) = entry {
            anyhow::ensure!(
                result.get("which").and_then(|v| v.as_str()) == Some(id),
                "`{id}` came back with another request's event: {result}"
            );
        }
    }
    anyhow::ensure!(refused < 2, "both requests were refused as overloaded");
    Ok(match refused {
        0 => "both ran, neither crossed".into(),
        _ => format!(
            "{} ran, {refused} refused as overloaded (a serial agent)",
            2 - refused
        ),
    })
}

/// A frame the agent cannot parse must produce an `ERROR`, not silence and not
/// a dead agent: the supervisor has a request waiting either way.
fn bad_message_check(agent: &mut Agent) -> anyhow::Result<String> {
    agent.send_raw(b"{ this is not json")?;
    let reply = agent.recv_matching("ERROR", |m| matches!(m, Message::Error { .. }))?;
    match reply {
        Message::Error { code, .. } => {
            anyhow::ensure!(
                code == ErrorCode::BadMessage,
                "reported {code:?} rather than `bad_message`"
            );
            // And it is still alive afterwards.
            agent.send(&Message::Ping { seq: 9, id: None })?;
            agent.recv_matching("PONG", |m| matches!(m, Message::Pong { seq: 9 }))?;
            Ok("reported, and the agent is still serving".into())
        }
        other => anyhow::bail!("expected ERROR, got {}", other.kind()),
    }
}

/// The word the spawning handler prints, which is how the suite tells a child
/// that started a program from one that could not.
const SPAWN_MARK: &str = "zygo-child-spawned";

/// What the script the suite sends has to return.
///
/// A value the conformance handler never produces, so "the script ran" and
/// "the agent ignored it and ran the handler it was warmed with" are told
/// apart rather than guessed at.
const SCRIPT_MARK: &str = "the-request";

/// Protocol 1.1: the script arrives with the request, and the *child* loads it.
///
/// This is what lets one warm interpreter serve ten thousand scripts instead
/// of ten thousand zygotes serving one each — the measurement behind it is in
/// `docs/book/25-performance.md`, where a warm script costs 9.98 MB of proportional
/// memory and an embedder has far more than a thousand of them.
///
/// Optional, and the three outcomes are all legitimate:
///
/// * the script ran — the agent implements 1.1;
/// * the agent's own handler ran instead — it ignored a field it does not
///   know, which is exactly what §5 tells it to do, and it serves functions
///   only;
/// * the request failed — also fine, as long as it *answered*: an agent may
///   refuse what it cannot do, and silence is the only wrong answer.
fn script_in_exec_check(agent: &mut Agent, script: Option<&Path>) -> anyhow::Result<Outcome> {
    // The suite cannot know what language the agent under test speaks, and a
    // Python script sent to a Node agent fails in a way indistinguishable
    // from "this agent does not implement 1.1". So it is given one rather
    // than guessed at — which is also why its absence is a skip and not a
    // failure. (This check sent Python to the Node agent until the Node agent
    // implemented 1.1 and still could not pass.)
    let Some(script) = script else {
        return Ok(Outcome::Skipped(
            "not asked: pass --script <file> in this agent's own language to check \
             protocol 1.1"
                .into(),
        ));
    };
    let source = std::fs::read_to_string(script)
        .with_context(|| format!("could not read the script {}", script.display()))?;
    let digest = zygo_core::scripts::ScriptDigest::of(&source);

    // First the shape every 1.1 agent has to manage, because it decides what
    // the rest of this check means: an agent that does not run a `source`
    // script is not implementing 1.1 at all, and the two probes below would
    // then be asking a question it never claimed to answer.
    let inline = zygo_core::protocol::Script::inline(source);
    let event = serde_json::json!({ "n": 11 });
    match ran_the_script(agent, "c-script", &event, inline.clone())? {
        ScriptRun::Ran => {}
        ScriptRun::NotSupported(why) => return Ok(Outcome::Skipped(why)),
    }

    // Then the shape the supervisor actually sends. `path` is preferred
    // wherever the sandbox can be written into, because a `source` has to
    // pass through the zygote's memory and the zygote is shared between
    // tenants — so an agent that only reads `source` fails the common case.
    //
    // The file is on this host rather than in a sandbox, which is the one
    // thing this harness can arrange; what is under test is whether the agent
    // opens a path it is given, not where the path came from.
    let staged = tempfile::Builder::new()
        .prefix("zygo-conformance-")
        .suffix(&script_suffix(script))
        .tempfile()
        .context("could not stage a script for the `path` shape")?;
    std::fs::write(staged.path(), inline.source.as_deref().unwrap_or_default())
        .context("could not write the staged script")?;
    let by_path =
        zygo_core::protocol::Script::at(staged.path().display().to_string(), digest.to_string());
    let path_shape = ran_the_script(agent, "c-script-path", &event, by_path)?;

    // And the rule that makes `path` safe to use: a sandbox has one uid, so
    // the child can replace the file it is about to load. The digest arrives
    // on the supervisor's connection, where it cannot. An agent that runs the
    // script anyway has no defence against that swap (§3.8).
    let mut tampered = inline.clone();
    tampered.digest = Some(zygo_core::scripts::ScriptDigest::of("not this script").to_string());
    let refused = matches!(
        ran_the_script(agent, "c-script-digest", &event, tampered)?,
        ScriptRun::NotSupported(_)
    );

    match (path_shape, refused) {
        (ScriptRun::Ran, true) => Ok(Outcome::Pass(
            "the request's own script ran, from `source` and from `path`, and a \
             digest that did not match was refused"
                .into(),
        )),
        (ScriptRun::Ran, false) => anyhow::bail!(
            "the agent ran a script whose bytes do not hash to the `digest` in \
             its own EXEC\n  → §3.8: hash what you read and answer ERROR / \
             handler_load instead. Without it a tenant can swap \
             /run/script/<hash> for its own code and be served it"
        ),
        (ScriptRun::NotSupported(why), _) => Ok(Outcome::Partial(format!(
            "`source` works and `path` does not ({why})\n    → the supervisor \
             sends `path` whenever it can write into the sandbox, which is the \
             usual case, so this agent would fail those requests"
        ))),
    }
}

/// The suffix of the script the suite was given, so the staged copy is the
/// same kind of file. A Node agent resolves `require` by extension.
fn script_suffix(script: &Path) -> String {
    match script.extension().and_then(|e| e.to_str()) {
        Some(extension) => format!(".{extension}"),
        None => String::new(),
    }
}

/// Whether the agent ran *this* script, or did something else it is entitled
/// to do.
enum ScriptRun {
    Ran,
    NotSupported(String),
}

/// Send one `EXEC` carrying a script and find out what happened to it.
fn ran_the_script(
    agent: &mut Agent,
    id: &str,
    event: &serde_json::Value,
    script: zygo_core::protocol::Script,
) -> anyhow::Result<ScriptRun> {
    agent.send(&Message::Exec {
        id: id.to_string(),
        event: event.clone(),
        timeout_ms: 30_000,
        env_overrides: Default::default(),
        script: Some(script),
        stream: false,
        workspace: None,
    })?;

    let done = loop {
        match agent.recv()? {
            Message::Forked { id: forked, .. } if forked == id => {
                agent.send(&Message::Go { id: forked })?;
            }
            done @ Message::Done { .. } if done.request_id() == Some(id) => break done,
            Message::Error { code, message, .. } => {
                return Ok(ScriptRun::NotSupported(format!(
                    "refused with {code:?} ({})",
                    first_line(&message)
                )));
            }
            _ => {}
        }
    };

    match done {
        Message::Done {
            exit_code,
            result,
            error,
            ..
        } => {
            if result.get("from").and_then(|v| v.as_str()) == Some(SCRIPT_MARK) {
                return Ok(ScriptRun::Ran);
            }
            if result == *event {
                return Ok(ScriptRun::NotSupported(
                    "this agent serves functions only: it ignored the script and ran \
                     the handler it was warmed with, which §5 allows"
                        .into(),
                ));
            }
            anyhow::ensure!(
                exit_code != 0 || error.is_some(),
                "the agent answered neither the script's result nor its own \
                 handler's: {result}"
            );
            Ok(ScriptRun::NotSupported(format!(
                "the script failed rather than running ({})",
                error.map(|e| first_line(&e)).unwrap_or_default()
            )))
        }
        other => anyhow::bail!("expected DONE, got {}", other.kind()),
    }
}

/// `ZYGO_CHILD_SECCOMP`: the supervisor's tightening of the forked child.
///
/// Under `strict` the supervisor hands the agent a seccomp program that the
/// *child* is to install before any handler code, removing `execve` and
/// process creation from the request without removing them from the agent.
/// `spec/protocol.md` §3 gives an agent two acceptable answers and no third:
/// install it, or fail the request. Running the request anyway, with the
/// sandbox's filter alone, is the outcome this check exists to catch — the
/// `sh` and Node example agents both did it silently before it existed.
///
/// The positive path is proved first, on the agent that is already warm: if
/// the handler cannot start a program *without* a filter, then its failing to
/// start one with a filter proves nothing at all.
fn child_seccomp_check(
    warm: &mut Agent,
    binary: &Path,
    args: &[String],
) -> anyhow::Result<Outcome> {
    let Some(filter) = strict_child_filter() else {
        return Ok(Outcome::Skipped(
            "not asked: seccomp is a Linux facility, and this is not Linux".into(),
        ));
    };

    // The control. A handler that cannot spawn here — no `/bin/echo`, a
    // language without a way to start a program — makes the real check
    // vacuous, so it is a skip rather than a pass.
    let control = one_request(warm, "c-spawn", serde_json::json!({ "spawn": SPAWN_MARK }))?;
    if !spawned(&control) {
        return Ok(Outcome::Skipped(format!(
            "not asked: the handler did not start a program even without a filter, \
             so a refusal would prove nothing ({})",
            summarise(&control)
        )));
    }

    // The same request, on a new agent that was told to tighten its children.
    let mut tightened = Agent::start_with_env(
        binary,
        args,
        &[(zygo_core::protocol::CHILD_SECCOMP_ENV, filter)],
    )?;
    // A pool agent holds no handler, so this copy needs the same one sent to
    // it — otherwise the request below is refused for having no code to run
    // and the refusal reads as the filter doing its job.
    tightened.pool = warm.pool.clone();
    // A start-up `ERROR` is a conforming answer too: an agent that cannot
    // decode the program refuses to serve rather than serving unfiltered.
    match tightened.recv()? {
        Message::Ready { .. } => {}
        Message::Error { code, message, .. } => {
            return Ok(Outcome::Pass(format!(
                "refused to start under the filter ({code:?}: {})",
                first_line(&message)
            )));
        }
        other => anyhow::bail!(
            "the first message under {} was {} rather than READY",
            zygo_core::protocol::CHILD_SECCOMP_ENV,
            other.kind()
        ),
    }

    match one_request(
        &mut tightened,
        "c-filtered",
        serde_json::json!({ "spawn": SPAWN_MARK }),
    ) {
        Ok(done) => {
            anyhow::ensure!(
                !spawned(&done),
                "the child started a program with {} set: the filter was ignored and \
                 the request ran with the sandbox's filter alone",
                zygo_core::protocol::CHILD_SECCOMP_ENV
            );
            Ok(Outcome::Pass(format!(
                "the child could not start a program ({})",
                summarise(&done)
            )))
        }
        // `one_request` turns an `ERROR` into a failure; here it is an answer.
        Err(e) if e.to_string().contains("the agent reported") => Ok(Outcome::Pass(format!(
            "the request was refused rather than run unfiltered ({})",
            first_line(&e.to_string())
        ))),
        Err(e) => Err(e),
    }
}

/// The filter is installed *before the script's first line*, not merely before
/// the handler is called.
///
/// The difference is the whole of protocol 1.1's safety. A script's module
/// body is request code: it runs with the tenant's input reachable, it can
/// start a program, and under `strict` it must not be able to. An agent that
/// loaded the script and installed the filter afterwards would pass every
/// other check in this suite and give a `strict` pool nothing.
///
/// Asked with a script whose *module body* starts a program, which is why it
/// is a separate file and a separate flag: the suite cannot write one in an
/// agent's language, for the same reason `--script` exists.
fn script_before_filter_check(
    warm: &mut Agent,
    binary: &Path,
    args: &[String],
    spawning: Option<&Path>,
) -> anyhow::Result<Outcome> {
    let Some(filter) = strict_child_filter() else {
        return Ok(Outcome::Skipped(
            "not asked: seccomp is a Linux facility, and this is not Linux".into(),
        ));
    };
    let Some(spawning) = spawning else {
        return Ok(Outcome::Skipped(
            "not asked: pass --script-spawn <file> — a script whose module body starts \
             a program — to check when the child filter is installed"
                .into(),
        ));
    };
    let source = std::fs::read_to_string(spawning)
        .with_context(|| format!("could not read {}", spawning.display()))?;
    let script = zygo_core::protocol::Script::inline(source);

    // The control, on the warm agent, which has no filter: a script that
    // cannot start a program here would make the refusal below meaningless.
    let control = one_script_request(warm, "c-spawn-script", script.clone())?;
    if !spawned(&control) {
        return Ok(Outcome::Skipped(format!(
            "not asked: the script did not start a program even without a filter, \
             so a refusal would prove nothing ({})",
            summarise(&control)
        )));
    }

    let mut tightened = Agent::start_with_env(
        binary,
        args,
        &[(zygo_core::protocol::CHILD_SECCOMP_ENV, filter)],
    )?;
    match tightened.recv()? {
        Message::Ready { .. } => {}
        Message::Error { code, message, .. } => {
            return Ok(Outcome::Pass(format!(
                "refused to start under the filter ({code:?}: {})",
                first_line(&message)
            )));
        }
        other => anyhow::bail!("expected READY, got {}", other.kind()),
    }

    match one_script_request(&mut tightened, "c-spawn-filtered", script) {
        Ok(done) => {
            anyhow::ensure!(
                !spawned(&done),
                "the script started a program from its module body with {} set: the \
                 filter goes on after the script loads, so a `strict` pool does not \
                 restrict the script's own import-time code (spec/protocol.md §2)",
                zygo_core::protocol::CHILD_SECCOMP_ENV
            );
            Ok(Outcome::Pass(format!(
                "the script could not start a program while loading ({})",
                summarise(&done)
            )))
        }
        Err(e) if e.to_string().contains("the agent reported") => Ok(Outcome::Pass(format!(
            "the request was refused rather than loaded unfiltered ({})",
            first_line(&e.to_string())
        ))),
        Err(e) => Err(e),
    }
}

/// Whether the handler's program ran, as seen from outside.
fn spawned(done: &Message) -> bool {
    match done {
        Message::Done { stdout, .. } => stdout.contains(SPAWN_MARK),
        _ => false,
    }
}

fn summarise(done: &Message) -> String {
    match done {
        Message::Done {
            exit_code, error, ..
        } => match error {
            Some(e) => format!("exit {exit_code}: {}", first_line(e)),
            None => format!("exit {exit_code}"),
        },
        other => other.kind().to_string(),
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.len() > 90 {
        format!("{}…", &line[..89])
    } else {
        line.to_string()
    }
}

/// The `strict` child program, base64 as the supervisor sends it.
///
/// `None` where there is no such thing to build — every platform that is not
/// Linux, and a Linux architecture with no syscall table compiled in.
#[cfg(target_os = "linux")]
fn strict_child_filter() -> Option<String> {
    use base64::Engine as _;
    use zygo_core::backend::ns::seccomp;

    let program = seccomp::child_program(zygo_core::spec::SeccompProfile::Strict).ok()??;
    Some(base64::engine::general_purpose::STANDARD.encode(seccomp::encode(&program)))
}

#[cfg(not(target_os = "linux"))]
fn strict_child_filter() -> Option<String> {
    None
}

fn shutdown_check(agent: &mut Agent) -> anyhow::Result<String> {
    agent.send(&Message::Shutdown { grace_ms: 2_000 })?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match agent.child.try_wait()? {
            Some(status) => return Ok(format!("exited with {status}")),
            None if Instant::now() >= deadline => {
                anyhow::bail!("still running 5 s after SHUTDOWN")
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// The same cycle, for a request that carries its own script.
fn one_script_request(
    agent: &mut Agent,
    id: &str,
    script: zygo_core::protocol::Script,
) -> anyhow::Result<Message> {
    agent.send(&Message::Exec {
        id: id.to_string(),
        event: serde_json::json!({ "spawn": SPAWN_MARK }),
        timeout_ms: 30_000,
        env_overrides: Default::default(),
        script: Some(script),
        stream: false,
        workspace: None,
    })?;
    await_done(agent, id)
}

/// `EXEC` → `FORKED` → `GO` → `DONE`, the whole cycle for one request.
/// How long the suite waits for a heartbeat before deciding there is none.
///
/// The reference agents beat every two seconds. Generous against that, and
/// short enough that an agent which does not implement 1.4 is not a ten-second
/// pause in a suite that otherwise runs in three.
const HEARTBEAT_WAIT: Duration = Duration::from_secs(6);

/// Protocol 1.4: a request that runs for a while is reported alive.
///
/// The supervisor raised its timeout ceiling to a day, which makes the
/// deadline a poor backstop on its own: a request wedged in the first minute
/// of a six-hour budget would hold its slot for the rest of it. A heartbeat is
/// what tells a request that is working from one that is not, and this checks
/// that an agent sends one — for the *request*, with its id, not the bare
/// `PING` that says only that the agent is alive.
///
/// Skipped when none arrives. An agent without heartbeats is bounded by the
/// request's own deadline, which is what bounded it before 1.4 existed.
fn heartbeat_check(agent: &mut Agent) -> anyhow::Result<Outcome> {
    let id = "c11";
    // Long enough that a two-second heartbeat lands well inside it, and the
    // check never waits for the handler.
    let request = agent.exec(id, serde_json::json!({ "sleep_ms": 30_000 }));
    agent.send(&request)?;

    let forked = agent.recv_matching("FORKED", |m| matches!(m, Message::Forked { .. }))?;
    let Message::Forked { pid, .. } = forked else {
        anyhow::bail!("expected FORKED, got {}", forked.kind());
    };
    agent.send(&Message::Go { id: id.to_string() })?;

    let started = Instant::now();
    let mut beat = None;
    while started.elapsed() < HEARTBEAT_WAIT {
        let left = HEARTBEAT_WAIT.saturating_sub(started.elapsed());
        match agent.quiet_for(left)? {
            Some(Message::Ping {
                id: Some(pinged), ..
            }) if pinged == id => {
                beat = Some(started.elapsed());
                break;
            }
            // Anything else is this request talking, which is not what is
            // being checked — a heartbeat is what an agent sends when there
            // is *nothing* to say.
            Some(_) => continue,
            None => break,
        }
    }

    // Tidy up either way: the handler is sleeping for thirty seconds and the
    // suite has eleven more checks to run.
    // SAFETY: signalling a process this suite's agent forked.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
    let _ = agent.recv_matching("DONE", |m| matches!(m, Message::Done { .. }));

    Ok(match beat {
        Some(at) => Outcome::Pass(format!("a heartbeat for this request after {at:?}")),
        None => Outcome::Skipped(format!(
            "nothing for {HEARTBEAT_WAIT:?} while a request ran: heartbeats are \
             not implemented"
        )),
    })
}

/// Protocol 1.3: a request that asked to stream gets its output early.
///
/// The check that matters is **order**, not content: the same text arrives in
/// `DONE` either way, and an agent that buffered every `CHUNK` and sent them
/// all at the end would pass any test that only looked at what was received.
/// So the handler prints, sleeps, and prints again, and the first chunk has to
/// arrive while it is still sleeping.
///
/// Skipped rather than failed when no chunk arrives at all: `stream` is an
/// optional field, and §5 says an agent ignores fields it does not know.
fn stream_check(agent: &mut Agent) -> anyhow::Result<Outcome> {
    let id = "c10";
    let early = "first";
    agent.send(&Message::Exec {
        id: id.to_string(),
        event: serde_json::json!({ "stdout": early, "sleep_ms": 1_500 }),
        timeout_ms: 30_000,
        env_overrides: Default::default(),
        script: agent.pool.clone(),
        stream: true,
        workspace: None,
    })?;

    let forked = agent.recv_matching("FORKED", |m| matches!(m, Message::Forked { .. }))?;
    anyhow::ensure!(
        matches!(forked, Message::Forked { .. }),
        "expected FORKED, got {}",
        forked.kind()
    );
    agent.send(&Message::Go { id: id.to_string() })?;

    let started = Instant::now();
    let mut first: Option<Duration> = None;
    let mut streamed = String::new();
    loop {
        match agent.recv()? {
            Message::Chunk {
                stream: zygo_core::protocol::Stream::Stdout,
                data,
                ..
            } => {
                first.get_or_insert_with(|| started.elapsed());
                streamed.push_str(&data);
            }
            done @ Message::Done { .. } if done.request_id() == Some(id) => {
                let Message::Done { stdout, .. } = done else {
                    unreachable!("matched above")
                };
                let Some(at) = first else {
                    return Ok(Outcome::Skipped(
                        "the request was answered, but nothing arrived before \
                         the DONE: `stream` is not implemented"
                            .into(),
                    ));
                };
                anyhow::ensure!(
                    streamed.contains(early),
                    "what the handler printed first is not in the chunks: {streamed:?}"
                );
                // The handler sleeps for 1.5 s after printing. A chunk that
                // arrives after that was buffered and sent with the result,
                // which is the thing streaming exists not to do.
                anyhow::ensure!(
                    at < Duration::from_millis(1_200),
                    "the first chunk arrived after {at:?}, so it waited for the                      handler to finish rather than being forwarded"
                );
                anyhow::ensure!(
                    stdout.contains(early),
                    "DONE no longer carries the output it always did: {stdout:?}"
                );
                return Ok(Outcome::Pass(format!(
                    "the first chunk arrived after {at:?}"
                )));
            }
            Message::Error { code, message, .. } => {
                anyhow::bail!("the agent reported {code:?}: {message}")
            }
            _ => {}
        }
    }
}

/// How long the handler sleeps for, so a cancel lands while it is running.
const CANCEL_SLEEP_MS: u64 = 5_000;

/// How long the agent has to answer after the request has been killed.
///
/// The supervisor's own bound is the same shape: an agent that does not answer
/// a killed request is one the supervisor marks broken and rewarms, because a
/// caller is waiting on that `DONE` and silence is not an outcome.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Protocol 1.2: a request that was cancelled comes back saying so.
///
/// This plays the supervisor's whole part, because the frame on its own is not
/// the cancel. A real supervisor writes `cgroup.kill`; here there is no
/// cgroup, so the child is killed by pid — which is the same thing from the
/// agent's side, and the *only* part an agent can be asked about. What is
/// being checked is the answer: `DONE`, for the right id, with `cancelled`.
///
/// Skipped rather than failed when the agent does not know the message. That
/// is conforming: rule 6 says an unknown message is an `ERROR` and not fatal,
/// and the supervisor knows it sent the kill.
fn cancel_check(agent: &mut Agent) -> anyhow::Result<Outcome> {
    let id = "c9";
    let request = agent.exec(id, serde_json::json!({ "sleep_ms": CANCEL_SLEEP_MS }));
    agent.send(&request)?;

    let forked = agent.recv_matching("FORKED", |m| matches!(m, Message::Forked { .. }))?;
    let Message::Forked { pid, .. } = forked else {
        anyhow::bail!("expected FORKED, got {}", forked.kind());
    };
    agent.send(&Message::Go { id: id.to_string() })?;

    // Long enough that the handler is inside its sleep. A cancel that lands
    // before `GO` is a different case and a better one — the supervisor
    // handles it by never sending `GO` — but it is not what this checks.
    std::thread::sleep(Duration::from_millis(200));
    agent.send(&Message::Cancel { id: id.to_string() })?;

    // The supervisor's half. `pid` is the agent's own child and this suite
    // runs the agent as a plain process, so there is no namespace to
    // translate through.
    // SAFETY: signalling a process this suite's agent forked.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };

    let started = Instant::now();
    loop {
        match agent.recv()? {
            done @ Message::Done { .. } if done.request_id() == Some(id) => {
                let Message::Done { cancelled, .. } = done else {
                    unreachable!("matched above")
                };
                anyhow::ensure!(
                    started.elapsed() < CANCEL_GRACE,
                    "the agent answered {:?} after the kill, past the {CANCEL_GRACE:?} bound",
                    started.elapsed()
                );
                return Ok(match cancelled {
                    true => Outcome::Pass("answered with `cancelled` after the kill".into()),
                    // The request was answered, which is rule 5, and the kill
                    // worked — so this is an agent that has not implemented
                    // 1.2, not one that is wrong.
                    false => Outcome::Skipped(
                        "answered the killed request, but without `cancelled`: \
                         CANCEL is not implemented"
                            .into(),
                    ),
                });
            }
            // `bad_message` for the CANCEL itself is the documented answer
            // from an agent that does not know it. Rule 6 says carry on, so
            // this keeps waiting for the `DONE` the kill will produce.
            Message::Error { code, message, .. }
                if code != zygo_core::protocol::ErrorCode::BadMessage =>
            {
                anyhow::bail!("the agent reported {code:?}: {message}")
            }
            _ => {}
        }
        anyhow::ensure!(
            started.elapsed() < CANCEL_GRACE,
            "the agent {STALLED}: no DONE for a killed request within {CANCEL_GRACE:?}"
        );
    }
}

fn one_request(agent: &mut Agent, id: &str, event: serde_json::Value) -> anyhow::Result<Message> {
    let request = agent.exec(id, event);
    agent.send(&request)?;
    await_done(agent, id)
}

fn await_done(agent: &mut Agent, id: &str) -> anyhow::Result<Message> {
    loop {
        match agent.recv()? {
            Message::Forked { id: forked, .. } if forked == id => {
                agent.send(&Message::Go { id: id.to_string() })?;
            }
            done @ Message::Done { .. } if done.request_id() == Some(id) => return Ok(done),
            Message::Error { code, message, .. } => {
                anyhow::bail!("the agent reported {code:?}: {message}")
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every "it never answered" failure has to carry the phrase, or the suite
    // carries on asking questions the agent is no longer in step to answer.
    #[test]
    fn a_missed_deadline_stops_the_run_and_a_wrong_answer_does_not() {
        let mut report = Report::new(true);
        report.bad("a check", "the agent said Pong when Forked was due");
        assert!(!report.stalled, "a wrong answer is still an answer");

        report.bad(
            "another",
            format!("the agent {STALLED}: nothing at all for 30s"),
        );
        assert!(report.stalled);
    }
}
