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
//! * if `event.stderr` is a string, write it to stderr.
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

/// How long to wait, after `FORKED`, for a `DONE` that must *not* arrive.
///
/// The child is required to do nothing until `GO`. Long enough that a child
/// which ignored the rule has finished the echo handler many times over,
/// short enough not to dominate the suite.
const GO_GRACE: Duration = Duration::from_millis(300);

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
    fn skipped(&mut self, what: &str) {
        if !self.json {
            println!(
                "  {} {what} — {}",
                self.style.yellow("SKIP"),
                self.style
                    .dim("not asked: the agent is no longer in step with this suite")
            );
        }
    }

    /// Record one check from a `Result`, so a failed step reads the same as a
    /// failed assertion rather than aborting the suite.
    fn check(&mut self, what: &str, outcome: anyhow::Result<String>) -> bool {
        match outcome {
            Ok(detail) => {
                self.ok(what, detail);
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
}

impl Agent {
    /// Start the agent with a connected socket at [`AGENT_FD`].
    fn start(binary: &Path, args: &[String]) -> anyhow::Result<Agent> {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        let (ours, theirs) = UnixStream::pair().context("could not create a socket pair")?;
        let their_fd = theirs.as_raw_fd();

        let mut command = std::process::Command::new(binary);
        command.args(args);
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
        })
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

/// An `EXEC` whose event the handler is contracted to echo.
fn exec(id: &str, event: serde_json::Value) -> Message {
    Message::Exec {
        id: id.to_string(),
        event,
        timeout_ms: 30_000,
        env_overrides: Default::default(),
    }
}

/// `zygo agent test <binary> [args…]`.
pub fn test(cli: &Cli, binary: &Path, args: &[String]) -> anyhow::Result<u8> {
    let mut report = Report::new(cli.json);
    if !cli.json {
        println!(
            "protocol conformance: {} (proto {PROTOCOL_VERSION})",
            binary.display()
        );
        println!(
            "  {}",
            Report::new(false)
                .style
                .dim("the handler must echo the event, and write `stdout`/`stderr` when present")
        );
        println!();
    }

    let mut agent = Agent::start(binary, args)?;

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
                report.skipped($what);
            } else {
                report.check($what, $check);
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

    // 9. Shutdown. Last, because it ends the agent.
    step!("SHUTDOWN makes the agent exit", shutdown_check(&mut agent));

    finish(cli, report)
}

fn finish(cli: &Cli, report: Report) -> anyhow::Result<u8> {
    if cli.json {
        output::json(&serde_json::json!({
            "passed": report.passed,
            "failed": report.failed,
            "checks": report
                .results
                .iter()
                .map(|(what, ok, detail)| serde_json::json!({
                    "check": what,
                    "passed": ok,
                    "detail": detail,
                }))
                .collect::<Vec<_>>(),
        }))?;
    } else {
        println!();
        if report.failed == 0 {
            println!(
                "{} {} checks passed — this agent conforms to protocol {PROTOCOL_VERSION}",
                report.style.green("✓"),
                report.passed
            );
        } else {
            println!(
                "{} {} passed, {} failed",
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
    agent.send(&Message::Ping { seq: 7 })?;
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
    agent.send(&exec("c1", serde_json::json!({ "n": 1 })))?;
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
    agent.send(&exec("a", serde_json::json!({ "which": "a" })))?;
    agent.send(&exec("b", serde_json::json!({ "which": "b" })))?;

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
            agent.send(&Message::Ping { seq: 9 })?;
            agent.recv_matching("PONG", |m| matches!(m, Message::Pong { seq: 9 }))?;
            Ok("reported, and the agent is still serving".into())
        }
        other => anyhow::bail!("expected ERROR, got {}", other.kind()),
    }
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

/// `EXEC` → `FORKED` → `GO` → `DONE`, the whole cycle for one request.
fn one_request(agent: &mut Agent, id: &str, event: serde_json::Value) -> anyhow::Result<Message> {
    agent.send(&exec(id, event))?;
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
