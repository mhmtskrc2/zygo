// SPDX-License-Identifier: Apache-2.0
//! A one-shot sandbox run for a client, with the client's own streams.
//!
//! What `zygo run` asks the supervisor to do on its behalf when it cannot
//! get into a delegated cgroup itself. The three descriptors arrive over
//! `SCM_RIGHTS` after the `RUN` frame; the sandbox is started on the
//! launcher thread and waited on here; and [`ClientWatch`] kills it if the
//! client hangs up first, which is the `PDEATHSIG` a client-started
//! sandbox would have had.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
#[cfg(target_os = "linux")]
use std::time::Duration;
use std::time::Instant;

use super::{ControlError, Response, Supervisor};
use crate::error::{Error, Result};
use crate::spec::{Layer, ResolveOptions, Spec};

impl Supervisor {
    /// Route one request to its function.
    /// Run a one-shot sandbox on a client's behalf, with the client's streams.
    ///
    /// The start goes to the launcher thread like every other start (see
    /// `Launcher`); the *wait* stays here, on this connection's own thread,
    /// because a launcher that waited would hold every other start on the
    /// machine for as long as this program ran.
    ///
    /// `started` is called with the pid and the cgroup as soon as the sandbox
    /// exists and before it is waited on, so the client can forward its
    /// terminal's signals to it — a Ctrl-C at the client reaches the client,
    /// and the sandbox is this process's child, not the client's — and so the
    /// connection can kill it if the client goes away (see `ClientWatch`).
    pub fn run(
        &self,
        spec: Option<&Spec>,
        layer: &Layer,
        options: &ResolveOptions,
        client: crate::pool::ClientStreams,
        mut started: impl FnMut(u32, Option<&std::path::Path>) -> Result<()>,
    ) -> std::result::Result<Response, Response> {
        let owned;
        let spec = match spec {
            Some(s) => s,
            None => {
                owned = Spec::default();
                &owned
            }
        };
        // `resolve`, not `resolve_for_serve`: this is `zygo run`'s own
        // resolution, one-shot rules and all, so a run through the
        // supervisor is the same run it would have been without one.
        let resolved = spec
            .resolve(None, layer, options)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;

        // On the launcher thread, never here: see [`Launcher`].
        let pool = Arc::clone(&self.pool);
        let oneshot = self
            .launcher
            .run(move || pool.start_oneshot(&resolved, client))
            .map_err(|e| Response::error(ControlError::WarmFailed, e))?
            .map_err(|e| Response::error(ControlError::WarmFailed, e))?;
        let crate::pool::Oneshot {
            mut sandbox,
            newroot,
        } = oneshot;

        started(sandbox.pid(), sandbox.cgroup())
            .map_err(|e| Response::error(ControlError::CallFailed, e))?;

        let clock = Instant::now();
        let waited = sandbox.wait();
        // Sampled during teardown, the last moment the cgroup exists.
        let kernel = sandbox.outcome();
        // Its `pivot_root` target: empty once the sandbox is gone, and left
        // in place if it is not, for the reason `zygo run` gives.
        let _ = std::fs::remove_dir(&newroot);

        Ok(Response::Ran {
            exit_code: match &waited {
                Ok(code) => (*code).clamp(0, 255),
                Err(e) => e.exit_code(),
            },
            timed_out: waited.as_ref().err().is_some_and(Error::timed_out),
            oom_killed: kernel.oom_kills > 0,
            peak_rss_kb: kernel.peak_rss_kb,
            wall_ms: clock.elapsed().as_secs_f64() * 1000.0,
        })
    }
}

/// What happens to a `RUN` sandbox when its client goes away.
///
/// A sandbox `zygo run` starts itself has `PR_SET_PDEATHSIG`: when that
/// `zygo` dies, the kernel kills the sandbox. A sandbox started here is this
/// process's child, and the client's death is only a hang-up on its socket —
/// so a thread watches the socket and delivers the kill the kernel would
/// have. Without it a client killed with `SIGKILL`, or a terminal that
/// vanished without a `SIGHUP`, left the program running to its deadline,
/// or for ever under `--allow-unlimited`, writing into a pipe nobody read.
///
/// `poll` for `POLLRDHUP` only, so data on the socket does not wake it, and
/// with a short timeout so the thread notices [`ClientWatch::finished`] and
/// ends soon after the run does rather than living as long as the
/// connection.
pub(super) struct ClientWatch {
    done: Arc<AtomicBool>,
    target: Arc<Mutex<Option<WatchTarget>>>,
}

/// The sandbox a hang-up kills: its init, and its cgroup when it has one.
#[derive(Clone)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct WatchTarget {
    pid: u32,
    cgroup: Option<PathBuf>,
}

impl ClientWatch {
    pub(super) fn start(client: &UnixStream) -> ClientWatch {
        let watch = ClientWatch {
            done: Arc::new(AtomicBool::new(false)),
            target: Arc::new(Mutex::new(None)),
        };
        // Its own descriptor, so a number the connection closes and the
        // kernel hands to somebody else is never polled here.
        if let Ok(own) = client.try_clone() {
            let done = Arc::clone(&watch.done);
            let target = Arc::clone(&watch.target);
            std::thread::Builder::new()
                .name("client-watch".into())
                .spawn(move || ClientWatch::watch(own, &done, &target))
                .ok();
        }
        watch
    }

    /// The sandbox exists: this is what a hang-up kills from now on.
    pub(super) fn started(&self, pid: u32, cgroup: Option<&std::path::Path>) {
        if let Ok(mut target) = self.target.lock() {
            *target = Some(WatchTarget {
                pid,
                cgroup: cgroup.map(PathBuf::from),
            });
        }
    }

    /// The sandbox has been reaped: nothing left to kill, and its pid may be
    /// somebody else's soon.
    pub(super) fn finished(&self) {
        self.done.store(true, Ordering::SeqCst);
    }

    #[cfg(target_os = "linux")]
    fn watch(own: UnixStream, done: &AtomicBool, target: &Mutex<Option<WatchTarget>>) {
        use std::os::fd::AsRawFd;
        /// `POLLRDHUP`: the peer shut its side down. Kernel ABI since 2.6.17.
        const POLLRDHUP: libc::c_short = 0x2000;
        let mut pfd = libc::pollfd {
            fd: own.as_raw_fd(),
            events: POLLRDHUP,
            revents: 0,
        };
        while !done.load(Ordering::SeqCst) {
            pfd.revents = 0;
            // SAFETY: `pfd` is a live local naming a descriptor this thread
            // owns; a 250 ms timeout bounds the call.
            let rc = unsafe { libc::poll(&mut pfd, 1, 250) };
            let hung_up = rc > 0
                && pfd.revents & (POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0;
            if !hung_up {
                continue;
            }
            // The start may still be queued on the launcher: wait for a pid
            // rather than letting a sandbox that starts a moment later run
            // for nobody.
            while !done.load(Ordering::SeqCst) {
                let target = target.lock().ok().and_then(|t| t.clone());
                let Some(WatchTarget { pid, cgroup }) = target else {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                };
                tracing::info!(pid, "RUN client went away; killing its sandbox");
                // `cgroup.kill` takes the subtree in one write and cannot
                // reach a reused pid; the pid is for a kernel without it.
                let killed = cgroup
                    .as_deref()
                    .is_some_and(|dir| matches!(crate::cgroup::kill(dir), Ok(true)));
                if !killed && !done.load(Ordering::SeqCst) {
                    // SAFETY: `kill` on a pid this process has not reaped.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
                }
                return;
            }
            return;
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn watch(_own: UnixStream, _done: &AtomicBool, _target: &Mutex<Option<WatchTarget>>) {}
}

/// The three descriptors a `RUN` sends after its frame: stdin, stdout, stderr.
///
/// Each arrives as one `SCM_RIGHTS` message with one byte of payload, and
/// `MSG_CMSG_CLOEXEC` is set at receipt, so none of them can leak into
/// anything else this supervisor spawns.
#[cfg(target_os = "linux")]
pub(super) fn receive_stdio(raw: &UnixStream) -> std::io::Result<[std::os::fd::OwnedFd; 3]> {
    use std::os::fd::AsRawFd;
    let fd = raw.as_raw_fd();
    Ok([
        crate::net::linux::recv_fd(fd)?,
        crate::net::linux::recv_fd(fd)?,
        crate::net::linux::recv_fd(fd)?,
    ])
}

#[cfg(not(target_os = "linux"))]
pub(super) fn receive_stdio(_raw: &UnixStream) -> std::io::Result<[std::os::fd::OwnedFd; 3]> {
    Err(std::io::Error::other("a one-shot sandbox is a Linux thing"))
}
