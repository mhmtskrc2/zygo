// SPDX-License-Identifier: Apache-2.0
//! The child side of the launch, between `clone3` and `execve`.
//!
//! **Every function here must be async-signal-safe.** No allocation, no
//! locking, no panicking, no formatting — the parent may be multi-threaded and
//! this child inherited exactly one of its threads. A single `format!` here can
//! deadlock on the allocator's mutex and hang the sandbox forever.
//!
//! Failures are therefore reported as a `(step, errno)` pair written to a pipe;
//! the parent turns that back into a sentence. The write end of that pipe is
//! `O_CLOEXEC`, so a successful `execve` closes it and the parent reads EOF —
//! success needs no message at all.

use std::os::raw::{c_char, c_int, c_void};

use std::ffi::CString;

use super::prepare::{
    AT_RECURSIVE, LOCKED_REMOUNT, MOUNT_ATTR_NODEV, MOUNT_ATTR_NOSUID, MOUNT_ATTR_RDONLY,
    PSEUDO_FLAGS, PreparedLaunch, PreparedOp, READONLY_REMOUNT, ROOT_FLAGS, SYS_MOUNT_SETATTR,
    SYSFS_FLAGS, Step,
};

/// Byte the parent sends once `uid_map`/`gid_map` are in place.
pub const READY_OK: u8 = 1;
/// Byte the parent sends when it could not write them.
pub const READY_FAILED: u8 = 2;

/// Exit status of a child that failed before `execve`. Matches the convention
/// `docker run` uses for "could not start the container".
pub const EXIT_LAUNCH_FAILED: c_int = 127;

const MNT_DETACH: c_int = 2;
const SYS_PIVOT_ROOT: libc::c_long = if cfg!(target_arch = "aarch64") {
    41
} else {
    155
};

/// Run the child's sequence. Never returns: it either `execve`s or `_exit`s.
///
/// # Safety
///
/// Must be called only in the child of a fork-like clone, with `plan` prepared
/// before that clone, and with both file descriptors owned by this child.
pub unsafe fn child_main(plan: &PreparedLaunch, ready_fd: c_int, err_fd: c_int) -> ! {
    // 0. Die with the parent — asked for first, before anything can happen
    //    that the parent's death should interrupt.
    // SAFETY: `prctl` with constant arguments touches no memory and is
    // async-signal-safe, which is all this side of the clone may call.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) };

    // 1. Wait for the parent to write the id maps. Until they exist this
    //    process has no valid uid and cannot mount anything.
    let mut signal = [0u8; 1];
    // SAFETY: `ready_fd` is owned by this child (the caller's contract) and the
    // read lands in a one-byte local that outlives the call.
    let n = unsafe { libc::read(ready_fd, signal.as_mut_ptr() as *mut c_void, 1) };
    if n != 1 {
        fail(err_fd, Step::WaitForIdMaps);
    }
    if signal[0] != READY_OK {
        fail_with(err_fd, Step::IdMapsRejected, libc::EPERM);
    }
    // A parent that died *before* the prctl above sends no signal. It may
    // still have written the byte first, so the byte proves nothing; the
    // pipe does. The parent holds its end open until this process has
    // exec'd or failed, so a hung-up pipe here means it is gone. After this
    // check a death is the prctl's to handle, and before it, this check's.
    //
    // It used to be `getppid() == 1`, after the hardening. In a new pid
    // namespace the parent is outside it and `getppid` is always 0, so that
    // check could never fire.
    let mut pfd = libc::pollfd {
        fd: ready_fd,
        events: 0,
        revents: 0,
    };
    // SAFETY: `pfd` is a live local for the whole call, and `poll` is
    // async-signal-safe.
    if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 && pfd.revents & libc::POLLHUP != 0 {
        fail_with(err_fd, Step::ParentDied, libc::ESRCH);
    }
    // SAFETY: `ready_fd` is this child's own descriptor and is never used
    // again.
    unsafe { libc::close(ready_fd) };

    // 2. The mount plan.
    for op in &plan.ops {
        // SAFETY: `apply`'s contract is this function's: child side of the
        // clone, `op` prepared before it, `err_fd` owned here.
        unsafe { apply(op, err_fd) };
    }

    // 3. Commit the new root.
    //
    //    `pivot_root(".", ".")` puts the old root *on top of* the new one at
    //    the same point, so it can be detached immediately. The alternative
    //    needs a spare directory inside the image to hold the old root, which
    //    a read-only image may not have.
    // SAFETY: `newroot` is a NUL-terminated `CString` the plan owns for as long
    // as this child runs.
    if unsafe { libc::chdir(plan.newroot.as_ptr()) } != 0 {
        fail(err_fd, Step::PivotRoot);
    }
    let dot = c".".as_ptr();
    // SAFETY: `pivot_root` by number; `dot` is a NUL-terminated literal the
    // kernel only reads.
    if unsafe { libc::syscall(SYS_PIVOT_ROOT, dot, dot) } != 0 {
        fail(err_fd, Step::PivotRoot);
    }
    // SAFETY: `umount2` reads the same literal and takes a constant flag.
    if unsafe { libc::umount2(dot, MNT_DETACH) } != 0 {
        fail(err_fd, Step::UnmountOldRoot);
    }
    // SAFETY: `chdir` on a NUL-terminated literal.
    if unsafe { libc::chdir(c"/".as_ptr()) } != 0 {
        fail(err_fd, Step::PivotRoot);
    }

    // 4. Adopt the terminal allocated for this sandbox, if there is one.
    //
    //    Done after `pivot_root` so the fd survives the root change (it is a
    //    descriptor, not a path) and before privileges are dropped, since
    //    `TIOCSCTTY` needs the process to be a session leader.
    //
    //    Three distinct streams take precedence over one terminal: they are
    //    the client's own stdin, stdout and stderr, handed to the supervisor
    //    to start this sandbox on the client's behalf, and collapsing them
    //    into one would send a program's stderr down its caller's stdout.
    if let Some(fds) = plan.stdio_streams {
        // SAFETY: `adopt_streams`' contract holds: child side, three
        // descriptors the plan says this child owns, all at 3 or above (checked
        // by the supervisor before the plan was built).
        unsafe { adopt_streams(fds, err_fd) };
    } else if let Some(fd) = plan.stdio {
        // SAFETY: `adopt_terminal`'s contract holds: child side, a descriptor
        // the plan says this child owns.
        unsafe { adopt_terminal(fd, err_fd) };
    }
    //    And the caller's signal dispositions, when the program is being
    //    started for somebody else.
    if let Some(mask) = plan.ignored_signals {
        // SAFETY: `signal` and `sigprocmask` are async-signal-safe, and this
        // process has exactly one thread.
        unsafe { reset_signals(mask) };
    }

    // 5. The runtime agent's socket, at a fixed descriptor so its argv can
    //    name it. `execve` would otherwise close it: Rust marks sockets
    //    close-on-exec, and the whole point is for the agent to inherit it.
    if let Some(fd) = plan.agent_fd {
        // SAFETY: `place_agent_socket`'s contract holds: child side, a
        // descriptor the plan says this child owns.
        unsafe { place_agent_socket(fd, err_fd) };
    }

    // 6. Identity of the sandbox as seen from inside.
    let host = plan.hostname.as_bytes();
    // SAFETY: `sethostname` reads exactly `host.len()` bytes of a string the
    // plan owns for the life of this child.
    if unsafe { libc::sethostname(host.as_ptr() as *const c_char, host.len()) } != 0 {
        // A sandbox with the wrong hostname still runs correctly; not fatal.
    }

    if plan.bring_up_loopback {
        // SAFETY: `bring_up_loopback`'s contract holds: child side, syscalls
        // only.
        unsafe { bring_up_loopback(err_fd) };
    }

    // SAFETY: `workdir` is a NUL-terminated `CString` the plan owns for the
    // life of this child.
    if unsafe { libc::chdir(plan.workdir.as_ptr()) } != 0 {
        // The image may not have the configured working directory; `/` always
        // exists and is a better outcome than refusing to start.
        // SAFETY: `chdir` on a NUL-terminated literal.
        if unsafe { libc::chdir(c"/".as_ptr()) } != 0 {
            fail(err_fd, Step::Chdir);
        }
    }

    // 6b. `/run/secrets`, and its descriptor out to the supervisor.
    //
    //     Here and nowhere else: the directory needs the mounts, which are
    //     done, and creating it needs capabilities this process is about to
    //     drop. See `hand_out_secrets_dir` for why a descriptor rather than a
    //     path.
    if let Some(sock) = plan.secrets_fd {
        // SAFETY: `hand_out_secrets_dir`'s contract holds: child side, and
        // `sock` is the plan's descriptor for this child.
        unsafe { hand_out_secrets_dir(sock, err_fd) };
    }

    // 7–10. Limits, privilege, Landlock and seccomp.
    // SAFETY: `harden`'s contract is this function's: child side, plan prepared
    // before the clone.
    unsafe { harden(plan, err_fd) };

    // 11. Hand the sandbox over — to the program, or to a loop that keeps it.
    if plan.hold {
        // SAFETY: `hold`'s contract holds: child side, and it never returns.
        unsafe { hold(err_fd) }
    }
    // SAFETY: `exec_or_fail`'s contract holds: `argv()` and the plan's `envp()`
    // are NUL-terminated pointer arrays built before the clone and owned by the
    // plan for as long as this child runs.
    unsafe { exec_or_fail(plan, plan.argv(), err_fd) }
}

/// Steps 7–10: everything that turns a process with the sandbox's view of the
/// world into one that cannot get out of it.
///
/// Shared by the sandbox init and by every warm-exec request process, which is
/// a fresh fork that has *entered* the namespaces and has to be constrained
/// exactly as the init was — or the second path would be a hole the first one
/// closed.
///
/// # Safety
/// Child side of a clone or fork: nothing here may allocate.
pub(super) unsafe fn harden(plan: &PreparedLaunch, err_fd: c_int) {
    for (resource, value) in &plan.rlimits {
        let limit = libc::rlimit {
            rlim_cur: *value as libc::rlim_t,
            rlim_max: *value as libc::rlim_t,
        };
        // SAFETY: `limit` is a live local and `setrlimit` only reads it;
        // `resource` is one of the constants the plan was built from.
        if unsafe { libc::setrlimit(*resource as _, &limit) } != 0 {
            fail(err_fd, Step::SetRlimit);
        }
    }

    // `no_new_privs` before dropping capabilities: it is what stops a setuid
    // binary inside the image from regaining any of them.
    // SAFETY: `prctl` with constant arguments; touches no memory.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        fail(err_fd, Step::NoNewPrivs);
    }

    // SAFETY: `drop_all_capabilities` is `prctl` and one `capset` on stack
    // data, all async-signal-safe.
    if plan.drop_capabilities && unsafe { !drop_all_capabilities() } {
        fail(err_fd, Step::DropCapabilities);
    }

    // Landlock, before seccomp: building the ruleset needs `open`, which the
    // seccomp profile still permits but which is cleaner to do while the
    // process is otherwise unconstrained. Both require `no_new_privs`, set
    // just above.
    if !plan.landlock.is_empty()
        // SAFETY: `landlock::apply`'s contract holds: `no_new_privs` was set
        // just above, the ruleset is plan data from before the clone, and it
        // allocates nothing.
        && let Err(e) = unsafe { super::landlock::apply(&plan.landlock) }
    {
        fail_with(err_fd, Step::ApplyLandlock, e.raw_os_error().unwrap_or(0));
    }

    // The syscall filter. After `no_new_privs` (the kernel requires it for an
    // unprivileged caller) and after every other setup step, because the
    // filter denies most of what those steps needed.
    if !plan.seccomp.is_empty()
        // SAFETY: `seccomp::install`'s contract holds: `no_new_privs` is set,
        // `plan.seccomp` outlives the call, and it allocates nothing.
        && let Err(e) = unsafe { super::seccomp::install(&plan.seccomp, plan.seccomp_log) }
    {
        fail_with(err_fd, Step::InstallSeccomp, e.raw_os_error().unwrap_or(0));
    }
}

/// Step 11: `execve` the program. On success this never returns and `err_fd`
/// closes on exec, which is how the parent learns it worked.
///
/// The candidates are the `PATH` search, pre-computed by the parent: `execvp`
/// would do it here, but it allocates, which this side of the clone may not.
/// The last failure is the one reported, so a command that simply does not
/// exist reports ENOENT rather than the errno of whichever directory happened
/// to be checked last.
///
/// # Safety
/// Child side of a clone or fork: nothing here may allocate.
pub(super) unsafe fn exec_or_fail(
    plan: &PreparedLaunch,
    argv: *const *const c_char,
    err_fd: c_int,
) -> ! {
    for candidate in &plan.program_candidates {
        // SAFETY: `candidate` is a NUL-terminated `CString`, and `argv` and
        // `envp()` are NUL-terminated pointer arrays the plan owns; a
        // successful `execve` replaces this image, and a failed one leaves
        // every pointer as it was for the next try.
        unsafe { libc::execve(candidate.as_ptr(), argv, plan.envp()) };
    }
    fail(err_fd, Step::Execve);
}

/// Create `/run/secrets` and hand its descriptor to the supervisor.
///
/// The supervisor delivers secrets as files it writes from *outside* the
/// sandbox, so that nothing inside ever holds a value. It used to reach them
/// through `/proc/<init>/root` — which works only while that process is
/// **dumpable**, and writing the id map cleared that for everything in here
/// before the mounts even began. As root the check is bypassed and nobody
/// noticed; unprivileged, every write was refused.
///
/// A descriptor has no such problem: it names the directory itself, it
/// survives the process that opened it, and it needs no `/proc` at all. This
/// is the one moment it can be taken — after the root is committed, so the
/// directory can exist, and before `harden`, which drops the capabilities
/// that creating it needs and may install a Landlock ruleset that forbids it.
///
/// The child keeps nothing: the descriptor is closed here, and the socket
/// with it. For a held sandbox this process goes on to `hold`, which is
/// deliberately unreadable to anything else; a copy of this descriptor left
/// open in it would be a way back in.
///
/// # Safety
/// Child side of the clone: nothing here allocates, and every call is
/// async-signal-safe.
unsafe fn hand_out_secrets_dir(sock: std::os::fd::RawFd, err_fd: c_int) {
    // SAFETY: child side of the clone: `mkdir`, `open`, `close` and `sendmsg`
    // are async-signal-safe and the path is a literal. `__errno_location` is
    // this thread's own errno. `send_fd`'s contract holds (a forked child,
    // stack only), and `sock` and `dir` are descriptors this child owns and
    // closes exactly once.
    unsafe {
        let path = c"/run/secrets";
        // `EEXIST` is fine and expected on a rewarm into a reused tmpfs.
        if libc::mkdir(path.as_ptr(), 0o700) != 0 && *libc::__errno_location() != libc::EEXIST {
            fail(err_fd, Step::HandOutSecretsDir);
        }
        let dir = libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        );
        if dir < 0 {
            fail(err_fd, Step::HandOutSecretsDir);
        }
        if crate::net::linux::send_fd(sock, dir).is_err() {
            fail(err_fd, Step::HandOutSecretsDir);
        }
        libc::close(dir);
        libc::close(sock);
    }
}

/// Keep a warm-exec sandbox alive as its init, forever.
///
/// Two duties. First, it is pid 1 of the pid namespace, so anything a request
/// leaves orphaned reparents here and has to be reaped or it stays a zombie
/// inside the sandbox. Second, it must be *unreadable*: this process is a
/// fork of the supervisor and still maps the supervisor's memory, in the same
/// namespaces as tenant code running under the same uid. `PR_SET_DUMPABLE`
/// off is what refuses `ptrace` and `/proc/1/mem` to that code; the seccomp
/// profile denies `ptrace` too, and this holds even if it did not.
///
/// Closing `err_fd` is the "ready" signal: the parent is waiting for end of
/// file on it, which `execve` would have provided and this never reaches.
///
/// # Safety
/// Child side of the clone: nothing here may allocate.
unsafe fn hold(err_fd: c_int) -> ! {
    // SAFETY: child side of the clone: `prctl` with constants, `close` on the
    // descriptor this child owns, and `close_from` is syscalls only.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        libc::close(err_fd);
        // Nothing else is needed, and everything else is the supervisor's:
        // its control socket, the agents' connections, whatever was open at
        // the clone. An init that never execs keeps them for ever otherwise.
        super::enter::close_from(3);
    }
    loop {
        let mut status: c_int = 0;
        // Blocks while there is an orphan to wait for; ECHILD when there is
        // none, in which case sleep and look again. A second of latency in
        // reaping a stray zombie is nothing, and it costs no CPU to wait.
        // SAFETY: `status` is a live local; pid -1 waits for any child, which
        // as pid 1 of the namespace is exactly what this init must reap.
        if unsafe { libc::waitpid(-1, &mut status, 0) } < 0 {
            let pause = libc::timespec {
                tv_sec: 1,
                tv_nsec: 0,
            };
            // SAFETY: `pause` is a live local and a null remainder pointer is
            // allowed.
            unsafe { libc::nanosleep(&pause, core::ptr::null_mut()) };
        }
    }
}

/// Apply one prepared mount.
///
/// # Safety
/// Child side of the clone: nothing here may allocate. `op` is the plan's,
/// prepared before the clone, and `err_fd` is this child's own descriptor.
unsafe fn apply(op: &PreparedOp, err_fd: c_int) {
    let null = core::ptr::null::<c_char>();

    match op {
        PreparedOp::MakeRootPrivate => {
            // SAFETY: `mount` reads NUL-terminated literals and the documented
            // null arguments; the propagation flags take no data pointer.
            let rc = unsafe {
                libc::mount(
                    null,
                    c"/".as_ptr(),
                    null,
                    (super::prepare::MS_REC | super::prepare::MS_PRIVATE) as libc::c_ulong,
                    core::ptr::null(),
                )
            };
            if rc != 0 {
                fail(err_fd, Step::MakeRootPrivate);
            }
        }

        PreparedOp::Overlay { target, options } => {
            // SAFETY: `target` and `options` are NUL-terminated `CString`s the
            // plan owns for the life of this child; `mount` only reads them.
            let rc = unsafe {
                libc::mount(
                    c"overlay".as_ptr(),
                    target.as_ptr(),
                    c"overlay".as_ptr(),
                    ROOT_FLAGS as libc::c_ulong,
                    options.as_ptr() as *const c_void,
                )
            };
            if rc != 0 {
                fail(err_fd, Step::MountRoot);
            }
        }

        PreparedOp::BindRoot {
            source,
            target,
            readonly,
            submounts,
        } => {
            // SAFETY: `source` and `target` are NUL-terminated `CString`s the
            // plan owns; a bind takes no data pointer.
            let rc = unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    null,
                    (super::prepare::MS_BIND | super::prepare::MS_REC) as libc::c_ulong,
                    core::ptr::null(),
                )
            };
            if rc != 0 {
                fail(err_fd, Step::MountRoot);
            }
            // A bind mount ignores flags at mount time; read-only has to be a
            // second, explicit step. Skipped only for a derived-layer build,
            // whose root is a private copy made to be written.
            if *readonly {
                // SAFETY: `lock_bind`'s contract holds: child side, and
                // `target` and `submounts` are the plan's `CString`s.
                unsafe { lock_bind(target, true, submounts, err_fd, Step::MountRoot) };
            }
        }

        PreparedOp::Tmpfs {
            target,
            options,
            flags,
        } => {
            // A tmpfs mounted over `/dev` hides whatever the image had there,
            // including the `shm` and `pts` directories the later mounts need.
            // Recreate the mount point on the fresh, writable filesystem.
            // SAFETY: `target` is a NUL-terminated `CString` the plan owns.
            unsafe { ensure_dir(target.as_ptr()) };
            // SAFETY: `target` and `options` are NUL-terminated `CString`s the
            // plan owns; `flags` is a plain integer.
            let rc = unsafe {
                libc::mount(
                    c"tmpfs".as_ptr(),
                    target.as_ptr(),
                    c"tmpfs".as_ptr(),
                    *flags as libc::c_ulong,
                    options.as_ptr() as *const c_void,
                )
            };
            if rc != 0 {
                fail(err_fd, Step::MountTmpfs);
            }
        }

        PreparedOp::Bind {
            source,
            target,
            readonly,
            submounts,
        } => {
            // The mount point, where the image did not already provide one.
            // The rootfs view creates every target the plan names, but only
            // on the *image's* tree: a target inside a tmpfs this plan
            // mounted — `/run/script`, where a pool's scripts are bound in —
            // exists nowhere until now. Harmless where it is already there,
            // and where the parent is read-only it fails and the mount below
            // reports the real problem.
            // SAFETY: both are NUL-terminated `CString`s the plan owns, which
            // is `ensure_mount_point`'s only requirement.
            unsafe { ensure_mount_point(source.as_ptr(), target.as_ptr()) };
            // SAFETY: `source` and `target` are NUL-terminated `CString`s the
            // plan owns; a bind takes no data pointer.
            let rc = unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    null,
                    (super::prepare::MS_BIND | super::prepare::MS_REC) as libc::c_ulong,
                    core::ptr::null(),
                )
            };
            if rc != 0 {
                fail_at(err_fd, Step::MountBind, target.as_ptr());
            }
            let step = if *readonly {
                Step::RemountReadOnly
            } else {
                Step::MountBind
            };
            // SAFETY: `lock_bind`'s contract holds: child side, and `target`
            // and `submounts` are the plan's `CString`s.
            unsafe { lock_bind(target, *readonly, submounts, err_fd, step) };
        }

        PreparedOp::Proc { target } => {
            // SAFETY: `target` is a NUL-terminated `CString` the plan owns; the
            // rest are literals and a null data pointer.
            let rc = unsafe {
                libc::mount(
                    c"proc".as_ptr(),
                    target.as_ptr(),
                    c"proc".as_ptr(),
                    PSEUDO_FLAGS as libc::c_ulong,
                    core::ptr::null(),
                )
            };
            if rc != 0 {
                fail(err_fd, Step::MountProc);
            }
        }

        PreparedOp::Sysfs { target } => {
            // SAFETY: `target` is a NUL-terminated `CString` the plan owns; the
            // rest are literals and a null data pointer.
            let rc = unsafe {
                libc::mount(
                    c"sysfs".as_ptr(),
                    target.as_ptr(),
                    c"sysfs".as_ptr(),
                    SYSFS_FLAGS as libc::c_ulong,
                    core::ptr::null(),
                )
            };
            // `/sys` is a convenience, not a boundary: some kernels refuse it
            // in a user namespace whose network namespace it does not own. The
            // sandbox runs fine without it.
            let _ = rc;
        }

        PreparedOp::DevPts { target, options } => {
            // SAFETY: `target` is a NUL-terminated `CString` the plan owns.
            unsafe { ensure_dir(target.as_ptr()) };
            // SAFETY: `target` and `options` are NUL-terminated `CString`s the
            // plan owns; the type is a literal.
            let rc = unsafe {
                libc::mount(
                    c"devpts".as_ptr(),
                    target.as_ptr(),
                    c"devpts".as_ptr(),
                    // No flags: a fresh `devpts` instance, not a bind and
                    // not recursive.
                    0,
                    options.as_ptr() as *const c_void,
                )
            };
            // Only `--tty` needs a pty; not having one is not a failure.
            let _ = rc;
        }

        PreparedOp::DevNode { source, target } => {
            // The target lives on the `/dev` tmpfs mounted a moment ago, so it
            // is writable: create an empty file to bind over. `mknod` is not
            // available to an unprivileged user namespace (PoC 6).
            // SAFETY: `target` is a NUL-terminated `CString` the plan owns;
            // `open` only reads it.
            let fd = unsafe { libc::open(target.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o644) };
            if fd >= 0 {
                // SAFETY: `fd` was just returned by `open` and nothing else
                // holds it.
                unsafe { libc::close(fd) };
            }
            // SAFETY: `source` and `target` are NUL-terminated `CString`s the
            // plan owns; a bind takes no data pointer.
            let rc = unsafe {
                libc::mount(
                    source.as_ptr(),
                    target.as_ptr(),
                    null,
                    super::prepare::MS_BIND as libc::c_ulong,
                    core::ptr::null(),
                )
            };
            if rc != 0 {
                fail(err_fd, Step::MountDevNode);
            }
        }

        PreparedOp::Mask { target } => {
            // SAFETY: all-zero bytes are a valid `stat`: it is plain data the
            // kernel fills in.
            let mut st: libc::stat = unsafe { core::mem::zeroed() };
            // SAFETY: `target` is a NUL-terminated `CString` the plan owns and
            // `st` is a live local for the call.
            if unsafe { libc::stat(target.as_ptr(), &mut st) } != 0 {
                // Kernels differ in which of these exist; a path that is not
                // there needs no masking.
                return;
            }
            let rc = if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
                // An empty read-only tmpfs hides a directory's contents.
                // SAFETY: `target` is a NUL-terminated `CString` the plan owns;
                // the rest are literals, including the options string.
                unsafe {
                    libc::mount(
                        c"tmpfs".as_ptr(),
                        target.as_ptr(),
                        c"tmpfs".as_ptr(),
                        (ROOT_FLAGS | PSEUDO_FLAGS) as libc::c_ulong,
                        c"size=0,mode=000".as_ptr() as *const c_void,
                    )
                }
            } else {
                // SAFETY: `target` is a NUL-terminated `CString` the plan owns;
                // the source is a literal and a bind takes no data pointer.
                unsafe {
                    libc::mount(
                        c"/dev/null".as_ptr(),
                        target.as_ptr(),
                        null,
                        super::prepare::MS_BIND as libc::c_ulong,
                        core::ptr::null(),
                    )
                }
            };
            if rc != 0 {
                fail(err_fd, Step::MaskPath);
            }
        }

        PreparedOp::RemountReadOnly { target } => {
            // SAFETY: all-zero bytes are a valid `stat`: it is plain data the
            // kernel fills in.
            let mut st: libc::stat = unsafe { core::mem::zeroed() };
            // SAFETY: `target` is a NUL-terminated `CString` the plan owns and
            // `st` is a live local for the call.
            if unsafe { libc::stat(target.as_ptr(), &mut st) } != 0 {
                return;
            }
            // Bind it to itself first: a path that is not already a mount point
            // cannot be remounted.
            // SAFETY: `target` is a NUL-terminated `CString` the plan owns; a
            // bind to itself takes no data pointer.
            unsafe {
                libc::mount(
                    target.as_ptr(),
                    target.as_ptr(),
                    null,
                    super::prepare::MS_BIND as libc::c_ulong,
                    core::ptr::null(),
                )
            };
            // SAFETY: `target` is a NUL-terminated `CString` the plan owns; a
            // remount takes null source, type and data.
            let rc = unsafe {
                libc::mount(
                    null,
                    target.as_ptr(),
                    null,
                    READONLY_REMOUNT as libc::c_ulong,
                    core::ptr::null(),
                )
            };
            if rc != 0 {
                fail(err_fd, Step::RemountReadOnly);
            }
        }
    }
}

/// `struct mount_attr`, version 0.
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// Make a recursive bind `nosuid,nodev` — and read-only when `readonly` —
/// all the way down.
///
/// A remount changes only the mount it names. Every mount *below* the source
/// that `MS_REC` carried in would keep its own flags: a `:ro` volume with a
/// writable filesystem mounted somewhere inside it would be writable there.
/// Landlock covers that on 5.13 and later, but not below.
///
/// `mount_setattr(AT_RECURSIVE)` (5.12) changes the whole subtree in one call,
/// and only ever adds attributes, so it cannot trip over the flags a user
/// namespace locks. An older kernel gets the same result one mount at a time,
/// from the list the parent read out of the mount table before the clone.
///
/// # Safety
/// Child side of the clone: nothing here allocates.
unsafe fn lock_bind(
    target: &CString,
    readonly: bool,
    submounts: &[CString],
    err_fd: c_int,
    step: Step,
) {
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_NOSUID
            | MOUNT_ATTR_NODEV
            | if readonly { MOUNT_ATTR_RDONLY } else { 0 },
        attr_clr: 0,
        propagation: 0,
        userns_fd: 0,
    };
    // SAFETY: `attr` is a live local of the kernel's `struct mount_attr` layout
    // (`repr(C)`, four `u64`s), the size passed is its own, and `target` is a
    // NUL-terminated `CString` the plan owns.
    let rc = unsafe {
        libc::syscall(
            SYS_MOUNT_SETATTR,
            libc::AT_FDCWD,
            target.as_ptr(),
            AT_RECURSIVE,
            &attr as *const MountAttr,
            core::mem::size_of::<MountAttr>(),
        )
    };
    if rc == 0 {
        return;
    }
    if errno() != libc::ENOSYS {
        fail_at(err_fd, step, target.as_ptr());
    }
    let flags = if readonly {
        READONLY_REMOUNT
    } else {
        LOCKED_REMOUNT
    };
    // SAFETY: `remount_keeping_locks`' contract holds: child side, and `target`
    // is a NUL-terminated `CString` alive for the call.
    unsafe { remount_keeping_locks(target.as_ptr(), flags, true, err_fd, step) };
    for sub in submounts {
        // SAFETY: as above, for each submount `CString` the plan owns.
        unsafe { remount_keeping_locks(sub.as_ptr(), flags, false, err_fd, step) };
    }
}

/// `mount -o remount,bind,<flags>` that repeats whatever the mount already
/// has among the flags a user namespace locks — `noexec` and the atime
/// setting. Dropping one of those is `EPERM`; keeping it costs nothing.
///
/// A submount that is no longer there (`must_exist` false) needs no lock: it
/// went away between the parent reading the table and this remount.
///
/// # Safety
/// Child side of the clone: nothing here allocates.
unsafe fn remount_keeping_locks(
    path: *const c_char,
    flags: u64,
    must_exist: bool,
    err_fd: c_int,
    step: Step,
) {
    // `ST_*` as `statvfs` reports them, and the `MS_*` a remount spells them
    // with. Only relatime's numbers differ. (`statvfs` rather than `statfs`,
    // whose flag word the `libc` crate keeps private; both libcs build it from
    // the one `statfs` syscall, with no allocation.)
    const KEEP: [(u64, u64); 4] = [(8, 8), (1024, 1024), (2048, 2048), (4096, 1 << 21)];

    // SAFETY: all-zero bytes are a valid `statvfs`: plain data the kernel fills
    // in.
    let mut st: libc::statvfs = unsafe { core::mem::zeroed() };
    // SAFETY: `path` is NUL-terminated (every caller passes a `CString`'s
    // pointer) and `st` is a live local for the call.
    if unsafe { libc::statvfs(path, &mut st) } != 0 {
        if must_exist {
            fail_at(err_fd, step, path);
        }
        return;
    }
    let mut keep = 0;
    for (st_bit, ms_bit) in KEEP {
        if st.f_flag & st_bit != 0 {
            keep |= ms_bit;
        }
    }
    let null = core::ptr::null::<c_char>();
    // SAFETY: `path` is NUL-terminated (the callers pass a `CString`'s
    // pointer); a remount takes null source, type and data.
    let rc = unsafe {
        libc::mount(
            null,
            path,
            null,
            (flags | keep) as libc::c_ulong,
            core::ptr::null(),
        )
    };
    if rc != 0 {
        fail_at(err_fd, step, path);
    }
}

const TIOCSCTTY: u32 = 0x540E;

/// Move the agent's socket to [`crate::pool::AGENT_FD`] and let it survive
/// `execve`.
///
/// # Safety
/// Child side of the clone: nothing here may allocate. `fd` is a descriptor
/// this child owns.
unsafe fn place_agent_socket(fd: c_int, err_fd: c_int) {
    const TARGET: c_int = crate::pool::AGENT_FD;

    // SAFETY: `fd` is a descriptor this child owns and `TARGET` is a fixed
    // number; `dup2` touches no memory.
    if fd != TARGET && unsafe { libc::dup2(fd, TARGET) } < 0 {
        fail(err_fd, Step::PlaceAgentSocket);
    }
    if fd != TARGET {
        // SAFETY: `fd` is this child's own descriptor, already copied to
        // `TARGET`, and is not used again.
        unsafe { libc::close(fd) };
    }
    // Clearing FD_CLOEXEC is the point: without it `execve` closes the socket
    // and the agent starts with nothing to talk to.
    // SAFETY: `fcntl` on the descriptor just placed at `TARGET`; touches no
    // memory.
    if unsafe { libc::fcntl(TARGET, libc::F_SETFD, 0) } < 0 {
        fail(err_fd, Step::PlaceAgentSocket);
    }
}

/// Make `fd` the sandbox's controlling terminal and its stdin, stdout, stderr.
///
/// A new session is required first: a process can only acquire a controlling
/// terminal if it is a session leader and has none already. Being pid 1 of a
/// fresh pid namespace is not the same thing — the session is inherited from
/// the launcher, along with *its* controlling terminal, which is precisely the
/// one the sandbox must not have.
/// Three descriptors become stdin, stdout and stderr, each its own.
///
/// A new session first, as in [`adopt_terminal`]: the one inherited is the
/// supervisor's, and a sandbox must not sit in it. Whether the terminal, if
/// stdin is one, becomes this session's controlling terminal is then up to
/// the kernel: it is normally the *client's shell's* already, taking it away
/// needs `CAP_SYS_ADMIN` in the terminal's own namespace, and `TIOCSCTTY`
/// says `EPERM`. That is fine. The three are used as plain descriptors, the
/// terminal keeps belonging to the shell, and a Ctrl-C typed there goes
/// where the terminal sends it — to the client, which relays it here. A
/// pipe says `ENOTTY`, which is the other refusal that means "not
/// applicable" rather than "failed".
///
/// The descriptors are numbered 3 or above by construction — they arrived
/// over `SCM_RIGHTS`, and this process's own 0, 1 and 2 were never sent — so
/// the three `dup2`s cannot overwrite a source before it is read. The
/// supervisor checks that before building the plan, because this function
/// may not: it runs between `clone3` and `execve`, where nothing may
/// allocate or fail with a message.
///
/// # Safety
/// Child side of the clone: nothing here may allocate. The three descriptors
/// are this child's own, each at 3 or above.
unsafe fn adopt_streams(fds: [c_int; 3], err_fd: c_int) {
    // SAFETY: `setsid` takes no arguments and touches no memory.
    if unsafe { libc::setsid() } < 0 {
        fail(err_fd, Step::AdoptTerminal);
    }
    // SAFETY: `ioctl` with an integer argument on a descriptor this child owns;
    // it reads no memory.
    if unsafe { libc::ioctl(fds[0], TIOCSCTTY as _, 0) } != 0
        && !matches!(errno(), libc::ENOTTY | libc::EPERM)
    {
        fail(err_fd, Step::AdoptTerminal);
    }
    for (target, source) in fds.iter().enumerate() {
        // SAFETY: `dup2` between descriptors this child owns; touches no
        // memory. The sources are at 3 or above, so no target overwrites an
        // unread source (see above).
        if unsafe { libc::dup2(*source, target as c_int) } < 0 {
            fail(err_fd, Step::AdoptTerminal);
        }
    }
}

/// # Safety
/// Child side of the clone: nothing here may allocate. `fd` is a descriptor
/// this child owns.
unsafe fn adopt_terminal(fd: c_int, err_fd: c_int) {
    // SAFETY: `setsid` takes no arguments and touches no memory.
    if unsafe { libc::setsid() } < 0 {
        fail(err_fd, Step::AdoptTerminal);
    }
    // A terminal becomes the controlling one. Anything else — a pipe, which
    // is what the venv builder hands over to capture `pip` — is simply used as
    // stdio: `TIOCSCTTY` says ENOTTY, and that is the one refusal that means
    // "not applicable" rather than "failed".
    // SAFETY: `ioctl` with an integer argument on a descriptor this child owns;
    // it reads no memory.
    if unsafe { libc::ioctl(fd, TIOCSCTTY as _, 0) } != 0 && errno() != libc::ENOTTY {
        fail(err_fd, Step::AdoptTerminal);
    }
    for target in 0..3 {
        // SAFETY: `dup2` from a descriptor this child owns onto 0, 1 and 2;
        // touches no memory.
        if unsafe { libc::dup2(fd, target) } < 0 {
            fail(err_fd, Step::AdoptTerminal);
        }
    }
    if fd > 2 {
        // SAFETY: `fd` is this child's own descriptor and has been copied to 0,
        // 1 and 2; it is not used again.
        unsafe { libc::close(fd) };
    }
}

/// Give the program the signal dispositions its caller has, not this
/// process's.
///
/// Dispositions survive `clone3` and `SIG_IGN` survives `execve`, so a
/// program started here for a client would otherwise run with whatever the
/// supervisor was started with — and a supervisor a script put in the
/// background with `&` has `SIGINT` ignored, which made Ctrl-C do nothing in
/// every one-shot it started. `mask` is what the client ignores, bit `n - 1`
/// for signal `n`; everything else goes back to its default, and nothing
/// stays blocked. `SIGKILL` and `SIGSTOP` cannot be set and are skipped, and
/// the two real-time signals libc keeps for itself refuse, which is nothing
/// to reset either.
///
/// # Safety
/// Child side of the clone: nothing here may allocate, and the process has
/// exactly one thread, which is what makes `signal` and `sigprocmask` safe
/// to call.
unsafe fn reset_signals(mask: u64) {
    for signal in 1..=64 {
        if signal == libc::SIGKILL || signal == libc::SIGSTOP {
            continue;
        }
        let disposition = if mask & (1u64 << (signal - 1)) != 0 {
            libc::SIG_IGN
        } else {
            libc::SIG_DFL
        };
        // SAFETY: `signal` with a valid signal number and one of the two
        // standard dispositions; async-signal-safe, and this process has one
        // thread.
        unsafe { libc::signal(signal, disposition) };
    }
    // SAFETY: all-zero bytes are a valid `sigset_t`, and `sigemptyset`
    // initialises it properly below anyway.
    let mut none: libc::sigset_t = unsafe { std::mem::zeroed() };
    // SAFETY: `none` is a live local; a null old-set pointer is allowed; both
    // calls are async-signal-safe.
    unsafe {
        libc::sigemptyset(&mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
    }
}

/// The mount point for a bind, made where it is missing.
///
/// Every parent first: a target nested in a tmpfs this plan mounted —
/// `/tmp/windmill/cache/py_runtime`, where Windmill's worker binds its Python —
/// has no parents until now, and a single `mkdir` of the target failed with
/// ENOENT. Then a directory, or an empty file when the source is a file: a
/// file cannot be bound onto a directory. All of it harmless where the path
/// already exists or the parent is read-only; the mount reports what matters.
///
/// No allocation: the path is copied into a stack buffer and cut at each `/`.
///
/// # Safety
/// Child side of the clone: nothing here may allocate. Both pointers are
/// NUL-terminated strings alive for the call.
unsafe fn ensure_mount_point(source: *const c_char, target: *const c_char) {
    // SAFETY: `target` is NUL-terminated: every caller passes a `CString`'s
    // pointer.
    let len = unsafe { libc::strlen(target) };
    let mut buf = [0u8; 4096];
    if len == 0 || len >= buf.len() {
        // SAFETY: `target` is NUL-terminated, which is `ensure_dir`'s only
        // requirement.
        unsafe { ensure_dir(target) };
        return;
    }
    // SAFETY: `strlen` found `len` readable bytes at `target`, `len <
    // buf.len()` was checked just above, and a stack buffer cannot overlap the
    // plan's heap string.
    unsafe { core::ptr::copy_nonoverlapping(target.cast::<u8>(), buf.as_mut_ptr(), len) };
    buf[len] = 0;
    for i in 1..len {
        if buf[i] == b'/' {
            buf[i] = 0;
            // SAFETY: `buf` was NUL-terminated at index `i` a moment ago, so
            // `mkdir` reads a valid C string.
            unsafe { libc::mkdir(buf.as_ptr() as *const c_char, 0o755) };
            buf[i] = b'/';
        }
    }
    // SAFETY: all-zero bytes are a valid `stat`: plain data the kernel fills
    // in.
    let mut st: libc::stat = unsafe { core::mem::zeroed() };
    let is_file =
        // SAFETY: `source` is NUL-terminated (a `CString`'s pointer) and `st`
        // is a live local for the call.
        unsafe { libc::stat(source, &mut st) } == 0 && (st.st_mode & libc::S_IFMT) != libc::S_IFDIR;
    if is_file {
        // SAFETY: `target` is NUL-terminated (a `CString`'s pointer); `open`
        // only reads it.
        let fd = unsafe {
            libc::open(
                target,
                libc::O_CREAT | libc::O_WRONLY | libc::O_CLOEXEC,
                0o644,
            )
        };
        if fd >= 0 {
            // SAFETY: `fd` was just returned by `open` and nothing else holds
            // it.
            unsafe { libc::close(fd) };
        }
    } else {
        // SAFETY: `target` is NUL-terminated, which is `ensure_dir`'s only
        // requirement.
        unsafe { ensure_dir(target) };
    }
}

/// `mkdir` a mount point, ignoring "already there" and "read-only".
///
/// Only useful where the parent is writable — a fresh tmpfs. On the read-only
/// root it fails harmlessly, because the image store already put the directory
/// there.
///
/// # Safety
/// `path` is a NUL-terminated string alive for the call.
unsafe fn ensure_dir(path: *const c_char) {
    // SAFETY: `path` is NUL-terminated: every caller passes a `CString`'s
    // pointer or a buffer it terminated itself.
    unsafe { libc::mkdir(path, 0o755) };
}

/// `ifreq`, declared here rather than taken from libc so the layout is the same
/// under glibc and musl.
#[repr(C)]
struct IfReq {
    name: [c_char; 16],
    flags: libc::c_short,
    _padding: [u8; 22],
}

// `ioctl`'s request parameter is `c_ulong` under glibc and `c_int` under musl,
// so these are kept width-neutral and cast at the call site.
const SIOCGIFFLAGS: u32 = 0x8913;
const SIOCSIFFLAGS: u32 = 0x8914;
const IFF_UP: libc::c_short = 1;

/// Bring `lo` up in the new network namespace.
///
/// A fresh netns has a loopback device but it is down, so even a sandbox with
/// `network = "none"` cannot talk to itself — which breaks anything using a
/// local socket, including the warm-execution protocol.
///
/// # Safety
/// Child side of the clone: nothing here may allocate, and `err_fd` is this
/// child's own descriptor.
unsafe fn bring_up_loopback(err_fd: c_int) {
    // SAFETY: `socket` takes integers only; touches no memory.
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        fail(err_fd, Step::BringUpLoopback);
    }

    // SAFETY: all-zero bytes are a valid `IfReq`: plain data with the kernel's
    // layout (checked by `ifreq_matches_the_kernel_layout`).
    let mut req: IfReq = unsafe { core::mem::zeroed() };
    req.name[0] = b'l' as c_char;
    req.name[1] = b'o' as c_char;

    // SAFETY: `req` is a live local of the layout the ioctl expects, and `sock`
    // is a descriptor this function just opened.
    if unsafe { libc::ioctl(sock, SIOCGIFFLAGS as _, &mut req) } != 0 {
        // SAFETY: `sock` is this function's own descriptor and is not used
        // after the failure report.
        unsafe { libc::close(sock) };
        fail(err_fd, Step::BringUpLoopback);
    }
    req.flags |= IFF_UP;
    // SAFETY: `req` is a live local of the layout the ioctl expects;
    // `SIOCSIFFLAGS` only reads it.
    if unsafe { libc::ioctl(sock, SIOCSIFFLAGS as _, &req) } != 0 {
        // SAFETY: `sock` is this function's own descriptor and is not used
        // after the failure report.
        unsafe { libc::close(sock) };
        fail(err_fd, Step::BringUpLoopback);
    }
    // SAFETY: `sock` is this function's own descriptor and is not used again.
    unsafe { libc::close(sock) };
}

const SYS_CAPSET: libc::c_long = if cfg!(target_arch = "aarch64") {
    91
} else {
    126
};
const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
/// Highest capability the kernel is likely to know about. Dropping a number it
/// does not know returns EINVAL, which is ignored.
const CAP_LAST_CAP_GUESS: c_int = 63;

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: c_int,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Empty every capability set: bounding, ambient, effective, permitted and
/// inheritable (design doc §3.3 step 5).
///
/// A tenant that looks like root inside the sandbox — uid 0 mapped to an
/// unprivileged host uid — must hold no capability at all, or "root inside"
/// starts meaning something.
///
/// # Safety
/// Child side of the clone: nothing here may allocate. Must run after
/// `no_new_privs` is set and before `execve`, or a dropped capability could
/// come back.
unsafe fn drop_all_capabilities() -> bool {
    // Ambient first: a capability left there would be re-raised on execve.
    // SAFETY: `prctl` with constant arguments; touches no memory.
    unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    };

    for cap in 0..=CAP_LAST_CAP_GUESS {
        // EINVAL simply means this kernel has no such capability.
        // SAFETY: `prctl` with constant arguments; touches no memory.
        unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) };
    }

    let header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [CapData::default(); 2];
    // SAFETY: `header` and `data` are live locals with the kernel's layouts
    // (checked by `capability_structs_match_the_kernel_layout`), and version 3
    // wants exactly the two `CapData` passed.
    unsafe { libc::syscall(SYS_CAPSET, &header, data.as_ptr()) == 0 }
}

/// The current `errno`. Reads the thread-local through libc; nothing is
/// allocated, so it is usable on the child side of the clone.
pub(super) fn errno() -> c_int {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Report the failing step with the current `errno` and exit.
pub(super) fn fail(err_fd: c_int, step: Step) -> ! {
    fail_with(err_fd, step, errno())
}

/// [`fail`], naming the path the step failed on. "Applying a bind mount
/// failed: No such file or directory" did not say which of a job's twenty
/// mounts it was; the path follows the eight bytes and [`decode_failure`]
/// hands it back.
pub(super) fn fail_at(err_fd: c_int, step: Step, path: *const c_char) -> ! {
    let errno = errno();
    let mut payload = [0u8; 8];
    payload[..4].copy_from_slice(&(step as u32).to_ne_bytes());
    payload[4..].copy_from_slice(&errno.to_ne_bytes());
    // SAFETY: `path` is NUL-terminated (every caller passes a `CString`'s
    // pointer), `payload` is a live local, and `write` only reads the bytes it
    // is given; `_exit` never returns.
    unsafe {
        let len = libc::strlen(path).min(4000);
        libc::write(err_fd, payload.as_ptr() as *const c_void, payload.len());
        libc::write(err_fd, path as *const c_void, len);
        libc::_exit(EXIT_LAUNCH_FAILED)
    }
}

/// Report a step and an explicit errno, then exit.
///
/// The payload is a fixed eight bytes so the parent can read it with one call
/// and never has to parse anything.
fn fail_with(err_fd: c_int, step: Step, errno: c_int) -> ! {
    let mut payload = [0u8; 8];
    payload[..4].copy_from_slice(&(step as u32).to_ne_bytes());
    payload[4..].copy_from_slice(&errno.to_ne_bytes());
    // SAFETY: `payload` is a live local and `write` only reads it; `_exit`
    // never returns.
    unsafe {
        libc::write(err_fd, payload.as_ptr() as *const c_void, payload.len());
        libc::_exit(EXIT_LAUNCH_FAILED)
    }
}

/// Decode what `fail_with` wrote.
pub fn decode_failure(payload: &[u8]) -> Option<(Step, i32)> {
    if payload.len() < 8 {
        return None;
    }
    let step = u32::from_ne_bytes(payload[..4].try_into().ok()?);
    let errno = i32::from_ne_bytes(payload[4..8].try_into().ok()?);
    Step::from_u32(step).map(|s| (s, errno))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_payloads_round_trip() {
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&(Step::MountProc as u32).to_ne_bytes());
        payload[4..].copy_from_slice(&libc::EPERM.to_ne_bytes());

        let (step, errno) = decode_failure(&payload).unwrap();
        assert_eq!(step, Step::MountProc);
        assert_eq!(errno, libc::EPERM);
    }

    #[test]
    fn a_short_or_unknown_payload_decodes_to_nothing() {
        assert!(decode_failure(&[0u8; 4]).is_none());
        assert!(decode_failure(&[]).is_none());

        let mut unknown = [0u8; 8];
        unknown[..4].copy_from_slice(&9999u32.to_ne_bytes());
        assert!(decode_failure(&unknown).is_none());
    }

    #[test]
    fn ifreq_matches_the_kernel_layout() {
        // The kernel's `struct ifreq` is 40 bytes on both LP64 targets.
        assert_eq!(core::mem::size_of::<IfReq>(), 40);
    }

    #[test]
    fn capability_structs_match_the_kernel_layout() {
        assert_eq!(core::mem::size_of::<CapHeader>(), 8);
        assert_eq!(core::mem::size_of::<CapData>(), 12);
        assert_eq!(LINUX_CAPABILITY_VERSION_3, 0x20080522);
    }

    #[test]
    fn pivot_root_syscall_number_matches_the_architecture() {
        if cfg!(target_arch = "aarch64") {
            assert_eq!(SYS_PIVOT_ROOT, 41);
        } else if cfg!(target_arch = "x86_64") {
            assert_eq!(SYS_PIVOT_ROOT, 155);
        }
    }
}
