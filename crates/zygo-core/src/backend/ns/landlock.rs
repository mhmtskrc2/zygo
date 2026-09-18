//! Landlock filesystem and network restriction (design doc §3.3 step 6, §3.8).
//!
//! Landlock is *defence in depth*, not the boundary. The boundary is the mount
//! namespace: a sandbox cannot name a path that was never mounted into it.
//! Landlock adds a second, independent answer to "may this process touch this
//! object?", so a bug in the mount plan — a bind mount that should have been
//! read-only, a `/proc` mask that did not apply — does not immediately become a
//! filesystem escape.
//!
//! It is also **optional by kernel version**: v1 needs Linux 5.13, network
//! rules need 6.7, `ioctl` restriction needs 6.10. A kernel without it runs the
//! sandbox with everything else intact, and `zygo doctor` says so rather than
//! the launcher pretending. That degradation is the part that can be tested on
//! an older kernel; the restriction itself cannot.
//!
//! The rule set is computed as pure data from the mount plan — the same source
//! of truth the mounts come from, so the two cannot disagree about which paths
//! are writable.

use std::ffi::CString;
use std::path::Path;

use crate::sandbox::MountPlan;
use crate::spec::Network;

// Access rights, ABI v1 (Linux 5.13).
pub const ACCESS_FS_EXECUTE: u64 = 1 << 0;
pub const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
pub const ACCESS_FS_READ_FILE: u64 = 1 << 2;
pub const ACCESS_FS_READ_DIR: u64 = 1 << 3;
pub const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
pub const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
pub const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
pub const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
pub const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
pub const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
pub const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
pub const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
pub const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
/// ABI v2 (6.2): linking and renaming across directories.
pub const ACCESS_FS_REFER: u64 = 1 << 13;
/// ABI v3 (6.2): `truncate`.
pub const ACCESS_FS_TRUNCATE: u64 = 1 << 14;
/// ABI v5 (6.10): `ioctl` on device files.
pub const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15;

/// ABI v4 (6.7): network access rights.
pub const ACCESS_NET_BIND_TCP: u64 = 1 << 0;
pub const ACCESS_NET_CONNECT_TCP: u64 = 1 << 1;

/// Everything a given ABI version knows how to restrict.
///
/// Asking the kernel to handle a right it does not know returns `EINVAL` and
/// the whole ruleset is refused, so this has to be exact rather than optimistic.
pub fn handled_fs_access(abi: u32) -> u64 {
    let mut access = ACCESS_FS_EXECUTE
        | ACCESS_FS_WRITE_FILE
        | ACCESS_FS_READ_FILE
        | ACCESS_FS_READ_DIR
        | ACCESS_FS_REMOVE_DIR
        | ACCESS_FS_REMOVE_FILE
        | ACCESS_FS_MAKE_CHAR
        | ACCESS_FS_MAKE_DIR
        | ACCESS_FS_MAKE_REG
        | ACCESS_FS_MAKE_SOCK
        | ACCESS_FS_MAKE_FIFO
        | ACCESS_FS_MAKE_BLOCK
        | ACCESS_FS_MAKE_SYM;
    if abi >= 2 {
        access |= ACCESS_FS_REFER;
    }
    if abi >= 3 {
        access |= ACCESS_FS_TRUNCATE;
    }
    if abi >= 5 {
        access |= ACCESS_FS_IOCTL_DEV;
    }
    access
}

/// Network rights a given ABI can restrict. Zero before v4.
pub fn handled_net_access(abi: u32) -> u64 {
    if abi >= 4 {
        ACCESS_NET_BIND_TCP | ACCESS_NET_CONNECT_TCP
    } else {
        0
    }
}

/// Rights granted on a read-only path: look, read, execute. Nothing that
/// changes anything.
pub fn read_only_rights(abi: u32) -> u64 {
    let mut rights = ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR;
    if abi >= 5 {
        // Reading a device node is useless without ioctl on it; `/dev/tty` and
        // `/dev/urandom` both need it.
        rights |= ACCESS_FS_IOCTL_DEV;
    }
    rights
}

/// Rights granted on a writable path: everything except making device nodes.
///
/// `MAKE_CHAR` and `MAKE_BLOCK` stay out on purpose — a sandbox that can create
/// a device node in its own writable scratch can reach hardware the mount plan
/// deliberately kept away from it.
pub fn read_write_rights(abi: u32) -> u64 {
    let mut rights = read_only_rights(abi)
        | ACCESS_FS_WRITE_FILE
        | ACCESS_FS_REMOVE_DIR
        | ACCESS_FS_REMOVE_FILE
        | ACCESS_FS_MAKE_DIR
        | ACCESS_FS_MAKE_REG
        | ACCESS_FS_MAKE_SOCK
        | ACCESS_FS_MAKE_FIFO
        | ACCESS_FS_MAKE_SYM;
    if abi >= 2 {
        rights |= ACCESS_FS_REFER;
    }
    if abi >= 3 {
        rights |= ACCESS_FS_TRUNCATE;
    }
    rights
}

/// One `LANDLOCK_RULE_PATH_BENEATH` rule: a path, and what is allowed under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRule {
    /// Path *inside* the sandbox, opened after `pivot_root`.
    pub path: CString,
    pub rights: u64,
}

/// The whole ruleset, materialised before `clone3`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruleset {
    pub abi: u32,
    pub handled_fs: u64,
    pub handled_net: u64,
    pub rules: Vec<PathRule>,
}

impl Ruleset {
    /// Whether there is anything to apply. An unavailable ABI produces none.
    pub fn is_empty(&self) -> bool {
        self.abi == 0
    }
}

/// Build the ruleset for a sandbox.
///
/// Read-only on `/` — the whole image — and read-write on exactly the paths the
/// mount plan made writable. Deriving the writable set from the plan rather
/// than restating it is the point: a mount added later is covered automatically,
/// and the two can never disagree.
///
/// `abi == 0` means the kernel has no Landlock; the result is empty and the
/// caller skips it.
pub fn build(abi: u32, plan: &MountPlan, network: Network) -> Ruleset {
    if abi == 0 {
        return Ruleset {
            abi: 0,
            handled_fs: 0,
            handled_net: 0,
            rules: Vec::new(),
        };
    }

    let mut rules = vec![PathRule {
        path: CString::new("/").expect("`/` has no interior NUL"),
        rights: read_only_rights(abi),
    }];

    for target in plan.writable_targets() {
        // Targets are staged under `newroot`; inside the sandbox they are
        // absolute paths, which is what will be opened after `pivot_root`.
        let inside = inside_path(&plan.newroot, target);
        if inside == Path::new("/") {
            continue; // the root rule already covers it
        }
        if let Ok(path) = CString::new(inside.to_string_lossy().as_bytes()) {
            rules.push(PathRule {
                path,
                rights: read_write_rights(abi),
            });
        }
    }

    rules.sort_by(|a, b| a.path.cmp(&b.path));
    rules.dedup_by(|a, b| a.path == b.path);

    Ruleset {
        abi,
        handled_fs: handled_fs_access(abi),
        // Handling a right while adding no rule for it denies it outright, so
        // a sandbox with no network gets TCP refused at the process level too —
        // on top of having no interfaces to use (design doc §3.8).
        handled_net: match network {
            Network::None => handled_net_access(abi),
            // Egress and full still go through nftables and pasta; restricting
            // `connect` here would break them, and the allowlist lives there.
            _ => 0,
        },
        rules,
    }
}

/// Strip the staging prefix, leaving the path as the sandbox will see it.
fn inside_path(newroot: &Path, target: &Path) -> std::path::PathBuf {
    match target.strip_prefix(newroot) {
        Ok(rest) => Path::new("/").join(rest),
        Err(_) => target.to_path_buf(),
    }
}

// ---------------------------------------------------------------------------
// syscalls
// ---------------------------------------------------------------------------

const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;

const CREATE_RULESET_VERSION: libc::c_uint = 1;
const RULE_PATH_BENEATH: libc::c_uint = 1;

/// `struct landlock_ruleset_attr`.
///
/// The network field only exists from ABI v4; the size passed to the kernel
/// selects which layout is meant, so a v1 kernel is given 8 bytes and never
/// sees the second field.
#[repr(C)]
#[derive(Debug, Default)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

/// `struct landlock_path_beneath_attr`, which the kernel declares **packed**:
/// a `u64` immediately followed by an `s32`, 12 bytes rather than 16.
#[repr(C, packed)]
#[derive(Debug)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Supported Landlock ABI version, or 0 when the kernel has none.
///
/// Probed rather than derived from the kernel version: a distribution can
/// disable Landlock in its config, and a seccomp policy on the host can make
/// the syscall return `ENOSYS` on a kernel that has it.
pub fn abi_version() -> u32 {
    // SAFETY: the version query takes a null attribute pointer and zero size by
    // definition, and only reads a constant out of the kernel.
    let rc = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            core::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    if rc > 0 { rc as u32 } else { 0 }
}

/// Apply a ruleset to the calling process.
///
/// # Safety
///
/// Async-signal-safe: `open`, `close` and three `landlock_*` syscalls, with no
/// allocation, so it is callable between `clone3` and `execve`.
///
/// `PR_SET_NO_NEW_PRIVS` must already be set — the kernel refuses
/// `landlock_restrict_self` without it.
pub unsafe fn apply(ruleset: &Ruleset) -> Result<(), std::io::Error> {
    if ruleset.is_empty() {
        return Ok(());
    }

    let attr = RulesetAttr {
        handled_access_fs: ruleset.handled_fs,
        handled_access_net: ruleset.handled_net,
    };
    // Before ABI v4 the struct is one field long, and passing the larger size
    // is rejected.
    let attr_size = if ruleset.abi >= 4 {
        core::mem::size_of::<RulesetAttr>()
    } else {
        core::mem::size_of::<u64>()
    };

    let ruleset_fd = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &attr as *const RulesetAttr,
            attr_size,
            0usize,
        )
    };
    if ruleset_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ruleset_fd = ruleset_fd as libc::c_int;

    for rule in &ruleset.rules {
        // `O_PATH` opens the object without reading it, which is all a rule
        // needs and is permitted even where `open` for read would not be.
        let fd = unsafe { libc::open(rule.path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            // A path the image does not have needs no rule; skipping it is
            // strictly more restrictive than inventing one.
            continue;
        }

        let beneath = PathBeneathAttr {
            allowed_access: rule.rights,
            parent_fd: fd,
        };
        let rc = unsafe {
            libc::syscall(
                SYS_LANDLOCK_ADD_RULE,
                ruleset_fd,
                RULE_PATH_BENEATH,
                &beneath as *const PathBeneathAttr,
                0usize,
            )
        };
        unsafe { libc::close(fd) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(ruleset_fd) };
            return Err(err);
        }
    }

    let rc = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0usize) };
    unsafe { libc::close(ruleset_fd) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{RootfsView, limits::Limits, mount};
    use crate::spec::{Bytes, Cpu, Duration, Mount};
    use std::path::PathBuf;

    fn limits() -> Limits {
        Limits {
            mem: Bytes::from_mib(256),
            mem_high: Bytes::from_mib(230),
            swap: Bytes(0),
            oom_group: true,
            cpu: Cpu(1.0),
            pids: 64,
            timeout: Duration::from_secs(30),
            scratch: Bytes::from_mib(64),
            scratch_inodes: 10_000,
            io_read: None,
            io_write: None,
            nofile: 1024,
            fsize: Bytes::from_mib(64),
        }
    }

    fn plan(mounts: &[Mount]) -> MountPlan {
        let view = RootfsView::Flat {
            dir: PathBuf::from("/flat/x"),
        };
        mount::plan("/newroot", &view, &limits(), mounts)
    }

    #[test]
    fn an_unavailable_abi_produces_nothing_to_apply() {
        let r = build(0, &plan(&[]), Network::None);
        assert!(r.is_empty());
        assert!(r.rules.is_empty());
        assert_eq!(r.handled_fs, 0);
    }

    #[test]
    fn the_root_is_read_only_and_everything_writable_comes_from_the_mount_plan() {
        let mounts = vec![
            "/host/ro:/ro".parse::<Mount>().unwrap(),
            "/host/rw:/rw:rw".parse::<Mount>().unwrap(),
        ];
        let r = build(1, &plan(&mounts), Network::None);

        let rights = |p: &str| {
            r.rules
                .iter()
                .find(|rule| rule.path.to_str().unwrap() == p)
                .map(|rule| rule.rights)
        };

        assert_eq!(
            rights("/"),
            Some(read_only_rights(1)),
            "the image is read-only"
        );
        assert_eq!(
            rights("/rw"),
            Some(read_write_rights(1)),
            "an rw mount is writable"
        );
        assert_eq!(
            rights("/ro"),
            None,
            "a ro mount needs no rule beyond the root's"
        );
        // The tmpfs scratch areas are writable and come from the plan, not from
        // a second list that could fall out of step with it.
        assert_eq!(rights("/tmp"), Some(read_write_rights(1)));
        assert_eq!(rights("/run"), Some(read_write_rights(1)));
        assert_eq!(rights("/dev/shm"), Some(read_write_rights(1)));
    }

    #[test]
    fn writable_paths_are_named_as_the_sandbox_sees_them() {
        let r = build(1, &plan(&[]), Network::None);
        for rule in &r.rules {
            let path = rule.path.to_str().unwrap();
            assert!(path.starts_with('/'), "{path} is not absolute");
            assert!(
                !path.starts_with("/newroot"),
                "{path} still carries the staging prefix"
            );
        }
    }

    #[test]
    fn rules_are_sorted_and_unique() {
        let r = build(1, &plan(&[]), Network::None);
        let mut sorted = r.rules.clone();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(r.rules, sorted);

        let mut paths: Vec<&CStr2> = Vec::new();
        type CStr2 = CString;
        for rule in &r.rules {
            assert!(
                !paths.contains(&&rule.path),
                "{:?} appears twice",
                rule.path
            );
            paths.push(&rule.path);
        }
    }

    /// Asking the kernel to handle a right it does not know returns EINVAL and
    /// the whole ruleset is refused — so the mask has to match the ABI exactly.
    #[test]
    fn handled_rights_grow_with_the_abi_and_never_shrink() {
        let v1 = handled_fs_access(1);
        let v2 = handled_fs_access(2);
        let v3 = handled_fs_access(3);
        let v5 = handled_fs_access(5);

        assert_eq!(v1 & ACCESS_FS_REFER, 0, "REFER is ABI v2");
        assert_ne!(v2 & ACCESS_FS_REFER, 0);
        assert_eq!(v2 & ACCESS_FS_TRUNCATE, 0, "TRUNCATE is ABI v3");
        assert_ne!(v3 & ACCESS_FS_TRUNCATE, 0);
        assert_eq!(v3 & ACCESS_FS_IOCTL_DEV, 0, "IOCTL_DEV is ABI v5");
        assert_ne!(v5 & ACCESS_FS_IOCTL_DEV, 0);

        // Monotonic: a newer ABI handles everything an older one did.
        for pair in [(v1, v2), (v2, v3), (v3, v5)] {
            assert_eq!(
                pair.0 & pair.1,
                pair.0,
                "ABI regression: {:x} -> {:x}",
                pair.0,
                pair.1
            );
        }
    }

    #[test]
    fn network_rights_appear_only_from_abi_v4() {
        assert_eq!(handled_net_access(3), 0);
        assert_eq!(
            handled_net_access(4),
            ACCESS_NET_BIND_TCP | ACCESS_NET_CONNECT_TCP
        );
    }

    /// Handling a network right while adding no rule for it denies it. That is
    /// wanted for `network = "none"` and wrong for egress, where the allowlist
    /// lives in nftables and pasta.
    #[test]
    fn tcp_is_denied_only_when_the_sandbox_has_no_network_at_all() {
        let none = build(4, &plan(&[]), Network::None);
        assert_ne!(
            none.handled_net, 0,
            "a no-network sandbox should not reach TCP"
        );

        for network in [Network::Egress, Network::Full, Network::Host] {
            let r = build(4, &plan(&[]), network);
            assert_eq!(
                r.handled_net, 0,
                "{network} routes through the network policy, not Landlock"
            );
        }
    }

    #[test]
    fn a_sandbox_cannot_create_device_nodes_even_where_it_can_write() {
        let rw = read_write_rights(5);
        assert_eq!(
            rw & ACCESS_FS_MAKE_CHAR,
            0,
            "MAKE_CHAR would reach hardware"
        );
        assert_eq!(rw & ACCESS_FS_MAKE_BLOCK, 0);
        // But ordinary file creation has to work, or /tmp is useless.
        assert_ne!(rw & ACCESS_FS_MAKE_REG, 0);
        assert_ne!(rw & ACCESS_FS_MAKE_DIR, 0);
    }

    #[test]
    fn read_only_rights_permit_nothing_that_changes_anything() {
        let ro = read_only_rights(5);
        for (name, bit) in [
            ("WRITE_FILE", ACCESS_FS_WRITE_FILE),
            ("REMOVE_FILE", ACCESS_FS_REMOVE_FILE),
            ("REMOVE_DIR", ACCESS_FS_REMOVE_DIR),
            ("MAKE_REG", ACCESS_FS_MAKE_REG),
            ("MAKE_DIR", ACCESS_FS_MAKE_DIR),
            ("TRUNCATE", ACCESS_FS_TRUNCATE),
        ] {
            assert_eq!(ro & bit, 0, "a read-only path must not grant {name}");
        }
        assert_ne!(ro & ACCESS_FS_READ_FILE, 0);
        assert_ne!(ro & ACCESS_FS_EXECUTE, 0, "the image's binaries must run");
    }

    /// The kernel declares this struct packed; a 16-byte version would make the
    /// kernel read the fd from padding.
    #[test]
    fn path_beneath_attr_is_packed() {
        assert_eq!(core::mem::size_of::<PathBeneathAttr>(), 12);
        assert_eq!(core::mem::size_of::<RulesetAttr>(), 16);
    }

    /// The probe returning 0 is not by itself evidence that the syscall was
    /// wired up correctly — a wrong syscall number would also "return 0". On a
    /// kernel without Landlock the errno must be exactly `ENOSYS`: anything
    /// else means the number reaches a *different* syscall, which would be a
    /// silent failure to restrict anything.
    #[test]
    fn the_abi_probe_either_reports_a_version_or_fails_with_enosys() {
        let abi = abi_version();
        if abi > 0 {
            assert!(abi <= 16, "implausible Landlock ABI {abi}");
            return;
        }
        let errno = std::io::Error::last_os_error().raw_os_error();
        assert_eq!(
            errno,
            Some(libc::ENOSYS),
            "landlock_create_ruleset(444) returned an unexpected errno; \
             on a kernel without Landlock it must be ENOSYS, and anything else \
             suggests the syscall number reaches something else"
        );
    }

    /// Whatever the kernel supports, building a ruleset must never panic and
    /// must never claim a restriction it cannot apply.
    #[test]
    fn building_against_this_hosts_real_abi_is_consistent() {
        let abi = abi_version();
        let r = build(abi, &plan(&[]), Network::None);
        assert_eq!(r.abi, abi);
        if abi == 0 {
            assert!(r.is_empty(), "no ABI means nothing may be claimed");
            assert!(r.rules.is_empty());
        } else {
            assert!(!r.rules.is_empty());
            assert_eq!(r.handled_fs, handled_fs_access(abi));
        }
    }

    #[test]
    fn staging_prefixes_are_stripped_but_absolute_paths_are_left_alone() {
        assert_eq!(
            inside_path(Path::new("/newroot"), Path::new("/newroot/tmp")),
            PathBuf::from("/tmp")
        );
        assert_eq!(
            inside_path(Path::new("/newroot"), Path::new("/newroot")),
            PathBuf::from("/")
        );
        assert_eq!(
            inside_path(Path::new("/newroot"), Path::new("/elsewhere")),
            PathBuf::from("/elsewhere")
        );
    }
}
