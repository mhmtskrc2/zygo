// SPDX-License-Identifier: Apache-2.0
//! A warm-exec function: a held sandbox, and a fresh process per request.
//!
//! The other half of the warm model, for a program that starts in a
//! millisecond and has nothing to amortise: the sandbox is kept and each
//! request is entered into it. Linux only, because entering a sandbox is
//! six `setns` calls; the module is compiled out elsewhere and
//! [`Function`](super::Function) has no `Exec` arm there.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use super::request::{
    KILL_GRACE, RequestLease, Requests, kill_request, next_request_id, request_cgroup, resident_kb,
};
use super::scripts::{SCRIPT_DIR_IN_SANDBOX, ScriptLease, Scripts, place_script};
use super::secrets::{Secrets, SecretsAt, place_secrets};
use super::timing::{CallTiming, CpuAccounting};
use super::{Counters, Outcome, Status};
use crate::error::{Error, Result};
use crate::protocol::Metrics;
use crate::sandbox::SandboxState;

/// A warm-exec function: a held sandbox, and a fresh process per request.
///
/// The other half of the warm model, for everything
/// that is not an interpreter worth keeping warm: a Go or Rust binary starts
/// in a millisecond, so there is nothing to amortise and no agent to write.
/// The sandbox — namespaces, mounts, cgroup, hardening — is what is kept.
///
/// A request costs a fork of the supervisor, six `setns` calls, a second fork,
/// the hardening steps, and an `execve`: 1–3 ms plus the program's own
/// start-up. The event goes in on stdin, the result comes out
/// on stdout as JSON, and stderr is captured separately.
pub struct WarmExec {
    pub(super) name: String,
    /// See [`Status::tenant`].
    pub(super) tenant: String,
    /// See [`Status::image`].
    pub(super) image: String,
    /// Host pid of the held init: the process whose death means the sandbox
    /// is gone.
    pub(super) init_pid: u32,
    /// `/run/secrets` inside the sandbox, handed out by its own init before
    /// it hardened.
    ///
    /// A descriptor because there is no path: `/proc/<pid>/root` is
    /// traversable only while a process is dumpable, and writing the id map
    /// clears that for everything in the sandbox. As root the check is
    /// bypassed, which is why this worked in a container for a year and on
    /// nobody's laptop.
    pub(super) secrets_dir: Option<std::os::fd::OwnedFd>,
    /// Hardening and `execve` candidates, prepared once. Every request is a
    /// process that has to be constrained exactly as the init was.
    pub(super) plan: crate::backend::ns::prepare::PreparedLaunch,
    /// Owns the init and the namespace descriptors. Dropping it kills the
    /// sandbox, which as pid 1's death takes every request in it along.
    pub(super) sandbox: Mutex<Box<dyn crate::backend::Sandbox>>,
    pub(super) counters: Mutex<Counters>,
    /// See [`WarmFn`](super::WarmFn)'s field of the same name. A warm-exec
    /// request is a process in a cgroup like any other, so a cancel is the
    /// same write.
    pub(super) in_flight: Requests,
    pub(super) state: Mutex<SandboxState>,
    pub(super) tenant_cgroup: Option<PathBuf>,
    /// The held sandbox's own generation, where its requests are admitted.
    pub(super) generation_cgroup: Option<PathBuf>,
    pub(super) per_request_cgroup: bool,
    pub(super) timeout: std::time::Duration,
    pub(super) secrets: Mutex<Secrets>,
    /// Scripts written into the sandbox for the requests running them. As on
    /// [`WarmFn`](super::WarmFn), and for a pool of these it is the only way
    /// code arrives: the script is a **file named on a command line**, so
    /// there is no `source` shape to fall back to.
    pub(super) scripts: Mutex<Scripts>,
    /// The host side of `/run/script`: this sandbox's alone, removed with it.
    pub(super) script_dir: PathBuf,
}

impl WarmExec {
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Host pid of the held init process. See [`WarmFn::init_pid`](super::WarmFn::init_pid).
    pub fn init_pid(&self) -> u32 {
        self.init_pid
    }

    /// Stop a request this sandbox is running. See [`WarmFn::cancel`](super::WarmFn::cancel).
    ///
    /// There is no agent to tell, so this is only the kill — which is the
    /// whole of a cancel anyway.
    pub fn cancel(&self, id: &str, caller: Option<&str>) -> Option<bool> {
        let request = self.in_flight.get(id)?;
        if !request.is_for(caller) {
            return None;
        }
        Some(request.cancel())
    }

    pub fn in_flight_ids(&self) -> Vec<String> {
        self.in_flight.ids()
    }

    pub fn status(&self) -> Status {
        let counters = self.counters.lock().expect("counters");
        Status {
            name: self.name.clone(),
            tenant: self.tenant.clone(),
            image: self.image.clone(),
            state: self.state(),
            runtime: "exec".into(),
            rss_kb: resident_kb(self.init_pid).unwrap_or(0),
            imports_ms: 0.0,
            requests: counters.requests,
            failures: counters.failures,
        }
    }

    pub fn state(&self) -> SandboxState {
        *self.state.lock().expect("state")
    }

    pub fn is_healthy(&self) -> bool {
        self.state() != SandboxState::Failed
    }

    pub fn timeout(&self) -> std::time::Duration {
        self.timeout
    }

    pub fn set_secrets(&self, values: BTreeMap<String, String>) {
        self.secrets.lock().expect("secrets").values = values;
    }

    pub fn pause(&self) -> Result<()> {
        let mut state = self.state.lock().expect("state");
        if *state != SandboxState::Warm {
            return Ok(());
        }
        if let Some(dir) = &self.tenant_cgroup {
            crate::cgroup::freeze(dir, true)?;
        }
        *state = SandboxState::Paused;
        Ok(())
    }

    pub fn resume(&self) -> Result<()> {
        let mut state = self.state.lock().expect("state");
        if *state != SandboxState::Paused {
            return Ok(());
        }
        if let Some(dir) = &self.tenant_cgroup {
            crate::cgroup::freeze(dir, false)?;
        }
        *state = SandboxState::Warm;
        Ok(())
    }

    pub fn cpu_accounting(&self) -> Option<CpuAccounting> {
        CpuAccounting::read(self.tenant_cgroup.as_ref()?)
    }

    /// Write one request's script into the sandbox and say where it is.
    ///
    /// [`WarmFn::place_script`](super::WarmFn::place_script) falls back to sending the bytes in the `EXEC`
    /// when it cannot write into the sandbox. There is no such fallback here:
    /// the script is named on a command line, so a path is the only shape it
    /// has. A caller that already arranged delivery — a `path` of their own —
    /// is taken at their word, as on the agent path.
    fn place_script(
        &self,
        script: crate::protocol::Script,
    ) -> Result<(String, Option<ScriptLease<'_>>)> {
        if let Some(path) = script.path {
            return Ok((path, None));
        }
        let Some(source) = script.source else {
            return Err(Error::Spec(crate::spec::SpecError::invalid(
                "script",
                "carries neither `source` nor `path`",
            )));
        };
        let digest = crate::scripts::ScriptDigest::of(&source);
        let lease = place_script(&self.scripts, &self.script_dir, digest.hex(), &source)?;
        Ok((
            format!("{SCRIPT_DIR_IN_SANDBOX}/{}", digest.hex()),
            Some(lease),
        ))
    }

    /// Serve one request: enter, admit, run, collect.
    pub fn call_timed(
        &self,
        event: serde_json::Value,
        timeout: std::time::Duration,
    ) -> Result<(Outcome, CallTiming)> {
        self.call_script_timed(event, None, timeout, None)
    }

    /// The same, for a request that brought its own script.
    ///
    /// The **warm-exec pool** shape: one held sandbox running a program the
    /// operator named — `sh`, a static binary — and each request's script
    /// written into `/run/script/<digest>` and named as the last word of the
    /// command line. For a language whose runtime starts in under a
    /// millisecond there is nothing for an agent to amortise, and the warm
    /// protocol would only be a moving part.
    ///
    /// The script has to be a **file**: `source` on the wire has nowhere to go
    /// when the thing that loads it is `execve`. A sandbox this process cannot
    /// write into is therefore a refusal rather than a fallback — unlike the
    /// agent path, which still has `EXEC.source`.
    ///
    /// Nothing checks the digest here, and nothing needs to: `/run/script` is
    /// a **read-only bind mount**, so the file the supervisor wrote is the
    /// file that is `execve`d. The agent path carries the digest because its
    /// fallback shape puts the bytes on the wire.
    ///
    /// `secrets` is a pool request's own values, or `None` for a function's;
    /// see `place_secrets` in `pool/secrets.rs`.
    pub fn call_script_timed(
        &self,
        event: serde_json::Value,
        script: Option<crate::protocol::Script>,
        timeout: std::time::Duration,
        secrets: Option<BTreeMap<String, String>>,
    ) -> Result<(Outcome, CallTiming)> {
        use crate::backend::ns::enter;
        use std::io::Write as _;

        // Placed before anything is forked, and held for the whole request:
        // the lease is what keeps the file there while it runs and removes it
        // when the last request using it is done.
        let (script_path, _script_lease) = match script {
            None => (None, None),
            Some(script) => {
                let (path, lease) = self.place_script(script)?;
                (Some(path), lease)
            }
        };
        let argv = match &script_path {
            None => None,
            Some(path) => Some(self.plan.argv_with(path).map_err(|e| Error::Primitive {
                operation: "prepare a request's argv",
                remedy: "the script path contains a NUL byte; this is an internal error".into(),
                source: std::io::Error::other(e.to_string()),
            })?),
        };

        let id = next_request_id();
        let started = Instant::now();
        // A warm-exec function is one tenant's, and `owned_by` settled that
        // before the request arrived, so the entry needs no owner of its own.
        let request = self.in_flight.start(&id, None, None);
        let _lease = RequestLease {
            requests: &self.in_flight,
            id: &id,
        };

        // The request's cgroup exists before the request does, so the request
        // can be created inside it rather than moved there (see
        // `enter::enter_with`). A warm-exec function is one tenant's and its
        // limits were set at warm time; there is nothing per-request to narrow.
        let request_cgroup = request_cgroup(
            self.generation_cgroup.as_deref(),
            self.per_request_cgroup,
            &id,
            None,
        );
        let request_cgroup_dir = request_cgroup
            .as_deref()
            .and_then(|dir| std::fs::File::open(dir).ok());

        let entered = {
            let sandbox = self.sandbox.lock().expect("sandbox");
            let ns = sandbox
                .namespaces()
                .ok_or_else(|| Error::BackendUnavailable {
                    backend: "pool",
                    reason: "the sandbox has no namespace descriptors".into(),
                    remedy: "internal error; the function will be rewarmed".into(),
                })?;
            enter::enter_with(
                &self.plan,
                ns,
                argv.as_ref(),
                request_cgroup_dir
                    .as_ref()
                    .map(std::os::fd::AsRawFd::as_raw_fd),
            )
        };
        drop(request_cgroup_dir);
        let entered = match entered {
            Ok(entered) => entered,
            Err(e) => {
                if let Some(dir) = &request_cgroup {
                    let _ = crate::cgroup::Hierarchy::remove(dir);
                }
                // Could not even get a process into the sandbox. If the init
                // is gone the sandbox is gone; otherwise this request failed
                // and the next may not.
                // SAFETY: signal 0 checks existence, delivers nothing.
                if unsafe { libc::kill(self.init_pid as libc::pid_t, 0) } != 0 {
                    *self.state.lock().expect("state") = SandboxState::Failed;
                }
                self.record(false);
                return Err(e);
            }
        };
        let forked = Instant::now();

        // The request is parked until `go` is written: this is the window in
        // which it can be given its secrets — and, when the kernel would not
        // create it inside its cgroup, moved there — before it has run a
        // single instruction of tenant code. Its pid is already a host pid —
        // the helper's `clone3` returned it in the host's namespace — so
        // nothing needs translating.
        if let Some(dir) = &request_cgroup
            && !entered.placed
        {
            let _ = crate::cgroup::attach(dir, entered.pid);
        }
        // Through the descriptor the sandbox handed out at launch, never
        // through `/proc`: a held sandbox is not dumpable, and nothing an
        // unprivileged supervisor does changes that.
        let at = match &self.secrets_dir {
            Some(dir) => SecretsAt::Dir(std::os::fd::AsFd::as_fd(dir)),
            // No descriptor means the sandbox could not hand one out, which
            // the launcher already refused to start over — so this is only
            // reachable for a function with no secrets to place.
            None => SecretsAt::Proc(self.init_pid),
        };
        request.running_at(request_cgroup.as_deref(), entered.pid);
        let _secrets = match place_secrets(&self.secrets, at, secrets) {
            Ok(lease) => lease,
            Err(e) => {
                kill_request(request_cgroup.as_deref(), entered.pid);
                let _ = entered.reap_helper();
                self.record(false);
                return Err(e);
            }
        };
        let admitted = Instant::now();

        // As on the agent path: a cancel that beat the `go` write means the
        // request never starts, rather than starting and being killed.
        if request.cancelled() {
            kill_request(request_cgroup.as_deref(), entered.pid);
        }

        let mut entered = entered;
        if let Some(go) = entered.go.take() {
            let mut go = std::fs::File::from(go);
            let _ = go.write_all(&[1]);
            // Dropped here: closed.
        }
        // The event, then end of file — which is how a program that reads
        // stdin to completion knows the event is whole. Taking the descriptor
        // out and dropping it is the close; a copy left in `entered` would
        // hold the pipe open and such a program would wait for ever.
        if let Some(stdin) = entered.stdin.take() {
            let mut stdin = std::fs::File::from(stdin);
            let body = serde_json::to_vec(&event).unwrap_or_else(|_| b"null".to_vec());
            let _ = stdin.write_all(&body);
        }

        // Everything the request says, with the deadline enforced while it
        // is said. The function's own limit wins over the caller's.
        let budget = timeout.min(self.timeout);
        let deadline = budget.saturating_sub(admitted - started);
        let collected = collect_request(&entered, deadline, || {
            kill_request(request_cgroup.as_deref(), entered.pid)
        });

        let exit_status = read_status(&entered, STATUS_GRACE);
        let _ = entered.reap_helper();
        let done = Instant::now();

        drop(_secrets);
        if let Some(dir) = request_cgroup {
            let _ = crate::cgroup::Hierarchy::remove(&dir);
        }
        let cleaned = Instant::now();

        let outcome = collected
            .and_then(|c| into_outcome(c, exit_status, done - admitted))
            .map(|o| Outcome {
                id: id.clone(),
                cancelled: request.cancelled(),
                ..o
            });
        match &outcome {
            Ok(o) => self.record(o.succeeded()),
            Err(_) => self.record(false),
        }

        let timing = CallTiming {
            lock: std::time::Duration::ZERO,
            fork: forked - started,
            admit: admitted - forked,
            run: done - admitted,
            release: cleaned - done,
        };
        outcome.map(|o| (o, timing))
    }

    fn record(&self, ok: bool) {
        let mut counters = self.counters.lock().expect("counters");
        counters.requests += 1;
        if !ok {
            counters.failures += 1;
        }
    }
}

/// What a warm-exec request produced on its pipes.
struct Collected {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
    /// A hardening or `execve` failure reported before the program ran.
    launch_failure: Option<(crate::backend::ns::prepare::Step, i32)>,
}

/// Drain the request's stdout, stderr and error pipes until all three reach
/// end of file, killing the request at `deadline` and draining on.
///
/// `poll` over the three together, because reading them in turn deadlocks: a
/// program that fills stderr while we block on stdout never gets to finish.
fn collect_request(
    entered: &crate::backend::ns::enter::Entered,
    deadline: std::time::Duration,
    mut kill: impl FnMut(),
) -> Result<Collected> {
    use std::os::fd::AsRawFd;

    let mut collected = Collected {
        stdout: Vec::new(),
        stderr: Vec::new(),
        timed_out: false,
        launch_failure: None,
    };
    let mut err_buf = Vec::new();
    let mut open = [true; 3];
    let fds = [
        entered.stdout.as_raw_fd(),
        entered.stderr.as_raw_fd(),
        entered.err.as_raw_fd(),
    ];
    let mut due = Instant::now() + deadline;
    let mut killed = false;

    while open.iter().any(|o| *o) {
        let mut polls: Vec<libc::pollfd> = fds
            .iter()
            .zip(open)
            .filter(|(_, o)| *o)
            .map(|(fd, _)| libc::pollfd {
                fd: *fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        let left = due.saturating_duration_since(Instant::now());
        let ms = left.as_millis().min(i32::MAX as u128) as i32;
        // SAFETY: `polls` is a live slice of `pollfd`s over descriptors we own.
        let rc = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, ms) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(Error::primitive("poll", "warm-exec request pipes", e));
        }
        if rc == 0 {
            if killed {
                // Killed, and still no end of file: something else holds the
                // pipes open. Give up rather than wait for ever.
                break;
            }
            collected.timed_out = true;
            killed = true;
            kill();
            due = Instant::now() + KILL_GRACE;
            continue;
        }
        for p in &polls {
            if p.revents == 0 {
                continue;
            }
            let which = fds.iter().position(|fd| *fd == p.fd).expect("known fd");
            let mut buf = [0u8; 64 * 1024];
            // SAFETY: `buf` is live; the fd is ours.
            let n = unsafe { libc::read(p.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n <= 0 {
                open[which] = false;
                continue;
            }
            let chunk = &buf[..n as usize];
            match which {
                0 => collected.stdout.extend_from_slice(chunk),
                1 => collected.stderr.extend_from_slice(chunk),
                _ => err_buf.extend_from_slice(chunk),
            }
        }
    }

    if !err_buf.is_empty() {
        collected.launch_failure = crate::backend::ns::child::decode_failure(&err_buf);
    }
    Ok(collected)
}

/// The request's wait status from the helper, or `None` if the helper never
/// reported one *within `grace`*.
///
/// The bound is the whole point. This read used to block for ever, and
/// it runs immediately after `collect_request` has already given up waiting
/// for the request — so the one case it is reached in is the case where
/// something is wrong, and it answered that by hanging the caller instead of
/// the request. A deadline that is enforced up to the last step and then
/// abandoned at it is not a deadline.
///
/// The descriptor is put in non-blocking mode and polled, because there is no
/// timed `read` for a pipe. Four bytes arrive in one write or not at all.
fn read_status(
    entered: &crate::backend::ns::enter::Entered,
    grace: std::time::Duration,
) -> Option<i32> {
    use std::io::Read as _;
    use std::os::fd::AsRawFd as _;

    let fd = entered.status.try_clone().ok()?;
    let raw = fd.as_raw_fd();
    let deadline = Instant::now() + grace;
    let mut file = std::fs::File::from(fd);
    let mut bytes = [0u8; 4];
    let mut have = 0;

    loop {
        // SAFETY: `raw` is an open descriptor this process owns.
        let ready = unsafe {
            let mut pfd = libc::pollfd {
                fd: raw,
                events: libc::POLLIN,
                revents: 0,
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let ms = left.as_millis().min(i32::MAX as u128) as libc::c_int;
            libc::poll(&mut pfd, 1, ms)
        };
        if ready <= 0 {
            return None;
        }
        match file.read(&mut bytes[have..]) {
            Ok(0) => return None, // the helper closed without reporting
            Ok(n) => {
                have += n;
                if have == 4 {
                    return Some(i32::from_ne_bytes(bytes));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// How long `read_status` waits for the helper's four bytes.
///
/// Generous against the thing it bounds: the helper writes them immediately
/// after `waitpid` returns, so a second is several orders of magnitude more
/// than the good case needs, and any amount of waiting is better than none.
const STATUS_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// Turn what the request left behind into an [`Outcome`].
fn into_outcome(
    c: Collected,
    wait_status: Option<i32>,
    elapsed: std::time::Duration,
) -> Result<Outcome> {
    if let Some((step, errno)) = c.launch_failure {
        return Err(Error::Primitive {
            operation: step.describe(),
            remedy: step
                .remedy(errno)
                .unwrap_or(
                    "run `zygo doctor`; a warm-exec request needs the same primitives as a sandbox",
                )
                .to_string(),
            source: std::io::Error::from_raw_os_error(errno),
        });
    }

    let exit_code = match wait_status {
        Some(status) if libc::WIFEXITED(status) => libc::WEXITSTATUS(status),
        Some(status) if libc::WIFSIGNALED(status) => 128 + libc::WTERMSIG(status),
        _ => 1,
    };
    let stdout = String::from_utf8_lossy(&c.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&c.stderr).into_owned();

    // The contract: stdout is the JSON result. A program that printed
    // something else has not failed to run, so its exit code stands — but its
    // answer cannot be handed on as a result, and saying so is more useful
    // than a mangled one.
    let (result, error) = if c.timed_out {
        (
            serde_json::Value::Null,
            Some("killed by SIGKILL (the deadline expired)".to_string()),
        )
    } else if exit_code != 0 {
        (
            serde_json::Value::Null,
            Some(format!("the program exited {exit_code}")),
        )
    } else if stdout.trim().is_empty() {
        (serde_json::Value::Null, None)
    } else {
        match serde_json::from_str(&stdout) {
            Ok(value) => (value, None),
            Err(e) => (
                serde_json::Value::Null,
                Some(format!("stdout is not JSON: {e}")),
            ),
        }
    };

    Ok(Outcome {
        // Filled in by the caller, which is the side that knows both: it
        // assigned the id and it is holding the request's registry entry.
        id: String::new(),
        cancelled: false,
        stuck: false,
        workspace: None,
        tenant: "default".into(),
        function: "resize".into(),
        script: None,
        exit_code,
        result,
        stdout: if error.is_some() {
            stdout
        } else {
            String::new()
        },
        stderr,
        error,
        metrics: Metrics {
            // Measured here, because on this path there is nobody else to
            // measure it: a warm-exec request is a bare process, not an agent
            // that reports on itself. It was `Metrics::default()` — every
            // warm-exec request in the log timed at zero, so `zygo stats`
            // reported a p50 of `0.0 ms` for a function that was working.
            wall_ms: elapsed.as_secs_f64() * 1000.0,
            ..Metrics::default()
        },
        timed_out: c.timed_out,
    })
}
