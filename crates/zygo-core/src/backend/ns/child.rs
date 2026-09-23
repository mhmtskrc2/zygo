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

use super::prepare::{
    PSEUDO_FLAGS, PreparedLaunch, PreparedOp, READONLY_REMOUNT, ROOT_FLAGS, SYSFS_FLAGS, Step,
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
    // 1. Wait for the parent to write the id maps. Until they exist this
    //    process has no valid uid and cannot mount anything.
    let mut signal = [0u8; 1];
    let n = unsafe { libc::read(ready_fd, signal.as_mut_ptr() as *mut c_void, 1) };
    if n != 1 {
        fail(err_fd, Step::WaitForIdMaps);
    }
    unsafe { libc::close(ready_fd) };
    if signal[0] != READY_OK {
        fail_with(err_fd, Step::IdMapsRejected, libc::EPERM);
    }

    // 2. The mount plan.
    for op in &plan.ops {
        unsafe { apply(op, err_fd) };
    }

    // 3. Commit the new root.
    //
    //    `pivot_root(".", ".")` puts the old root *on top of* the new one at
    //    the same point, so it can be detached immediately. The alternative
    //    needs a spare directory inside the image to hold the old root, which
    //    a read-only image may not have.
    if unsafe { libc::chdir(plan.newroot.as_ptr()) } != 0 {
        fail(err_fd, Step::PivotRoot);
    }
    let dot = c".".as_ptr();
    if unsafe { libc::syscall(SYS_PIVOT_ROOT, dot, dot) } != 0 {
        fail(err_fd, Step::PivotRoot);
    }
    if unsafe { libc::umount2(dot, MNT_DETACH) } != 0 {
        fail(err_fd, Step::UnmountOldRoot);
    }
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
        unsafe { adopt_streams(fds, err_fd) };
    } else if let Some(fd) = plan.stdio {
        unsafe { adopt_terminal(fd, err_fd) };
    }
    //    And the caller's signal dispositions, when the program is being
    //    started for somebody else.
    if let Some(mask) = plan.ignored_signals {
        unsafe { reset_signals(mask) };
    }

    // 5. The runtime agent's socket, at a fixed descriptor so its argv can
    //    name it. `execve` would otherwise close it: Rust marks sockets
    //    close-on-exec, and the whole point is for the agent to inherit it.
    if let Some(fd) = plan.agent_fd {
        unsafe { place_agent_socket(fd, err_fd) };
    }

    // 6. Identity of the sandbox as seen from inside.
    let host = plan.hostname.as_bytes();
    if unsafe { libc::sethostname(host.as_ptr() as *const c_char, host.len()) } != 0 {
        // A sandbox with the wrong hostname still runs correctly; not fatal.
    }

    if plan.bring_up_loopback {
        unsafe { bring_up_loopback(err_fd) };
    }

    if unsafe { libc::chdir(plan.workdir.as_ptr()) } != 0 {
        // The image may not have the configured working directory; `/` always
        // exists and is a better outcome than refusing to start.
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
        unsafe { hand_out_secrets_dir(sock, err_fd) };
    }

    // 7–10. Limits, privilege, Landlock, seccomp, and dying with the parent.
    unsafe { harden(plan, err_fd) };

    // 11. Hand the sandbox over — to the program, or to a loop that keeps it.
    if plan.hold {
        unsafe { hold(err_fd) }
    }
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
        if unsafe { libc::setrlimit(*resource as _, &limit) } != 0 {
            fail(err_fd, Step::SetRlimit);
        }
    }

    // `no_new_privs` before dropping capabilities: it is what stops a setuid
    // binary inside the image from regaining any of them.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        fail(err_fd, Step::NoNewPrivs);
    }

    if plan.drop_capabilities && unsafe { !drop_all_capabilities() } {
        fail(err_fd, Step::DropCapabilities);
    }

    // Landlock, before seccomp: building the ruleset needs `open`, which the
    // seccomp profile still permits but which is cleaner to do while the
    // process is otherwise unconstrained. Both require `no_new_privs`, set
    // just above.
    if !plan.landlock.is_empty()
        && let Err(e) = unsafe { super::landlock::apply(&plan.landlock) }
    {
        fail_with(err_fd, Step::ApplyLandlock, e.raw_os_error().unwrap_or(0));
    }

    // The syscall filter. After `no_new_privs` (the kernel requires it for an
    // unprivileged caller) and after every other setup step, because the
    // filter denies most of what those steps needed.
    if !plan.seccomp.is_empty()
        && let Err(e) = unsafe { super::seccomp::install(&plan.seccomp, plan.seccomp_log) }
    {
        fail_with(err_fd, Step::InstallSeccomp, e.raw_os_error().unwrap_or(0));
    }

    // Die with the parent. Checked immediately afterwards, because a parent
    // that exited *before* the prctl would never trigger it. `getppid` is 0
    // when the parent is outside this pid namespace — the warm-exec helper —
    // and that is not "the parent died".
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) };
    if unsafe { libc::getppid() } == 1 {
        fail_with(err_fd, Step::ParentDied, libc::ESRCH);
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
        if unsafe { libc::waitpid(-1, &mut status, 0) } < 0 {
            let pause = libc::timespec {
                tv_sec: 1,
                tv_nsec: 0,
            };
            unsafe { libc::nanosleep(&pause, core::ptr::null_mut()) };
        }
    }
}

/// Apply one prepared mount.
unsafe fn apply(op: &PreparedOp, err_fd: c_int) {
    let null = core::ptr::null::<c_char>();

    match op {
        PreparedOp::MakeRootPrivate => {
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
        } => {
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
            // second, explicit remount. Skipped only for a derived-layer build,
            // whose root is a private copy made to be written.
            if *readonly {
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
                    fail(err_fd, Step::MountRoot);
                }
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
            unsafe { ensure_dir(target.as_ptr()) };
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
        } => {
            // The mount point, where the image did not already provide one.
            // The rootfs view creates every target the plan names, but only
            // on the *image's* tree: a target inside a tmpfs this plan
            // mounted — `/run/script`, where a pool's scripts are bound in —
            // exists nowhere until now. Harmless where it is already there,
            // and where the parent is read-only it fails and the mount below
            // reports the real problem.
            unsafe { ensure_dir(target.as_ptr()) };
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
                fail(err_fd, Step::MountBind);
            }
            if *readonly {
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

        PreparedOp::Proc { target } => {
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
            unsafe { ensure_dir(target.as_ptr()) };
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
            let fd = unsafe { libc::open(target.as_ptr(), libc::O_CREAT | libc::O_WRONLY, 0o644) };
            if fd >= 0 {
                unsafe { libc::close(fd) };
            }
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
            let mut st: libc::stat = unsafe { core::mem::zeroed() };
            if unsafe { libc::stat(target.as_ptr(), &mut st) } != 0 {
                // Kernels differ in which of these exist; a path that is not
                // there needs no masking.
                return;
            }
            let rc = if st.st_mode & libc::S_IFMT == libc::S_IFDIR {
                // An empty read-only tmpfs hides a directory's contents.
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
            let mut st: libc::stat = unsafe { core::mem::zeroed() };
            if unsafe { libc::stat(target.as_ptr(), &mut st) } != 0 {
                return;
            }
            // Bind it to itself first: a path that is not already a mount point
            // cannot be remounted.
            unsafe {
                libc::mount(
                    target.as_ptr(),
                    target.as_ptr(),
                    null,
                    super::prepare::MS_BIND as libc::c_ulong,
                    core::ptr::null(),
                )
            };
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

const TIOCSCTTY: u32 = 0x540E;

/// Move the agent's socket to [`crate::pool::AGENT_FD`] and let it survive
/// `execve`.
unsafe fn place_agent_socket(fd: c_int, err_fd: c_int) {
    const TARGET: c_int = crate::pool::AGENT_FD;

    if fd != TARGET && unsafe { libc::dup2(fd, TARGET) } < 0 {
        fail(err_fd, Step::PlaceAgentSocket);
    }
    if fd != TARGET {
        unsafe { libc::close(fd) };
    }
    // Clearing FD_CLOEXEC is the point: without it `execve` closes the socket
    // and the agent starts with nothing to talk to.
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
unsafe fn adopt_streams(fds: [c_int; 3], err_fd: c_int) {
    if unsafe { libc::setsid() } < 0 {
        fail(err_fd, Step::AdoptTerminal);
    }
    if unsafe { libc::ioctl(fds[0], TIOCSCTTY as _, 0) } != 0
        && !matches!(errno(), libc::ENOTTY | libc::EPERM)
    {
        fail(err_fd, Step::AdoptTerminal);
    }
    for (target, source) in fds.iter().enumerate() {
        if unsafe { libc::dup2(*source, target as c_int) } < 0 {
            fail(err_fd, Step::AdoptTerminal);
        }
    }
}

unsafe fn adopt_terminal(fd: c_int, err_fd: c_int) {
    if unsafe { libc::setsid() } < 0 {
        fail(err_fd, Step::AdoptTerminal);
    }
    // A terminal becomes the controlling one. Anything else — a pipe, which
    // is what the venv builder hands over to capture `pip` — is simply used as
    // stdio: `TIOCSCTTY` says ENOTTY, and that is the one refusal that means
    // "not applicable" rather than "failed".
    if unsafe { libc::ioctl(fd, TIOCSCTTY as _, 0) } != 0 && errno() != libc::ENOTTY {
        fail(err_fd, Step::AdoptTerminal);
    }
    for target in 0..3 {
        if unsafe { libc::dup2(fd, target) } < 0 {
            fail(err_fd, Step::AdoptTerminal);
        }
    }
    if fd > 2 {
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
        unsafe { libc::signal(signal, disposition) };
    }
    let mut none: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut none);
        libc::sigprocmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
    }
}

/// `mkdir` a mount point, ignoring "already there" and "read-only".
///
/// Only useful where the parent is writable — a fresh tmpfs. On the read-only
/// root it fails harmlessly, because the image store already put the directory
/// there.
unsafe fn ensure_dir(path: *const c_char) {
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
unsafe fn bring_up_loopback(err_fd: c_int) {
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        fail(err_fd, Step::BringUpLoopback);
    }

    let mut req: IfReq = unsafe { core::mem::zeroed() };
    req.name[0] = b'l' as c_char;
    req.name[1] = b'o' as c_char;

    if unsafe { libc::ioctl(sock, SIOCGIFFLAGS as _, &mut req) } != 0 {
        unsafe { libc::close(sock) };
        fail(err_fd, Step::BringUpLoopback);
    }
    req.flags |= IFF_UP;
    if unsafe { libc::ioctl(sock, SIOCSIFFLAGS as _, &req) } != 0 {
        unsafe { libc::close(sock) };
        fail(err_fd, Step::BringUpLoopback);
    }
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
unsafe fn drop_all_capabilities() -> bool {
    // Ambient first: a capability left there would be re-raised on execve.
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
        unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0) };
    }

    let header = CapHeader {
        version: LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [CapData::default(); 2];
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

/// Report a step and an explicit errno, then exit.
///
/// The payload is a fixed eight bytes so the parent can read it with one call
/// and never has to parse anything.
fn fail_with(err_fd: c_int, step: Step, errno: c_int) -> ! {
    let mut payload = [0u8; 8];
    payload[..4].copy_from_slice(&(step as u32).to_ne_bytes());
    payload[4..].copy_from_slice(&errno.to_ne_bytes());
    unsafe {
        libc::write(err_fd, payload.as_ptr() as *const c_void, payload.len());
        libc::_exit(EXIT_LAUNCH_FAILED)
    }
}

/// Decode what [`fail_with`] wrote.
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
