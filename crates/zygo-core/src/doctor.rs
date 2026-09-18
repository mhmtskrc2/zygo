//! Environment probing behind `zygo doctor` (design doc §4.1).
//!
//! The output is the honest answer to "will this host actually enforce what
//! Zygo promises?". Risk R2 is that a rootless host without cgroup delegation
//! silently applies no limits at all; requirement N4 says that must never be a
//! silent condition, so this module exists to make it loud, with the one-line
//! fix printed next to it.
//!
//! The probes are thin; the interesting logic — kernel version comparison,
//! Landlock ABI to feature mapping, overall verdict — is pure and tested.

use std::fmt;

use crate::spec::Isolation;

/// Outcome of a single check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    /// Everything Zygo needs is present.
    Ok,
    /// Present but degraded: a fallback applies.
    Degraded,
    /// Not present, and not needed unless an optional feature is used.
    Absent,
    /// Missing something mandatory. `ns` sandboxes cannot run safely.
    Failed,
}

impl Status {
    pub const fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Degraded => "degraded",
            Status::Absent => "-",
            Status::Failed => "FAIL",
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One line of `zygo doctor` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Short name, e.g. `cgroup v2`.
    pub name: &'static str,
    pub status: Status,
    /// What was found, e.g. `6.8.0` or `delegated (cpu io memory pids)`.
    pub detail: String,
    /// Copy-pasteable fix, when there is one.
    pub remedy: Option<String>,
}

// Most constructors are only reachable from the Linux probe; on other hosts
// `probe::all` returns a single `failed` check.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Ok,
            detail: detail.into(),
            remedy: None,
        }
    }

    fn failed(name: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Failed,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }

    fn degraded(name: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Degraded,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }

    fn absent(name: &'static str, detail: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Absent,
            detail: detail.into(),
            remedy: Some(remedy.into()),
        }
    }
}

/// The full report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// Worst status in the report — what the exit code is derived from.
    pub fn worst(&self) -> Status {
        self.checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(Status::Ok)
    }

    /// Whether a given backend can be used on this host.
    ///
    /// `Degraded` counts as usable: it means a fallback applies, which is the
    /// whole point of having one. Only `Failed` — something mandatory missing —
    /// disqualifies a backend. A kernel without unprivileged overlayfs still
    /// runs sandboxes; the store just flattens the layers.
    pub fn supports(&self, isolation: Isolation) -> bool {
        let needed: &[&str] = match isolation {
            Isolation::Ns => &["kernel", "user namespaces", "cgroup v2", "seccomp"],
            Isolation::Vm => &["kvm"],
            Isolation::Gvisor => &["runsc"],
        };
        needed.iter().all(|n| {
            self.checks
                .iter()
                .find(|c| c.name == *n)
                .is_some_and(|c| matches!(c.status, Status::Ok | Status::Degraded))
        })
    }

    /// Exit code for `zygo doctor`: 0 when usable, 1 when something mandatory
    /// is missing.
    pub fn exit_code(&self) -> i32 {
        i32::from(self.worst() == Status::Failed)
    }
}

/// A `major.minor.patch` kernel version, comparable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct KernelVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl KernelVersion {
    /// Parse a `uname -r` string, ignoring the distro suffix
    /// (`6.8.0-31-generic`, `5.15.0-91-generic`, `6.1.0-18-arm64`).
    pub fn parse(release: &str) -> Option<Self> {
        let core = release.split(['-', '+']).next().unwrap_or(release);
        let mut it = core.split('.');
        Some(KernelVersion {
            major: it.next()?.parse().ok()?,
            minor: it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            patch: it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
        })
    }

    pub const fn at_least(self, major: u32, minor: u32) -> bool {
        self.major > major || (self.major == major && self.minor >= minor)
    }
}

impl fmt::Display for KernelVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Hard floor: `clone3` (5.3) is what the launcher is built on, and there is no
/// fallback for it.
pub const MIN_KERNEL: (u32, u32) = (5, 3);
/// Below this, overlayfs is unavailable inside a user namespace and the image
/// store has to flatten layers instead — slower and heavier on disk, but
/// correct (design doc appendix C, risk R3).
pub const OVERLAY_KERNEL: (u32, u32) = (5, 11);
/// Above this everything in appendix C is present: Landlock networking,
/// `cgroup.kill`, `memory.peak`.
pub const RECOMMENDED_KERNEL: (u32, u32) = (6, 1);

/// What a given Landlock ABI level can restrict (design doc appendix C).
pub fn landlock_features(abi: u32) -> &'static str {
    match abi {
        0 => "unavailable",
        1..=3 => "fs",
        4 => "fs + net",
        _ => "fs + net + ioctl",
    }
}

/// Run every check for this host.
pub fn run() -> Report {
    Report {
        checks: probe::all(),
    }
}

#[cfg(target_os = "linux")]
mod probe {
    use std::path::Path;

    use super::*;
    use crate::cgroup;

    pub fn all() -> Vec<Check> {
        vec![
            kernel(),
            user_namespaces(),
            cgroup_v2(),
            overlayfs(),
            landlock(),
            seccomp(),
            subuid(),
            kvm(),
            runsc(),
        ]
    }

    fn kernel() -> Check {
        let release = rustix::system::uname()
            .release()
            .to_string_lossy()
            .into_owned();
        match KernelVersion::parse(&release) {
            None => Check::degraded("kernel", release, "could not parse the kernel version"),
            Some(v) if !v.at_least(MIN_KERNEL.0, MIN_KERNEL.1) => Check::failed(
                "kernel",
                v.to_string(),
                format!(
                    "Zygo's launcher is built on clone3, which needs Linux {}.{}+; \
                     upgrade, or use --isolation vm",
                    MIN_KERNEL.0, MIN_KERNEL.1
                ),
            ),
            Some(v) if !v.at_least(RECOMMENDED_KERNEL.0, RECOMMENDED_KERNEL.1) => Check::degraded(
                "kernel",
                v.to_string(),
                format!(
                    "{}.{}+ is recommended; on this kernel some of Landlock, \
                     cgroup.kill and memory.peak fall back",
                    RECOMMENDED_KERNEL.0, RECOMMENDED_KERNEL.1
                ),
            ),
            Some(v) => Check::ok("kernel", v.to_string()),
        }
    }

    fn user_namespaces() -> Check {
        let max =
            read_trimmed("/proc/sys/user/max_user_namespaces").and_then(|s| s.parse::<u64>().ok());
        match max {
            Some(0) => Check::failed(
                "user namespaces",
                "disabled",
                "sudo sysctl -w user.max_user_namespaces=15000",
            ),
            Some(_) => Check::ok("user namespaces", "enabled"),
            None if Path::new("/proc/self/ns/user").exists() => {
                Check::ok("user namespaces", "enabled")
            }
            None => Check::failed(
                "user namespaces",
                "unavailable",
                "enable CONFIG_USER_NS, or run Zygo as root",
            ),
        }
    }

    fn cgroup_v2() -> Check {
        if !Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            return Check::failed(
                "cgroup v2",
                "not the unified hierarchy",
                "boot with systemd.unified_cgroup_hierarchy=1",
            );
        }
        let own = current_cgroup_dir();
        let missing = own
            .as_deref()
            .map(cgroup::missing_controllers)
            .unwrap_or_default();
        if missing.is_empty() {
            let have = own
                .as_deref()
                .map(cgroup::available_controllers)
                .unwrap_or_default();
            Check::ok("cgroup v2", format!("delegated ({})", have.join(" ")))
        } else {
            // This is the R2 case: without these, limits are not enforced and a
            // sandbox must not start at all.
            Check::failed(
                "cgroup v2",
                format!("not delegated (missing: {})", missing.join(" ")),
                "mkdir -p ~/.config/systemd/user/user@.service.d && \
                 printf '[Service]\\nDelegate=cpu cpuset io memory pids\\n' > \
                 ~/.config/systemd/user/user@.service.d/delegate.conf && \
                 systemctl --user daemon-reexec",
            )
        }
    }

    fn current_cgroup_dir() -> Option<std::path::PathBuf> {
        let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
        Some(Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/')))
    }

    /// Whether overlayfs works *inside a user namespace* — which is a
    /// different question from whether the filesystem exists.
    ///
    /// Unprivileged overlayfs landed in 5.11. Before that, `/proc/filesystems`
    /// still lists `overlay` and a privileged mount still succeeds, so reading
    /// that file alone reports support that a rootless sandbox does not have.
    /// PoC 6 measured exactly this divergence on 5.10.
    fn overlayfs() -> Check {
        let present = std::fs::read_to_string("/proc/filesystems")
            .map(|s| s.contains("overlay"))
            .unwrap_or(false);
        let new_enough = KernelVersion::parse(&rustix::system::uname().release().to_string_lossy())
            .is_some_and(|v| v.at_least(OVERLAY_KERNEL.0, OVERLAY_KERNEL.1));

        match (present, new_enough) {
            (true, true) => Check::ok("overlayfs (userns)", "supported"),
            (true, false) => Check::degraded(
                "overlayfs (userns)",
                format!("needs {}.{}+", OVERLAY_KERNEL.0, OVERLAY_KERNEL.1),
                "image layers will be flattened instead; this costs disk and first-run time",
            ),
            (false, _) => Check::degraded(
                "overlayfs (userns)",
                "filesystem unavailable",
                "image layers will be flattened instead; this costs disk and first-run time",
            ),
        }
    }

    fn landlock() -> Check {
        match landlock_abi() {
            0 => Check::degraded(
                "landlock",
                "unavailable",
                "kernel 5.13+ adds filesystem allowlisting; seccomp still applies",
            ),
            abi => Check::ok(
                "landlock",
                format!("ABI v{abi} ({})", landlock_features(abi)),
            ),
        }
    }

    /// `landlock_create_ruleset(NULL, 0, LANDLOCK_CREATE_RULESET_VERSION)`
    /// returns the supported ABI version, or `-ENOSYS`/`-EOPNOTSUPP`.
    fn landlock_abi() -> u32 {
        const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
        const LANDLOCK_CREATE_RULESET_VERSION: libc::c_uint = 1;
        // SAFETY: the version query takes a null attr pointer and zero size by
        // definition; it only reads a constant out of the kernel.
        let rc = unsafe {
            libc::syscall(
                SYS_LANDLOCK_CREATE_RULESET,
                std::ptr::null::<libc::c_void>(),
                0usize,
                LANDLOCK_CREATE_RULESET_VERSION,
            )
        };
        if rc > 0 { rc as u32 } else { 0 }
    }

    fn seccomp() -> Check {
        match read_trimmed("/proc/sys/kernel/seccomp/actions_avail") {
            Some(actions) if actions.contains("errno") => Check::ok("seccomp", "supported"),
            Some(_) => Check::degraded(
                "seccomp",
                "limited",
                "the kernel lacks SECCOMP_RET_ERRNO; syscall filtering will be coarse",
            ),
            None => Check::failed(
                "seccomp",
                "unavailable",
                "enable CONFIG_SECCOMP_FILTER, or use --isolation vm",
            ),
        }
    }

    fn subuid() -> Check {
        let uid = unsafe { libc::getuid() };
        if uid == 0 {
            return Check::ok("subuid/subgid", "running as root");
        }
        let user = std::env::var("USER").unwrap_or_default();
        let has_range = ["/etc/subuid", "/etc/subgid"].iter().all(|f| {
            std::fs::read_to_string(f)
                .map(|s| {
                    s.lines().any(|l| {
                        let name = l.split(':').next().unwrap_or("");
                        name == user || name == uid.to_string()
                    })
                })
                .unwrap_or(false)
        });
        if has_range {
            Check::ok("subuid/subgid", "configured")
        } else {
            Check::degraded(
                "subuid/subgid",
                "no range for this user",
                format!(
                    "sudo usermod --add-subuids 100000-165535 --add-subgids 100000-165535 {user}"
                ),
            )
        }
    }

    fn kvm() -> Check {
        let path = Path::new("/dev/kvm");
        if !path.exists() {
            return Check::absent(
                "kvm",
                "no /dev/kvm",
                "the `vm` backend needs KVM; use --isolation ns here",
            );
        }
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(_) => Check::ok("kvm", "/dev/kvm"),
            Err(_) => Check::degraded(
                "kvm",
                "/dev/kvm not writable",
                "sudo usermod -aG kvm $USER, then log in again",
            ),
        }
    }

    fn runsc() -> Check {
        match which("runsc") {
            Some(p) => Check::ok("runsc", p),
            None => Check::absent("runsc", "not installed", "zygo backend install gvisor"),
        }
    }

    fn read_trimmed(path: &str) -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn which(bin: &str) -> Option<String> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join(bin))
                .find(|p| p.is_file())
                .map(|p| p.display().to_string())
        })
    }
}

#[cfg(not(target_os = "linux"))]
mod probe {
    use super::*;

    /// On a non-Linux host there is nothing to enforce isolation with. Rather
    /// than printing a page of failures, say the one true thing: this platform
    /// needs the shim, which is phase 5 (design doc §3.11).
    pub fn all() -> Vec<Check> {
        vec![Check::failed(
            "platform",
            format!("{} is not a Zygo host", std::env::consts::OS),
            "Zygo sandboxes are Linux-only. macOS support runs a hidden Linux VM \
             (phase 5); until then, run Zygo inside a Linux VM or container.",
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_versions_parse_from_distro_releases() {
        let v = KernelVersion::parse("6.8.0-31-generic").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (6, 8, 0));
        assert_eq!(KernelVersion::parse("5.15.0-91-generic").unwrap().minor, 15);
        assert_eq!(KernelVersion::parse("6.1.0-18-arm64").unwrap().major, 6);
        assert_eq!(KernelVersion::parse("6.10").unwrap().patch, 0);
        assert_eq!(KernelVersion::parse("6.6.1+").unwrap().patch, 1);
        assert!(KernelVersion::parse("not-a-kernel").is_none());
    }

    #[test]
    fn kernel_version_comparison_matches_appendix_c() {
        let v50 = KernelVersion::parse("5.0.0").unwrap();
        let v510 = KernelVersion::parse("5.10.0").unwrap();
        let v515 = KernelVersion::parse("5.15.0").unwrap();
        let v61 = KernelVersion::parse("6.1.0").unwrap();

        // The hard floor is clone3, not overlayfs: the store can flatten.
        assert!(!v50.at_least(MIN_KERNEL.0, MIN_KERNEL.1));
        assert!(v510.at_least(MIN_KERNEL.0, MIN_KERNEL.1));

        // 5.10 runs, but without unprivileged overlayfs (PoC 6 measured this).
        assert!(!v510.at_least(OVERLAY_KERNEL.0, OVERLAY_KERNEL.1));
        assert!(v515.at_least(OVERLAY_KERNEL.0, OVERLAY_KERNEL.1));

        assert!(!v515.at_least(RECOMMENDED_KERNEL.0, RECOMMENDED_KERNEL.1));
        assert!(v61.at_least(RECOMMENDED_KERNEL.0, RECOMMENDED_KERNEL.1));
        assert!(v61 > v515);
    }

    /// A kernel that can run sandboxes must not be reported as unusable just
    /// because one optimisation is unavailable — that is what the flatten
    /// fallback exists for.
    #[test]
    fn the_kernel_floor_is_clone3_not_overlayfs() {
        assert!(MIN_KERNEL < OVERLAY_KERNEL);
        assert!(OVERLAY_KERNEL < RECOMMENDED_KERNEL);
    }

    #[test]
    fn landlock_abi_maps_to_documented_features() {
        assert_eq!(landlock_features(0), "unavailable");
        assert_eq!(landlock_features(1), "fs");
        assert_eq!(landlock_features(3), "fs");
        assert_eq!(landlock_features(4), "fs + net");
        assert_eq!(landlock_features(5), "fs + net + ioctl");
    }

    fn report(checks: Vec<(&'static str, Status)>) -> Report {
        Report {
            checks: checks
                .into_iter()
                .map(|(name, status)| Check {
                    name,
                    status,
                    detail: String::new(),
                    remedy: None,
                })
                .collect(),
        }
    }

    #[test]
    fn the_verdict_is_the_worst_check() {
        assert_eq!(report(vec![("a", Status::Ok)]).worst(), Status::Ok);
        assert_eq!(
            report(vec![("a", Status::Ok), ("b", Status::Degraded)]).worst(),
            Status::Degraded
        );
        assert_eq!(
            report(vec![("a", Status::Degraded), ("b", Status::Failed)]).worst(),
            Status::Failed
        );
    }

    #[test]
    fn only_a_failure_makes_doctor_exit_nonzero() {
        assert_eq!(report(vec![("a", Status::Degraded)]).exit_code(), 0);
        assert_eq!(report(vec![("a", Status::Absent)]).exit_code(), 0);
        assert_eq!(report(vec![("a", Status::Failed)]).exit_code(), 1);
    }

    #[test]
    fn backend_support_follows_the_relevant_checks() {
        let r = report(vec![
            ("kernel", Status::Ok),
            ("user namespaces", Status::Ok),
            ("cgroup v2", Status::Ok),
            ("seccomp", Status::Ok),
            ("kvm", Status::Absent),
            ("runsc", Status::Absent),
        ]);
        assert!(r.supports(Isolation::Ns));
        assert!(!r.supports(Isolation::Vm), "no KVM, no vm backend");
        assert!(!r.supports(Isolation::Gvisor));

        // Missing cgroup delegation disqualifies `ns` entirely (requirement N4).
        let r = report(vec![
            ("kernel", Status::Ok),
            ("user namespaces", Status::Ok),
            ("cgroup v2", Status::Failed),
            ("seccomp", Status::Ok),
        ]);
        assert!(!r.supports(Isolation::Ns));
    }

    /// A degraded check means a fallback applies, not that the host is unusable
    /// — an older kernel without unprivileged overlayfs still runs sandboxes.
    #[test]
    fn degraded_checks_do_not_disqualify_a_backend() {
        let r = report(vec![
            ("kernel", Status::Degraded),
            ("user namespaces", Status::Ok),
            ("cgroup v2", Status::Ok),
            ("seccomp", Status::Ok),
        ]);
        assert!(r.supports(Isolation::Ns));
        assert_eq!(r.exit_code(), 0, "degraded is not a failure");
    }

    #[test]
    fn a_report_can_always_be_produced_on_this_host() {
        // Whatever platform the tests run on, `doctor` must not panic — it is
        // the command users reach for when nothing else works.
        let r = run();
        assert!(!r.checks.is_empty());
    }
}
