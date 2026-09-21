//! The mount plan (design doc §3.3, step 2).
//!
//! The plan is computed as pure data and only then applied, for three reasons:
//! it can be unit tested on a non-Linux host, it can be printed by
//! `zygo run --dry-run`, and the same plan can be translated into an OCI
//! `config.json` for the `gvisor` backend instead of being executed directly.

use std::path::{Path, PathBuf};

use super::limits::Limits;
use crate::spec::{Mount, MountMode};

/// How the image is presented as the sandbox root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootfsView {
    /// Image layers as overlayfs `lowerdir`s, no `upperdir` — a read-only root.
    /// Ordered top layer first, which is the order overlayfs expects.
    Overlay { lower: Vec<PathBuf> },
    /// Flattened single directory, bound read-only. The fallback for kernels
    /// that refuse overlayfs inside a user namespace (design doc R3).
    Flat { dir: PathBuf },
}

/// Mount flags, kept abstract so the plan stays platform independent; the
/// launcher translates them to `MS_*`.
///
/// These are genuinely *flags*, not filesystem options: passing `nosuid` in a
/// tmpfs option string makes the kernel reject the mount with EINVAL, because
/// tmpfs only understands `size`, `nr_inodes` and `mode` there.
pub mod flags {
    pub const NOSUID: u64 = 1 << 0;
    pub const NODEV: u64 = 1 << 1;
    pub const NOEXEC: u64 = 1 << 2;
    pub const RDONLY: u64 = 1 << 3;
}

/// A single step of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MountOp {
    /// `mount --make-rprivate /` — stop propagation back to the host.
    MakeRootPrivate,
    Overlay {
        lower: Vec<PathBuf>,
        target: PathBuf,
    },
    /// Read-only bind of the flattened rootfs.
    BindRoot { source: PathBuf, target: PathBuf },
    Tmpfs {
        target: PathBuf,
        /// Filesystem options only: `size`, `nr_inodes`, `mode`.
        options: String,
        /// `flags::*`, applied as mount flags rather than options.
        flags: u64,
    },
    Bind {
        source: PathBuf,
        target: PathBuf,
        mode: MountMode,
    },
    /// A fresh `proc` for the new pid namespace.
    Proc { target: PathBuf },
    /// Read-only `sysfs`.
    Sysfs { target: PathBuf },
    /// `devpts` for `/dev/pts`.
    DevPts { target: PathBuf },
    /// A device node created by bind-mounting the host's, so no `mknod`
    /// capability is needed in a user namespace.
    DevNode { name: &'static str },
    /// Hide a path by bind-mounting `/dev/null` (files) or an empty read-only
    /// tmpfs (directories) over it.
    Mask { target: PathBuf },
    /// Re-mount an existing mount read-only.
    RemountReadOnly { target: PathBuf },
}

/// The ordered plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountPlan {
    /// Directory the new root is assembled in, before `pivot_root`.
    pub newroot: PathBuf,
    pub ops: Vec<MountOp>,
}

/// Device nodes present in every sandbox (design doc §3.3: "minimal static
/// set"). The real `/dev` is never bound.
pub const DEV_NODES: &[&str] = &["null", "zero", "full", "random", "urandom", "tty"];

/// Paths under `/proc` and `/sys` that leak host state or allow host
/// modification. Bind-mounted over, following Docker's default set plus the
/// entries listed in design doc §3.3.
pub const MASKED_PATHS: &[&str] = &[
    "/proc/acpi",
    "/proc/asound",
    "/proc/kcore",
    "/proc/keys",
    "/proc/latency_stats",
    "/proc/sched_debug",
    "/proc/scsi",
    "/proc/timer_list",
    "/proc/timer_stats",
    "/sys/firmware",
    "/sys/devices/virtual/powercap",
];

/// Paths that must exist but must not be writable.
pub const READONLY_PATHS: &[&str] = &[
    "/proc/bus",
    "/proc/fs",
    "/proc/irq",
    "/proc/sys",
    "/proc/sysrq-trigger",
];

/// What has to exist at a mount point before something can be mounted over it.
///
/// A bind mount needs its target to be the *same kind of object* as its source:
/// mounting a file onto a directory fails with `ENOTDIR`. The sandbox root is
/// read-only, so the target cannot be created after the fact and the kind has
/// to be decided here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountPointKind {
    Directory,
    File,
}

/// A path that must exist inside the sandbox before the plan is applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountPoint {
    /// Absolute path inside the sandbox.
    pub path: PathBuf,
    pub kind: MountPointKind,
}

/// Everything the plan mounts over, as absolute paths inside the sandbox.
///
/// A read-only root cannot have anything created inside it, so these have to
/// exist before the root is assembled — the image store fills in whatever the
/// The paths the launcher mounts itself, which a spec may therefore not.
///
/// Exported because the spec's validator has to refuse the same set: it used
/// to carry its own shorter list — `/`, `/proc`, `/sys`, `/dev` — so a mount
/// onto `/tmp` or `/run` was accepted by the validator and then silently
/// mounted over by the launcher (B-19). One list, in the module that does the
/// mounting.
pub const MANAGED_TARGETS: &[&str] = &[
    "/proc", "/sys", "/dev", "/dev/pts", "/dev/shm", "/tmp", "/run",
];

/// image itself lacks. Derived from the same list the plan uses, so the two
/// cannot drift apart.
///
/// The kind of each spec mount follows its *source*: `./config.json:/app/cfg`
/// needs a file at `/app/cfg`, `./cache:/cache` needs a directory. A source
/// that does not exist yet is taken to be a directory, which is what
/// `docker run -v` does with one.
pub fn required_mount_points(mounts: &[Mount]) -> Vec<MountPoint> {
    let mut points: Vec<MountPoint> = MANAGED_TARGETS
        .iter()
        .map(|p| MountPoint {
            path: PathBuf::from(p),
            kind: MountPointKind::Directory,
        })
        .collect();

    points.extend(mounts.iter().map(|m| MountPoint {
        path: m.target.clone(),
        kind: if m.source.is_file() {
            MountPointKind::File
        } else {
            MountPointKind::Directory
        },
    }));
    points
}

/// Build the plan for one sandbox.
///
/// `newroot` is the staging directory the root is assembled in; the launcher
/// `pivot_root`s into it afterwards.
pub fn plan(
    newroot: impl Into<PathBuf>,
    rootfs: &RootfsView,
    limits: &Limits,
    mounts: &[Mount],
) -> MountPlan {
    let newroot = newroot.into();
    let inside = |p: &str| newroot.join(p.trim_start_matches('/'));
    let mut ops = Vec::new();

    // 1. Detach from host propagation before touching anything else, so no
    //    mount below can leak back out.
    ops.push(MountOp::MakeRootPrivate);

    // 2. The root view.
    match rootfs {
        RootfsView::Overlay { lower } => ops.push(MountOp::Overlay {
            lower: lower.clone(),
            target: newroot.clone(),
        }),
        RootfsView::Flat { dir } => ops.push(MountOp::BindRoot {
            source: dir.clone(),
            target: newroot.clone(),
        }),
    }

    // 3. Pseudo-filesystems. `/proc` is the new pid namespace's own.
    ops.push(MountOp::Proc {
        target: inside("/proc"),
    });
    ops.push(MountOp::Sysfs {
        target: inside("/sys"),
    });

    // 4. A synthetic `/dev`: a small tmpfs with bound device nodes. `nosuid`
    //    and `noexec` because nothing in `/dev` should ever be executed.
    ops.push(MountOp::Tmpfs {
        target: inside("/dev"),
        options: "size=64k,mode=755".to_string(),
        flags: flags::NOSUID | flags::NOEXEC,
    });
    for name in DEV_NODES {
        ops.push(MountOp::DevNode { name });
    }
    ops.push(MountOp::DevPts {
        target: inside("/dev/pts"),
    });
    // `/dev/shm` is billed to the memory cgroup like any other tmpfs, so it is
    // sized from the scratch budget rather than left at the kernel default of
    // half of RAM.
    ops.push(MountOp::Tmpfs {
        target: inside("/dev/shm"),
        options: format!("size={},mode=1777", limits.scratch.get() / 4),
        flags: flags::NOSUID | flags::NODEV | flags::NOEXEC,
    });

    // 5. Writable scratch.
    ops.push(MountOp::Tmpfs {
        target: inside("/tmp"),
        options: limits.scratch_mount_options(),
        flags: flags::NOSUID | flags::NODEV,
    });
    ops.push(MountOp::Tmpfs {
        target: inside("/run"),
        options: "size=1048576,mode=755".to_string(),
        flags: flags::NOSUID | flags::NODEV | flags::NOEXEC,
    });

    // 6. User bind mounts, parents before children so a nested mount is not
    //    shadowed by the one above it.
    let mut sorted: Vec<&Mount> = mounts.iter().collect();
    sorted.sort_by_key(|m| (depth(&m.target), m.target.clone()));
    for m in sorted {
        ops.push(MountOp::Bind {
            source: m.source.clone(),
            target: inside(&m.target.to_string_lossy()),
            mode: m.mode,
        });
    }

    // 7. Masking last: it must survive anything mounted above.
    for p in MASKED_PATHS {
        ops.push(MountOp::Mask { target: inside(p) });
    }
    for p in READONLY_PATHS {
        ops.push(MountOp::RemountReadOnly { target: inside(p) });
    }

    MountPlan { newroot, ops }
}

/// Render mount flags for `--dry-run`.
fn describe_flags(value: u64) -> String {
    let mut out = String::new();
    for (bit, name) in [
        (flags::NOSUID, "nosuid"),
        (flags::NODEV, "nodev"),
        (flags::NOEXEC, "noexec"),
        (flags::RDONLY, "ro"),
    ] {
        if value & bit != 0 {
            out.push(',');
            out.push_str(name);
        }
    }
    out
}

fn depth(p: &Path) -> usize {
    p.components().count()
}

impl MountPlan {
    /// Human-readable rendering for `--dry-run` and for error messages.
    pub fn describe(&self) -> Vec<String> {
        self.ops
            .iter()
            .map(|op| match op {
                MountOp::MakeRootPrivate => "make-rprivate /".to_string(),
                MountOp::Overlay { lower, target } => format!(
                    "overlay ro {} -> {}",
                    lower
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(":"),
                    target.display()
                ),
                MountOp::BindRoot { source, target } => {
                    format!("bind ro {} -> {}", source.display(), target.display())
                }
                MountOp::Tmpfs {
                    target,
                    options,
                    flags,
                } => format!(
                    "tmpfs {} ({options}{})",
                    target.display(),
                    describe_flags(*flags)
                ),
                MountOp::Bind {
                    source,
                    target,
                    mode,
                } => format!(
                    "bind {} {} -> {}",
                    match mode {
                        MountMode::Ro => "ro",
                        MountMode::Rw => "rw",
                    },
                    source.display(),
                    target.display()
                ),
                MountOp::Proc { target } => format!("proc {}", target.display()),
                MountOp::Sysfs { target } => format!("sysfs ro {}", target.display()),
                MountOp::DevPts { target } => format!("devpts {}", target.display()),
                MountOp::DevNode { name } => format!("dev node /dev/{name}"),
                MountOp::Mask { target } => format!("mask {}", target.display()),
                MountOp::RemountReadOnly { target } => {
                    format!("remount ro {}", target.display())
                }
            })
            .collect()
    }

    /// Every writable path in the plan. Used by tests and by `zygo doctor` to
    /// answer "what can this sandbox write to?" without reading the code.
    pub fn writable_targets(&self) -> Vec<&Path> {
        self.ops
            .iter()
            .filter_map(|op| match op {
                MountOp::Tmpfs { target, .. } => Some(target.as_path()),
                MountOp::Bind {
                    target,
                    mode: MountMode::Rw,
                    ..
                } => Some(target.as_path()),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Bytes, Cpu, Duration};

    fn limits() -> Limits {
        Limits {
            mem: Bytes::from_mib(256),
            mem_high: Bytes::from_mib(230),
            swap: Bytes(0),
            oom_group: true,
            connections: 256,
            bandwidth: None,
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

    fn overlay() -> RootfsView {
        RootfsView::Overlay {
            lower: vec![
                PathBuf::from("/data/layers/b"),
                PathBuf::from("/data/layers/a"),
            ],
        }
    }

    #[test]
    fn propagation_is_cut_before_anything_is_mounted() {
        let p = plan("/newroot", &overlay(), &limits(), &[]);
        assert_eq!(p.ops[0], MountOp::MakeRootPrivate);
    }

    #[test]
    fn the_root_is_overlay_without_an_upperdir() {
        let p = plan("/newroot", &overlay(), &limits(), &[]);
        match &p.ops[1] {
            MountOp::Overlay { lower, target } => {
                assert_eq!(lower.len(), 2);
                assert_eq!(target, Path::new("/newroot"));
            }
            other => panic!("expected an overlay root, got {other:?}"),
        }
    }

    #[test]
    fn the_flat_fallback_binds_a_single_directory() {
        let view = RootfsView::Flat {
            dir: PathBuf::from("/data/flat/abc"),
        };
        let p = plan("/newroot", &view, &limits(), &[]);
        assert!(matches!(p.ops[1], MountOp::BindRoot { .. }));
    }

    #[test]
    fn only_tmpfs_and_explicit_rw_binds_are_writable() {
        let mounts = vec![
            "/host/ro:/ro".parse::<Mount>().unwrap(),
            "/host/rw:/rw:rw".parse::<Mount>().unwrap(),
        ];
        let p = plan("/newroot", &overlay(), &limits(), &mounts);
        let w = p.writable_targets();

        assert!(w.contains(&Path::new("/newroot/tmp")));
        assert!(w.contains(&Path::new("/newroot/run")));
        assert!(w.contains(&Path::new("/newroot/dev/shm")));
        assert!(w.contains(&Path::new("/newroot/rw")));
        assert!(
            !w.contains(&Path::new("/newroot/ro")),
            "ro bind must not be writable"
        );
        // The root itself is never writable: no overlay upperdir exists.
        assert!(!w.contains(&Path::new("/newroot")));
    }

    #[test]
    fn scratch_size_reaches_the_tmp_tmpfs() {
        let p = plan("/newroot", &overlay(), &limits(), &[]);
        let tmp = p
            .ops
            .iter()
            .find_map(|op| match op {
                MountOp::Tmpfs {
                    target, options, ..
                } if target.ends_with("tmp") => Some(options),
                _ => None,
            })
            .unwrap();
        assert!(tmp.contains("size=67108864"), "{tmp}");
        assert!(tmp.contains("nr_inodes=10000"), "{tmp}");
        // `nosuid` is a flag, not a tmpfs option: passing it here is EINVAL.
        assert!(!tmp.contains("nosuid"), "{tmp}");
    }

    #[test]
    fn dev_is_synthetic_and_never_binds_the_host() {
        let p = plan("/newroot", &overlay(), &limits(), &[]);
        let nodes: Vec<&str> = p
            .ops
            .iter()
            .filter_map(|op| match op {
                MountOp::DevNode { name } => Some(*name),
                _ => None,
            })
            .collect();
        assert_eq!(nodes, DEV_NODES);

        let binds_real_dev = p
            .ops
            .iter()
            .any(|op| matches!(op, MountOp::Bind { source, .. } if source == Path::new("/dev")));
        assert!(!binds_real_dev);
    }

    #[test]
    fn proc_and_sys_are_masked_and_the_masks_come_last() {
        let p = plan("/newroot", &overlay(), &limits(), &[]);
        let masked: Vec<&Path> = p
            .ops
            .iter()
            .filter_map(|op| match op {
                MountOp::Mask { target } => Some(target.as_path()),
                _ => None,
            })
            .collect();
        assert!(masked.contains(&Path::new("/newroot/proc/kcore")));
        assert!(masked.contains(&Path::new("/newroot/sys/firmware")));

        let first_mask = p
            .ops
            .iter()
            .position(|op| matches!(op, MountOp::Mask { .. }))
            .unwrap();
        let last_bind = p
            .ops
            .iter()
            .rposition(|op| matches!(op, MountOp::Bind { .. } | MountOp::Tmpfs { .. }))
            .unwrap();
        assert!(
            first_mask > last_bind,
            "masks must be applied after every other mount"
        );
    }

    #[test]
    fn proc_sys_is_remounted_read_only() {
        let p = plan("/newroot", &overlay(), &limits(), &[]);
        let ro: Vec<&Path> = p
            .ops
            .iter()
            .filter_map(|op| match op {
                MountOp::RemountReadOnly { target } => Some(target.as_path()),
                _ => None,
            })
            .collect();
        assert!(ro.contains(&Path::new("/newroot/proc/sys")));
        assert!(ro.contains(&Path::new("/newroot/proc/sysrq-trigger")));
    }

    #[test]
    fn nested_binds_are_ordered_parent_first() {
        let mounts = vec![
            "/h/deep:/a/b/c:rw".parse::<Mount>().unwrap(),
            "/h/shallow:/a:rw".parse::<Mount>().unwrap(),
            "/h/mid:/a/b:rw".parse::<Mount>().unwrap(),
        ];
        let p = plan("/newroot", &overlay(), &limits(), &mounts);
        let order: Vec<PathBuf> = p
            .ops
            .iter()
            .filter_map(|op| match op {
                MountOp::Bind { target, .. } => Some(target.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            order,
            [
                PathBuf::from("/newroot/a"),
                PathBuf::from("/newroot/a/b"),
                PathBuf::from("/newroot/a/b/c"),
            ]
        );
    }

    fn point_kind(points: &[MountPoint], path: &str) -> Option<MountPointKind> {
        points
            .iter()
            .find(|p| p.path == Path::new(path))
            .map(|p| p.kind)
    }

    #[test]
    fn required_mount_points_cover_everything_the_plan_mounts() {
        let mounts = vec![
            "/host/a:/data:rw".parse::<Mount>().unwrap(),
            "/host/b:/srv/cache".parse::<Mount>().unwrap(),
        ];
        let points = required_mount_points(&mounts);

        for standard in [
            "/proc", "/sys", "/dev", "/dev/pts", "/dev/shm", "/tmp", "/run",
        ] {
            assert_eq!(
                point_kind(&points, standard),
                Some(MountPointKind::Directory),
                "{standard} missing or not a directory"
            );
        }
        assert!(point_kind(&points, "/data").is_some());
        assert!(point_kind(&points, "/srv/cache").is_some());
    }

    /// `./config.json:/app/config.json` has to land on a *file*: bind-mounting
    /// a file onto a directory fails with ENOTDIR, which is exactly what
    /// `zygo run` reported before the kind was tracked.
    #[test]
    fn a_file_source_needs_a_file_mount_point() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.json");
        std::fs::write(&file, b"{}").unwrap();
        let subdir = dir.path().join("cache");
        std::fs::create_dir(&subdir).unwrap();

        let mounts = vec![
            format!("{}:/app/config.json:ro", file.display())
                .parse::<Mount>()
                .unwrap(),
            format!("{}:/cache:rw", subdir.display())
                .parse::<Mount>()
                .unwrap(),
        ];
        let points = required_mount_points(&mounts);

        assert_eq!(
            point_kind(&points, "/app/config.json"),
            Some(MountPointKind::File)
        );
        assert_eq!(
            point_kind(&points, "/cache"),
            Some(MountPointKind::Directory)
        );
    }

    /// `docker run -v` creates a directory for a source that is not there yet;
    /// matching that is less surprising than refusing.
    #[test]
    fn a_missing_source_is_assumed_to_be_a_directory() {
        let mounts = vec!["/does/not/exist:/data:rw".parse::<Mount>().unwrap()];
        assert_eq!(
            point_kind(&required_mount_points(&mounts), "/data"),
            Some(MountPointKind::Directory)
        );
    }

    /// If the plan grows a mount whose target is not in the list, the sandbox
    /// fails at launch with a confusing ENOENT. Cross-check the two.
    #[test]
    fn the_plan_never_mounts_over_a_path_that_was_not_reserved() {
        let mounts = vec!["/host/a:/data:rw".parse::<Mount>().unwrap()];
        let points = required_mount_points(&mounts);
        let plan = plan("/newroot", &overlay(), &limits(), &mounts);

        for op in &plan.ops {
            let target = match op {
                MountOp::Tmpfs { target, .. }
                | MountOp::Bind { target, .. }
                | MountOp::Proc { target }
                | MountOp::Sysfs { target }
                | MountOp::DevPts { target } => target,
                // Masks and remounts act on paths the pseudo-filesystems
                // provide, and the root is the mount point itself.
                _ => continue,
            };
            let inside = Path::new("/").join(target.strip_prefix("/newroot").unwrap());
            assert!(
                points.iter().any(|p| p.path == inside),
                "{} is mounted but not reserved",
                inside.display()
            );
        }
    }

    #[test]
    fn describe_renders_every_op() {
        let p = plan("/newroot", &overlay(), &limits(), &[]);
        assert_eq!(p.describe().len(), p.ops.len());
        assert!(p.describe()[0].starts_with("make-rprivate"));
    }
}
