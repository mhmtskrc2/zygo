// SPDX-License-Identifier: Apache-2.0
//! The control socket: bound before the process detaches, one thread per
//! connection, and nobody else's uid.
//!
//! [`Listener::bind`] takes the socket or explains who has it;
//! [`Listener::serve`] is the accept loop, with the idle thread and the
//! `SIGTERM` drain around it. The peer check at the bottom fails closed:
//! a connection whose owner cannot be read is refused, because the
//! alternative is arbitrary code execution as this user.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;

use super::dispatch::handle;
use super::lifecycle::{DEFAULT_TERM_GRACE, TERMINATING, TIER_INTERVAL};
use super::{ControlError, Response, Supervisor};
use crate::error::{Error, IoContext, Result};
use crate::paths::Paths;

/// A bound control socket, with the pid file that names its owner.
///
/// Separate from [`Supervisor`] so the socket can be bound — and a conflicting
/// supervisor detected — *before* a background process is spawned. Otherwise
/// the failure would arrive after the parent had already detached.
#[derive(Debug)]
pub struct Listener {
    pub(super) listener: UnixListener,
    socket: PathBuf,
    pid_file: PathBuf,
}

impl Listener {
    /// Take the control socket, or explain who already has it.
    ///
    /// A socket left behind by a supervisor that died is not a conflict: it is
    /// a file nobody is listening on. The two are told apart by connecting —
    /// the only test that cannot be fooled by a stale pid file, a recycled pid,
    /// or a socket whose owner is wedged.
    pub fn bind(paths: &Paths) -> Result<Listener> {
        paths.ensure()?;
        let socket = paths.supervisor_sock();

        if socket.exists() {
            match UnixStream::connect(&socket) {
                Ok(_) => {
                    return Err(Error::BackendUnavailable {
                        backend: "supervisor",
                        reason: format!(
                            "a supervisor is already listening on {}",
                            socket.display()
                        ),
                        remedy: "use it, or stop it with `zygo stop --all`".into(),
                    });
                }
                // Nobody is home. Anything else — a permission error, say — is
                // a real problem and must not be papered over by unlinking.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    std::fs::remove_file(&socket).at(&socket)?;
                }
                Err(e) => return Err(Error::primitive("connect", "supervisor socket", e)),
            }
        }

        let listener = UnixListener::bind(&socket).at(&socket)?;
        restrict(&socket)?;

        let pid_file = paths.supervisor_pid();
        let mut file = std::fs::File::create(&pid_file).at(&pid_file)?;
        writeln!(file, "{}", std::process::id()).at(&pid_file)?;

        Ok(Listener {
            listener,
            socket,
            pid_file,
        })
    }

    pub fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    /// Accept connections until a `SHUTDOWN` arrives.
    ///
    /// One thread per connection. A control connection is a CLI invocation, so
    /// there are units of them rather than thousands, and a thread apiece keeps
    /// the request path a straight line with no executor in it.
    pub fn serve(self, supervisor: Arc<Supervisor>) -> Result<()> {
        // Unblocks `accept` when `SHUTDOWN` sets the flag: without it the loop
        // would sit in `accept` until some unrelated client happened to connect.
        self.listener
            .set_nonblocking(false)
            .map_err(|e| Error::primitive("set_nonblocking", "control socket", e))?;

        // The idle policy runs on its own thread rather than on whichever
        // connection happens to arrive, so a supervisor nobody is talking to
        // still gives its memory back.
        let tiering = {
            let supervisor = Arc::clone(&supervisor);
            std::thread::Builder::new()
                .name("zygo-idle".into())
                .spawn(move || {
                    while !supervisor.is_stopping() {
                        std::thread::sleep(TIER_INTERVAL);
                        let tiered = supervisor.tier_idle();
                        for name in &tiered.paused {
                            tracing::info!(function = %name, "idle: paused");
                        }
                        for name in &tiered.cooled {
                            tracing::info!(function = %name, "idle: gone cold");
                        }
                        // Both directions in one pass, on one thread: a pool
                        // that is shrinking must not be growing at the same
                        // time, and the cheapest way to guarantee that is for
                        // the same loop to do both.
                        for name in supervisor.scale_runtimes() {
                            tracing::info!(runtime = %name, "load: grew by one zygote");
                        }
                    }
                })
                .map_err(|e| Error::primitive("spawn", "idle thread", e))?
        };

        // `SIGTERM` drains, like `DRAIN` does, rather than stopping where it
        // stands. A supervisor killed mid-request loses that request's answer,
        // and a `systemctl restart` or a container stop is exactly the moment
        // somebody is waiting for one.
        //
        // The handler cannot do the draining — it runs on a signal stack and
        // may not allocate or lock — so it sets the same flag `SHUTDOWN` sets
        // and wakes the accept loop by connecting to the socket, which is what
        // `handle` already does. The draining itself happens below, once the
        // loop is out.
        #[cfg(unix)]
        let _term = {
            let path = supervisor.paths().supervisor_sock();
            crate::supervisor::on_terminate(move || {
                TERMINATING.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = UnixStream::connect(&path);
            })
        };

        let mut threads = Vec::new();
        for stream in self.listener.incoming() {
            if TERMINATING.load(std::sync::atomic::Ordering::SeqCst) {
                tracing::info!("SIGTERM: draining");
                supervisor.drain(DEFAULT_TERM_GRACE);
                supervisor.shutdown();
                break;
            }
            if supervisor.is_stopping() {
                break;
            }
            let stream = match stream {
                Ok(stream) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(Error::primitive("accept", "control socket", e)),
            };
            let supervisor = Arc::clone(&supervisor);
            threads.push(std::thread::spawn(move || {
                // One bad client must not take the supervisor with it.
                if let Err(e) = handle(&supervisor, stream) {
                    tracing::debug!("control connection ended: {e}");
                }
            }));
            threads.retain(|t: &std::thread::JoinHandle<()>| !t.is_finished());
        }

        for t in threads {
            let _ = t.join();
        }
        let _ = tiering.join();
        let _ = supervisor.stop(None, true);
        Ok(())
    }
}

impl Drop for Listener {
    /// Take the socket and pid file with us. A supervisor that exits cleanly
    /// must not leave a file that makes the next one think it has a conflict.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.pid_file);
    }
}

/// Refuse a connection from another user.
///
/// The socket is `0600` inside a `0700` directory, so this should be
/// unreachable — which is exactly why it is worth checking. File modes are one
/// `umask`, one `--data-root` on a shared path, or one container bind mount
/// away from being wrong, and the consequence is arbitrary code execution as
/// this user.
pub(super) fn reject_foreign_peer(stream: &UnixStream) -> Option<Response> {
    peer_verdict(current_uid(), peer_uid(stream))
}

/// The decision itself, separated from the syscall that feeds it.
///
/// Fails **closed**. `peer_uid` returns `None` when the credentials could not
/// be read at all, and the honest reading of that is "this connection's owner
/// is unknown" — which, for a check whose failure mode is arbitrary code
/// execution as this user, has to be a refusal. It read as `None` meaning
/// *allowed* until this was found, because the syscall and the policy shared
/// one `?`.
pub(super) fn peer_verdict(ours: u32, peer: Option<u32>) -> Option<Response> {
    match peer {
        None => Some(Response::error(
            ControlError::Unauthorised,
            "cannot read the credentials of the process at the other end of this \
             connection, so it is refused"
                .to_string(),
        )),
        Some(peer) if peer != ours => Some(Response::error(
            ControlError::Unauthorised,
            format!("this supervisor belongs to uid {ours}, not uid {peer}"),
        )),
        Some(_) => None,
    }
}

#[cfg(target_os = "linux")]
pub(super) fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;

    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are live and correctly sized for SO_PEERCRED,
    // and the fd is owned by `stream` for the duration of the call.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    };
    (rc == 0).then_some(cred.uid)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn peer_uid(stream: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;

    let (mut uid, mut gid) = (0u32, 0u32);
    // SAFETY: both out-params are live for the call and the fd is owned by
    // `stream`. `getpeereid` is the BSD/macOS spelling of `SO_PEERCRED`.
    let rc = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
    (rc == 0).then_some(uid)
}

pub(super) fn current_uid() -> u32 {
    // SAFETY: `getuid` takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

/// `0600` on a path that only its owner may use.
fn restrict(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).at(path)
}
