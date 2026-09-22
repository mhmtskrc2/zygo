//! Everything the child needs, allocated before `clone3`.
//!
//! After a fork-like clone the child may only perform async-signal-safe work:
//! the parent can be multi-threaded (the CLI runs a tokio runtime), and a child
//! that inherits one thread while another held the allocator's lock deadlocks
//! the moment it allocates. That rules out `String`, `Vec` growth, `format!`
//! and every error type that builds a message.
//!
//! So the plan is materialised here, in the parent, as `CString`s and raw
//! pointers. The child then does nothing but issue syscalls against them — and
//! reports failure as a `(step, errno)` pair rather than a message.

use std::ffi::{CString, NulError, OsStr};
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::sandbox::{MountOp, SandboxConfig};
use crate::spec::MountMode;

/// Which step of the child's sequence failed. Sent to the parent as a number,
/// turned back into a sentence there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum Step {
    WaitForIdMaps = 1,
    IdMapsRejected = 2,
    MakeRootPrivate = 3,
    MountRoot = 4,
    MountProc = 5,
    MountSys = 6,
    MountTmpfs = 7,
    MountBind = 8,
    MountDevNode = 9,
    MountDevPts = 10,
    RemountReadOnly = 11,
    MaskPath = 12,
    PivotRoot = 13,
    UnmountOldRoot = 14,
    SetHostname = 15,
    Chdir = 16,
    BringUpLoopback = 17,
    SetRlimit = 18,
    NoNewPrivs = 19,
    DropCapabilities = 20,
    AdoptTerminal = 21,
    PlaceAgentSocket = 26,
    ApplyLandlock = 22,
    InstallSeccomp = 23,
    ParentDied = 24,
    Execve = 25,
    /// Warm-exec: the helper could not enter the sandbox's namespaces.
    Setns = 27,
    /// Warm-exec: the request process could not enter the mount namespace.
    EnterMountNamespace = 28,
    /// Warm-exec: wiring the request's pipes onto stdin/stdout/stderr.
    WireStdio = 29,
    /// The sandbox could not create `/run/secrets` or hand its descriptor
    /// back. See `child::hand_out_secrets_dir`.
    HandOutSecretsDir = 30,
}

impl Step {
    pub fn from_u32(v: u32) -> Option<Step> {
        use Step::*;
        Some(match v {
            1 => WaitForIdMaps,
            2 => IdMapsRejected,
            3 => MakeRootPrivate,
            4 => MountRoot,
            5 => MountProc,
            6 => MountSys,
            7 => MountTmpfs,
            8 => MountBind,
            9 => MountDevNode,
            10 => MountDevPts,
            11 => RemountReadOnly,
            12 => MaskPath,
            13 => PivotRoot,
            14 => UnmountOldRoot,
            15 => SetHostname,
            16 => Chdir,
            17 => BringUpLoopback,
            18 => SetRlimit,
            19 => NoNewPrivs,
            20 => DropCapabilities,
            21 => AdoptTerminal,
            26 => PlaceAgentSocket,
            22 => ApplyLandlock,
            23 => InstallSeccomp,
            24 => ParentDied,
            25 => Execve,
            27 => Setns,
            28 => EnterMountNamespace,
            29 => WireStdio,
            30 => HandOutSecretsDir,
            _ => return None,
        })
    }

    /// What the user is told, plus what to do about it.
    pub fn describe(self) -> &'static str {
        use Step::*;
        match self {
            WaitForIdMaps => "waiting for the parent to write the id maps",
            IdMapsRejected => "the parent could not write uid_map/gid_map",
            MakeRootPrivate => "making the mount tree private (mount --make-rprivate /)",
            MountRoot => "mounting the image as the sandbox root",
            MountProc => "mounting /proc",
            MountSys => "mounting /sys",
            MountTmpfs => "mounting a tmpfs (/tmp, /run or /dev)",
            MountBind => "applying a bind mount from the spec",
            MountDevNode => "creating a device node in /dev",
            MountDevPts => "mounting /dev/pts",
            RemountReadOnly => "remounting a path read-only",
            MaskPath => "masking a path under /proc or /sys",
            PivotRoot => "pivot_root into the new root",
            UnmountOldRoot => "detaching the old root",
            SetHostname => "setting the sandbox hostname",
            Chdir => "entering the working directory",
            BringUpLoopback => "bringing up the loopback interface",
            SetRlimit => "applying an rlimit",
            NoNewPrivs => "setting PR_SET_NO_NEW_PRIVS",
            DropCapabilities => "dropping capabilities",
            AdoptTerminal => "adopting the sandbox's own terminal",
            PlaceAgentSocket => "placing the runtime agent's socket",
            ApplyLandlock => "applying the Landlock ruleset",
            InstallSeccomp => "installing the seccomp filter",
            ParentDied => "the launcher exited while the sandbox was starting",
            Execve => "executing the sandboxed program",
            Setns => "entering the sandbox's namespaces for a warm-exec request",
            EnterMountNamespace => "entering the sandbox's mount namespace",
            WireStdio => "wiring the request's pipes to stdin, stdout and stderr",
            HandOutSecretsDir => "creating /run/secrets and handing its descriptor back",
        }
    }

    /// Advice for the common failures, shown under the error.
    pub fn remedy(self, errno: i32) -> Option<&'static str> {
        use Step::*;
        match (self, errno) {
            (MountRoot, libc::EINVAL) => Some(
                "overlayfs inside a user namespace needs Linux 5.11+; \
                 run `zygo doctor` — the store can fall back to a flattened rootfs",
            ),
            (MountProc, libc::EPERM) => Some(
                "mounting a fresh /proc requires being *in* the new pid namespace; \
                 this is a launcher bug, please report it",
            ),
            (Execve, libc::ENOENT) => Some(
                "the program does not exist inside the image — check the command and the image",
            ),
            (Execve, libc::EACCES) => Some("the program is not executable inside the image"),
            (ApplyLandlock, libc::EINVAL) => Some(
                "the kernel rejected the Landlock ruleset; \
                 `zygo doctor` reports the ABI version available here",
            ),
            (InstallSeccomp, libc::EINVAL) => Some(
                "the kernel rejected the syscall filter; \
                 `zygo doctor` reports whether seccomp is available here",
            ),
            (_, libc::EPERM) | (_, libc::EACCES) => Some(
                "the kernel refused a sandbox primitive; run `zygo doctor` for the requirements",
            ),
            _ => None,
        }
    }
}

/// One mount, pre-rendered into the exact syscall arguments.
#[derive(Debug)]
pub enum PreparedOp {
    MakeRootPrivate,
    /// `mount -t overlay overlay -o lowerdir=… <target>`
    Overlay {
        target: CString,
        options: CString,
    },
    /// Recursive bind of a flattened rootfs, remounted read-only unless the
    /// config asked for a writable root (the derived-layer builder only).
    BindRoot {
        source: CString,
        target: CString,
        readonly: bool,
    },
    Tmpfs {
        target: CString,
        options: CString,
        flags: u64,
    },
    Bind {
        source: CString,
        target: CString,
        readonly: bool,
    },
    Proc {
        target: CString,
    },
    Sysfs {
        target: CString,
    },
    DevPts {
        target: CString,
        options: CString,
    },
    /// Bind the host's device node; `mknod` is unavailable to an unprivileged
    /// user namespace (confirmed by PoC 6).
    DevNode {
        source: CString,
        target: CString,
    },
    /// Hide a path: `/dev/null` over a file, an empty read-only tmpfs over a
    /// directory. Which one is decided in the child, after `/proc` exists.
    Mask {
        target: CString,
    },
    RemountReadOnly {
        target: CString,
    },
}

/// The fully materialised launch plan.
///
/// Deliberately not `Send`: it holds raw pointers into its own `CString`s and
/// is only ever used by the thread that built it and by that thread's clone.
pub struct PreparedLaunch {
    pub ops: Vec<PreparedOp>,
    pub newroot: CString,
    pub hostname: CString,
    pub workdir: CString,
    /// `(resource, value)` pairs for `setrlimit`.
    pub rlimits: Vec<(i32, u64)>,
    /// Bring `lo` up inside the new network namespace.
    pub bring_up_loopback: bool,
    pub drop_capabilities: bool,
    /// The seccomp program, compiled here because generating BPF allocates and
    /// the child may not.
    pub seccomp: Vec<super::seccomp::SockFilter>,
    /// The Landlock ruleset, empty when the kernel has no Landlock.
    pub landlock: super::landlock::Ruleset,
    /// Terminal for the sandbox's stdio, when one was allocated for it.
    pub stdio: Option<std::os::fd::RawFd>,
    /// Three distinct streams for stdin, stdout and stderr, when a caller
    /// handed its own over. Applied instead of `stdio` when both are set.
    pub stdio_streams: Option<[std::os::fd::RawFd; 3]>,
    /// Reset the program's signal dispositions to these: see
    /// [`SandboxConfig::ignored_signals`].
    pub ignored_signals: Option<u64>,
    /// The runtime agent's connected socket, to be placed at `AGENT_FD`.
    pub agent_fd: Option<std::os::fd::RawFd>,

    /// A socket to send `/run/secrets`'s descriptor back on, for a held
    /// sandbox.
    ///
    /// The supervisor cannot reach that directory by path: `/proc/<pid>/root`
    /// is traversable only while the process is dumpable, and writing the id
    /// map cleared that for everything in this sandbox before the mounts even
    /// began. So the child opens it and hands it out while it still can.
    pub secrets_fd: Option<std::os::fd::RawFd>,
    /// Hold the sandbox open instead of running the program (warm-exec).
    pub hold: bool,

    /// Absolute paths to try, in order. More than one when the command had no
    /// `/` and has to be looked up along `PATH`.
    pub program_candidates: Vec<CString>,
    /// Owns the strings the pointer arrays below point at.
    _argv: Vec<CString>,
    _envp: Vec<CString>,
    argv_ptrs: Vec<*const c_char>,
    envp_ptrs: Vec<*const c_char>,
}

// SAFETY: the raw pointers in `argv_ptrs` and `envp_ptrs` point into the heap
// buffers of `_argv` and `_envp`, which the struct owns and never mutates
// after construction. Moving the struct moves the `Vec` headers, not the
// buffers, so the pointers stay valid; sharing it between threads shares only
// reads. The supervisor keeps one per warm-exec function and uses it from
// whichever thread a request arrives on.
unsafe impl Send for PreparedLaunch {}
unsafe impl Sync for PreparedLaunch {}

impl PreparedLaunch {
    pub fn argv(&self) -> *const *const c_char {
        self.argv_ptrs.as_ptr()
    }

    pub fn envp(&self) -> *const *const c_char {
        self.envp_ptrs.as_ptr()
    }
}

impl std::fmt::Debug for PreparedLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedLaunch")
            .field("program", &self.program_candidates)
            .field("ops", &self.ops.len())
            .field("newroot", &self.newroot)
            .finish_non_exhaustive()
    }
}

// Mount flags. Spelled out rather than pulled from `libc` so the plan reads
// like the design document.
const MS_RDONLY: u64 = 1;
const MS_NOSUID: u64 = 2;
const MS_NODEV: u64 = 4;
const MS_NOEXEC: u64 = 8;
pub const MS_REMOUNT: u64 = 32;
pub const MS_BIND: u64 = 4096;
pub const MS_REC: u64 = 16384;
pub const MS_PRIVATE: u64 = 262144;

#[derive(Debug, thiserror::Error)]
pub enum PrepareError {
    #[error("path contains an interior NUL byte: {0}")]
    Nul(#[from] NulError),
    #[error("{0} is required but missing from the launch config")]
    Missing(&'static str),

    #[error(transparent)]
    Seccomp(#[from] super::seccomp::SeccompError),
}

fn cstr(path: &Path) -> Result<CString, PrepareError> {
    Ok(CString::new(path.as_os_str().as_bytes())?)
}

fn cstr_str(s: &str) -> Result<CString, PrepareError> {
    Ok(CString::new(s)?)
}

/// Render a [`SandboxConfig`] into something the child can execute without
/// allocating.
pub fn prepare(config: &SandboxConfig) -> Result<PreparedLaunch, PrepareError> {
    let mut ops = Vec::with_capacity(config.mounts.ops.len());

    for op in &config.mounts.ops {
        ops.push(match op {
            MountOp::MakeRootPrivate => PreparedOp::MakeRootPrivate,

            MountOp::Overlay { lower, target } => {
                let joined: Vec<&OsStr> = lower.iter().map(|p| p.as_os_str()).collect();
                let mut options = b"lowerdir=".to_vec();
                for (i, part) in joined.iter().enumerate() {
                    if i > 0 {
                        options.push(b':');
                    }
                    options.extend_from_slice(part.as_bytes());
                }
                PreparedOp::Overlay {
                    target: cstr(target)?,
                    options: CString::new(options)?,
                }
            }

            MountOp::BindRoot { source, target } => PreparedOp::BindRoot {
                source: cstr(source)?,
                target: cstr(target)?,
                readonly: !config.writable_root,
            },

            MountOp::Tmpfs {
                target,
                options,
                flags,
            } => PreparedOp::Tmpfs {
                target: cstr(target)?,
                options: cstr_str(options)?,
                flags: mount_flags(*flags),
            },

            MountOp::Bind {
                source,
                target,
                mode,
            } => PreparedOp::Bind {
                source: cstr(source)?,
                target: cstr(target)?,
                readonly: *mode == MountMode::Ro,
            },

            MountOp::Proc { target } => PreparedOp::Proc {
                target: cstr(target)?,
            },

            MountOp::Sysfs { target } => PreparedOp::Sysfs {
                target: cstr(target)?,
            },

            MountOp::DevPts { target } => PreparedOp::DevPts {
                target: cstr(target)?,
                // `newinstance` keeps the sandbox's ptys out of the host's
                // devpts, so a tenant cannot see another's terminal.
                options: cstr_str("newinstance,ptmxmode=0666,mode=0620")?,
            },

            MountOp::DevNode { name } => PreparedOp::DevNode {
                source: cstr_str(&format!("/dev/{name}"))?,
                target: cstr(&config.mounts.newroot.join("dev").join(name))?,
            },

            MountOp::Mask { target } => PreparedOp::Mask {
                target: cstr(target)?,
            },

            MountOp::RemountReadOnly { target } => PreparedOp::RemountReadOnly {
                target: cstr(target)?,
            },
        });
    }

    // rlimits, from the resolved limits.
    let rlimits = config
        .limits
        .rlimits()
        .into_iter()
        .map(|(kind, value)| (rlimit_resource(kind), value))
        .collect();

    // `execve` takes a path, not a command name: `zygo run python:3.12 python3`
    // has to become `/usr/local/bin/python3`. The search cannot happen in the
    // child — `execvp` allocates — so the candidates are built here and the
    // child simply tries them in order.
    let program = config.argv.first().ok_or(PrepareError::Missing("argv"))?;
    let program_candidates = exec_candidates(program, &config.env)?;
    let argv: Vec<CString> = config
        .argv
        .iter()
        .map(|a| cstr_str(a))
        .collect::<Result<_, _>>()?;
    let envp: Vec<CString> = config
        .env
        .iter()
        .map(|(k, v)| cstr_str(&format!("{k}={v}")))
        .collect::<Result<_, _>>()?;

    let mut argv_ptrs: Vec<*const c_char> = argv.iter().map(|s| s.as_ptr()).collect();
    argv_ptrs.push(core::ptr::null());
    let mut envp_ptrs: Vec<*const c_char> = envp.iter().map(|s| s.as_ptr()).collect();
    envp_ptrs.push(core::ptr::null());

    Ok(PreparedLaunch {
        ops,
        newroot: cstr(&config.mounts.newroot)?,
        // The hostname is the tenant name: `hostname` inside the sandbox should
        // say something useful, and it leaks nothing the tenant does not know.
        hostname: cstr_str(&config.id.tenant)?,
        workdir: cstr(&config.workdir)?,
        rlimits,
        bring_up_loopback: config.network != crate::spec::Network::Host,
        drop_capabilities: true,
        seccomp: super::seccomp::program(config.seccomp)?,
        stdio: config.stdio,
        stdio_streams: config.stdio_streams,
        ignored_signals: config.ignored_signals,
        agent_fd: config.agent_fd,
        secrets_fd: None,
        hold: config.hold,
        landlock: super::landlock::build(
            super::landlock::abi_version(),
            &config.mounts,
            super::landlock::NetPolicy::of(config.network, &config.allow),
            config.writable_root,
        ),
        program_candidates,
        _argv: argv,
        _envp: envp,
        argv_ptrs,
        envp_ptrs,
    })
}

/// Translate the plan's abstract flags into the kernel's `MS_*` bits.
fn mount_flags(abstract_flags: u64) -> u64 {
    use crate::sandbox::mount::flags;
    let mut out = 0;
    for (bit, ms) in [
        (flags::NOSUID, MS_NOSUID),
        (flags::NODEV, MS_NODEV),
        (flags::NOEXEC, MS_NOEXEC),
        (flags::RDONLY, MS_RDONLY),
    ] {
        if abstract_flags & bit != 0 {
            out |= ms;
        }
    }
    out
}

/// Docker's default `PATH`, used when the image config sets none.
use crate::sandbox::DEFAULT_PATH;

/// Absolute paths to try for `program`, in order.
///
/// A name containing `/` is taken literally, exactly as `execve` and every
/// shell treat it. Otherwise it is joined onto each `PATH` entry — the search
/// `execvp` would do, but done here where allocating is still allowed.
fn exec_candidates(program: &str, env: &[(String, String)]) -> Result<Vec<CString>, PrepareError> {
    if program.contains('/') {
        return Ok(vec![cstr_str(program)?]);
    }
    let path = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str())
        .unwrap_or(DEFAULT_PATH);

    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| cstr_str(&format!("{}/{program}", dir.trim_end_matches('/'))))
        .collect()
}

// `RLIMIT_*` is `c_int` under musl and `c_uint` under glibc, so exactly one
// of the two libcs makes each cast below redundant — and clippy, run against
// that one, calls it an error. The cast stays.
#[allow(clippy::unnecessary_cast)]
fn rlimit_resource(kind: crate::sandbox::RlimitKind) -> i32 {
    use crate::sandbox::RlimitKind::*;
    match kind {
        NoFile => libc::RLIMIT_NOFILE as i32,
        FSize => libc::RLIMIT_FSIZE as i32,
        Core => libc::RLIMIT_CORE as i32,
        NProc => libc::RLIMIT_NPROC as i32,
    }
}

/// Mount flag set for a read-only bind remount.
pub const READONLY_REMOUNT: u64 = MS_REMOUNT | MS_BIND | MS_RDONLY | MS_NOSUID | MS_NODEV;
/// Flags for the root mount.
pub const ROOT_FLAGS: u64 = MS_RDONLY | MS_NOSUID | MS_NODEV;
/// Flags for `/proc` and `/sys`.
pub const PSEUDO_FLAGS: u64 = MS_NOSUID | MS_NODEV | MS_NOEXEC;
/// Flags for a read-only `/sys`.
pub const SYSFS_FLAGS: u64 = MS_RDONLY | MS_NOSUID | MS_NODEV | MS_NOEXEC;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::RootfsView;
    use crate::spec::{Layer, ResolveOptions, resolve_standalone};
    use std::path::PathBuf;

    fn config(view: RootfsView) -> SandboxConfig {
        let f = resolve_standalone(
            "demo",
            &Layer {
                image: Some("alpine".into()),
                cmd: Some(vec!["/bin/echo".into(), "hi".into()]),
                mounts: Some(vec!["/host/data:/data:rw".parse().unwrap()]),
                ..Default::default()
            },
            &ResolveOptions::default(),
        )
        .unwrap();
        SandboxConfig::from_resolved(
            &f,
            &view,
            "/newroot",
            vec!["/bin/echo".into(), "hi".into()],
            &[],
        )
    }

    fn overlay() -> RootfsView {
        RootfsView::Overlay {
            lower: vec![PathBuf::from("/layers/b"), PathBuf::from("/layers/a")],
        }
    }

    #[test]
    fn every_mount_op_is_prepared() {
        let cfg = config(overlay());
        let p = prepare(&cfg).unwrap();
        assert_eq!(p.ops.len(), cfg.mounts.ops.len());
        assert!(matches!(p.ops[0], PreparedOp::MakeRootPrivate));
    }

    #[test]
    fn overlay_options_are_colon_joined_top_layer_first() {
        let p = prepare(&config(overlay())).unwrap();
        let options = p
            .ops
            .iter()
            .find_map(|op| match op {
                PreparedOp::Overlay { options, .. } => Some(options.to_str().unwrap()),
                _ => None,
            })
            .unwrap();
        assert_eq!(options, "lowerdir=/layers/b:/layers/a");
    }

    #[test]
    fn a_flat_rootfs_becomes_a_bind() {
        let view = RootfsView::Flat {
            dir: PathBuf::from("/flat/abc"),
        };
        let p = prepare(&config(view)).unwrap();
        assert!(
            p.ops
                .iter()
                .any(|op| matches!(op, PreparedOp::BindRoot { .. }))
        );
    }

    #[test]
    fn device_nodes_bind_the_hosts_rather_than_mknod() {
        let p = prepare(&config(overlay())).unwrap();
        let null = p
            .ops
            .iter()
            .find_map(|op| match op {
                PreparedOp::DevNode { source, target } if source.to_bytes() == b"/dev/null" => {
                    Some(target.to_str().unwrap())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(null, "/newroot/dev/null");
    }

    #[test]
    fn writable_binds_keep_their_mode() {
        let p = prepare(&config(overlay())).unwrap();
        let rw = p
            .ops
            .iter()
            .find_map(|op| match op {
                PreparedOp::Bind {
                    target, readonly, ..
                } if target.to_bytes().ends_with(b"/data") => Some(*readonly),
                _ => None,
            })
            .unwrap();
        assert!(!rw, "a `:rw` mount must not be prepared read-only");
    }

    #[test]
    fn abstract_flags_translate_to_kernel_bits() {
        use crate::sandbox::mount::flags;
        assert_eq!(mount_flags(flags::NOSUID), MS_NOSUID);
        assert_eq!(
            mount_flags(flags::NOSUID | flags::NOEXEC),
            MS_NOSUID | MS_NOEXEC
        );
        assert_eq!(mount_flags(0), 0);
    }

    /// tmpfs only understands `size`, `nr_inodes` and `mode` in its option
    /// string; a flag word smuggled in there makes the mount fail with EINVAL,
    /// which is exactly how this was found on a real kernel.
    #[test]
    fn tmpfs_options_never_contain_mount_flags() {
        let p = prepare(&config(overlay())).unwrap();
        for op in &p.ops {
            if let PreparedOp::Tmpfs {
                options, target, ..
            } = op
            {
                let text = options.to_str().unwrap();
                for flag in ["nosuid", "nodev", "noexec", "ro,", "rw,"] {
                    assert!(
                        !text.contains(flag),
                        "`{flag}` is a mount flag, not a tmpfs option: {} has `{text}`",
                        target.to_str().unwrap()
                    );
                }
            }
        }
    }

    #[test]
    fn argv_and_envp_are_null_terminated() {
        let p = prepare(&config(overlay())).unwrap();
        assert_eq!(p.argv_ptrs.len(), 3, "two args plus the NULL");
        assert!(p.argv_ptrs.last().unwrap().is_null());
        assert!(p.envp_ptrs.last().unwrap().is_null());
        assert_eq!(p.program_candidates[0].to_str().unwrap(), "/bin/echo");
    }

    #[test]
    fn an_absolute_command_is_the_only_candidate() {
        let c = exec_candidates("/bin/echo", &[]).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].to_str().unwrap(), "/bin/echo");
        // A relative path with a slash is also taken literally, as execve does.
        let c = exec_candidates("./tool", &[]).unwrap();
        assert_eq!(c[0].to_str().unwrap(), "./tool");
    }

    /// `zygo run python:3.12 python3` has to find the interpreter the way
    /// `docker run` does, rather than failing with ENOENT.
    #[test]
    fn a_bare_command_is_searched_along_path() {
        let c = exec_candidates("python3", &[]).unwrap();
        let paths: Vec<&str> = c.iter().map(|s| s.to_str().unwrap()).collect();
        assert!(paths.contains(&"/usr/local/bin/python3"), "{paths:?}");
        assert!(paths.contains(&"/usr/bin/python3"), "{paths:?}");
        assert_eq!(paths.len(), DEFAULT_PATH.split(':').count());
    }

    #[test]
    fn the_images_own_path_is_honoured_when_it_sets_one() {
        let env = [("PATH".to_string(), "/opt/bin:/bin/".to_string())];
        let c = exec_candidates("tool", &env).unwrap();
        let paths: Vec<&str> = c.iter().map(|s| s.to_str().unwrap()).collect();
        assert_eq!(paths, ["/opt/bin/tool", "/bin/tool"]);
    }

    #[test]
    fn the_environment_is_rendered_as_key_equals_value() {
        let p = prepare(&config(overlay())).unwrap();
        let entries: Vec<&str> = p._envp.iter().map(|c| c.to_str().unwrap()).collect();
        assert!(
            entries.iter().any(|e| e.starts_with("ZYGO_TENANT=")),
            "{entries:?}"
        );
        assert!(entries.iter().all(|e| e.contains('=')));
    }

    // Same libc difference as `rlimit_resource` itself: see the note there.
    #[allow(clippy::unnecessary_cast)]
    #[test]
    fn rlimits_map_onto_kernel_resource_numbers() {
        let p = prepare(&config(overlay())).unwrap();
        assert!(
            p.rlimits
                .iter()
                .any(|(r, v)| *r == libc::RLIMIT_NOFILE as i32 && *v == 1024)
        );
        assert!(
            p.rlimits
                .iter()
                .any(|(r, v)| *r == libc::RLIMIT_CORE as i32 && *v == 0)
        );
        assert!(
            p.rlimits
                .iter()
                .any(|(r, v)| *r == libc::RLIMIT_NPROC as i32 && *v == 64)
        );
    }

    #[test]
    fn a_path_with_a_nul_byte_is_rejected_rather_than_truncated() {
        assert!(cstr(Path::new("/a\0b")).is_err());
    }

    #[test]
    fn steps_round_trip_and_describe_themselves() {
        for raw in (1..=26u32).filter(|n| *n != 0) {
            let step = Step::from_u32(raw).unwrap_or_else(|| panic!("step {raw} unmapped"));
            assert_eq!(step as u32, raw);
            assert!(!step.describe().is_empty());
        }
        assert_eq!(Step::from_u32(0), None);
        assert_eq!(Step::from_u32(99), None);
    }

    #[test]
    fn remedies_point_at_the_real_cause() {
        assert!(
            Step::MountRoot
                .remedy(libc::EINVAL)
                .unwrap()
                .contains("5.11")
        );
        assert!(
            Step::Execve
                .remedy(libc::ENOENT)
                .unwrap()
                .contains("does not exist inside the image")
        );
        assert!(Step::Chdir.remedy(libc::ENOENT).is_none());
    }

    #[test]
    fn loopback_is_brought_up_except_with_host_networking() {
        let mut cfg = config(overlay());
        assert!(prepare(&cfg).unwrap().bring_up_loopback);
        cfg.network = crate::spec::Network::Host;
        assert!(!prepare(&cfg).unwrap().bring_up_loopback);
    }
}
