//! One sandbox, one command, output captured.
//!
//! What `zygo run` does, for a caller that is not a terminal: the HTTP API's
//! `POST /run` and the MCP server's `run_code` tool both need a sandbox whose
//! streams come back as strings rather than passing through to a tty.
//!
//! It spawns `zygo run` as a child rather than launching a sandbox in-process,
//! and the reason is worth stating because the opposite looks cheaper. A
//! one-shot sandbox has no supervisor and no RPC boundary — `zygo run` does not
//! involve one either — so the only thing in-process would buy is a saved
//! `fork`/`exec`, about a millisecond against a cold start of eighteen. What it
//! would cost is a second copy of resolve, pull, derive, rootfs view, network
//! setup and backend selection. That copy would drift, and it would drift in
//! the code that builds sandbox boundaries, which is the worst place in this
//! repository for two implementations of one idea.
//!
//! The sandbox is described by a [`Layer`] written to a temporary spec file, so
//! the child resolves exactly the layering rules everything else uses rather
//! than a flag mapping invented here.

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Context;
use zygo_core::spec::{Layer, Spec};

/// How often the child is checked against its deadline.
///
/// Polling rather than a signal because the deadline is an outer bound, not
/// the sandbox's own `timeout` — the launcher enforces that against the whole
/// process tree, with a timer and `cgroup.kill`. Ten milliseconds of slack on
/// a bound measured in seconds costs nothing and keeps this dependency-free.
const POLL: Duration = Duration::from_millis(10);

/// How long to keep collecting output after the child has been reaped.
///
/// Reaping the child does not close its pipes: anything that inherited them
/// and outlived it holds the write end open, and a read to end-of-file then
/// waits for *that* process instead. Killing the whole process group is what
/// prevents it; this is the bound for the case where something escaped the
/// group, and it turns a hang into a truncated answer.
const COLLECT_GRACE: Duration = Duration::from_secs(2);

/// What a one-shot sandbox said.
#[derive(Debug, Clone)]
pub struct Captured {
    /// The command's exit status, or `-1` when a signal ended it.
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    /// The sandbox ran out of time: either the launcher's own deadline, which
    /// it reports in the outcome file, or [`Captured::abandoned`].
    ///
    /// Reported rather than inferred. A deadline kill and an out-of-memory
    /// kill are both exit 137, so a caller deciding between "too slow" and
    /// "too much memory" cannot tell from the status alone.
    pub timed_out: bool,
    /// The *outer* bound fired and this module killed the child.
    ///
    /// Distinct from `timed_out`, and the distinction is the difference
    /// between a sandbox doing its job and Zygo failing to do its own. A
    /// sandbox killed by its own `timeout` ran, and everything it said is
    /// here; a child killed by this bound was abandoned, and what it would
    /// have said is unknown.
    pub abandoned: bool,
    /// The kernel killed something in the sandbox for running out of memory.
    pub oom_killed: bool,
    /// Peak resident memory of the sandbox, from its cgroup. Zero when the
    /// kernel did not report one.
    pub peak_rss_kb: u64,
    pub wall_ms: f64,
    /// Whether the program ran at all, as the child reported it.
    ///
    /// `false` is Zygo failing to build the sandbox — a missing image, a
    /// host that cannot, a mount that does not exist — and a caller should
    /// report it as *unavailable* rather than as the program's failure.
    /// Also `false` when the child wrote no outcome at all, which is a child
    /// that died before it could say anything.
    pub started: bool,
    /// How far the child got: `plan`, `start` or `run`. See the outcome
    /// file's own `phase`.
    pub phase: String,
}

/// What `zygo run --outcome` writes. The fields it does not carry — the two
/// streams — come back over the pipes.
#[derive(serde::Deserialize, Default)]
struct Outcome {
    // `exit_code` is in the file and deliberately not read here: the child's
    // wait status is the authority on that, and taking it from a file the
    // child wrote would report success for a child that was killed after
    // writing it.
    timed_out: bool,
    oom_killed: bool,
    #[serde(default)]
    peak_rss_kb: u64,
    #[serde(default)]
    started: bool,
    #[serde(default)]
    phase: String,
}

/// Run one sandbox to completion and collect its output.
///
/// `layer` describes the sandbox as a `[fn.<name>]` table would; `image` and
/// `argv` are passed to the child as arguments, so whatever the layer says
/// about them is overridden by these. `deadline` is the outer bound described
/// on [`Captured::timed_out`].
pub fn run(
    exe: &Path,
    mut layer: Layer,
    image: &str,
    argv: &[String],
    stdin: &[u8],
    deadline: Duration,
) -> anyhow::Result<Captured> {
    // Carried as arguments instead, and leaving them in the spec would make
    // the resolved command depend on which of two places set it.
    layer.image = None;
    layer.cmd = None;

    let spec = Spec {
        defaults: layer,
        ..Default::default()
    };
    let toml = toml::to_string(&spec).context("cannot express that sandbox as a spec file")?;
    let file = tempfile::Builder::new()
        .prefix("zygo-run-")
        .suffix(".toml")
        .tempfile()
        .context("cannot create a temporary spec file")?;
    std::fs::write(file.path(), &toml)
        .with_context(|| format!("cannot write {}", file.path().display()))?;

    // Where the child records *why* it ended. Beside the spec file, in the
    // same temporary directory, so it is removed with it.
    let outcome_path = file.path().with_extension("outcome");

    let started = Instant::now();
    let mut command = std::process::Command::new(exe);
    command
        .arg("run")
        .arg("--outcome")
        .arg(&outcome_path)
        // Zygo's own progress lines share a descriptor with the sandbox's
        // standard error, and this caller hands both to a program — or to a
        // model. "pulling python:3.12-slim" is not something the sandbox said.
        .arg("--quiet")
        .arg("--file")
        .arg(file.path())
        .arg(image)
        .args(argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // The child runs code the caller chose, and has no business holding
        // the API's own credential.
        .env_remove("ZYGO_API_TOKEN");
    // A process group of its own, so the deadline can kill the whole tree.
    // `kill` reaches the child and nothing it started, and the thing a
    // sandbox run consists of is precisely the things it started.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("cannot start `{}`", exe.display()))?;

    // Each stream gets a thread. Writing the input inline would deadlock on
    // anything larger than a pipe buffer against a child that has not started
    // reading, and reading one stream to the end before the other would
    // deadlock on a child that fills the one being ignored.
    let mut sink = child.stdin.take().expect("piped");
    let input = stdin.to_vec();
    std::thread::spawn(move || {
        // This process sets `SIGPIPE` back to its default disposition, so that
        // `zygo doctor | head` ends quietly rather than aborting. The cost is
        // that writing to a child that has already gone would kill *this*
        // process — an API serving other requests — instead of returning
        // `EPIPE`. Blocking the signal in this thread is what turns it back
        // into an error, and it has to be per-thread because the disposition
        // is not.
        block_sigpipe();
        let _ = sink.write_all(&input);
        // Dropping it closes the pipe, which is what gives the child EOF.
        drop(sink);
    });

    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    let mut out = child.stdout.take().expect("piped");
    let mut err = child.stderr.take().expect("piped");
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        let _ = out_tx.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
        let _ = err_tx.send(buf);
    });

    let mut timed_out = false;
    let status = loop {
        match child.try_wait().context("cannot wait for the sandbox")? {
            Some(status) => break status,
            None if started.elapsed() >= deadline => {
                timed_out = true;
                kill_group(&mut child);
                break child.wait().context("cannot reap the sandbox")?;
            }
            None => std::thread::sleep(POLL),
        }
    };

    // Bounded rather than joined: see `COLLECT_GRACE`. An empty answer here
    // means the output never arrived, which is what the caller should see.
    let stdout = out_rx.recv_timeout(COLLECT_GRACE).unwrap_or_default();
    let stderr = err_rx.recv_timeout(COLLECT_GRACE).unwrap_or_default();

    // The child's own account, when it got far enough to write one. A child
    // this module killed never did, and the outer deadline is then the only
    // thing there is to report.
    let outcome: Option<Outcome> = std::fs::read(&outcome_path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok());
    let _ = std::fs::remove_file(&outcome_path);
    // A child killed by the outer bound wrote nothing; whether its program
    // ran is unknown, and "unknown" is reported as not started, which is
    // the answer a caller acts on correctly either way.
    let (ran, phase) = match &outcome {
        Some(o) => (o.started, o.phase.clone()),
        None => (false, String::new()),
    };
    let outcome = outcome.unwrap_or_default();

    Ok(Captured {
        exit_code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        timed_out: timed_out || outcome.timed_out,
        abandoned: timed_out,
        oom_killed: outcome.oom_killed,
        peak_rss_kb: outcome.peak_rss_kb,
        wall_ms: started.elapsed().as_secs_f64() * 1000.0,
        started: ran,
        phase,
    })
}

/// Kill the child and everything it started.
///
/// The child leads a process group of its own (see [`run`]), so one signal to
/// the negated pid reaches the whole tree. Without it the child dies and its
/// descendants keep the output pipes open, and reading to end-of-file then
/// waits for processes that were supposed to have been killed — which is how
/// a two-hundred-millisecond deadline took thirty seconds to return.
#[cfg(unix)]
fn kill_group(child: &mut std::process::Child) {
    // SAFETY: `pid` is this process's own child, which has not been reaped —
    // `try_wait` returned `None` — so the pid cannot have been reused.
    let sent = unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
    if sent != 0 {
        // The group may be gone between the check and here, or the platform
        // may have refused `process_group`. Either way the child itself is
        // still reachable.
        let _ = child.kill();
    }
}

#[cfg(not(unix))]
fn kill_group(child: &mut std::process::Child) {
    let _ = child.kill();
}

/// Block `SIGPIPE` for the calling thread, so a write to a closed pipe is an
/// error rather than the end of this process.
#[cfg(unix)]
fn block_sigpipe() {
    // SAFETY: a zeroed `sigset_t` is what `sigemptyset` expects, and the mask
    // is applied to this thread only.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGPIPE);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

#[cfg(not(unix))]
fn block_sigpipe() {}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;

    /// A stand-in for the `zygo` binary, so these tests exercise this module
    /// rather than a kernel. It is spawned exactly as the real one is, which
    /// is what makes the argument order and the spec file observable.
    fn fake_zygo(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("zygo");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write the stand-in");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        (dir, path)
    }

    /// The child is spawned as `run --file <spec> <image> <argv…>`, the spec
    /// holds the limits, and `image` and `cmd` are *not* in it — leaving them
    /// there would make the resolved command depend on which of two places set
    /// it.
    ///
    /// Read out of the child rather than out of the struct: what can break is
    /// the TOML and the argument order, and neither is visible from here.
    #[test]
    fn the_child_gets_the_command_as_arguments_and_the_limits_as_a_spec() {
        let (_dir, exe) = fake_zygo(r#"echo "argv: $*"; echo "--- spec ---"; cat "$6""#);
        let layer: Layer = serde_json::from_str(
            r#"{"image":"python:3.12-slim","cmd":["ignored"],"mem":"128M","timeout":"5s"}"#,
        )
        .expect("a layer");

        let captured = run(
            &exe,
            layer,
            "python:3.12-slim",
            &["python3".into(), "-c".into(), "print(1)".into()],
            b"",
            Duration::from_secs(10),
        )
        .expect("the stand-in ran");

        assert_eq!(captured.exit_code, 0, "{captured:?}");
        let (argv, spec) = captured
            .stdout
            .split_once("--- spec ---")
            .expect("both halves");
        assert!(
            argv.contains("run --outcome")
                && argv.contains("--quiet --file")
                && argv.contains("python:3.12-slim python3 -c print(1)"),
            "argument order changed: {argv}"
        );
        assert!(spec.contains("mem = \"128M\""), "limits missing: {spec}");
        assert!(spec.contains("timeout = \"5s\""), "limits missing: {spec}");
        assert!(
            !spec.contains("image =") && !spec.contains("cmd ="),
            "the spec should not also name the command: {spec}"
        );
    }

    /// The child's account of *why* it ended is read back.
    ///
    /// A deadline kill and an out-of-memory kill are both exit 137, so the
    /// status cannot carry this and the child writes it to the file
    /// `--outcome` names. The stand-in writes one and this checks it arrives —
    /// including that the exit code comes from the wait status and not from
    /// the file, which a child killed after writing it would have lied about.
    #[test]
    fn why_the_sandbox_ended_comes_back_with_its_output() {
        let (_dir, exe) = fake_zygo(
            r#"printf '{"exit_code":0,"timed_out":false,"oom_killed":true,"peak_rss_kb":65536}' > "$3"
               echo ran
               exit 137"#,
        );

        let captured = run(
            &exe,
            Layer::default(),
            "alpine:3",
            &[],
            b"",
            Duration::from_secs(10),
        )
        .expect("the stand-in ran");

        assert_eq!(captured.stdout.trim(), "ran");
        assert!(captured.oom_killed, "{captured:?}");
        assert!(!captured.timed_out, "{captured:?}");
        assert!(
            !captured.abandoned,
            "a child that finished on its own was reported as abandoned"
        );
        assert_eq!(captured.peak_rss_kb, 65_536);
        assert_eq!(
            captured.exit_code, 137,
            "the exit code must come from the wait status, not the file"
        );
    }

    /// A child that never wrote an outcome is reported from what is known
    /// here, rather than as a silent set of `false`s that look like an answer.
    #[test]
    fn a_child_that_wrote_no_outcome_still_reports_the_outer_deadline() {
        let (_dir, exe) = fake_zygo("sleep 30");
        let captured = run(
            &exe,
            Layer::default(),
            "alpine:3",
            &[],
            b"",
            Duration::from_millis(200),
        )
        .expect("the stand-in ran");

        assert!(captured.timed_out, "{captured:?}");
        assert!(captured.abandoned, "{captured:?}");
        assert!(!captured.oom_killed, "{captured:?}");
    }

    /// Standard input reaches the sandbox, and a body larger than a pipe
    /// buffer does too.
    ///
    /// The size is the point. Writing the input inline rather than on its own
    /// thread passes at a few bytes and deadlocks for ever at 64 kB, so a test
    /// with a short string would report that this works when it does not.
    #[test]
    fn a_large_standard_input_reaches_the_child() {
        let (_dir, exe) = fake_zygo("wc -c");
        let input = vec![b'x'; 1 << 20];

        let captured = run(
            &exe,
            Layer::default(),
            "alpine:3",
            &[],
            &input,
            Duration::from_secs(20),
        )
        .expect("the stand-in ran");

        assert_eq!(captured.exit_code, 0, "{captured:?}");
        assert_eq!(captured.stdout.trim(), (1 << 20).to_string());
    }

    /// A deadline that expires kills the child *and everything it started*,
    /// and says so.
    ///
    /// Three things have to hold, and each one has failed here at some point:
    ///
    /// * The positive case. The same stand-in under a deadline it fits inside
    ///   must report `timed_out: false` — otherwise "it timed out" is also
    ///   satisfied by a stand-in that never ran.
    /// * The call returns promptly. It first did not: killing the child left
    ///   its `sleep` holding the output pipe, and reading that pipe to
    ///   end-of-file waited the full thirty seconds.
    /// * The grandchild is dead. This is the one an elapsed-time assertion
    ///   cannot make, because giving up on the pipe after a grace period looks
    ///   identical from the outside while leaving a process behind.
    #[test]
    fn a_deadline_kills_the_whole_tree_and_reports_it() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let pidfile = dir.path().join("grandchild.pid");
        let (_bin, exe) = fake_zygo(&format!(
            "sleep \"$ZYGO_TEST_SLEEP\" &\necho $! > {}\nwait",
            pidfile.display()
        ));

        // SAFETY: the test harness has other threads, but none of them reads
        // this variable; it exists only to reach the stand-in.
        unsafe { std::env::set_var("ZYGO_TEST_SLEEP", "0") };
        let quick = run(
            &exe,
            Layer::default(),
            "alpine:3",
            &[],
            b"",
            Duration::from_secs(10),
        )
        .expect("the stand-in ran");
        assert!(!quick.timed_out, "a prompt child was called slow");
        assert_eq!(quick.exit_code, 0, "{quick:?}");

        unsafe { std::env::set_var("ZYGO_TEST_SLEEP", "30") };
        let slow = run(
            &exe,
            Layer::default(),
            "alpine:3",
            &[],
            b"",
            Duration::from_millis(200),
        )
        .expect("the stand-in ran");
        assert!(slow.timed_out, "the deadline did not fire: {slow:?}");
        assert!(
            slow.abandoned,
            "the outer bound has to be distinguishable from a sandbox's own \
             timeout: {slow:?}"
        );
        assert_ne!(slow.exit_code, 0, "a killed child reported success");
        assert!(
            slow.wall_ms < 5_000.0,
            "it waited for the child instead of killing it: {slow:?}"
        );

        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("the stand-in recorded its grandchild")
            .trim()
            .parse()
            .expect("a pid");
        // Signal 0 asks whether the process could be signalled: 0 means it is
        // still there. Reaping is the shell's business and it is gone, so a
        // survivor stays visible rather than becoming a zombie this would miss.
        std::thread::sleep(Duration::from_millis(100));
        // SAFETY: signal 0 delivers nothing; it only reports reachability.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "the grandchild outlived the deadline (pid {pid})");
    }
}
