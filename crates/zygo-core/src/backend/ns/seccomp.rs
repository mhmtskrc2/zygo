//! seccomp-bpf filters (design doc appendix B).
//!
//! The filter is an **allowlist**: anything not named returns `EPERM`. That
//! direction matters — a denylist silently gains a hole every time the kernel
//! grows a syscall, and it grows several per release. One exception, for a
//! reason: `clone3` returns `ENOSYS` where it is not allowed, because that is
//! the only answer glibc's `pthread_create` falls back from.
//!
//! The program is built in the parent, as a plain `Vec<SockFilter>`, and only
//! *installed* in the child. Everything after `clone3` has to be
//! allocation-free, and generating BPF is not.
//!
//! The three profiles come from appendix B. `default` is the one PoC 5
//! validated against numpy, pandas, Pillow, pydantic and requests — the same
//! syscall set, expressed here rather than as a Docker profile.

use super::syscalls;
use crate::spec::SeccompProfile;

/// One classic-BPF instruction, the shape `SECCOMP_SET_MODE_FILTER` expects.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// `struct sock_fprog`.
#[repr(C)]
#[derive(Debug)]
pub struct SockFprog {
    pub len: u16,
    pub filter: *const SockFilter,
}

// Classic BPF opcodes.
const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_JGT: u16 = 0x20;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;
const BPF_ALU: u16 = 0x04;
const BPF_AND: u16 = 0x50;
const BPF_JA: u16 = 0x00;

// `struct seccomp_data` field offsets.
const OFF_NR: u32 = 0;
const OFF_ARCH: u32 = 4;
/// Low 32 bits of `args[0]`. Little-endian only, which both supported
/// architectures are.
const OFF_ARG0_LO: u32 = 16;
/// Low 32 bits of `args[1]` — `ioctl`'s request number.
const OFF_ARG1_LO: u32 = 24;

// Filter return values.
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ERRNO: u32 = 0x0005_0000;
/// Killing the whole process rather than the thread: a thread killed mid-syscall
/// leaves the rest of the sandbox running in an unknown state.
const RET_KILL_PROCESS: u32 = 0x8000_0000;

/// `CLONE_NEW*` bits. A sandbox that can create namespaces can undo its own
/// confinement, so `clone` is allowed only with all of these clear.
const CLONE_NEW_MASK: u32 = 0x0002_0000   // NEWNS
    | 0x0400_0000   // NEWUTS
    | 0x0800_0000   // NEWIPC
    | 0x1000_0000   // NEWUSER
    | 0x2000_0000   // NEWPID
    | 0x4000_0000   // NEWNET
    | 0x0200_0000; // NEWCGROUP

fn stmt(code: u16, k: u32) -> SockFilter {
    SockFilter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

/// Where a branch goes.
///
/// Classic BPF encodes jumps as 8-bit *distances*, counted from the instruction
/// after the branch. Computing those by hand is how the first version of this
/// generator sent every `clone` to the deny instead of to its argument check —
/// a one-instruction error that every structural test passed. Branch targets
/// are therefore named here and resolved in a second pass.
///
/// A label may be defined more than once. A branch goes to the *nearest*
/// definition after it, which is what keeps every distance under 256 however
/// long the allowlist grows: [`Assembler::island`] drops a copy of the four
/// returns into the middle of the chain, and the comparisons around it jump
/// there instead of to the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Label {
    /// Fall through to the next instruction.
    Next,
    Allow,
    Deny,
    /// `ENOSYS` rather than `EPERM`: the answer for `clone3`, see [`program`].
    NoSys,
    Kill,
    CloneCheck,
    IoctlCheck,
    /// The start of the one-comparison-per-syscall chain.
    Chain,
    /// Just past an island, for the jump that steps over it.
    Skip,
}

/// Comparisons between two islands. Anything well under 255 works; the
/// distance to the next island is this plus the island itself.
const ISLAND_EVERY: usize = 64;

/// An instruction before its branch distances are known.
#[derive(Debug, Clone, Copy)]
enum Pending {
    Load(u32),
    And(u32),
    JumpEq {
        k: u32,
        jt: Label,
        jf: Label,
    },
    JumpGt {
        k: u32,
        jt: Label,
        jf: Label,
    },
    Goto(Label),
    Ret(u32),
    /// Not an instruction: names the position that follows.
    Mark(Label),
}

#[derive(Default)]
struct Assembler {
    pending: Vec<Pending>,
}

impl Assembler {
    fn push(&mut self, insn: Pending) -> &mut Self {
        self.pending.push(insn);
        self
    }

    fn load(&mut self, offset: u32) -> &mut Self {
        self.push(Pending::Load(offset))
    }
    fn and(&mut self, mask: u32) -> &mut Self {
        self.push(Pending::And(mask))
    }
    fn jgt(&mut self, k: u32, jt: Label, jf: Label) -> &mut Self {
        self.push(Pending::JumpGt { k, jt, jf })
    }
    fn jeq(&mut self, k: u32, jt: Label, jf: Label) -> &mut Self {
        self.push(Pending::JumpEq { k, jt, jf })
    }
    fn goto(&mut self, target: Label) -> &mut Self {
        self.push(Pending::Goto(target))
    }
    fn ret(&mut self, k: u32) -> &mut Self {
        self.push(Pending::Ret(k))
    }
    fn mark(&mut self, label: Label) -> &mut Self {
        self.push(Pending::Mark(label))
    }

    /// The four returns, where straight-line code steps over them and every
    /// branch before them can land on them.
    fn island(&mut self) -> &mut Self {
        self.goto(Label::Skip)
            .mark(Label::Deny)
            .ret(RET_ERRNO | libc::EPERM as u32)
            .mark(Label::NoSys)
            .ret(RET_ERRNO | libc::ENOSYS as u32)
            .mark(Label::Kill)
            .ret(RET_KILL_PROCESS)
            .mark(Label::Allow)
            .ret(RET_ALLOW)
            .mark(Label::Skip)
    }

    /// Resolve labels into distances and emit the program.
    fn assemble(&self) -> Result<Vec<SockFilter>, SeccompError> {
        // First pass: where each label lands, counting only real instructions.
        let mut positions: Vec<(Label, usize)> = Vec::new();
        let mut index = 0usize;
        for insn in &self.pending {
            match insn {
                Pending::Mark(label) => positions.push((*label, index)),
                _ => index += 1,
            }
        }
        // The nearest definition at or after `from`: BPF cannot jump back.
        let position_of = |label: Label, from: usize| -> Option<usize> {
            positions
                .iter()
                .find(|(l, p)| *l == label && *p >= from)
                .map(|(_, p)| *p)
        };

        // Second pass: emit, converting each target into a distance.
        let mut out = Vec::with_capacity(index);
        for insn in &self.pending {
            let here = out.len();
            let distance = |label: Label| -> Result<u8, SeccompError> {
                if label == Label::Next {
                    return Ok(0);
                }
                // `here + 1` because a branch counts from the instruction after
                // it; a backward jump cannot be encoded at all.
                let target = position_of(label, here + 1).ok_or(
                    if positions.iter().any(|(l, _)| *l == label) {
                        SeccompError::BackwardJump
                    } else {
                        SeccompError::UnresolvedLabel
                    },
                )?;
                let delta = target - (here + 1);
                u8::try_from(delta).map_err(|_| SeccompError::JumpTooFar { delta })
            };

            out.push(match *insn {
                Pending::Mark(_) => continue,
                Pending::Load(offset) => stmt(BPF_LD | BPF_W | BPF_ABS, offset),
                Pending::And(mask) => stmt(BPF_ALU | BPF_AND | BPF_K, mask),
                Pending::Ret(value) => stmt(BPF_RET | BPF_K, value),
                Pending::JumpEq { k, jt, jf } => SockFilter {
                    code: BPF_JMP | BPF_JEQ | BPF_K,
                    jt: distance(jt)?,
                    jf: distance(jf)?,
                    k,
                },
                Pending::JumpGt { k, jt, jf } => SockFilter {
                    code: BPF_JMP | BPF_JGT | BPF_K,
                    jt: distance(jt)?,
                    jf: distance(jf)?,
                    k,
                },
                Pending::Goto(target) => SockFilter {
                    code: BPF_JMP | BPF_JA,
                    jt: 0,
                    jf: 0,
                    k: distance(target)? as u32,
                },
            });
        }
        Ok(out)
    }
}

/// Syscalls every profile allows.
///
/// This is PoC 5's validated set: the five reference packages exercise their
/// real code paths — numpy's BLAS threads, Pillow's codecs, pandas' file I/O,
/// pydantic's Rust core, requests' TLS setup — under exactly these.
///
/// Plus the whole extended-attribute family, which PoC 5 did not reach and
/// the first real consumer did (the first adoption report, Z-1). Only
/// `getxattr` and `lgetxattr` were here; `listxattr` was not, so it answered
/// `EPERM` — which `shutil.copy2` reports as `[Errno 1] Operation not
/// permitted` naming a file that plainly exists, and `pip install --target`
/// is a `copy2` per file. Nine tracebacks about `RECORD` and `WHEEL` do not
/// point at `listxattr`. Listing and reading attributes on a tmpfs or an
/// overlay the sandbox owns leaks nothing; a `user.*` write is bounded by the
/// mount; `trusted.*` and `security.*` writes are refused by the kernel to an
/// unprivileged uid before any filter is consulted. So all twelve are allowed
/// on every profile — `strict` too, because a `network = "none"` function
/// copies files like any other.
///
/// Every syscall the kernel added from 5.10 to 6.10 (numbers 440–462) was
/// decided one by one when the table grew to cover them; the ones here are the
/// newer spellings of something already allowed — `fchmodat2` is glibc's
/// `chmod`, `epoll_pwait2` and the `futex_*` family extend calls in this list
/// — or calls that can only take privilege away from their caller: Landlock
/// and `mseal`. `map_shadow_stack` is x86's CET, which glibc uses where the
/// CPU has it. Docker's default profile allows the same set. The rest are in
/// [`NEWER_REFUSED`].
pub const BASE_ALLOWLIST: &[&str] = &[
    "accept4",
    "access",
    "arch_prctl",
    "bind",
    "brk",
    "capget",
    "capset",
    "chdir",
    "chmod",
    "chown",
    "clock_getres",
    "clock_gettime",
    "clock_nanosleep",
    "close",
    "close_range",
    "connect",
    "copy_file_range",
    "dup",
    "dup2",
    "dup3",
    "epoll_create",
    "epoll_create1",
    "epoll_ctl",
    "epoll_pwait",
    "epoll_pwait2",
    "epoll_wait",
    "eventfd",
    "eventfd2",
    "execve",
    "execveat",
    "exit",
    "exit_group",
    "faccessat",
    "faccessat2",
    "fadvise64",
    "fallocate",
    "fchdir",
    "fchmod",
    "fchmodat",
    "fchmodat2",
    "fchown",
    "fchownat",
    "fcntl",
    "fdatasync",
    "fgetxattr",
    "flistxattr",
    "flock",
    // x86 only, and that is the whole reason they were missing. musl's
    // `fork()` uses `SYS_fork` where the architecture has one and falls back
    // to `clone(SIGCHLD)` where it does not — so on aarch64, where there is
    // no `fork` syscall at all, the allowlist never needed an entry and
    // nobody noticed there was none. On x86_64 every musl image in a Zygo
    // sandbox got `can't fork: Operation not permitted` from its shell.
    //
    // Safe by construction: neither can create a namespace, which is the only
    // thing the `clone` flags check exists to refuse. `vfork` for the same
    // reason — it is what `posix_spawn` reaches for.
    "fork",
    "fremovexattr",
    "fsetxattr",
    "fstat",
    "fstatfs",
    "fsync",
    "ftruncate",
    "futex",
    "futex_requeue",
    "futex_wait",
    "futex_waitv",
    "futex_wake",
    "get_robust_list",
    "getcwd",
    "getdents64",
    "getegid",
    "geteuid",
    "getgid",
    "getgroups",
    "getpeername",
    "getpgrp",
    "getpid",
    "getppid",
    "getpriority",
    "getrandom",
    "getresgid",
    "getresuid",
    "getrlimit",
    "getrusage",
    "getsid",
    "getsockname",
    "getsockopt",
    "gettid",
    "gettimeofday",
    "getuid",
    "getxattr",
    "ioctl",
    "kill",
    "landlock_add_rule",
    "landlock_create_ruleset",
    "landlock_restrict_self",
    "lchown",
    "lgetxattr",
    "link",
    "linkat",
    "listen",
    "listxattr",
    "llistxattr",
    "lremovexattr",
    "lseek",
    "lsetxattr",
    "lstat",
    "madvise",
    "map_shadow_stack",
    "membarrier",
    "memfd_create",
    "mkdir",
    "mkdirat",
    "mlock",
    "mmap",
    "mprotect",
    "mremap",
    "mseal",
    "msync",
    "munlock",
    "munmap",
    "nanosleep",
    "newfstatat",
    "open",
    "openat",
    "pipe",
    "pipe2",
    "poll",
    "ppoll",
    "prctl",
    "pread64",
    "preadv",
    "preadv2",
    "prlimit64",
    "pselect6",
    "pwrite64",
    "pwritev",
    "pwritev2",
    "read",
    "readlink",
    "readlinkat",
    "readv",
    "recvfrom",
    "recvmsg",
    "removexattr",
    "rename",
    "renameat",
    "renameat2",
    "rmdir",
    "rseq",
    "rt_sigaction",
    "rt_sigpending",
    "rt_sigprocmask",
    "rt_sigqueueinfo",
    "rt_sigreturn",
    "rt_sigsuspend",
    "rt_sigtimedwait",
    "sched_get_priority_max",
    "sched_get_priority_min",
    "sched_getaffinity",
    "sched_getattr",
    "sched_getparam",
    "sched_getscheduler",
    "sched_rr_get_interval",
    "sched_setaffinity",
    "sched_setattr",
    "sched_setparam",
    "sched_setscheduler",
    "sched_yield",
    "select",
    "sendmmsg",
    "sendmsg",
    "sendto",
    "set_robust_list",
    "set_tid_address",
    "setfsgid",
    "setfsuid",
    "setgid",
    "setgroups",
    "setitimer",
    "setpgid",
    "setpriority",
    "setregid",
    "setresgid",
    "setresuid",
    "setreuid",
    "setrlimit",
    "setsid",
    "setsockopt",
    "setuid",
    "setxattr",
    "shutdown",
    "sigaltstack",
    "signalfd4",
    "socket",
    "socketpair",
    "stat",
    "statfs",
    "statx",
    "symlink",
    "symlinkat",
    "sysinfo",
    "tgkill",
    "time",
    "timer_create",
    "timer_delete",
    "timer_getoverrun",
    "timer_gettime",
    "timer_settime",
    "timerfd_create",
    "timerfd_gettime",
    "timerfd_settime",
    "times",
    "truncate",
    "umask",
    "uname",
    "unlink",
    "unlinkat",
    "utimensat",
    "vfork",
    "wait4",
    "waitid",
    "write",
    "writev",
];

/// Syscalls `permissive` adds back. Roughly Docker's default profile: enough to
/// debug a package that the tighter profiles break, and nothing that Docker
/// itself would refuse.
///
/// It overlaps [`NEVER_ALLOWED`] on purpose and only where it must — `ptrace`,
/// `setns`, `unshare`, `mount`, `umount2`, `pivot_root`,
/// `process_vm_readv`/`writev` — because that is what this profile is *for*:
/// debugging inside the sandbox, and Zygo's own derived-layer builds, where
/// `dpkg` needs `mknod` and `chroot`. A `permissive` sandbox is a weaker
/// sandbox and the documentation says so.
///
/// `io_uring_*` and `userfaultfd` were here and are not any more. The escape
/// suite runs every appendix-B vector against all three profiles now, and
/// found them reachable under this one; the comment above claimed they were
/// what Docker allows, and Docker removed io_uring from its default profile
/// in 20.10.18 (moby#43991) for the same reason appendix B excludes it — it
/// is a large, fast-moving kernel surface reachable with no capability at
/// all. `userfaultfd` was wider here than in Docker, which grants it only
/// with `CAP_SYS_PTRACE`; a Zygo sandbox holds no capabilities, so "as Docker
/// does" meant "always". Neither is needed to debug a package or to run
/// `apt-get`.
pub const PERMISSIVE_EXTRA: &[&str] = &[
    "clone3",
    "personality",
    "ptrace",
    "process_vm_readv",
    "process_vm_writev",
    "setns",
    "unshare",
    "mount",
    "umount2",
    "pivot_root",
    "sync",
    "syncfs",
    "mknod",
    "mknodat",
    // `dpkg` changes the ownership of symlinks it unpacks; on x86_64 glibc
    // does that with the legacy syscall (arm64 has only `fchownat`).
    "chroot",
    "sethostname",
    "setdomainname",
];

/// Syscalls `strict` removes from the base set.
///
/// Socket *creation* and the calls that make a socket reach somewhere —
/// `connect`, `bind`, `listen`, `accept4` — because a `network = "none"`
/// sandbox has nothing to talk to and a stricter tenant should not be able to
/// try; `ptrace` and `mount`, because nothing legitimate in a handler reaches
/// for them.
///
/// Deliberately **not** the data calls — `sendto`, `recvfrom`, `sendmsg`,
/// `setsockopt` and the rest. They were removed at first, and the seccomp
/// compatibility matrix found every `strict` function dead before its handler
/// ran: the agent speaks to the supervisor over a socket it *inherited*, and
/// a socket it cannot `recvfrom` is a supervisor it cannot hear. Transferring
/// bytes on a descriptor a process was handed is not a capability; opening
/// one is, and that is what stays removed.
///
/// Deliberately **not** `socketpair` either, for the same reason one step
/// further on. A socketpair is two ends of one channel, both created here and
/// reachable from nowhere else: it is `pipe2` with a nicer API, and it grants
/// no more reach than `pipe2` does. Removing it looked free until the Node
/// agent was run under `strict` and could not start a single worker —
/// libuv creates *every* stdio pipe and every IPC channel with
/// `socketpair()`, so a Node process under this profile cannot spawn a child
/// it can hear at all (`spawn EPERM`, before any handler code). CPython uses
/// `pipe2` and never noticed. What `strict` is for is a function that cannot
/// reach the network; `socket`, `connect`, `bind`, `listen` and `accept4` are
/// what reach it, and those stay removed.
pub const STRICT_REMOVED: &[&str] = &[
    "socket", "connect", "bind", "listen", "accept4", "ptrace", "mount", "umount2",
];

/// What a runtime agent's *forked child* additionally loses under `strict`.
///
/// `execve` is the interesting one, and it cannot be in [`STRICT_REMOVED`]: the
/// launcher installs its filter immediately before `execve`-ing the sandboxed
/// program, so a profile without it produces a sandbox that cannot start at all
/// — which is exactly what happened the first time this was run.
///
/// Design doc §3.4.1 puts this tightening where it belongs: the agent's child,
/// which is already running the interpreter and never needs to exec again.
/// [`child_program`] turns it into a filter; the supervisor hands that to the
/// agent in `ZYGO_CHILD_SECCOMP`, and the agent installs it after `fork()` and
/// before the handler runs. `fork` and `vfork` are listed for completeness —
/// glibc creates processes through `clone`, which the program checks by flag.
pub const STRICT_CHILD_REMOVED: &[&str] = &["execve", "execveat", "fork", "vfork"];

/// The `clone` flag that makes a thread rather than a process.
const CLONE_THREAD: u32 = 0x0001_0000;

/// The filter a runtime agent installs in its forked child under `strict`.
///
/// The child is already running the interpreter, so it never needs another
/// program: `execve` and `execveat` are refused. It never needs another
/// *process* either: `fork`, `vfork` and any `clone` without `CLONE_THREAD`
/// are refused, so a handler cannot fork-bomb its way to `pids.max` — while a
/// thread, which is a `clone` *with* the flag, still works. Everything else
/// falls through to `ret ALLOW`, which is not a grant: the sandbox's own
/// filter stays installed underneath, filters stack, and the kernel takes the
/// strictest answer.
///
/// `None` for the profiles that do not tighten the child — the difference
/// between `default` and `strict` is meant to be visible.
pub fn child_program(profile: SeccompProfile) -> Result<Option<Vec<SockFilter>>, SeccompError> {
    if profile != SeccompProfile::Strict {
        return Ok(None);
    }
    if !syscalls::is_supported() {
        return Err(SeccompError::UnsupportedArch {
            arch: std::env::consts::ARCH,
        });
    }

    let mut asm = Assembler::default();
    asm.load(OFF_ARCH)
        .jeq(syscalls::AUDIT_ARCH, Label::Next, Label::Kill)
        .load(OFF_NR);
    for nr in STRICT_CHILD_REMOVED
        .iter()
        .filter_map(|name| syscalls::number(name))
    {
        asm.jeq(nr, Label::Deny, Label::Next);
    }
    let clone_nr = syscalls::number("clone");
    if let Some(nr) = clone_nr {
        asm.jeq(nr, Label::CloneCheck, Label::Next);
    }
    asm.goto(Label::Allow);
    if clone_nr.is_some() {
        asm.mark(Label::CloneCheck)
            .load(OFF_ARG0_LO)
            .and(CLONE_THREAD)
            .jeq(0, Label::Deny, Label::Allow);
    }
    asm.mark(Label::Deny)
        .ret(RET_ERRNO | libc::EPERM as u32)
        .mark(Label::Kill)
        .ret(RET_KILL_PROCESS)
        .mark(Label::Allow)
        .ret(RET_ALLOW);
    Ok(Some(asm.assemble()?))
}

/// The raw `struct sock_filter` array, byte for byte as `prctl(PR_SET_SECCOMP)`
/// reads it, so an agent in any language can install a program without
/// knowing what is in it. Native byte order: the bytes never leave the host.
pub fn encode(prog: &[SockFilter]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prog.len() * 8);
    for insn in prog {
        out.extend_from_slice(&insn.code.to_ne_bytes());
        out.push(insn.jt);
        out.push(insn.jf);
        out.extend_from_slice(&insn.k.to_ne_bytes());
    }
    out
}

/// Syscalls the filter handles with an argument check rather than a plain
/// comparison, so they appear in no allowlist.
///
/// They still need numbers, which is why they are named here: the table is
/// generated from these constants, and `clone`'s absence from it produced a
/// filter that denied every `fork`.
pub const SPECIAL_CASED: &[&str] = &["clone"];

/// Syscalls that must never be reachable (design doc appendix B).
///
/// Nothing in `default` or `strict` grants these — they are simply absent from
/// both allowlists. The constant exists so a test can assert that, rather than
/// the absence being something a reader has to verify by reading 190 names.
///
/// `permissive` is the documented exception and is not a tenant profile: it
/// exists to debug a package the tighter profiles break, and to run Zygo's own
/// derived-layer builds, so it grants `ptrace`, `setns`, `unshare`, `mount`,
/// `umount2`, `pivot_root` and `process_vm_readv`/`writev` from this list. The
/// test below asserts exactly that set, so a syscall joining
/// [`PERMISSIVE_EXTRA`] by accident fails rather than widening the profile
/// quietly — which is how `io_uring` and `userfaultfd` came to be reachable
/// there.
pub const NEVER_ALLOWED: &[&str] = &[
    "bpf",
    "io_uring_setup",
    "io_uring_enter",
    "io_uring_register",
    "userfaultfd",
    "keyctl",
    "add_key",
    "request_key",
    "perf_event_open",
    "ptrace",
    "process_vm_readv",
    "process_vm_writev",
    "kcmp",
    "mount",
    "umount2",
    "pivot_root",
    "setns",
    "unshare",
    "open_by_handle_at",
    "name_to_handle_at",
    "quotactl",
    "reboot",
    "swapon",
    "swapoff",
    "kexec_load",
    "kexec_file_load",
    "init_module",
    "finit_module",
    "delete_module",
    "acct",
    "settimeofday",
    "clock_settime",
    "vhangup",
    "ioperm",
    "iopl",
];

/// Syscalls newer than 5.10 that no profile allows, each one decided rather
/// than left to the default. They answer `EPERM`, like every syscall this
/// build knows and does not allow; only numbers above the table answer
/// `ENOSYS`.
///
/// The new mount API and `mount_setattr` for the reason `mount` is refused;
/// `quotactl_fd`, `process_madvise`, `process_mrelease` and `pidfd_getfd`
/// act on other processes or on the filesystem as a whole; `memfd_secret`
/// takes memory out of the kernel's direct map; `cachestat` reports what is
/// in the page cache, a side channel between tenants; `statmount` and
/// `listmount` describe the host's mounts; `set_mempolicy_home_node` and the
/// `lsm_*` calls reach NUMA and security-module state no handler needs.
///
/// A test holds every table entry above [`REVIEWED_THROUGH`] to one of this
/// list or an allowlist, so regenerating the table against newer headers
/// fails until someone has decided on what they brought.
pub const NEWER_REFUSED: &[&str] = &[
    "cachestat",
    "fsconfig",
    "fsmount",
    "fsopen",
    "fspick",
    "listmount",
    "lsm_get_self_attr",
    "lsm_list_modules",
    "lsm_set_self_attr",
    "memfd_secret",
    "mount_setattr",
    "move_mount",
    "open_tree",
    "pidfd_getfd",
    "process_madvise",
    "process_mrelease",
    "quotactl_fd",
    "set_mempolicy_home_node",
    "statmount",
];

/// The highest syscall number decided one by one: `mseal`, Linux 6.10. Above
/// 439 (`faccessat2`, the newest syscall 5.10 has) each entry is allowed by a
/// profile or listed in [`NEWER_REFUSED`]; at or below it, anything not
/// allowed is refused, as it always was.
pub const REVIEWED_THROUGH: u32 = 462;

/// The syscall names a profile permits, sorted and deduplicated.
pub fn allowed_names(profile: SeccompProfile) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = match profile {
        SeccompProfile::Permissive => BASE_ALLOWLIST
            .iter()
            .chain(PERMISSIVE_EXTRA.iter())
            .copied()
            .collect(),
        SeccompProfile::Default => BASE_ALLOWLIST.to_vec(),
        SeccompProfile::Strict => BASE_ALLOWLIST
            .iter()
            .copied()
            .filter(|n| !STRICT_REMOVED.contains(n))
            .collect(),
    };
    names.sort_unstable();
    names.dedup();
    names
}

/// `ioctl` requests denied on every profile.
///
/// `ioctl` itself has to be allowed — terminals, sockets and half of libc need
/// it — so these are filtered on the request number instead.
///
/// `TIOCSTI` pushes a character into a terminal's *input* queue. A sandbox that
/// inherits the caller's terminal can use it to type into the user's shell,
/// which reads the characters as though they had been typed, after Zygo has
/// exited. Measured working from inside a sandbox on Linux 5.10; kernel 6.2
/// added `dev.tty.legacy_tiocsti` to disable it, but the host kernel cannot be
/// relied on. `TIOCLINUX` reaches the same input queue by another route.
pub const DENIED_IOCTLS: &[(&str, u32)] = &[("TIOCSTI", 0x5412), ("TIOCLINUX", 0x541C)];

#[derive(Debug, thiserror::Error)]
pub enum SeccompError {
    #[error("internal: a seccomp branch target was never defined")]
    UnresolvedLabel,

    #[error("internal: a seccomp branch would jump backwards, which BPF cannot encode")]
    BackwardJump,

    #[error("internal: a seccomp branch of {delta} instructions exceeds BPF's 8-bit limit")]
    JumpTooFar { delta: usize },

    #[error(
        "seccomp filtering is not implemented for {arch}\n  \
         → use --seccomp permissive to run without a filter, understanding that \
         the syscall surface is then unrestricted"
    )]
    UnsupportedArch { arch: &'static str },

    #[error("the seccomp filter needs {len} instructions, more than the kernel's 4096 limit")]
    TooLong { len: usize },

    #[error("installing the seccomp filter failed: {0}")]
    Install(#[source] std::io::Error),
}

/// Build the BPF program for a profile.
///
/// ```text
///   load  arch
///   jne   AUDIT_ARCH        -> kill        ; a foreign ABI means other numbers
///   load  nr
///   jeq   clone             -> clone_check ; allowed, but not with CLONE_NEW*
///   jeq   ioctl             -> ioctl_check ; allowed, but not TIOCSTI
///   goto  chain
/// clone_check:  load args[0]; and CLONE_NEW*; jeq 0 -> allow, else deny
/// ioctl_check:  load args[1]; jeq <denied request> -> deny; ...; goto allow
/// chain:
///   jeq   <allowed>         -> allow       ; one comparison per syscall
///   ...                                    ; every 64: an island of the
///   jgt   <highest known>   -> nosys       ;   four returns, stepped over
///   goto  deny
/// deny:  ret ERRNO(EPERM)
/// nosys: ret ERRNO(ENOSYS)
/// kill:  ret KILL_PROCESS
/// allow: ret ALLOW
/// ```
///
/// Classic BPF branches reach at most 255 instructions forward. A single
/// chain ending in one set of returns hit that wall at about 250 — twenty
/// more syscalls in `permissive` and the build would have failed. The islands
/// take the limit away: every branch lands on the nearest copy of its return.
pub fn program(profile: SeccompProfile) -> Result<Vec<SockFilter>, SeccompError> {
    build(&allowed_names(profile))
}

/// [`program`], for any allowlist — the tests hand it far longer ones than
/// any profile has, to show the length limit is gone.
fn build(names: &[&str]) -> Result<Vec<SockFilter>, SeccompError> {
    if !syscalls::is_supported() {
        return Err(SeccompError::UnsupportedArch {
            arch: std::env::consts::ARCH,
        });
    }
    let clone_nr = syscalls::number("clone");
    let ioctl_nr = syscalls::number("ioctl").filter(|_| names.contains(&"ioctl"));
    // `clone3` takes its flags in a struct the filter cannot read, so a
    // profile that inspects `clone`'s flags cannot allow it. But it must not
    // answer `EPERM` either: glibc's `pthread_create` tries `clone3` first and
    // falls back to `clone` **only on `ENOSYS`** — on `EPERM` it gives up, and
    // every threaded program dies with "can't start new thread". Found by the
    // seccomp compatibility matrix: `pip` starts a thread for its progress
    // bar on a large download, and `numpy` starts several on import. Docker's
    // profile answers `ENOSYS` for the same reason.
    let clone3_nosys = syscalls::number("clone3").filter(|_| !names.contains(&"clone3"));

    // These two are compared against their arguments, not merely their number.
    let simple: Vec<u32> = names
        .iter()
        .filter(|n| **n != "clone" && **n != "ioctl")
        .filter_map(|n| syscalls::number(n))
        .collect();

    let mut asm = Assembler::default();

    // The architecture check comes first: the numbers below mean one thing per
    // ABI, and a process that re-executes under another would otherwise be
    // filtered against the wrong table.
    asm.load(OFF_ARCH)
        .jeq(syscalls::AUDIT_ARCH, Label::Next, Label::Kill)
        .load(OFF_NR);

    if let Some(nr) = clone_nr {
        asm.jeq(nr, Label::CloneCheck, Label::Next);
    }
    if let Some(nr) = clone3_nosys {
        asm.jeq(nr, Label::NoSys, Label::Next);
    }
    if let Some(nr) = ioctl_nr {
        asm.jeq(nr, Label::IoctlCheck, Label::Next);
    }
    asm.goto(Label::Chain);

    // The two argument checks sit here, near the top, rather than after the
    // chain: the branches that reach them are the first few instructions, and
    // the far end of a long chain would be out of their reach.
    if clone_nr.is_some() {
        asm.mark(Label::CloneCheck)
            .load(OFF_ARG0_LO)
            .and(CLONE_NEW_MASK)
            .jeq(0, Label::Allow, Label::Deny);
    }
    if ioctl_nr.is_some() {
        asm.mark(Label::IoctlCheck).load(OFF_ARG1_LO);
        for (_name, request) in DENIED_IOCTLS {
            asm.jeq(*request, Label::Deny, Label::Next);
        }
        asm.goto(Label::Allow);
    }

    asm.mark(Label::Chain);
    for (i, nr) in simple.iter().enumerate() {
        if i % ISLAND_EVERY == 0 {
            asm.island();
        }
        asm.jeq(*nr, Label::Allow, Label::Next);
    }
    // Anything left is refused — but *how* it is refused matters for a number
    // this build has never heard of.
    //
    // A libc probes for a new syscall and falls back to the old way **on
    // `ENOSYS` only**; on `EPERM` it reports failure. That is already written
    // down here for `clone3`, where the consequence was "can't start new
    // thread". It is the same for every syscall added after this table was
    // generated: `fchmodat2` (6.6) is what glibc uses for `chmod`, it is not
    // in the table, it was answered `EPERM`, and `python3 -m venv` died with
    // `[Errno 1] Operation not permitted: '/venv/bin/activate.fish'` on a
    // 6.17 runner while a 6.8 one was fine.
    //
    // So: above the highest number the table knows, answer `ENOSYS`. Syscall
    // numbers only ever go up, so that is exactly the set this build cannot
    // have an opinion about — and saying "there is no such syscall here" is
    // both true and the answer a fallback can act on. Numbers the table *does*
    // know and this profile does not allow keep `EPERM`: refusing something
    // that exists is a different statement, and the escape suite asserts it.
    if let Some(highest) = syscalls::TABLE.iter().map(|(_, nr)| *nr).max() {
        asm.load(OFF_NR).jgt(highest, Label::NoSys, Label::Deny);
    }
    asm.goto(Label::Deny);

    asm.mark(Label::Deny)
        .ret(RET_ERRNO | libc::EPERM as u32)
        .mark(Label::NoSys)
        .ret(RET_ERRNO | libc::ENOSYS as u32)
        .mark(Label::Kill)
        .ret(RET_KILL_PROCESS)
        .mark(Label::Allow)
        .ret(RET_ALLOW);

    let prog = asm.assemble()?;
    if prog.len() > 4096 {
        return Err(SeccompError::TooLong { len: prog.len() });
    }
    Ok(prog)
}

const SYS_SECCOMP: libc::c_long = if cfg!(target_arch = "aarch64") {
    277
} else {
    317
};
const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;
/// Apply to every thread, not just the calling one.
const SECCOMP_FILTER_FLAG_TSYNC: libc::c_uint = 1;
/// Write every non-`ALLOW` verdict to the kernel's audit log (4.14+).
///
/// With `SECCOMP_RET_ERRNO` the kernel is otherwise silent: the program sees
/// `EPERM` from a syscall its traceback never names, and nothing anywhere
/// says which syscall it was. The first adoption report spent a session on
/// exactly that — `listxattr`, seen as nine tracebacks about `RECORD` and
/// `WHEEL`. Under `ZYGO_LOG=debug` the launcher sets this flag, and each
/// refusal then appears in `dmesg` / `journalctl -k` as
/// `audit: type=1326 … comm="python3" … syscall=<n> …`, where `<n>` is the
/// number in [`syscalls::TABLE`]. The kernel's own `actions_logged` sysctl
/// includes `errno` by default, which is what makes the line appear.
const SECCOMP_FILTER_FLAG_LOG: libc::c_uint = 2;

/// Install a filter on the calling process.
///
/// `log_denials` asks the kernel to write every refusal to its audit log —
/// see [`SECCOMP_FILTER_FLAG_LOG`]. A kernel too old to know the flag answers
/// `EINVAL`, and the filter is then installed without it: the policy matters
/// more than the log line, and an unfiltered sandbox is not an option.
///
/// # Safety
///
/// Async-signal-safe: two syscalls and no allocation, so it is callable between
/// `clone3` and `execve`. `prog` must outlive the call.
///
/// `PR_SET_NO_NEW_PRIVS` must already be set, or the kernel refuses the filter
/// for an unprivileged caller.
pub unsafe fn install(prog: &[SockFilter], log_denials: bool) -> Result<(), std::io::Error> {
    let fprog = SockFprog {
        len: prog.len() as u16,
        filter: prog.as_ptr(),
    };
    let apply = |flags: libc::c_uint| unsafe {
        libc::syscall(
            SYS_SECCOMP,
            SECCOMP_SET_MODE_FILTER,
            flags,
            &fprog as *const SockFprog,
        )
    };
    let mut rc = apply(if log_denials {
        SECCOMP_FILTER_FLAG_TSYNC | SECCOMP_FILTER_FLAG_LOG
    } else {
        SECCOMP_FILTER_FLAG_TSYNC
    });
    if rc != 0 && log_denials {
        rc = apply(SECCOMP_FILTER_FLAG_TSYNC);
    }
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Sorted, and every name known to *some* architecture.
    ///
    /// Sortedness is not cosmetic here: it is how a reader finds out whether
    /// a syscall is allowed, and `fork` went missing for exactly as long as
    /// nobody could look it up.
    #[test]
    fn the_allowlist_is_sorted() {
        let mut sorted = BASE_ALLOWLIST.to_vec();
        sorted.sort_unstable();
        assert_eq!(BASE_ALLOWLIST, sorted.as_slice());
    }

    /// The bug this pair fixes: musl's `fork()` uses `SYS_fork` where the
    /// architecture has one, and `clone(SIGCHLD)` where it does not. aarch64
    /// has no `fork` syscall, so its absence from the allowlist cost nothing
    /// and was invisible; on x86_64 every musl image in a sandbox got
    /// `can't fork: Operation not permitted` from its shell.
    #[test]
    fn a_process_can_fork_however_its_libc_spells_it() {
        for name in ["fork", "vfork", "clone"] {
            let known_here = crate::backend::ns::syscalls::number(name).is_some();
            let allowed = BASE_ALLOWLIST.contains(&name) || SPECIAL_CASED.contains(&name);
            assert!(
                allowed || !known_here,
                "`{name}` exists on this architecture and nothing allows it"
            );
        }
    }

    use super::*;

    #[test]
    fn instruction_layout_matches_the_kernel() {
        assert_eq!(core::mem::size_of::<SockFilter>(), 8);
    }

    #[test]
    fn profiles_are_ordered_by_how_much_they_permit() {
        let permissive = allowed_names(SeccompProfile::Permissive).len();
        let default = allowed_names(SeccompProfile::Default).len();
        let strict = allowed_names(SeccompProfile::Strict).len();
        assert!(strict < default, "{strict} !< {default}");
        assert!(default < permissive, "{default} !< {permissive}");
    }

    /// Appendix B's exclusions are the point of the whole profile. If one of
    /// them ever appears in an allowlist, the sandbox has a hole that reading
    /// 190 names would not reveal.
    /// What `permissive` is allowed to grant from [`NEVER_ALLOWED`], exactly.
    ///
    /// Not "some of them": the list. `io_uring_setup`, `io_uring_enter`,
    /// `io_uring_register` and `userfaultfd` were in `PERMISSIVE_EXTRA` and
    /// are not in this set, which is what the escape suite found when it
    /// started running appendix B's vectors against all three profiles.
    const PERMISSIVE_MAY_GRANT: &[&str] = &[
        "ptrace",
        "setns",
        "unshare",
        "mount",
        "umount2",
        "pivot_root",
        "process_vm_readv",
        "process_vm_writev",
    ];

    #[test]
    fn permissive_grants_exactly_the_forbidden_syscalls_it_is_documented_to() {
        let allowed = allowed_names(SeccompProfile::Permissive);
        // Sorted, because the order here is `NEVER_ALLOWED`'s and carries no
        // meaning; what is being asserted is the set.
        let mut granted: Vec<&str> = NEVER_ALLOWED
            .iter()
            .copied()
            .filter(|name| allowed.contains(name))
            .collect();
        granted.sort_unstable();
        let mut documented = PERMISSIVE_MAY_GRANT.to_vec();
        documented.sort_unstable();
        assert_eq!(
            granted, documented,
            "`permissive` grants a different set of appendix B's exclusions than \
             it is documented to. Adding one is a decision, not a detail: this \
             profile is what `apt` builds run under."
        );
    }

    #[test]
    fn the_forbidden_syscalls_are_absent_from_default_and_strict() {
        for profile in [SeccompProfile::Default, SeccompProfile::Strict] {
            let allowed = allowed_names(profile);
            for name in NEVER_ALLOWED {
                assert!(
                    !allowed.contains(name),
                    "{name} is allowed by the {profile} profile"
                );
            }
        }
    }

    #[test]
    fn strict_removes_the_socket_family() {
        let strict = allowed_names(SeccompProfile::Strict);
        for name in ["socket", "connect", "bind", "listen", "ptrace"] {
            assert!(
                !strict.contains(&name),
                "{name} survived the strict profile"
            );
        }
        // But not `socketpair`, which reaches nothing: it is `pipe2` with a
        // nicer API, and libuv builds every Node stdio pipe out of one. A
        // `strict` Node function could not start a worker while it was
        // removed.
        assert!(
            strict.contains(&"socketpair"),
            "strict removed socketpair, which is how Node makes a pipe"
        );
        // But the sandbox still has to be able to run and exit — and an agent
        // has to be able to use the control socket it was handed. `strict`
        // once removed `recvfrom`, and every strict function died before its
        // handler ran.
        for name in [
            "read",
            "write",
            "mmap",
            "exit_group",
            "sendto",
            "recvfrom",
            "sendmsg",
            "recvmsg",
        ] {
            assert!(strict.contains(&name), "{name} is required even in strict");
        }
    }

    /// The launcher installs its filter immediately before `execve`. A profile
    /// that denies it produces a sandbox that cannot start — the tightening
    /// belongs in the agent's already-running child (design doc §3.4.1).
    #[test]
    fn no_profile_denies_the_exec_that_starts_the_sandbox() {
        for profile in [
            SeccompProfile::Permissive,
            SeccompProfile::Default,
            SeccompProfile::Strict,
        ] {
            assert!(
                allowed_names(profile).contains(&"execve"),
                "{profile} cannot start a sandbox at all"
            );
        }
        assert!(
            STRICT_CHILD_REMOVED.contains(&"execve"),
            "the child-side tightening is where execve belongs"
        );
    }

    #[test]
    fn permissive_is_for_debugging_and_says_so_by_allowing_ptrace() {
        let permissive = allowed_names(SeccompProfile::Permissive);
        assert!(permissive.contains(&"ptrace"));
        assert!(permissive.contains(&"unshare"));
    }

    #[test]
    fn allowlists_have_no_duplicates() {
        for profile in [
            SeccompProfile::Permissive,
            SeccompProfile::Default,
            SeccompProfile::Strict,
        ] {
            let names = allowed_names(profile);
            let mut unique = names.clone();
            unique.dedup();
            assert_eq!(names.len(), unique.len(), "{profile} has duplicates");
        }
    }

    /// Every syscall newer than 5.10 is either allowed or refused on purpose,
    /// and nothing in the table is newer than that review. A table
    /// regenerated against newer headers fails here until each new name is
    /// placed in an allowlist or in [`NEWER_REFUSED`].
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn every_syscall_newer_than_5_10_has_been_decided() {
        let decided: Vec<&str> = BASE_ALLOWLIST
            .iter()
            .chain(PERMISSIVE_EXTRA)
            .chain(NEVER_ALLOWED)
            .chain(NEWER_REFUSED)
            .chain(SPECIAL_CASED)
            .copied()
            .collect();
        for (name, nr) in syscalls::TABLE {
            assert!(
                *nr <= REVIEWED_THROUGH,
                "{name} ({nr}) is newer than the review: allow it in BASE_ALLOWLIST or \
                 refuse it in NEWER_REFUSED, then raise REVIEWED_THROUGH"
            );
            if *nr > 439 {
                assert!(decided.contains(name), "{name} ({nr}) was never decided on");
            }
        }
        for name in NEWER_REFUSED {
            for profile in [
                SeccompProfile::Permissive,
                SeccompProfile::Default,
                SeccompProfile::Strict,
            ] {
                assert!(
                    !allowed_names(profile).contains(name),
                    "{profile} allows {name}"
                );
            }
        }
    }

    /// A profile can only grant or deny what it can name. A syscall missing
    /// from the table is silently dropped from the filter, which made
    /// `permissive` unable to allow `unshare` while every structural test
    /// still passed.
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    #[test]
    fn every_syscall_a_profile_mentions_is_in_the_table() {
        // These genuinely do not exist on arm64 — it has only the `*at` and
        // `*6` forms, and the x86 port I/O calls are x86-only. Computed from
        // the difference between the two generated tables, not guessed.
        let absent_on_aarch64 = [
            "access",
            "arch_prctl",
            "chmod",
            "chown",
            "dup2",
            "epoll_create",
            "epoll_wait",
            "eventfd",
            "fork",
            "getpgrp",
            "ioperm",
            "iopl",
            "lchown",
            "link",
            "lstat",
            "map_shadow_stack",
            "mkdir",
            "mknod",
            "open",
            "pipe",
            "poll",
            "readlink",
            "rename",
            "rmdir",
            "select",
            "stat",
            "symlink",
            "time",
            "unlink",
            "vfork",
        ];
        let allowed_missing: &[&str] = if cfg!(target_arch = "aarch64") {
            &absent_on_aarch64
        } else {
            &[]
        };

        let mentioned: Vec<&str> = BASE_ALLOWLIST
            .iter()
            .chain(PERMISSIVE_EXTRA)
            .chain(NEVER_ALLOWED)
            .chain(NEWER_REFUSED)
            .chain(SPECIAL_CASED)
            .copied()
            .collect();
        let missing: Vec<&str> = mentioned
            .iter()
            .copied()
            .filter(|n| syscalls::number(n).is_none())
            .collect();

        for name in &missing {
            assert!(
                allowed_missing.contains(name),
                "{name} is mentioned by a profile but has no number here"
            );
        }
    }

    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    mod program {
        use super::*;

        /// What the kernel would do with a given syscall.
        #[derive(Debug, PartialEq, Eq)]
        enum Verdict {
            Allow,
            Deny(u32),
            Kill,
        }

        /// A miniature BPF interpreter over the opcodes this module emits.
        ///
        /// Structural assertions — "there is an AND instruction somewhere" —
        /// cannot catch a wrong jump offset, and a wrong offset is how a filter
        /// silently denies `fork` or silently permits `unshare`. Running the
        /// program is the only assertion that actually tests the policy.
        fn evaluate(prog: &[SockFilter], arch: u32, nr: u32, arg0: u64) -> Verdict {
            evaluate_args(prog, arch, nr, arg0, 0)
        }

        fn evaluate_args(prog: &[SockFilter], arch: u32, nr: u32, arg0: u64, arg1: u64) -> Verdict {
            let mut acc: u32 = 0;
            let mut pc = 0usize;
            let mut steps = 0;

            loop {
                steps += 1;
                assert!(steps < 10_000, "the program does not terminate");
                let insn = *prog
                    .get(pc)
                    .expect("execution ran past the end of the program");

                match insn.code {
                    c if c == BPF_LD | BPF_W | BPF_ABS => {
                        acc = match insn.k {
                            OFF_NR => nr,
                            OFF_ARCH => arch,
                            OFF_ARG0_LO => arg0 as u32,
                            OFF_ARG1_LO => arg1 as u32,
                            other => panic!("unexpected load offset {other}"),
                        };
                        pc += 1;
                    }
                    c if c == BPF_ALU | BPF_AND | BPF_K => {
                        acc &= insn.k;
                        pc += 1;
                    }
                    c if c == BPF_JMP | BPF_JEQ | BPF_K => {
                        let taken = acc == insn.k;
                        pc += 1 + usize::from(if taken { insn.jt } else { insn.jf });
                    }
                    c if c == BPF_JMP | BPF_JGT | BPF_K => {
                        let taken = acc > insn.k;
                        pc += 1 + usize::from(if taken { insn.jt } else { insn.jf });
                    }
                    c if c == BPF_JMP | BPF_JA => {
                        pc += 1 + insn.k as usize;
                    }
                    c if c == BPF_RET | BPF_K => {
                        return match insn.k {
                            RET_ALLOW => Verdict::Allow,
                            RET_KILL_PROCESS => Verdict::Kill,
                            v => Verdict::Deny(v & 0xffff),
                        };
                    }
                    other => panic!("unexpected opcode {other:#x}"),
                }
            }
        }

        fn verdict(profile: SeccompProfile, name: &str, arg0: u64) -> Verdict {
            let prog = program(profile).unwrap();
            let nr = syscalls::number(name)
                .unwrap_or_else(|| panic!("{name} has no number on this architecture"));
            evaluate(&prog, syscalls::AUDIT_ARCH, nr, arg0)
        }

        /// A number newer than this build's table is "no such syscall", not
        /// "refused" — the distinction a libc fallback turns on.
        ///
        /// `clone3` had this written for it by hand. Everything added after
        /// the table was generated needs it too: `fchmodat2` (kernel 6.6) is
        /// what glibc uses for `chmod`, it is not in the table, and answering
        /// it `EPERM` killed `python3 -m venv` on a 6.17 runner with
        /// `[Errno 1] Operation not permitted: '/venv/bin/activate.fish'`.
        #[test]
        fn a_syscall_newer_than_the_table_says_there_is_no_such_syscall() {
            let highest = syscalls::TABLE
                .iter()
                .map(|(_, nr)| *nr)
                .max()
                .expect("a supported architecture has a table");
            let prog = program(SeccompProfile::Default).unwrap();
            let at = |nr| evaluate(&prog, syscalls::AUDIT_ARCH, nr, 0);

            assert_eq!(
                at(highest + 1),
                Verdict::Deny(libc::ENOSYS as u32),
                "a number this build cannot have an opinion about"
            );
            assert_eq!(at(highest + 99), Verdict::Deny(libc::ENOSYS as u32));

            // Something the table knows and this profile refuses keeps
            // `EPERM`: refusing what exists is a different statement, and the
            // escape suite asserts it.
            assert_eq!(
                verdict(SeccompProfile::Default, "ptrace", 0),
                Verdict::Deny(libc::EPERM as u32),
                "`ptrace` exists here and is refused"
            );
        }

        /// Z-1 from the first adoption report: `listxattr` answered `EPERM`,
        /// which `shutil.copy2` reports as `[Errno 1] Operation not permitted`
        /// on a file that plainly exists — and `pip install --target` is a
        /// `copy2` per file. Every spelling of every operation on an extended
        /// attribute is allowed, under every profile; the reasoning is on
        /// [`BASE_ALLOWLIST`]. `strict` keeps them on purpose: a `network =
        /// "none"` function copies files like any other, and a profile that
        /// broke `copy2` in a way nothing documents would be the bug again.
        #[test]
        fn the_xattr_family_is_allowed_by_every_profile() {
            for name in [
                "getxattr",
                "lgetxattr",
                "fgetxattr",
                "listxattr",
                "llistxattr",
                "flistxattr",
                "setxattr",
                "lsetxattr",
                "fsetxattr",
                "removexattr",
                "lremovexattr",
                "fremovexattr",
            ] {
                for profile in [
                    SeccompProfile::Permissive,
                    SeccompProfile::Default,
                    SeccompProfile::Strict,
                ] {
                    assert_eq!(
                        verdict(profile, name, 0),
                        Verdict::Allow,
                        "{profile}: `{name}` must be allowed, or `shutil.copy2` dies with EPERM"
                    );
                }
            }
        }

        /// A legacy spelling and its `*at` form get the same answer.
        ///
        /// Two bugs came out of them differing, both invisible on aarch64 and
        /// both fatal on x86_64. `fork` is not a syscall on aarch64, so musl
        /// reaches for `clone` there and `SYS_fork` here, and nothing allowed
        /// it: `/bin/sh: can't fork`. `chmod` is not a syscall on aarch64
        /// either, so glibc reaches for `fchmodat` there and `chmod` here, and
        /// only the first was allowed: `python3 -m venv` died with `[Errno 1]
        /// Operation not permitted` partway through its activation scripts.
        ///
        /// Denying one spelling while permitting the other is not a boundary —
        /// it is the same call with the same reach — so the two must agree,
        /// whichever way. `mknod`/`mknodat` agree by both being refused, which
        /// is the boundary the escape suite actually checks.
        #[test]
        fn a_legacy_spelling_and_its_at_form_agree() {
            let pairs = [
                ("chmod", "fchmodat"),
                ("chown", "fchownat"),
                ("lchown", "fchownat"),
                ("mkdir", "mkdirat"),
                ("mknod", "mknodat"),
                ("open", "openat"),
                ("readlink", "readlinkat"),
                ("rename", "renameat"),
                ("rmdir", "unlinkat"),
                ("symlink", "symlinkat"),
                ("unlink", "unlinkat"),
            ];
            for (legacy, modern) in pairs {
                let (Some(_), Some(_)) = (syscalls::number(legacy), syscalls::number(modern))
                else {
                    continue; // one of them does not exist on this architecture
                };
                assert_eq!(
                    verdict(SeccompProfile::Default, legacy, 0),
                    verdict(SeccompProfile::Default, modern, 0),
                    "`{legacy}` and `{modern}` are the same call and must get the same answer"
                );
            }
        }

        #[test]
        fn ordinary_syscalls_are_allowed() {
            for name in ["read", "write", "openat", "mmap", "exit_group", "execve"] {
                assert_eq!(
                    verdict(SeccompProfile::Default, name, 0),
                    Verdict::Allow,
                    "{name} should be allowed"
                );
            }
        }

        /// The first run of the real launcher failed with `can't fork:
        /// Operation not permitted`: the `clone` comparison jumped to the deny
        /// instead of to its argument check. Structural tests all passed.
        #[test]
        fn a_plain_fork_is_allowed_but_a_namespace_clone_is_not() {
            const SIGCHLD: u64 = 17;
            const CLONE_NEWUSER: u64 = 0x1000_0000;
            const CLONE_NEWNET: u64 = 0x4000_0000;
            const CLONE_VM_THREAD: u64 = 0x0000_0100 | 0x0000_0200 | 0x0000_0400;

            assert_eq!(
                verdict(SeccompProfile::Default, "clone", SIGCHLD),
                Verdict::Allow,
                "a plain fork must work — this is the warm path itself"
            );
            assert_eq!(
                verdict(SeccompProfile::Default, "clone", CLONE_VM_THREAD),
                Verdict::Allow,
                "thread creation must work"
            );
            // glibc reaches `clone` only if `clone3` says ENOSYS. EPERM here
            // is "can't start new thread" in every program with a thread —
            // which the compatibility matrix found in `pip` and `numpy`.
            for profile in [SeccompProfile::Default, SeccompProfile::Strict] {
                assert_eq!(
                    verdict(profile, "clone3", 0),
                    Verdict::Deny(libc::ENOSYS as u32),
                    "{profile}: clone3 must be ENOSYS so glibc falls back to clone"
                );
            }
            assert_eq!(
                verdict(SeccompProfile::Permissive, "clone3", 0),
                Verdict::Allow,
                "permissive lists clone3 outright"
            );
            assert_eq!(
                verdict(SeccompProfile::Default, "clone", SIGCHLD | CLONE_NEWUSER),
                Verdict::Deny(libc::EPERM as u32),
                "a sandbox that can nest a user namespace can escape its own"
            );
            assert_eq!(
                verdict(SeccompProfile::Default, "clone", SIGCHLD | CLONE_NEWNET),
                Verdict::Deny(libc::EPERM as u32)
            );
        }

        #[test]
        fn the_forbidden_syscalls_are_denied_when_actually_run() {
            for name in NEVER_ALLOWED {
                let Some(nr) = syscalls::number(name) else {
                    continue; // not present on this architecture
                };
                let prog = program(SeccompProfile::Default).unwrap();
                assert_eq!(
                    evaluate(&prog, syscalls::AUDIT_ARCH, nr, 0),
                    Verdict::Deny(libc::EPERM as u32),
                    "{name} was permitted by the default profile"
                );
            }
        }

        /// Denied — and the denial says *why*, because that changes what a
        /// caller does next.
        ///
        /// This used to assert `EPERM`. It is `ENOSYS` now, deliberately: a
        /// libc that probes for a syscall added after this table was
        /// generated falls back to the old way on `ENOSYS` and gives up on
        /// `EPERM`, which is how `fchmodat2` broke `python3 -m venv` on a
        /// 6.17 kernel. Both are refusals; only one is one the caller can
        /// work around, and neither lets the syscall run.
        #[test]
        fn an_unknown_syscall_number_is_denied_as_not_existing() {
            let prog = program(SeccompProfile::Default).unwrap();
            let verdict = evaluate(&prog, syscalls::AUDIT_ARCH, 9999, 0);
            assert_eq!(
                verdict,
                Verdict::Deny(libc::ENOSYS as u32),
                "the allowlist must deny by default, including future syscalls"
            );
            assert!(
                !matches!(verdict, Verdict::Allow),
                "and it must never be allowed"
            );
        }

        /// A filter that skips the architecture check is bypassed by
        /// re-executing under the other ABI, where the numbers mean other things.
        #[test]
        fn a_foreign_architecture_is_killed_outright() {
            let prog = program(SeccompProfile::Default).unwrap();
            let read = syscalls::number("read").unwrap();
            assert_eq!(
                evaluate(&prog, 0xdead_beef, read, 0),
                Verdict::Kill,
                "a syscall from another ABI must not merely be denied"
            );
        }

        /// Measured working from inside a real sandbox before this filter
        /// existed: `ioctl(1, TIOCSTI, "x")` pushes a character into the
        /// terminal's *input* queue, so a sandbox holding the caller's terminal
        /// can type into the user's shell — and the shell reads it after Zygo
        /// has exited.
        #[test]
        fn terminal_injection_ioctls_are_denied_on_every_profile() {
            for profile in [
                SeccompProfile::Permissive,
                SeccompProfile::Default,
                SeccompProfile::Strict,
            ] {
                let prog = program(profile).unwrap();
                let Some(ioctl) = syscalls::number("ioctl") else {
                    continue;
                };
                for (name, request) in DENIED_IOCTLS {
                    assert_eq!(
                        evaluate_args(&prog, syscalls::AUDIT_ARCH, ioctl, 1, *request as u64),
                        Verdict::Deny(libc::EPERM as u32),
                        "{profile} permits ioctl({name})"
                    );
                }
            }
        }

        /// `ioctl` itself has to keep working: terminals, sockets and half of
        /// libc depend on it, so the filter is on the request, not the syscall.
        #[test]
        fn ordinary_ioctls_still_work() {
            let prog = program(SeccompProfile::Default).unwrap();
            let ioctl = syscalls::number("ioctl").unwrap();
            for request in [
                0x5401u64, // TCGETS — every isatty() call
                0x5413,    // TIOCGWINSZ — terminal size
                0x8910,    // SIOCGIFNAME
                0,
            ] {
                assert_eq!(
                    evaluate_args(&prog, syscalls::AUDIT_ARCH, ioctl, 1, request),
                    Verdict::Allow,
                    "ioctl request {request:#x} should be allowed"
                );
            }
        }

        /// The tightening the design puts in the agent's child, run rather
        /// than inspected: a program is refused, a process is refused, a
        /// thread is not — and the profiles that do not tighten hand back
        /// nothing at all rather than an empty filter.
        #[test]
        fn the_child_filter_refuses_programs_and_processes_but_not_threads() {
            const SIGCHLD: u64 = 17;
            const CLONE_VM_FS_FILES_THREAD: u64 = 0x100 | 0x200 | 0x400 | 0x1_0000;
            const CLONE_VM_VFORK: u64 = 0x100 | 0x4000;

            let prog = child_program(SeccompProfile::Strict)
                .unwrap()
                .expect("strict tightens the child");
            let nr = |name: &str| syscalls::number(name).unwrap();
            let run = |nr: u32, arg0: u64| evaluate(&prog, syscalls::AUDIT_ARCH, nr, arg0);
            let eperm = Verdict::Deny(libc::EPERM as u32);

            assert_eq!(run(nr("execve"), 0), eperm, "no new program");
            assert_eq!(run(nr("execveat"), 0), eperm);
            assert_eq!(run(nr("clone"), SIGCHLD), eperm, "a fork is a clone");
            assert_eq!(
                run(nr("clone"), CLONE_VM_VFORK),
                eperm,
                "posix_spawn's vfork-style clone is a process too"
            );
            assert_eq!(
                run(nr("clone"), CLONE_VM_FS_FILES_THREAD),
                Verdict::Allow,
                "a thread must still work"
            );
            for name in ["read", "write", "mmap", "openat", "exit_group", "prctl"] {
                assert_eq!(run(nr(name), 0), Verdict::Allow, "{name} falls through");
            }
            assert_eq!(
                evaluate(&prog, syscalls::AUDIT_ARCH ^ 1, nr("read"), 0),
                Verdict::Kill,
                "a foreign ABI is killed, as in every other program here"
            );

            for profile in [SeccompProfile::Default, SeccompProfile::Permissive] {
                assert_eq!(
                    child_program(profile).unwrap(),
                    None,
                    "{profile} does not tighten the child"
                );
            }
        }

        /// What the agent receives is bytes; the layout has to be the kernel's.
        #[test]
        fn the_child_filter_encodes_to_the_kernel_layout() {
            let prog = child_program(SeccompProfile::Strict).unwrap().unwrap();
            let bytes = encode(&prog);
            assert_eq!(bytes.len(), prog.len() * 8);
            let first = &bytes[..8];
            assert_eq!(u16::from_ne_bytes([first[0], first[1]]), prog[0].code);
            assert_eq!(first[2], prog[0].jt);
            assert_eq!(first[3], prog[0].jf);
            assert_eq!(
                u32::from_ne_bytes([first[4], first[5], first[6], first[7]]),
                prog[0].k
            );
            // The whole thing has to fit in the environment comfortably.
            assert!(bytes.len() < 512, "{} bytes", bytes.len());
        }

        #[test]
        fn strict_denies_the_socket_family_but_still_starts() {
            assert_eq!(
                verdict(SeccompProfile::Strict, "socket", 0),
                Verdict::Deny(libc::EPERM as u32)
            );
            assert_eq!(verdict(SeccompProfile::Strict, "execve", 0), Verdict::Allow);
            assert_eq!(verdict(SeccompProfile::Strict, "read", 0), Verdict::Allow);
        }

        #[test]
        fn permissive_allows_what_default_denies() {
            assert_eq!(
                verdict(SeccompProfile::Default, "unshare", 0),
                Verdict::Deny(libc::EPERM as u32)
            );
            assert_eq!(
                verdict(SeccompProfile::Permissive, "unshare", 0),
                Verdict::Allow
            );
        }

        #[test]
        fn every_program_starts_by_checking_the_architecture() {
            for profile in [
                SeccompProfile::Permissive,
                SeccompProfile::Default,
                SeccompProfile::Strict,
            ] {
                let p = program(profile).unwrap();
                assert_eq!(
                    p[0],
                    stmt(BPF_LD | BPF_W | BPF_ABS, OFF_ARCH),
                    "{profile}: the architecture must be the first thing loaded"
                );
                assert_eq!(p[1].k, syscalls::AUDIT_ARCH, "{profile}");
                // Where the kill *lands* is the assembler's business; that a
                // foreign ABI reaches it is asserted behaviourally in
                // `a_foreign_architecture_is_killed_outright`.
                assert!(
                    p.iter()
                        .any(|i| i.code == BPF_RET | BPF_K && i.k == RET_KILL_PROCESS),
                    "{profile}: no kill return in the program"
                );
            }
        }

        #[test]
        fn the_default_action_is_to_deny() {
            let p = program(SeccompProfile::Default).unwrap();
            let denies = p
                .iter()
                .filter(|i| i.code == BPF_RET | BPF_K && i.k == RET_ERRNO | libc::EPERM as u32)
                .count();
            assert!(denies >= 1, "no EPERM return in the program");
            assert_eq!(
                p.last().unwrap().k,
                RET_ALLOW,
                "the allow target belongs at the very end"
            );
        }

        #[test]
        fn clone_is_gated_on_its_namespace_flags() {
            let p = program(SeccompProfile::Default).unwrap();
            assert!(
                p.iter()
                    .any(|i| i.code == BPF_ALU | BPF_AND | BPF_K && i.k == CLONE_NEW_MASK),
                "clone's CLONE_NEW* mask is not checked"
            );
            assert!(
                p.iter()
                    .any(|i| i.code == BPF_LD | BPF_W | BPF_ABS && i.k == OFF_ARG0_LO),
                "clone's first argument is never loaded"
            );
        }

        /// Every `CLONE_NEW*` bit has to be in the mask; one omission lets a
        /// sandbox build a namespace it controls.
        #[test]
        fn the_clone_mask_covers_every_namespace_flag() {
            for (name, bit) in [
                ("NEWNS", 0x0002_0000u32),
                ("NEWCGROUP", 0x0200_0000),
                ("NEWUTS", 0x0400_0000),
                ("NEWIPC", 0x0800_0000),
                ("NEWUSER", 0x1000_0000),
                ("NEWPID", 0x2000_0000),
                ("NEWNET", 0x4000_0000),
            ] {
                assert!(CLONE_NEW_MASK & bit != 0, "CLONE_{name} is not masked");
            }
        }

        #[test]
        fn programs_fit_inside_the_kernels_limits() {
            for profile in [
                SeccompProfile::Permissive,
                SeccompProfile::Default,
                SeccompProfile::Strict,
            ] {
                let p = program(profile).unwrap();
                assert!(p.len() < 4096, "{profile}: {} instructions", p.len());
            }
        }

        /// The 8-bit branch limit used to cap the allowlist at about 250
        /// instructions — `permissive` was at 232. Every name the table knows,
        /// three times over, must still assemble, and still
        /// mean what it says at both ends of the chain.
        #[test]
        fn a_far_longer_allowlist_still_assembles_and_still_filters() {
            let everything: Vec<&str> = syscalls::TABLE
                .iter()
                .map(|(name, _)| *name)
                .filter(|n| !["clone3", "ioctl"].contains(n))
                .collect();
            // Three times over: the table has about 220 names on aarch64, and
            // a repeat costs one comparison like any new name would.
            let p = build(&everything.repeat(3)).unwrap();
            assert!(p.len() > 255, "the test must cross the old limit");
            for name in [
                everything[0],
                everything[everything.len() / 2],
                everything[everything.len() - 1],
            ] {
                let nr = syscalls::number(name).unwrap();
                let arg0 = if name == "clone" { 0x1000_0000 } else { 0 };
                let expected = if name == "clone" {
                    Verdict::Deny(libc::EPERM as u32)
                } else {
                    Verdict::Allow
                };
                assert_eq!(
                    evaluate(&p, syscalls::AUDIT_ARCH, nr, arg0),
                    expected,
                    "{name}"
                );
            }
            let clone = syscalls::number("clone").unwrap();
            assert_eq!(
                evaluate(&p, syscalls::AUDIT_ARCH, clone, 0x11),
                Verdict::Allow
            );
            assert_eq!(evaluate(&p, 0x1234, 0, 0), Verdict::Kill);
            let highest = syscalls::TABLE.iter().map(|(_, nr)| *nr).max().unwrap();
            assert_eq!(
                evaluate(&p, syscalls::AUDIT_ARCH, highest + 1, 0),
                Verdict::Deny(libc::ENOSYS as u32)
            );
        }

        #[test]
        fn no_jump_offset_saturates() {
            for profile in [
                SeccompProfile::Permissive,
                SeccompProfile::Default,
                SeccompProfile::Strict,
            ] {
                for (i, insn) in program(profile).unwrap().iter().enumerate() {
                    assert!(
                        insn.jt < u8::MAX && insn.jf < u8::MAX,
                        "{profile}: instruction {i} has a saturated jump"
                    );
                }
            }
        }

        /// Every jump must land inside the program: an out-of-range offset is
        /// rejected by the kernel's verifier, and a merely *wrong* one silently
        /// changes the policy.
        #[test]
        fn every_jump_lands_within_the_program() {
            for profile in [
                SeccompProfile::Permissive,
                SeccompProfile::Default,
                SeccompProfile::Strict,
            ] {
                let p = program(profile).unwrap();
                for (i, insn) in p.iter().enumerate() {
                    if insn.code & 0x07 != BPF_JMP {
                        continue;
                    }
                    let targets = if insn.code == BPF_JMP | BPF_JA {
                        vec![i + 1 + insn.k as usize]
                    } else {
                        vec![i + 1 + insn.jt as usize, i + 1 + insn.jf as usize]
                    };
                    for target in targets {
                        assert!(
                            target < p.len(),
                            "{profile}: instruction {i} jumps to {target}, past the end ({})",
                            p.len()
                        );
                    }
                }
            }
        }

        /// The last instruction of a BPF program must be a return: execution
        /// falling off the end is rejected by the verifier.
        #[test]
        fn the_program_ends_in_a_return() {
            for profile in [
                SeccompProfile::Permissive,
                SeccompProfile::Default,
                SeccompProfile::Strict,
            ] {
                let p = program(profile).unwrap();
                assert_eq!(p.last().unwrap().code, BPF_RET | BPF_K, "{profile}");
            }
        }
    }
}
