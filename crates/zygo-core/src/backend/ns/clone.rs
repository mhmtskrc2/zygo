// SPDX-License-Identifier: Apache-2.0
//! `clone3`: the first step of building a sandbox.
//!
//! `clone3` rather than `unshare` for a reason the first measurements made
//! concrete: `unshare(CLONE_NEWPID)` does *not* move the caller into the new
//! pid namespace, it only makes the caller's future children its first members.
//! A process that has merely unshared cannot mount a fresh `proc` — it gets
//! EPERM — so an `unshare`-based launcher needs a second fork before it can
//! build the mount plan.
//!
//! `clone3(CLONE_NEWPID)` makes the *child* pid 1 in the new namespace
//! directly, so the mount plan can proceed immediately.

use std::io;

/// `struct clone_args` from `include/uapi/linux/sched.h`.
///
/// Only the fields up to `cgroup` exist; the kernel accepts a struct shorter
/// than it knows about and zero-fills the rest, which is how `clone3` stays
/// forward compatible.
#[repr(C, align(8))]
#[derive(Debug, Default, Clone, Copy)]
pub struct CloneArgs {
    pub flags: u64,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    /// Signal delivered to the parent on exit. `SIGCHLD` to make `wait4` work.
    pub exit_signal: u64,
    /// Zero means "share the parent's stack, copy-on-write", i.e. fork-like.
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
    pub set_tid: u64,
    pub set_tid_size: u64,
    pub cgroup: u64,
}

#[cfg(target_os = "linux")]
const SYS_CLONE3: libc::c_long = 435;

/// Outcome of a successful [`clone3`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloneResult {
    /// In the parent: the child's pid *in the parent's* namespace.
    Parent { child: u32 },
    /// In the child.
    Child,
}

/// Create a child process with the given namespace flags.
///
/// # Safety
///
/// This is `fork`-shaped: it returns twice. Between the return in the child and
/// `execve`, only async-signal-safe operations are permitted — no allocation,
/// no locks, no Rust runtime machinery — because the parent may be
/// multi-threaded and the child inherits only the calling thread. Everything
/// the child needs must be prepared before this call (see
/// [`super::prepare::PreparedLaunch`]).
#[cfg(target_os = "linux")]
pub unsafe fn clone3(flags: u64) -> io::Result<CloneResult> {
    // SAFETY: the caller's contract is this function's.
    unsafe { clone3_into(flags, None) }
}

/// `CLONE_INTO_CGROUP` (Linux 5.7): the child is born in the cgroup `cgroup`
/// names, rather than moved there afterwards.
pub const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

/// [`clone3`], with the child created directly inside the cgroup whose
/// directory `cgroup` is open on.
///
/// Why it matters: moving a process into a cgroup (`cgroup.procs`) takes the
/// kernel's thread-group lock for writing, and the first writer after a quiet
/// spell waits out an RCU grace period. On an adopter's benchmark VM that was
/// 3.4–6.6 ms of a 6–10 ms sandbox start, every run. A child created in its
/// cgroup never migrates, so it never waits — which is how kern starts a box
/// in a few milliseconds. It is also the stricter order: the limits apply
/// from the child's first instruction rather than from the moment the
/// parent got round to moving it.
///
/// # Safety
///
/// As for [`clone3`].
#[cfg(target_os = "linux")]
pub unsafe fn clone3_into(
    flags: u64,
    cgroup: Option<std::os::fd::RawFd>,
) -> io::Result<CloneResult> {
    let mut args = CloneArgs {
        flags,
        exit_signal: libc::SIGCHLD as u64,
        ..Default::default()
    };
    if let Some(fd) = cgroup {
        args.flags |= CLONE_INTO_CGROUP;
        args.cgroup = fd as u64;
    }

    // SAFETY: `args` is a correctly sized, correctly aligned clone_args, and
    // the caller has accepted the fork-like contract documented above.
    let rc = unsafe {
        libc::syscall(
            SYS_CLONE3,
            &mut args as *mut CloneArgs,
            core::mem::size_of::<CloneArgs>(),
        )
    };

    match rc {
        -1 => Err(io::Error::last_os_error()),
        0 => Ok(CloneResult::Child),
        pid => Ok(CloneResult::Parent { child: pid as u32 }),
    }
}

/// The non-Linux stand-in for [`clone3_into`], so the callers compile
/// everywhere: it always fails with `Unsupported`.
///
/// # Safety
///
/// Nothing unsafe happens here; the signature matches the Linux one, and
/// so does its contract.
#[cfg(not(target_os = "linux"))]
pub unsafe fn clone3_into(
    _flags: u64,
    _cgroup: Option<std::os::fd::RawFd>,
) -> io::Result<CloneResult> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clone3 is a Linux syscall",
    ))
}

/// The non-Linux stand-in for [`clone3`], so the callers compile
/// everywhere: it always fails with `Unsupported`.
///
/// # Safety
///
/// Nothing unsafe happens here; the signature matches the Linux one, and
/// so does its contract.
#[cfg(not(target_os = "linux"))]
pub unsafe fn clone3(_flags: u64) -> io::Result<CloneResult> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clone3 is a Linux syscall",
    ))
}

/// Whether `clone3` exists on this kernel (5.3+).
///
/// Probed rather than inferred from the version, because a seccomp policy on
/// the *host* can make it return `ENOSYS` on a kernel that has it — which is
/// exactly what Docker's default profile does.
#[cfg(target_os = "linux")]
pub fn is_available() -> bool {
    // An intentionally invalid size: the kernel validates the size before it
    // does anything else, so this returns EINVAL when clone3 exists and
    // ENOSYS when it does not. Nothing is cloned either way.
    // SAFETY: a null pointer with size 0 is rejected by the kernel before it
    // reads anything, so no process is created and no memory is touched.
    let rc = unsafe { libc::syscall(SYS_CLONE3, core::ptr::null::<CloneArgs>(), 0usize) };
    debug_assert_eq!(rc, -1, "the probe must never actually clone");
    io::Error::last_os_error().raw_os_error() != Some(libc::ENOSYS)
}

#[cfg(not(target_os = "linux"))]
pub fn is_available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clone_args_matches_the_kernel_layout() {
        // 11 u64 fields; the kernel's CLONE_ARGS_SIZE_VER2.
        assert_eq!(core::mem::size_of::<CloneArgs>(), 88);
        assert_eq!(core::mem::align_of::<CloneArgs>(), 8);
    }

    #[test]
    fn default_clone_args_are_fork_like() {
        let a = CloneArgs::default();
        assert_eq!(
            a.stack, 0,
            "a zero stack means share the parent's, like fork"
        );
        assert_eq!(a.stack_size, 0);
        assert_eq!(a.set_tid_size, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn availability_probe_does_not_fork() {
        // If the probe ever actually cloned, the test binary would fork and the
        // harness would report duplicated results.
        let before = std::process::id();
        let _ = is_available();
        assert_eq!(std::process::id(), before);
    }
}
