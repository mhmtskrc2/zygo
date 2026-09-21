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

    /// Whether **the host** has what a backend needs.
    ///
    /// Deliberately not "whether you can use it": that also depends on
    /// whether this build implements the backend, and asking a backend here
    /// is a cycle — [`crate::backend::ns::NsBackend::availability`] calls
    /// [`run`], so a `supports` that consulted it recursed until the stack
    /// ran out. It did, on a real host, as a `SIGSEGV` from `zygo doctor`.
    /// The two questions are joined where the claim is actually printed,
    /// in the CLI's `doctor`, which is the layer that may know both.
    ///
    /// `Degraded` counts as usable: it means a fallback applies, which is the
    /// point of having one. A kernel without unprivileged overlayfs still
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

/// When each upstream kernel series was released, as `(major, minor, year,
/// month)`, ordered oldest first.
///
/// Used to say how old a host's kernel *series* is, which is the closest
/// honest thing to a CVE warning that needs no network and no feed. It is a
/// floor on knowledge, not a claim of completeness: a series newer than the
/// last row is newer than anything this table knows and is never warned
/// about, and one older than the first row is warned about as "older than".
/// Add a row when a series ships; a stale table under-warns, never over-warns.
pub const SERIES_RELEASED: &[(u32, u32, i32, u32)] = &[
    (5, 4, 2019, 11),
    (5, 10, 2020, 12),
    (5, 15, 2021, 10),
    (6, 0, 2022, 10),
    (6, 1, 2022, 12),
    (6, 2, 2023, 2),
    (6, 3, 2023, 4),
    (6, 4, 2023, 6),
    (6, 5, 2023, 8),
    (6, 6, 2023, 10),
    (6, 7, 2024, 1),
    (6, 8, 2024, 3),
    (6, 9, 2024, 5),
    (6, 10, 2024, 7),
    (6, 11, 2024, 9),
    (6, 12, 2024, 11),
    (6, 13, 2025, 1),
    (6, 14, 2025, 3),
];

/// Series that are still receiving upstream stable updates when this was
/// written. A long-term series two years old is a different proposition from
/// a development series two years old, and the warning should say which.
pub const LTS_SERIES: &[(u32, u32)] = &[(5, 4), (5, 10), (5, 15), (6, 1), (6, 6), (6, 12)];

/// How old a series may be before `zygo doctor` says so.
///
/// Two years: long enough that an ordinary distro upgrade cycle does not trip
/// it, short enough that a host nobody has touched since the last engineer
/// left does.
pub const KERNEL_AGE_WARN_MONTHS: i64 = 24;

/// What is known about a kernel's age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KernelAge {
    /// Months since the series was released upstream.
    pub months: i64,
    /// `false` when the series predates the table, so `months` is a minimum.
    pub exact: bool,
    pub lts: bool,
}

impl KernelAge {
    pub fn is_old(self) -> bool {
        self.months >= KERNEL_AGE_WARN_MONTHS
    }
}

/// How old `version`'s series is at `now`, or `None` when it is newer than
/// anything [`SERIES_RELEASED`] knows — in which case it cannot be old.
pub fn kernel_age(version: KernelVersion, now: std::time::SystemTime) -> Option<KernelAge> {
    let secs = now.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    let (year, month, _) = civil_from_unix(secs);
    let lts = LTS_SERIES.contains(&(version.major, version.minor));

    let newest = SERIES_RELEASED.last()?;
    if (version.major, version.minor) > (newest.0, newest.1) {
        return None;
    }
    // The newest row at or below this series: a kernel between two known
    // series is at least as old as the one below it.
    let (row, exact) = match SERIES_RELEASED
        .iter()
        .rev()
        .find(|(ma, mi, _, _)| (*ma, *mi) <= (version.major, version.minor))
    {
        Some(row) => (row, (row.0, row.1) == (version.major, version.minor)),
        // Older than every row: use the oldest, and say it is a minimum.
        None => (SERIES_RELEASED.first()?, false),
    };
    Some(KernelAge {
        months: (year - row.2) as i64 * 12 + (month as i64 - row.3 as i64),
        exact,
        lts,
    })
}

/// Unix seconds → `(year, month, day)`.
///
/// Howard Hinnant's `civil_from_days`, which is exact and needs no date
/// library. Only the year and month are used, but the whole conversion is
/// cheaper to write correctly than to write partially.
fn civil_from_unix(secs: i64) -> (i32, u32, u32) {
    let days = secs.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
    (year, month, day)
}

/// What a given Landlock ABI level can restrict (design doc appendix C).
pub fn landlock_features(abi: u32) -> &'static str {
    match abi {
        0 => "unavailable",
        1..=3 => "fs",
        4 => "fs + net",
        _ => "fs + net + ioctl",
    }
}

/// Run every check for this host, against the data directory in use.
///
/// The directory is an argument because one check reads it: the guest kernel
/// lives under it, and a `doctor` that looked it up with `Paths::from_env()`
/// reported the default directory's kernel while the command it is advising
/// was pointed at another by `--data-root`.
pub fn run(paths: &crate::Paths) -> Report {
    Report {
        checks: probe::all(paths),
    }
}

#[cfg(target_os = "linux")]
mod probe {
    use std::path::Path;

    use super::*;
    use crate::cgroup;

    pub fn all(paths: &crate::Paths) -> Vec<Check> {
        vec![
            kernel(),
            kernel_age_check(),
            user_namespaces(),
            cgroup_v2(),
            overlayfs(),
            landlock(),
            seccomp(),
            subuid(),
            kvm(),
            guest_kernel(paths),
            runsc(),
            egress(),
        ]
    }

    /// What `network = "egress"` and `"full"` need: `pasta` to move packets
    /// without a privilege, and `nft` to install the allowlist inside the
    /// sandbox's own network namespace.
    ///
    /// Absent rather than failed: the default is `network = "none"`, and a host
    /// that only ever runs sealed sandboxes is not broken for lacking these.
    fn egress() -> Check {
        match (which("pasta"), which("nft")) {
            (Some(pasta), Some(_)) => match which("tc") {
                Some(_) => Check::ok("egress (pasta + nft + tc)", pasta),
                // Not degraded: `tc` is only needed by a `bandwidth` limit,
                // and a sandbox that sets one without it refuses to start
                // with the package named.
                None => Check::ok(
                    "egress (pasta + nft + tc)",
                    format!("{pasta}; no `tc`, so `bandwidth` limits are unavailable"),
                ),
            },
            (pasta, nft) => {
                let mut missing = Vec::new();
                if pasta.is_none() {
                    missing.push("passt");
                }
                if nft.is_none() {
                    missing.push("nftables");
                }
                Check::absent(
                    "egress (pasta + nft)",
                    format!("{} not installed", missing.join(" and ")),
                    format!("sudo apt install {}", missing.join(" ")),
                )
            }
        }
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

    /// How old the kernel *series* is, which is as close to a CVE warning as
    /// something with no network and no feed can honestly get.
    ///
    /// Age is not the same as unpatched: a distro backports fixes into an old
    /// series without changing its version, which is exactly what the LTS
    /// series are for. What age does say is how much of a decade of kernel
    /// hardening this host is behind, and that `ns` leans on one kernel.
    fn kernel_age_check() -> Check {
        let release = rustix::system::uname()
            .release()
            .to_string_lossy()
            .into_owned();
        let Some(v) = KernelVersion::parse(&release) else {
            return Check::absent("kernel age", "unknown", "the kernel version did not parse");
        };
        match kernel_age(v, std::time::SystemTime::now()) {
            // Newer than this build's table: it cannot be old, and guessing
            // would be a warning that ages into a lie.
            None => Check::ok("kernel age", format!("{v} is newer than this build knows")),
            Some(age) if !age.is_old() => Check::ok(
                "kernel age",
                format!(
                    "{}.{} released about {} months ago",
                    v.major, v.minor, age.months
                ),
            ),
            Some(age) => {
                let years = age.months / 12;
                let detail = format!(
                    "{}.{} is {}{} years old{}",
                    v.major,
                    v.minor,
                    if age.exact { "" } else { "at least " },
                    years,
                    if age.lts { ", a long-term series" } else { "" }
                );
                Check::degraded(
                    "kernel age",
                    detail,
                    if age.lts {
                        "a long-term series still gets stable updates, so this is about \
                         features and hardening rather than open CVEs — keep it patched, \
                         and prefer a newer series for untrusted code"
                            .to_string()
                    } else {
                        "this series is out of upstream stable support; every `ns` control \
                         is a kernel feature, so an unpatched kernel defeats all of them \
                         at once — upgrade, or run untrusted code on `vm`"
                            .to_string()
                    },
                )
            }
        }
    }

    fn user_namespaces() -> Check {
        let max =
            read_trimmed("/proc/sys/user/max_user_namespaces").and_then(|s| s.parse::<u64>().ok());
        if max == Some(0) {
            return Check::failed(
                "user namespaces",
                "disabled",
                "sudo sysctl -w user.max_user_namespaces=15000",
            );
        }
        if max.is_none() && !Path::new("/proc/self/ns/user").exists() {
            return Check::failed(
                "user namespaces",
                "unavailable",
                "enable CONFIG_USER_NS, or run Zygo as root",
            );
        }

        // Creating the namespace is not the question. Ubuntu 24.04 ships
        // `kernel.apparmor_restrict_unprivileged_userns=1`, which lets an
        // ordinary user make a user namespace and then refuses to let them
        // write its id map — and *every* boundary Zygo builds starts with that
        // map. The old check read `max_user_namespaces`, reported "enabled",
        // and the sandbox died four steps later on `mount --make-rprivate /:
        // Permission denied`, which names none of this.
        //
        // So the map is written, in a child, the way the launcher writes it.
        match try_a_sandbox_primitive() {
            Ok(()) => Check::ok("user namespaces", "one can be built and mounted in"),
            Err(reason) if apparmor_restricts_userns() => Check::failed(
                "user namespaces",
                reason,
                "AppArmor is restricting unprivileged user namespaces on this host: \
                 sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 \
                 (or ship an AppArmor profile for the `zygo` binary)",
            ),
            Err(reason) => Check::failed(
                "user namespaces",
                reason,
                "an LSM is refusing what a sandbox does first; check `dmesg` for \
                 a denial from AppArmor or SELinux",
            ),
        }
    }

    /// Whether Ubuntu's AppArmor restriction on unprivileged user namespaces
    /// is switched on. Read, not attempted — the attempt is the caller's, and
    /// this only explains what it found.
    fn apparmor_restricts_userns() -> bool {
        read_trimmed("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
            .is_some_and(|v| v.trim() != "0")
    }

    /// Do what a sandbox does first, in a child, and report what happened.
    ///
    /// Three steps, because the interesting failures are at different ones:
    /// make a user and mount namespace, have the parent write the id map, and
    /// then make the mount tree private — which is the launcher's very first
    /// mount and the step Ubuntu 24.04 refuses when AppArmor is restricting
    /// unprivileged user namespaces.
    ///
    /// An earlier version stopped after the id map and reported `ok` on a host
    /// where `zygo run` died two calls later. The map is written by the
    /// *parent*, which that restriction permits; it is the mount inside the
    /// namespace that it denies.
    ///
    /// A child, because `unshare` here would change the namespace `zygo
    /// doctor` itself runs in. It is killed and reaped before this returns.
    fn try_a_sandbox_primitive() -> std::result::Result<(), String> {
        use std::ffi::{c_int, c_void};

        let pipe = || -> std::result::Result<(c_int, c_int), String> {
            let mut fds = [0 as c_int; 2];
            // SAFETY: `pipe` writes two descriptors into the array and
            // returns non-zero on failure.
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return Err("could not make a pipe".into());
            }
            Ok((fds[0], fds[1]))
        };
        let (up_read, up_write) = pipe()?;
        let (down_read, down_write) = pipe()?;

        // SAFETY: the child allocates nothing and takes no lock; every call
        // below is async-signal-safe.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err("could not fork".into());
        }
        if pid == 0 {
            unsafe {
                libc::close(up_read);
                libc::close(down_write);
                let made = libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) == 0;
                let byte = [u8::from(made)];
                libc::write(up_write, byte.as_ptr() as *const c_void, 1);
                if !made {
                    libc::_exit(0);
                }
                // Wait for the map. Without it this process has no valid uid
                // and the mount below would fail for that reason instead.
                let mut go = [0u8; 1];
                if libc::read(down_read, go.as_mut_ptr() as *mut c_void, 1) != 1 || go[0] == 0 {
                    libc::_exit(0);
                }
                let ok = libc::mount(
                    c"none".as_ptr(),
                    c"/".as_ptr(),
                    std::ptr::null(),
                    libc::MS_REC | libc::MS_PRIVATE,
                    std::ptr::null(),
                ) == 0;
                let answer = [if ok { b'y' } else { b'n' }];
                libc::write(up_write, answer.as_ptr() as *const c_void, 1);
                libc::_exit(0);
            }
        }

        let close_all = || unsafe {
            for fd in [up_read, up_write, down_read, down_write] {
                libc::close(fd);
            }
        };
        let reap = || unsafe {
            libc::kill(pid, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(pid, &mut status, 0);
        };
        let finish = |result: std::result::Result<(), String>| {
            reap();
            close_all();
            result
        };

        let read_one = |fd: c_int| -> Option<u8> {
            let mut byte = [0u8; 1];
            // SAFETY: one byte into a local array.
            (unsafe { libc::read(fd, byte.as_mut_ptr() as *mut c_void, 1) } == 1).then_some(byte[0])
        };

        match read_one(up_read) {
            Some(0) => return finish(Err("unshare(CLONE_NEWUSER) was refused".into())),
            None => return finish(Err("the child said nothing".into())),
            Some(_) => {}
        }

        // SAFETY: no arguments, cannot fail.
        let uid = unsafe { libc::getuid() };
        let path = format!("/proc/{pid}/uid_map");
        let wrote = std::fs::write(&path, format!("0 {uid} 1\n"));
        let go = [u8::from(wrote.is_ok())];
        // SAFETY: one byte out of a local array.
        unsafe { libc::write(down_write, go.as_ptr() as *const c_void, 1) };
        if let Err(e) = wrote {
            return finish(Err(format!("the id map was refused ({e})")));
        }

        match read_one(up_read) {
            Some(b'y') => finish(Ok(())),
            Some(_) => finish(Err(
                "the mount tree could not be made private inside the namespace".into(),
            )),
            None => finish(Err("the child died before it could mount".into())),
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
        let Some(own) = current_cgroup_dir() else {
            return Check::failed(
                "cgroup v2",
                "cannot read /proc/self/cgroup",
                cgroup::DELEGATION_REMEDY,
            );
        };
        // Attempted, not read: see `cgroup::probe_delegation`. This is the R2
        // case — without a cgroup it can own, a sandbox has no limits and must
        // not start at all.
        match cgroup::probe_delegation(&own) {
            Ok(have) => Check::ok("cgroup v2", format!("delegated ({})", have.join(" "))),
            // This cgroup will not take a child, which is the normal state of
            // an ssh session's scope and not by itself a problem: the commands
            // that build a sandbox step into a scope of their own. Ask whether
            // *that* works before calling anything broken.
            Err(reason) => match cgroup::probe_delegation_via_scope() {
                Ok(()) => Check::ok(
                    "cgroup v2",
                    "delegated through a scope of its own, which Zygo enters by itself",
                ),
                Err(scope_reason) => Check::failed(
                    "cgroup v2",
                    format!("{reason}; and no transient scope either ({scope_reason})"),
                    cgroup::DELEGATION_REMEDY,
                ),
            },
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
            // A range is only usable through the setuid helpers; without them
            // the launcher falls back to mapping the caller's single uid, and
            // the uid-level separation between tenants (§3.10) is lost.
            let helpers = ["newuidmap", "newgidmap"].iter().all(|h| {
                std::env::var_os("PATH")
                    .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(h).is_file()))
            });
            if helpers {
                Check::ok("subuid/subgid", "configured")
            } else {
                Check::degraded(
                    "subuid/subgid",
                    "range configured, but newuidmap/newgidmap are missing; \
                     every tenant maps to one host uid",
                    "install the uidmap package (apt-get install uidmap)",
                )
            }
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
        let fd = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(fd) => fd,
            // Absent rather than degraded: a `/dev/kvm` this user cannot open
            // is not a weaker `vm` backend, it is no `vm` backend. Reporting it
            // as degraded made `zygo doctor` list `vm` under "backends
            // available" on a host where it could not have started one.
            Err(_) => {
                return Check::absent(
                    "kvm",
                    "/dev/kvm is not readable and writable by this user",
                    "sudo usermod -aG kvm $USER, then log in again",
                );
            }
        };

        // Opening the device is not the question. A nested or restricted
        // hypervisor hands out `/dev/kvm` and then refuses `KVM_CREATE_VM`,
        // and this check used to call that host ready — which would have sent
        // the `vm` backend to a machine that cannot start a virtual machine
        // So it creates one and closes it,
        // which is the same probe that confirmed the Raspberry Pi.
        match create_vm(&fd) {
            Ok(()) => Check::ok("kvm", "/dev/kvm"),
            Err(e) => Check::absent(
                "kvm",
                format!("/dev/kvm opens but KVM_CREATE_VM failed: {e}"),
                "this looks like a nested or restricted hypervisor; use --isolation ns here",
            ),
        }
    }

    /// Ask KVM for a virtual machine, and give it straight back.
    ///
    /// The descriptor is closed by `OwnedFd`'s drop, which is what makes this
    /// safe to run from `doctor` — a diagnostic that leaves a VM behind is
    /// worse than one that reports less.
    #[cfg(target_os = "linux")]
    fn create_vm(kvm: &std::fs::File) -> std::io::Result<()> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        // `KVM_CREATE_VM` is `_IO(KVMIO, 0x01)` with `KVMIO` = 0xAE, on every
        // architecture. The machine type argument is zero, which means "this
        // host's default" — the only thing a probe should ask for.
        // `libc::Ioctl` is `c_ulong` on glibc and `c_int` on musl, so the
        // constant takes the target's own type rather than a fixed one.
        const KVM_CREATE_VM: libc::Ioctl = 0xAE01;

        // SAFETY: `kvm` is an open `/dev/kvm`; the ioctl takes an integer by
        // value and returns a descriptor or -1.
        let vm = unsafe { libc::ioctl(kvm.as_raw_fd(), KVM_CREATE_VM, 0) };
        if vm < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: a descriptor the kernel just created and nobody else holds.
        drop(unsafe { OwnedFd::from_raw_fd(vm) });
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn create_vm(_kvm: &std::fs::File) -> std::io::Result<()> {
        // There is no `/dev/kvm` to have opened, so this is unreachable; it
        // exists so the check above compiles and is reviewed everywhere.
        Ok(())
    }

    /// The `vm` backend's guest kernel, which is a file and not a link.
    ///
    /// A line of its own rather than folded into `kvm`, because the two fail
    /// for opposite reasons and have opposite remedies: a host without KVM
    /// cannot run the backend at all, and a host with KVM and no kernel is one
    /// command away. Folding them together sent people to the wrong one (D8).
    fn guest_kernel(paths: &crate::Paths) -> Check {
        let image = paths.krun().join(crate::backend::vm::KERNEL_FILE);
        if image.is_file() {
            let size = std::fs::metadata(&image).map(|m| m.len()).unwrap_or(0);
            Check::ok(
                "guest kernel",
                format!("{} ({} MB)", image.display(), size / (1024 * 1024)),
            )
        } else {
            Check::absent(
                "guest kernel",
                "not installed",
                "zygo backend install vm — the `vm` backend has nothing to boot without it",
            )
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
    /// than printing a page of failures, say the one true thing: every
    /// boundary Zygo builds is a Linux kernel feature, so this platform runs
    /// them somewhere else.
    ///
    /// On macOS that somewhere is a Linux VM, and the answer a user actually
    /// wants is *its* — which the CLI appends, because knowing how to reach
    /// the VM is the shim's business and not this library's.
    pub fn all(_paths: &crate::Paths) -> Vec<Check> {
        vec![Check::failed(
            "platform",
            format!(
                "{} has no kernel to build a sandbox in",
                std::env::consts::OS
            ),
            "this host runs sandboxes in a Linux VM; what that VM says about \
             itself is below",
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

    /// The date conversion the age check rests on, against dates whose
    /// answers are known — including the leap-year cases that are the only
    /// way a hand-written civil calendar goes wrong.
    #[test]
    fn unix_seconds_convert_to_the_right_civil_date() {
        assert_eq!(civil_from_unix(0), (1970, 1, 1));
        assert_eq!(civil_from_unix(951_782_400), (2000, 2, 29), "a leap year");
        assert_eq!(civil_from_unix(1_709_164_800), (2024, 2, 29), "another");
        assert_eq!(civil_from_unix(1_735_689_599), (2024, 12, 31));
        assert_eq!(civil_from_unix(1_735_689_600), (2025, 1, 1));
    }

    fn at(year: i32, month: u32) -> std::time::SystemTime {
        // Midday on the first, so no timezone or leap second can move the month.
        let days: i64 = (1970..year)
            .map(|y| if leap(y) { 366 } else { 365 })
            .sum::<i64>()
            + (1..month).map(|m| i64::from(days_in(year, m))).sum::<i64>();
        std::time::UNIX_EPOCH + std::time::Duration::from_secs((days * 86_400 + 43_200) as u64)
    }
    fn leap(y: i32) -> bool {
        (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
    }
    fn days_in(y: i32, m: u32) -> u32 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ if leap(y) => 29,
            _ => 28,
        }
    }

    #[test]
    fn a_kernel_series_is_aged_against_its_release() {
        let v = |ma, mi| KernelVersion {
            major: ma,
            minor: mi,
            patch: 0,
        };
        // 6.8 shipped in March 2024.
        let age = kernel_age(v(6, 8), at(2024, 9)).expect("known series");
        assert_eq!(age.months, 6);
        assert!(age.exact);
        assert!(!age.is_old());
        assert!(!age.lts);

        let age = kernel_age(v(6, 8), at(2026, 9)).expect("known series");
        assert_eq!(age.months, 30);
        assert!(age.is_old(), "two and a half years is old");

        // A series between two known rows is at least as old as the one below.
        let age = kernel_age(v(6, 5), at(2026, 8)).unwrap();
        assert!(age.exact, "6.5 is in the table");
        let between = kernel_age(
            KernelVersion {
                major: 6,
                minor: 12,
                patch: 40,
            },
            at(2026, 11),
        )
        .unwrap();
        assert_eq!(between.months, 24);
        assert!(between.lts, "6.12 is a long-term series");
    }

    /// The table is a floor on knowledge. A kernel newer than every row must
    /// not be warned about, or the warning ages into a lie the moment this
    /// build is a year old.
    #[test]
    fn a_kernel_newer_than_the_table_is_never_called_old() {
        let newest = SERIES_RELEASED.last().expect("a table");
        let newer = KernelVersion {
            major: newest.0,
            minor: newest.1 + 1,
            patch: 0,
        };
        assert_eq!(kernel_age(newer, at(2030, 1)), None);
        let much_newer = KernelVersion {
            major: newest.0 + 2,
            minor: 0,
            patch: 0,
        };
        assert_eq!(kernel_age(much_newer, at(2030, 1)), None);
    }

    /// And older than everything it knows is aged from the oldest row, marked
    /// as a minimum rather than reported as an exact figure.
    #[test]
    fn a_kernel_older_than_the_table_is_a_lower_bound() {
        let age = kernel_age(
            KernelVersion {
                major: 4,
                minor: 19,
                patch: 0,
            },
            at(2026, 9),
        )
        .expect("aged from the oldest row");
        assert!(!age.exact, "the figure is a minimum");
        assert!(age.is_old());
        // The oldest row is 5.4, November 2019.
        assert_eq!(age.months, (2026 - 2019) * 12 + 9 - 11);
    }

    #[test]
    fn the_table_is_ordered_and_its_lts_series_are_in_it() {
        let mut previous = (0, 0);
        for (major, minor, year, month) in SERIES_RELEASED {
            assert!(
                (*major, *minor) > previous,
                "{major}.{minor} is out of order"
            );
            previous = (*major, *minor);
            assert!((1..=12).contains(month), "{major}.{minor}: month {month}");
            assert!((2019..2100).contains(year), "{major}.{minor}: year {year}");
        }
        for (major, minor) in LTS_SERIES {
            assert!(
                SERIES_RELEASED
                    .iter()
                    .any(|(ma, mi, _, _)| (ma, mi) == (major, minor)),
                "{major}.{minor} is called long-term but has no release date"
            );
        }
    }

    /// An old kernel is a warning, never a refusal: `ns` still works on it,
    /// and a doctor that failed would stop a host that is merely behind.
    #[test]
    fn an_old_kernel_does_not_disqualify_a_backend() {
        let r = report(vec![
            ("kernel", Status::Ok),
            ("kernel age", Status::Degraded),
            ("user namespaces", Status::Ok),
            ("cgroup v2", Status::Ok),
            ("seccomp", Status::Ok),
        ]);
        assert!(r.supports(Isolation::Ns));
        assert_eq!(r.exit_code(), 0, "degraded is not a failure");
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

        // A host that has everything a backend needs is not a host that can
        // *run* it, if this build does not implement it. `vm` is the live
        // case: `doctor` printed `backends available: ns, vm` on a machine
        // with `/dev/kvm` while `backend list` said, correctly, that `vm` is
        // not built. `supports` answers only the host half by design — the
        // two are joined in the CLI, because doing it here recurses.
        let with_kvm = report(vec![("kvm", Status::Ok)]);
        assert!(with_kvm.supports(Isolation::Vm), "the host has KVM");
        assert!(
            crate::backend::for_isolation(Isolation::Vm, &crate::Paths::from_env()).is_err(),
            "and the vm backend is not built, which is the other half"
        );

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

    /// `doctor` answers about the data directory it was given.
    ///
    /// The guest-kernel line used to come from `Paths::from_env()` however
    /// `doctor` was invoked, so `zygo --data-root /tmp/x doctor` reported the
    /// kernel in the *default* directory — "ok" about a file the command it
    /// was advising would never open.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_guest_kernel_line_is_about_the_data_root_it_was_given() {
        let root = tempfile::tempdir().expect("a temp dir");
        let paths = crate::Paths::rooted(root.path());

        let absent = run(&paths);
        let line = absent
            .checks
            .iter()
            .find(|c| c.name == "guest kernel")
            .expect("doctor reports a guest kernel line on Linux");
        assert_eq!(
            line.status,
            Status::Absent,
            "a data root with no kernel must not report one: {line:?}"
        );

        std::fs::create_dir_all(paths.krun()).expect("the krun directory");
        std::fs::write(paths.krun().join(crate::backend::vm::KERNEL_FILE), b"file")
            .expect("the kernel file");

        let present = run(&paths);
        let line = present
            .checks
            .iter()
            .find(|c| c.name == "guest kernel")
            .expect("doctor reports a guest kernel line on Linux");
        assert_eq!(line.status, Status::Ok, "the installed kernel is reported");
        assert!(
            line.detail.contains(&root.path().display().to_string()),
            "the line names the given data root, not another: {line:?}"
        );
    }

    #[test]
    fn a_report_can_always_be_produced_on_this_host() {
        // Whatever platform the tests run on, `doctor` must not panic — it is
        // the command users reach for when nothing else works.
        let r = run(&crate::Paths::from_env());
        assert!(!r.checks.is_empty());
    }
}
