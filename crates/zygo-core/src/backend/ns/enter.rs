// SPDX-License-Identifier: Apache-2.0
//! Entering a held sandbox to run one request (warm-exec).
//!
//! The sandbox was built once and its init is holding it. A request is a
//! fresh process that has to end up *inside* — same namespaces, same
//! hardening, same cgroup discipline — without an agent in the box to fork it.
//! That is done in two hops, because two of the rules cannot be satisfied by
//! one process:
//!
//! ```text
//!   supervisor ──fork──► helper ──setns(user,pid,net,ipc,uts)──clone3──► request
//!                          │                              (into its cgroup) │
//!                          │ reports the request's HOST pid                │ setns(cgroup)
//!                          │ (clone3 returns it in the helper's ns)        │ waits for GO
//!                          │ waits, reports the exit status                │ setns(mnt)
//!                          │                                               │ harden, execve
//! ```
//!
//! - The helper enters the pid namespace but stays out of the mount
//!   namespace. Entering `pid` affects *children*, so the request is born
//!   inside it — and because the helper is still in the host's pid namespace,
//!   the pid `fork()` returns is the host pid. No translation, unlike the
//!   agent path, where `FORKED` carries a pid from inside the sandbox.
//! - The request enters the mount namespace itself, after `GO`. Entering it
//!   in the helper would take the helper's view of the host away before it
//!   has reported anything.
//!
//! - The request is **born in its own cgroup** (`CLONE_INTO_CGROUP`) when the
//!   caller hands over the directory. Moving a process into a cgroup after the
//!   fact takes the kernel's thread-group lock for writing, and on Linux 6.0
//!   and later the first writer after a quiet spell waits out an RCU grace
//!   period: measured at ~9 ms for 1 request in 100 on a 6.8 VM. A process
//!   created inside the cgroup never moves, so never waits. The kernel only
//!   allows the target to be inside the caller's cgroup namespace, which is
//!   why the helper stays in the host's and the request enters the sandbox's
//!   itself. A kernel or policy that refuses gets a plain `fork` and the old
//!   move, which [`Entered::placed`] tells the caller to make.
//!
//! Why this is allowed rootless: `setns` into a user namespace needs
//! `CAP_SYS_ADMIN` in it, and a process in the parent namespace with the
//! creator's uid has every capability there. The supervisor created it; the
//! helper is its fork. `tests/poc/poc9_netns_pool.py` measured exactly this
//! case (its third row).
//!
//! Everything between `fork` and `execve` is async-signal-safe: the parent is
//! multi-threaded, and a child that allocates can deadlock on a lock some
//! other thread held at the moment of the fork.

use std::os::fd::{AsRawFd, OwnedFd};

use libc::{c_int, c_void};

use super::child;
use super::prepare::{PreparedLaunch, Step};
use crate::backend::NamespaceFds;
use crate::error::{Error, Result};

/// The descriptors and pids of one request, on the supervisor's side.
#[derive(Debug)]
pub struct Entered {
    /// Host pid of the helper. Reaped by [`Entered::reap_helper`].
    pub helper: u32,
    /// Host pid of the request process, for the cgroup and for the deadline.
    pub pid: u32,
    /// Whether the request was created inside the cgroup the caller named.
    /// `false` when none was named, or when the kernel refused
    /// `CLONE_INTO_CGROUP` — then the caller moves it, as before.
    pub placed: bool,
    /// Write one byte here once the request is in its cgroup. `Option` so the
    /// caller can take it and drop it — the drop is the close.
    pub go: Option<OwnedFd>,
    /// The request's stdin: take it, write the event, drop it. The drop is
    /// the end of file the program is waiting for.
    pub stdin: Option<OwnedFd>,
    pub stdout: OwnedFd,
    pub stderr: OwnedFd,
    /// The request's exit status, four native-endian bytes, written by the
    /// helper after it has waited for the request.
    pub status: OwnedFd,
    /// The request's hardening failure report, if any: eight bytes in the
    /// same format as a launch failure, or end of file when `execve` worked.
    pub err: OwnedFd,
}

impl Entered {
    /// Collect the helper. It exits on its own once the request has.
    pub fn reap_helper(&self) -> Result<()> {
        let mut status: c_int = 0;
        // SAFETY: `helper` is a child of this process that has not been reaped.
        let rc = unsafe { libc::waitpid(self.helper as libc::pid_t, &mut status, 0) };
        if rc < 0 {
            return Err(Error::primitive(
                "waitpid",
                "the warm-exec helper could not be reaped",
                std::io::Error::last_os_error(),
            ));
        }
        Ok(())
    }
}

/// Fork the helper, have it enter the sandbox and fork the request, and
/// return once the request's host pid is known.
///
/// The request is created but *parked*: it runs nothing until [`Entered::go`]
/// is written, which is the supervisor's moment to put it in a cgroup. Same
/// handshake as `FORKED`/`GO` on the agent path, for the same reason.
pub fn enter(plan: &PreparedLaunch, ns: &NamespaceFds) -> Result<Entered> {
    enter_with(plan, ns, None, None)
}

/// The same, with one more argument on this request's argv.
///
/// The warm-exec **pool** shape: one held sandbox running the same program,
/// and each request's script named on the end of its command line. The tail is
/// materialised by the caller before the fork — see
/// [`crate::backend::ns::prepare::PreparedLaunch::argv_with`] — because the
/// side of a fork that `execve`s it may not allocate.
///
/// `cgroup` is a descriptor open on the request's own cgroup directory, when
/// the caller made one: the request is created inside it rather than moved
/// there. See the module notes, and [`Entered::placed`].
pub fn enter_with(
    plan: &PreparedLaunch,
    ns: &NamespaceFds,
    argv: Option<&crate::backend::ns::prepare::RequestArgv<'_>>,
    cgroup: Option<std::os::fd::RawFd>,
) -> Result<Entered> {
    let argv_ptr = match argv {
        Some(argv) => argv.as_ptr(),
        None => plan.argv(),
    };
    // Every pipe close-on-exec, and every copy made of one below keeps the
    // flag: the request `dup2`s the three it keeps onto 0, 1 and 2 (which
    // clears the flag on those three, as it must) and the rest vanish at
    // `execve` without anyone having to remember them. The renumbering used
    // `F_DUPFD`/`dup2`, which clear the flag, so the rest did not vanish at
    // all — see the comment there.
    let (go_r, go_w) = pipe_cloexec()?;
    let (stdin_r, stdin_w) = pipe_cloexec()?;
    let (stdout_r, stdout_w) = pipe_cloexec()?;
    let (stderr_r, stderr_w) = pipe_cloexec()?;
    let (status_r, status_w) = pipe_cloexec()?;
    let (err_r, err_w) = pipe_cloexec()?;

    // SAFETY: the child does nothing but syscalls on descriptors and plan data
    // that were fully materialised before the fork, and never returns.
    let helper = unsafe { libc::fork() };
    if helper < 0 {
        return Err(Error::primitive(
            "fork",
            "could not fork the warm-exec helper",
            std::io::Error::last_os_error(),
        ));
    }
    if helper == 0 {
        // SAFETY: see above. `helper_main` never returns.
        unsafe {
            helper_main(
                plan,
                ns,
                argv_ptr,
                cgroup.unwrap_or(-1),
                Ends {
                    go_r: go_r.as_raw_fd(),
                    stdin_r: stdin_r.as_raw_fd(),
                    stdout_w: stdout_w.as_raw_fd(),
                    stderr_w: stderr_w.as_raw_fd(),
                    status_w: status_w.as_raw_fd(),
                    err_w: err_w.as_raw_fd(),
                },
            )
        }
    }

    // Parent. Drop the ends the children own, so end-of-file can ever arrive
    // on the ones we read.
    drop((go_r, stdin_r, stdout_w, stderr_w, status_w, err_w));

    // The helper's first act after entering the namespaces is to report the
    // request's pid. Anything else — a short read, or a failure payload on the
    // error pipe — means it could not.
    let mut pid_bytes = [0u8; 4];
    if read_exact(status_r.as_raw_fd(), &mut pid_bytes).is_err() {
        let mut payload = [0u8; 8];
        let n = read_some(err_r.as_raw_fd(), &mut payload);
        reap(helper);
        return Err(match child::decode_failure(&payload[..n]) {
            Some((step, errno)) => Error::Primitive {
                operation: step.describe(),
                remedy: step
                    .remedy(errno)
                    .unwrap_or("run `zygo doctor`; entering a sandbox needs the same primitives as building one")
                    .to_string(),
                source: std::io::Error::from_raw_os_error(errno),
            },
            None => Error::primitive(
                "warm-exec helper",
                "the helper exited before reporting the request's pid",
                std::io::Error::other("no pid received"),
            ),
        });
    }

    // The top bit carries whether the request was born in its cgroup; pids
    // stop at 2^22 (`pid_max`'s ceiling), so it is never part of one.
    let word = u32::from_ne_bytes(pid_bytes);
    Ok(Entered {
        helper: helper as u32,
        pid: word & !PLACED_BIT,
        placed: word & PLACED_BIT != 0,
        go: Some(go_w),
        stdin: Some(stdin_w),
        stdout: stdout_r,
        stderr: stderr_r,
        status: status_r,
        err: err_r,
    })
}

/// Set in the pid word the helper reports when the request was created inside
/// its cgroup.
const PLACED_BIT: u32 = 1 << 31;

/// Raw descriptors the children work with.
#[derive(Clone, Copy)]
struct Ends {
    go_r: c_int,
    stdin_r: c_int,
    stdout_w: c_int,
    stderr_w: c_int,
    status_w: c_int,
    err_w: c_int,
}

/// `close_range(2)`, by number: the libc crate's constant is not on every
/// target, and the number is the same on every architecture.
const SYS_CLOSE_RANGE: libc::c_long = 436;

/// Close every descriptor from `first` upwards.
///
/// `close_range` on kernels that have it (5.9+); a loop otherwise. The loop's
/// ceiling is the soft `RLIMIT_NOFILE`, which is the most a process can have
/// open and is usually 1024.
///
/// # Safety
/// Child side of a fork: syscalls only.
pub(super) unsafe fn close_from(first: c_int) {
    // SAFETY: plain syscall; a failure only means the fallback runs.
    if unsafe { libc::syscall(SYS_CLOSE_RANGE, first, c_int::MAX, 0) } == 0 {
        return;
    }
    let mut limit = libc::rlimit {
        rlim_cur: 1024,
        rlim_max: 1024,
    };
    // SAFETY: `limit` is live for the call.
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) };
    let ceiling = limit.rlim_cur.min(65_536) as c_int;
    for fd in first..ceiling {
        // SAFETY: closing a descriptor we may or may not hold; EBADF is fine.
        unsafe { libc::close(fd) };
    }
}

/// The helper: enter, fork the request, report, wait, report, exit.
///
/// # Safety
/// Child side of a fork from a multi-threaded parent: nothing here allocates,
/// and it never returns.
unsafe fn helper_main(
    plan: &PreparedLaunch,
    ns: &NamespaceFds,
    argv: *const *const std::os::raw::c_char,
    into: c_int,
    ends: Ends,
) -> ! {
    // Die with the supervisor, so a request cannot outlive the thing that is
    // enforcing its deadline.
    // SAFETY: `prctl` with constant arguments; async-signal-safe and touches no
    // memory.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) };

    // Descriptor hygiene, and it is not optional. `fork` copied the
    // supervisor's whole table: the control socket, every agent's connection,
    // every other in-flight request's pipes — and the *parent's* ends of this
    // request's own pipes. Holding the parent's write end of stdin here means
    // the program never sees end of file; measured as `cat` echoing its input
    // and then waiting the full 30 s deadline. Holding another request's
    // stdin does the same to *that* request.
    //
    // Keep exactly the descriptors this and the request need — thirteen, and
    // a fourteenth when there is a cgroup to create the request in — at known
    // low numbers, and close everything else. Copies are made first so no
    // `dup2` can overwrite a descriptor that has not been copied yet.
    let needed = [
        ns.user.as_raw_fd(),
        ns.pid.as_raw_fd(),
        ns.net.as_raw_fd(),
        ns.ipc.as_raw_fd(),
        ns.uts.as_raw_fd(),
        ns.cgroup.as_raw_fd(),
        ns.mnt.as_raw_fd(),
        ends.go_r,
        ends.stdin_r,
        ends.stdout_w,
        ends.stderr_w,
        ends.status_w,
        ends.err_w,
        into,
    ];
    // The error pipe's place in `needed`, which a failure below reports down.
    const ERR: usize = 12;
    let count = if into >= 0 {
        needed.len()
    } else {
        needed.len() - 1
    };
    const FIRST: c_int = 3;
    const PARKED: c_int = 64;
    let mut parked = [0 as c_int; 14];
    for (i, fd) in needed.iter().take(count).enumerate() {
        // `F_DUPFD_CLOEXEC`, not `F_DUPFD`. The plain form *clears*
        // close-on-exec on the copy, and nothing set it again — so the tenant
        // program inherited all thirteen descriptors past `execve`: seven
        // namespace descriptors, second copies of its own stdio, and the
        // helper's error pipe. `setns` is denied by the seccomp filter, so it
        // was a leak rather than an escape, but the comment at the top of this
        // function promised the opposite.
        //
        // SAFETY: `fcntl` with F_DUPFD_CLOEXEC on a descriptor we hold.
        parked[i] = unsafe { libc::fcntl(*fd, libc::F_DUPFD_CLOEXEC, PARKED) };
        if parked[i] < 0 {
            child::fail(ends.err_w, Step::Setns);
        }
    }
    for (i, fd) in parked.iter().take(count).enumerate() {
        // `dup3` with `O_CLOEXEC` for the same reason: `dup2` clears the flag
        // on the new descriptor.
        //
        // SAFETY: both are descriptors we hold, and they differ — `parked` is
        // at 64 and above, the targets are 3..16, so `dup3`'s EINVAL for equal
        // descriptors cannot arise.
        if unsafe { libc::dup3(*fd, FIRST + i as c_int, libc::O_CLOEXEC) } < 0 {
            // The *parked* copy of the error pipe. `ends.err_w` cannot be
            // used here — this loop is overwriting 3..16 and the original may
            // already have been one of them — and the original code passed
            // `*fd`, the descriptor being duplicated, so a failure reported
            // itself down whichever pipe happened to be in hand.
            child::fail(parked[ERR], Step::Setns);
        }
    }
    // SAFETY: `close_from`'s contract holds: child side of the fork, syscalls
    // only. Everything at `FIRST + count` and above is a copy this helper does
    // not need.
    unsafe { close_from(FIRST + count as c_int) };

    let user = FIRST;
    let (pid_ns, net, ipc, uts, cgroup, mnt) = (
        FIRST + 1,
        FIRST + 2,
        FIRST + 3,
        FIRST + 4,
        FIRST + 5,
        FIRST + 6,
    );
    let ends = Ends {
        go_r: FIRST + 7,
        stdin_r: FIRST + 8,
        stdout_w: FIRST + 9,
        stderr_w: FIRST + 10,
        status_w: FIRST + 11,
        err_w: FIRST + 12,
    };
    let into = if count == needed.len() {
        FIRST + 13
    } else {
        -1
    };

    // User first: it is what grants the capability to enter the others. Mount
    // and cgroup are deliberately absent — the request enters those itself:
    // mount because the helper still needs the host's view, cgroup because
    // `CLONE_INTO_CGROUP` only accepts a cgroup inside the *caller's* cgroup
    // namespace, and the request's is not inside the sandbox's.
    for (fd, kind) in [
        (user, libc::CLONE_NEWUSER),
        (pid_ns, libc::CLONE_NEWPID),
        (net, libc::CLONE_NEWNET),
        (ipc, libc::CLONE_NEWIPC),
        (uts, libc::CLONE_NEWUTS),
    ] {
        // SAFETY: `setns` takes a descriptor this helper renumbered a moment
        // ago and a constant; it touches no memory.
        if unsafe { libc::setns(fd, kind) } != 0 {
            child::fail(ends.err_w, Step::Setns);
        }
    }

    // Born in its cgroup when there is one to be born in (see the module
    // notes). Any refusal — a kernel before 5.7, a policy, a cgroup that
    // went away — falls back to the plain fork and the supervisor's move.
    //
    // SAFETY: single-threaded here; the request does only syscalls.
    let (request, placed) = match into {
        // SAFETY: this helper is a single-threaded fork of the supervisor and
        // the request it forks runs only syscalls.
        -1 => (unsafe { libc::fork() }, false),
        // SAFETY: `clone3_into`'s fork-like contract holds for the same reason;
        // `dir` is the cgroup descriptor renumbered above.
        dir => match unsafe { super::clone::clone3_into(0, Some(dir)) } {
            Ok(super::clone::CloneResult::Child) => (0, true),
            Ok(super::clone::CloneResult::Parent { child }) => (child as libc::pid_t, true),
            // SAFETY: as for the plain `fork` above; the refusal left nothing
            // behind.
            Err(_) => (unsafe { libc::fork() }, false),
        },
    };
    if request < 0 {
        child::fail(ends.err_w, Step::Setns);
    }
    if request == 0 {
        // SAFETY: `request_main`'s contract is this function's: child side of a
        // fork, plan and argv built before it, descriptors renumbered by this
        // helper. It never returns.
        unsafe { request_main(plan, argv, cgroup, mnt, ends) }
    }
    if into >= 0 {
        // SAFETY: `into` is this helper's own copy of the cgroup descriptor and
        // is not used again.
        unsafe { libc::close(into) };
    }

    // Only the status pipe stays open here. Closing `err_w` matters: the
    // request's copy closes at `execve`, and the parent takes end of file on
    // that pipe as "it worked" — which never arrives while this copy is open.
    // SAFETY: each is a descriptor this helper owns, at the number it put it;
    // closing the copies here leaves the request's own untouched.
    unsafe {
        libc::close(ends.err_w);
        libc::close(ends.go_r);
        libc::close(ends.stdin_r);
        libc::close(ends.stdout_w);
        libc::close(ends.stderr_w);
    }

    // `fork()` returned the request's pid in *this* namespace — the host's.
    // The top bit says whether it was born in its cgroup.
    let word = if placed {
        request as u32 | PLACED_BIT
    } else {
        request as u32
    };
    let pid = word.to_ne_bytes();
    write_all_raw(ends.status_w, &pid);

    let mut status: c_int = 0;
    // SAFETY: waiting for our own child.
    while unsafe { libc::waitpid(request, &mut status, 0) } < 0 {
        if child::errno() != libc::EINTR {
            // SAFETY: `_exit` touches no memory and never returns.
            unsafe { libc::_exit(1) };
        }
    }
    write_all_raw(ends.status_w, &status.to_ne_bytes());
    // SAFETY: `_exit` touches no memory and never returns.
    unsafe { libc::_exit(0) }
}

/// The request process: wait for `GO`, enter the mount namespace, wire up
/// stdio, harden exactly as the sandbox init did, and run the program.
///
/// `cgroup` and `mnt` are namespace descriptors, already renumbered by the
/// helper.
///
/// # Safety
/// As [`helper_main`].
unsafe fn request_main(
    plan: &PreparedLaunch,
    argv: *const *const std::os::raw::c_char,
    cgroup: c_int,
    mnt: c_int,
    ends: Ends,
) -> ! {
    // SAFETY: `status_w` is the helper's pipe end at the number the helper put
    // it; the request does not report status, the helper does.
    unsafe {
        libc::close(ends.status_w);
    }

    // Die with the helper, asked for first. A helper that died in the moment
    // between the fork and this line sends nothing; the request then either
    // never gets `GO` and exits below, or runs to its deadline, which the
    // supervisor enforces by host pid, not through the helper.
    // SAFETY: `prctl` with constant arguments; async-signal-safe and touches no
    // memory.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) };

    // The sandbox's cgroup namespace, entered here rather than in the helper
    // so the helper could create this process inside its cgroup. It changes
    // only what `/proc/self/cgroup` shows; the process is already where it
    // belongs, or about to be moved there by the supervisor.
    // SAFETY: `cgroup` is the namespace descriptor the helper renumbered for
    // this request; `setns` takes it and a constant.
    if unsafe { libc::setns(cgroup, libc::CLONE_NEWCGROUP) } != 0 {
        child::fail(ends.err_w, Step::Setns);
    }

    // Parked until the supervisor has put this pid in its cgroup. Until then
    // every allocation would be billed to the tenant's cgroup as a whole
    // rather than to this request.
    let mut byte = [0u8; 1];
    // SAFETY: `go_r` is a descriptor this request owns and `byte` is a live
    // one-byte local.
    if unsafe { libc::read(ends.go_r, byte.as_mut_ptr() as *mut c_void, 1) } != 1 {
        // The supervisor gave up on us (a cgroup it could not create, say).
        // SAFETY: `_exit` touches no memory and never returns.
        unsafe { libc::_exit(127) }
    }
    // SAFETY: `go_r` is this request's own descriptor and is not used again.
    unsafe { libc::close(ends.go_r) };

    // Now the filesystem: the pivoted root the init built. Needs
    // `CAP_SYS_ADMIN` in the sandbox's user namespace, which the helper's
    // `setns(user)` granted and this fork inherited.
    // SAFETY: `mnt` is the namespace descriptor the helper renumbered for this
    // request; `setns` takes it and a constant.
    if unsafe { libc::setns(mnt, libc::CLONE_NEWNS) } != 0 {
        child::fail(ends.err_w, Step::EnterMountNamespace);
    }
    // SAFETY: `workdir` is a NUL-terminated `CString` the plan owns for the
    // life of this request.
    if unsafe { libc::chdir(plan.workdir.as_ptr()) } != 0
        // SAFETY: `chdir` on a NUL-terminated literal.
        && unsafe { libc::chdir(c"/".as_ptr()) } != 0
    {
        child::fail(ends.err_w, Step::Chdir);
    }

    // The event arrives on stdin; the result leaves on stdout. `dup2` onto the
    // standard descriptors clears close-on-exec on the copies, which is the
    // only reason these three survive `execve` when every other pipe does not.
    for (from, to) in [(ends.stdin_r, 0), (ends.stdout_w, 1), (ends.stderr_w, 2)] {
        // SAFETY: `dup2` between descriptors this request owns; it touches no
        // memory.
        if unsafe { libc::dup2(from, to) } < 0 {
            child::fail(ends.err_w, Step::WireStdio);
        }
    }

    // Exactly what the init went through, so a request is never less
    // constrained than the sandbox it is in.
    // SAFETY: `harden`'s contract holds: child side of a fork, plan built
    // before it, `err_w` owned here.
    unsafe { child::harden(plan, ends.err_w) };
    // SAFETY: `exec_or_fail`'s contract holds: `argv` and the plan's `envp()`
    // are NUL-terminated pointer arrays built before the fork and alive until
    // `execve` replaces this image.
    unsafe { child::exec_or_fail(plan, argv, ends.err_w) }
}

fn pipe_cloexec() -> Result<(OwnedFd, OwnedFd)> {
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .map_err(|e| Error::primitive("pipe", "internal warm-exec error", e.into()))
}

/// `write` in a loop, without touching the heap. For the children.
fn write_all_raw(fd: c_int, buf: &[u8]) {
    let mut written = 0;
    while written < buf.len() {
        // SAFETY: `buf` is live for the call.
        let n = unsafe {
            libc::write(
                fd,
                buf[written..].as_ptr() as *const c_void,
                buf.len() - written,
            )
        };
        if n <= 0 {
            return;
        }
        written += n as usize;
    }
}

/// Fill `buf` or fail. Parent side.
fn read_exact(fd: c_int, buf: &mut [u8]) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        // SAFETY: `buf` is live for the call.
        let n = unsafe {
            libc::read(
                fd,
                buf[filled..].as_mut_ptr() as *mut c_void,
                buf.len() - filled,
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "short read",
            ));
        }
        filled += n as usize;
    }
    Ok(())
}

/// Read what is there, up to `buf.len()`, returning how much. Parent side.
fn read_some(fd: c_int, buf: &mut [u8]) -> usize {
    // SAFETY: `buf` is live for the call.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len()) };
    if n < 0 { 0 } else { n as usize }
}

fn reap(pid: libc::pid_t) {
    let mut status: c_int = 0;
    // SAFETY: waiting for our own child.
    unsafe { libc::waitpid(pid, &mut status, 0) };
}
