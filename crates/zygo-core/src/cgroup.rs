//! The two-level cgroup v2 hierarchy (design doc §3.6).
//!
//! ```text
//! zygo.slice/                     memory.max = host RAM − reserve
//! ├── system/                     supervisor, image GC   memory.min = 512M
//! └── tenants/                    memory.max = tenant budget
//!     ├── tenant-A/               memory.max, pids.max, cpu.max
//!     │   └── g4242-1/            one generation of the sandbox
//!     │       ├── zygote
//!     │       ├── req-01f3…
//!     │       └── req-01f4…
//!     └── tenant-B/
//! ```
//!
//! The tenant carries the limits and outlives any one sandbox; a *generation*
//! is what one sandbox — its zygote and its requests — lives in and is torn
//! down with. Replacing or rewarming a function starts the new sandbox in a
//! generation of its own, so retiring the old one takes only the old one.
//!
//! The point of `system/` having a `memory.min` reservation is that a thousand
//! tenants all pressed against their limits must not be able to OOM the
//! supervisor — if the supervisor dies, nothing enforces the timeouts.
//!
//! Directory manipulation here is plain filesystem I/O, so the whole layout is
//! exercised against a temporary directory in the tests rather than needing a
//! Linux host.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, IoContext, Result};
use crate::sandbox::limits::{CgroupWrite, Limits};
use crate::spec::Bytes;

/// Memory kept out of the tenant budget for the supervisor and image store.
pub const SYSTEM_RESERVE: Bytes = Bytes(512 * 1024 * 1024);

/// Fraction of the `zygo.slice` budget tenants may collectively use.
pub const TENANT_BUDGET_FRACTION: f64 = 0.8;

/// Where Zygo's slice lives, and how to name things inside it.
#[derive(Debug, Clone)]
pub struct Hierarchy {
    /// Absolute path of `zygo.slice`, e.g.
    /// `/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/zygo.slice`.
    root: PathBuf,
}

impl Hierarchy {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Locate `zygo.slice` under the caller's own cgroup, which is where a
    /// rootless supervisor is allowed to create children.
    ///
    /// Reads `/proc/self/cgroup` (cgroup v2 emits a single `0::<path>` line).
    #[cfg(target_os = "linux")]
    pub fn discover() -> Result<Self> {
        const MOUNT: &str = "/sys/fs/cgroup";
        let text = std::fs::read_to_string("/proc/self/cgroup").at("/proc/self/cgroup")?;
        let rel = text
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .ok_or_else(|| Error::BackendUnavailable {
                backend: "ns",
                reason: "cgroup v2 is not the unified hierarchy on this host".into(),
                remedy: "boot with systemd.unified_cgroup_hierarchy=1, or use --isolation vm"
                    .into(),
            })?
            .trim();
        let current = Path::new(MOUNT).join(rel.trim_start_matches('/'));

        // If this process is already inside a `zygo.slice`, that is the slice —
        // do not nest another one inside it.
        //
        // `ensure` has to enable `subtree_control` on the slice's parent, and a
        // cgroup that delegates to its children may no longer hold processes of
        // its own. So the cgroup a first `zygo` ran from cannot host a second
        // one, and without this the next invocation would silently build a
        // second hierarchy somewhere else. Recognising the existing slice makes
        // repeated invocations idempotent, and is also what happens when the
        // supervisor spawns a child of itself.
        if let Some(existing) = current
            .ancestors()
            .find(|a| a.file_name().is_some_and(|n| n == "zygo.slice"))
        {
            return Ok(Self::new(existing));
        }

        Ok(Self::new(current.join("zygo.slice")))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn discover() -> Result<Self> {
        Err(Error::BackendUnavailable {
            backend: "ns",
            reason: "cgroup v2 only exists on Linux".into(),
            remedy: "run inside a Linux VM or container; macOS support is phase 5".into(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `zygo.slice/system` — the supervisor and the image store.
    pub fn system(&self) -> PathBuf {
        self.root.join("system")
    }

    /// `zygo.slice/tenants` — the parent of every tenant, and the place the
    /// aggregate budget is enforced.
    pub fn tenants(&self) -> PathBuf {
        self.root.join("tenants")
    }

    pub fn tenant(&self, name: &str) -> PathBuf {
        self.tenants().join(sanitise(name))
    }

    /// A fresh generation of a tenant's sandbox: `tenants/<name>/g<pid>-<n>`.
    ///
    /// Before generations existed the old and the new sandbox of a replaced
    /// function shared the tenant cgroup, and on a kernel with `cgroup.kill`
    /// (5.14+) retiring the old one killed its replacement — and then every
    /// rewarm after it, each retiring the one before. Found on 6.5; invisible
    /// on the 5.10 phase 0 measured on, where the fallback signalled one pid.
    ///
    /// The name is unique per supervisor lifetime: the pid keeps it apart from
    /// a dead supervisor's leftovers, the counter from this supervisor's own
    /// earlier generations of the same tenant.
    pub fn generation(&self, name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        self.tenant(name)
            .join(format!("g{}-{n}", std::process::id()))
    }

    /// Where a generation's long-lived process actually sits.
    ///
    /// Not the generation cgroup itself: that one parents the per-request
    /// cgroups, and cgroup v2 forbids a cgroup from holding processes while
    /// delegating to children. The design's own diagram (§3.6) shows this —
    /// `zygote` alongside `req-…` — and writing a pid one level up returns
    /// EBUSY.
    pub fn zygote(generation: &Path) -> PathBuf {
        generation.join("zygote")
    }

    /// Per-request cgroup, under the generation the request was forked in. A
    /// directory per request costs a `mkdir` and three small writes (~50 µs),
    /// and buys `cgroup.kill`: one write tears down the whole tree on timeout
    /// (open question A2 in the design doc).
    pub fn request(generation: &Path, request_id: &str) -> PathBuf {
        generation.join(format!("req-{}", sanitise(request_id)))
    }

    /// Create the top-level layout and its budgets. Idempotent.
    ///
    /// The first thing this does is move the calling process into
    /// `zygo.slice/system`, which is where the design says the supervisor
    /// belongs (§3.6) — and which is also what makes the rest possible.
    /// cgroup v2 forbids a cgroup from holding processes *and* delegating
    /// controllers to its children, so as long as Zygo's own process sits in
    /// `zygo.slice`'s parent, that parent can never enable the controllers the
    /// tenants below need (PoC 2 found this one level down; it applies at every
    /// level).
    pub fn ensure(&self, host_ram: Bytes) -> Result<()> {
        let total = Bytes(host_ram.get().saturating_sub(SYSTEM_RESERVE.get()));
        let tenant_budget = total.scaled(TENANT_BUDGET_FRACTION);

        create(&self.root)?;
        create(&self.system())?;

        // Step out of the way, then vacate anything else still in the slice.
        join_system(&self.system())?;
        vacate(&self.root, &self.system())?;

        // Delegation has to be enabled from the parent downwards: a controller
        // that the parent does not pass down cannot be enabled below it — and
        // a cgroup holding processes may not pass anything down at all.
        //
        // The parent is normally the transient scope Zygo re-executed itself
        // into, and the process that *started* the supervisor is sitting in it:
        // `zygo serve` re-execs into a scope, spawns the supervisor there, and
        // waits. So the supervisor stepping aside is not enough; its client is
        // still in the way, and the write below fails with `EBUSY` — leaving a
        // tenant cgroup with no `memory.max` and the message about delegation,
        // on a host where everything *was* delegated.
        //
        // Only Zygo's own processes are moved. Anything else in that cgroup
        // belongs to somebody, and taking it over would be a worse surprise
        // than the error; if it blocks delegation the check below still says
        // so. The verification suites have moved these processes by hand since
        // the first Raspberry Pi run, which is exactly why no suite could see
        // this: the harness was supplying what the product was missing.
        if let Some(parent) = self.root.parent() {
            vacate_ours(parent, &self.system())?;
            enable_controllers(parent, CONTROLLERS)?;
        }
        enable_controllers(&self.root, CONTROLLERS)?;

        // The reservation and the aggregate budget are protections, not
        // correctness requirements: without them a tenant is still bounded by
        // its own limits, which is what requirement N4 actually demands. They
        // are also the first things to be unavailable on a host that has not
        // delegated the memory controller, and failing here would turn a
        // degraded setup into a refusal to start. `create_tenant` is where the
        // hard check lives.
        let _ = write_file(
            &self.system().join("memory.min"),
            &SYSTEM_RESERVE.get().to_string(),
        );

        create(&self.tenants())?;
        enable_controllers(&self.tenants(), CONTROLLERS)?;
        let _ = write_file(
            &self.tenants().join("memory.max"),
            &tenant_budget.get().to_string(),
        );
        Ok(())
    }

    /// Total RAM on this host, for sizing the tenant budget.
    pub fn host_ram() -> Bytes {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|l| l.strip_prefix("MemTotal:"))
                    .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            })
            .map(|kb| Bytes(kb * 1024))
            // A conservative guess is better than refusing to start; the
            // per-tenant limits are what actually protect the host.
            .unwrap_or(Bytes(2 * 1024 * 1024 * 1024))
    }

    /// Create a tenant cgroup and apply its limits.
    ///
    /// Fails rather than silently running without limits. A cgroup whose
    /// controllers were never delegated looks perfectly normal — the directory
    /// exists, `mkdir` succeeded — but `memory.max` is simply absent and every
    /// write to it is ignored. That is risk R2, and requirement N4 says a
    /// sandbox must never start in that state.
    pub fn create_tenant(&self, name: &str, limits: &Limits) -> Result<PathBuf> {
        let dir = self.tenant(name);

        // Controllers have to be enabled at *every* level between the root and
        // the leaf; enabling them only at the top leaves the grandchild without
        // a single limit file (PoC 2).
        create(&self.tenants())?;
        enable_controllers(&self.root, CONTROLLERS)?;
        enable_controllers(&self.tenants(), CONTROLLERS)?;

        create(&dir)?;
        enable_controllers(&dir, CONTROLLERS)?;

        let missing = missing_limit_files(&dir);
        if !missing.is_empty() {
            return Err(Error::BackendUnavailable {
                backend: "ns",
                reason: format!(
                    "the cgroup {} has no {} — its controllers were not delegated",
                    dir.display(),
                    missing.join(", ")
                ),
                remedy: "delegate cgroup v2 controllers to your user session; \
                         `zygo doctor` prints the one-line fix"
                    .into(),
            });
        }

        apply(&dir, &limits.cgroup_writes())?;
        Ok(dir)
    }

    /// Create a generation under an existing tenant, with the leaf the
    /// sandbox's process will live in. Limits stay on the tenant so they cover
    /// every generation, the zygote and every per-request child together.
    pub fn create_generation(&self, name: &str) -> Result<PathBuf> {
        let dir = self.generation(name);
        create(&dir)?;
        enable_controllers(&dir, CONTROLLERS)?;
        create(&Self::zygote(&dir))?;
        Ok(dir)
    }

    /// Create a per-request cgroup. Limits are inherited from the tenant; only
    /// the wall-clock kill switch is per request.
    pub fn create_request(generation: &Path, request_id: &str) -> Result<PathBuf> {
        let dir = Self::request(generation, request_id);
        create(&dir)?;
        Ok(dir)
    }

    /// Remove a cgroup directory and its children. Safe to call twice.
    ///
    /// cgroups are removed with `rmdir`, which refuses a directory that still
    /// has child cgroups — so the leaves have to go first. Ordinary recursive
    /// deletion would not work either: the control files inside cannot be
    /// unlinked.
    pub fn remove(dir: &Path) -> Result<()> {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    Self::remove(&entry.path())?;
                }
            }
        }
        match std::fs::remove_dir(dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            // A cgroup still holding processes cannot be removed; the caller
            // has already killed them, but the kernel reaps asynchronously.
            Err(e) if e.raw_os_error() == Some(libc::EBUSY) => Ok(()),
            Err(e) => Err(Error::io(dir, e)),
        }
    }

    /// Remove tenant cgroups left behind by a supervisor that is gone.
    ///
    /// When a supervisor dies its sandboxes die with it (`PDEATHSIG`), but the
    /// directories do not: cgroups outlive their processes. A restarted
    /// supervisor would otherwise accumulate a `tenants/<name>` tree per
    /// lifetime, each with its own `req-*` leftovers, and reuse stale limits
    /// for a name it serves again.
    ///
    /// Only *empty* cgroups are removed, checked by reading `cgroup.procs`
    /// rather than by assuming. Anything still holding a process belongs to
    /// something alive, and leaving it is the safe reading — `Listener::bind`
    /// has already established that no other supervisor is listening, so this
    /// should find nothing, and if it does, the honest thing is not to kill it.
    ///
    /// Returns the names it cleaned up.
    pub fn clean_stale_tenants(&self) -> Vec<String> {
        let mut cleaned = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.tenants()) else {
            return cleaned;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            if !is_empty_subtree(&path) {
                continue;
            }
            if Self::remove(&path).is_ok()
                && !path.exists()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                cleaned.push(name.to_string());
            }
        }
        cleaned
    }
}

/// Whether a cgroup and everything beneath it holds no processes.
fn is_empty_subtree(dir: &Path) -> bool {
    if !members(dir).is_empty() {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .all(|e| is_empty_subtree(&e.path()))
}

/// Move a process into a cgroup by writing its pid to `cgroup.procs`.
pub fn attach(dir: &Path, pid: u32) -> Result<()> {
    write_file(&dir.join("cgroup.procs"), &pid.to_string())
}

/// Kill every process in a cgroup.
///
/// One write on kernel 5.14 and above, where `cgroup.kill` does it atomically.
/// Below that it falls back to [`kill_by_freezing`], which achieves the same
/// thing with three writes and a loop. Either way the guarantee is the one that
/// matters: *everything* in the cgroup dies, not just the process we happen to
/// have a pid for.
///
/// Returns whether anything was killed, so a caller can tell "nothing was
/// there" from "this kernel cannot do it".
pub fn kill(dir: &Path) -> Result<bool> {
    let path = dir.join("cgroup.kill");
    if !path.exists() {
        return kill_by_freezing(dir);
    }
    write_file(&path, "1")?;
    Ok(true)
}

/// Kill everything in a cgroup on a kernel without `cgroup.kill` (below 5.14).
///
/// Signalling the one pid we know about is not enough: a handler that forked
/// its own helpers leaves them running, still holding the result pipe open, so
/// the agent never sees end of file and never answers. Measured on 5.10 before
/// this existed — a handler that forked four spinners left all four alive and
/// wedged its function.
///
/// Freezing first is what makes the list of pids trustworthy: without it a
/// process can fork between the read and the signal, and the child is created
/// already unsignalled. Frozen tasks do not act on a pending `SIGKILL`, so the
/// thaw at the end is what actually kills them — and it has to happen even if a
/// signal failed, or the cgroup would be left frozen for ever.
fn kill_by_freezing(dir: &Path) -> Result<bool> {
    if !dir.join("cgroup.procs").exists() {
        return Ok(false);
    }
    // A cgroup with no freezer is still worth killing pid by pid; it just
    // cannot be done race-free.
    let froze = freeze(dir, true).is_ok();

    let mut signalled = false;
    for pid in members(dir) {
        // SAFETY: the pid came from this cgroup's `cgroup.procs` a moment ago.
        // A pid that has since exited is an unreaped child of ours or already
        // gone; `kill` returns ESRCH and nothing happens.
        if unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) } == 0 {
            signalled = true;
        }
    }

    if froze {
        freeze(dir, false)?;
    }
    Ok(signalled)
}

/// Pids currently in a cgroup, in the writer's own pid namespace.
pub fn members(dir: &Path) -> Vec<u32> {
    std::fs::read_to_string(dir.join("cgroup.procs"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// Freeze or thaw a subtree — how `idle_timeout` parks a warm zygote without
/// giving up its resident memory.
pub fn freeze(dir: &Path, frozen: bool) -> Result<()> {
    write_file(&dir.join("cgroup.freeze"), if frozen { "1" } else { "0" })
}

/// Peak memory usage, for per-request metrics (kernel ≥ 5.19).
pub fn peak_memory(dir: &Path) -> Option<Bytes> {
    std::fs::read_to_string(dir.join("memory.peak"))
        .ok()?
        .trim()
        .parse()
        .ok()
        .map(Bytes)
}

/// How many processes the kernel has killed here for running out of memory.
///
/// From `memory.events`, which counts events for this cgroup and its
/// descendants. Read *after* the sandbox has exited and *before* the cgroup is
/// removed: it is the only thing that can tell an out-of-memory kill from a
/// deadline kill, because both arrive as exit 137 and the wait status carries
/// nothing else. `None` when the file is absent, which is a kernel without the
/// memory controller delegated rather than a count of zero.
pub fn oom_kills(dir: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(dir.join("memory.events")).ok()?;
    let mut total = None;
    for line in text.lines() {
        // `oom_kill` counts processes killed; `oom` counts times the limit was
        // hit, which a group that recovered also reports. The kill is what a
        // caller means by "it ran out of memory".
        if let Some(rest) = line.strip_prefix("oom_kill ") {
            total = rest.trim().parse().ok();
        }
    }
    total
}

/// Apply a batch of limit writes.
pub fn apply(dir: &Path, writes: &[CgroupWrite]) -> Result<()> {
    for w in writes {
        write_file(&dir.join(w.file), &w.value)?;
    }
    Ok(())
}

/// Controllers delegated to this cgroup, from `cgroup.controllers`.
///
/// A rootless setup needs `Delegate=cpu cpuset io memory pids` on
/// `user@.service`; without it the limits silently do nothing, which is
/// requirement N4's failure mode and why `zygo doctor` checks it (risk R2).
pub fn available_controllers(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("cgroup.controllers"))
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Controllers Zygo cannot enforce limits without.
pub const REQUIRED_CONTROLLERS: &[&str] = &["memory", "pids", "cpu"];

/// Which of [`REQUIRED_CONTROLLERS`] are missing at `dir`.
pub fn missing_controllers(dir: &Path) -> Vec<&'static str> {
    let have = available_controllers(dir);
    REQUIRED_CONTROLLERS
        .iter()
        .copied()
        .filter(|c| !have.iter().any(|h| h == c))
        .collect()
}

/// Whether a sandbox could actually be given limits from `dir` — by
/// **attempting** it rather than by reading `cgroup.controllers`.
///
/// Reading is not enough, and a real host is where that shows. On a systemd
/// machine an ssh login sits in a `session-N.scope`, whose `cgroup.controllers`
/// dutifully lists everything `user.slice` delegated — and which still refuses
/// `mkdir`, because the scope itself is not delegated and systemd owns the
/// directory. `zygo doctor` reported `cgroup v2 … ok` on exactly such a host
/// while `zygo run` failed on the very next line. That is requirement N4's
/// failure mode wearing the opposite mask: not a silent lack of limits, but a
/// confident promise of them.
///
/// So this does the first thing [`Hierarchy::ensure`] does — create a child —
/// and then removes it. Nothing is moved and nothing is left behind.
///
/// `Ok` carries the controllers that are available; `Err` is a reason fit to
/// print.
pub fn probe_delegation(dir: &Path) -> std::result::Result<Vec<String>, String> {
    let missing = missing_controllers(dir);
    if !missing.is_empty() {
        return Err(format!("not delegated (missing: {})", missing.join(" ")));
    }
    let probe = dir.join(format!("zygo-doctor-{}", std::process::id()));
    // A leftover from a crashed probe would make this look like a success it
    // did not earn.
    let _ = std::fs::remove_dir(&probe);
    match std::fs::create_dir(&probe) {
        Ok(()) => {
            let _ = std::fs::remove_dir(&probe);
            Ok(available_controllers(dir))
        }
        Err(e) => Err(format!(
            "the controllers are delegated but this cgroup will not take a child ({e})"
        )),
    }
}

/// The same question, asked the way Zygo itself answers it.
///
/// A cgroup that will not take a child is not the end of the story: for the
/// commands that build a sandbox, `zygo` re-executes itself inside a
/// transient delegated scope rather than making the user type
/// `systemd-run --user --scope` every time. A check that stops at the first
/// answer therefore reports `FAIL` on a machine where `zygo serve` works,
/// which is the most misleading thing a diagnostic can do.
///
/// So this attempts the fallback instead of describing it: a scope is created
/// and the same `mkdir` is tried inside it. Nothing is left behind — the scope
/// ends with the probe.
pub fn probe_delegation_via_scope() -> std::result::Result<(), String> {
    // A raw string: the shell wants both kinds of quote and Rust must not
    // read either of them.
    let probe = r#"d=$(sed -n 's/^0:://p' /proc/self/cgroup | head -1)
p="/sys/fs/cgroup$d/zygo-doctor-$$"
mkdir "$p" && rmdir "$p""#;
    let output = std::process::Command::new("systemd-run")
        .args(["--user", "--scope", "-p", "Delegate=yes", "-q", "--"])
        .args(["/bin/sh", "-c", probe])
        .output()
        .map_err(|e| format!("systemd-run could not be started ({e})"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr)
            .lines()
            .next_back()
            .unwrap_or("systemd-run could not make a delegated scope")
            .trim()
            .to_string())
    }
}

/// What to do about a cgroup that cannot hold a sandbox.
///
/// Both halves, because on a systemd host neither works alone: the user
/// manager has to pass the controllers down, *and* whatever runs Zygo has to
/// be somewhere delegated. Applying only the first and trying again from the
/// same ssh session fails identically, which is what happened here.
pub const DELEGATION_REMEDY: &str = "mkdir -p ~/.config/systemd/user/user@.service.d && \
     printf '[Service]\\nDelegate=cpu cpuset io memory pids\\n' > \
     ~/.config/systemd/user/user@.service.d/delegate.conf && \
     systemctl --user daemon-reexec\n  \
     → then run Zygo from a delegated cgroup, not straight from an ssh \
     session: systemd-run --user --scope -p Delegate=yes -- zygo ...";

/// Controllers Zygo enables for its subtree.
const CONTROLLERS: &[&str] = &["cpu", "io", "memory", "pids"];

/// Limit files that must exist before a sandbox may start.
const REQUIRED_LIMIT_FILES: &[&str] = &["memory.max", "pids.max", "cpu.max"];

/// Which mandatory limit files are absent from `dir`.
fn missing_limit_files(dir: &Path) -> Vec<&'static str> {
    REQUIRED_LIMIT_FILES
        .iter()
        .copied()
        .filter(|f| !dir.join(f).exists())
        .collect()
}

/// Move the calling process into `system`.
///
/// Idempotent: writing a pid that is already there is a no-op.
fn join_system(system: &Path) -> Result<()> {
    // SAFETY: getpid cannot fail and has no preconditions.
    let pid = unsafe { libc::getpid() };
    match std::fs::write(system.join("cgroup.procs"), pid.to_string()) {
        Ok(()) => Ok(()),
        // Not being able to move is only fatal if the limits then fail to
        // apply, which `create_tenant` checks explicitly.
        Err(_) => Ok(()),
    }
}

/// Move every process out of `from` and into `into`.
///
/// cgroup v2 forbids a cgroup from holding processes *and* enabling controllers
/// for its children at the same time ("no internal processes"). Anything
/// already sitting in Zygo's slice therefore has to move aside before
/// `subtree_control` can be written — and `system/` is exactly where the
/// design says the supervisor belongs anyway (PoC 2).
fn vacate(from: &Path, into: &Path) -> Result<()> {
    let procs = match std::fs::read_to_string(from.join("cgroup.procs")) {
        Ok(text) => text,
        Err(_) => return Ok(()),
    };
    if procs.trim().is_empty() {
        return Ok(());
    }
    create(into)?;
    for pid in procs.split_whitespace() {
        // A process that exits between the read and the write is not an error.
        let _ = std::fs::write(into.join("cgroup.procs"), pid);
    }
    Ok(())
}

/// Move Zygo's *own* processes out of `from` and into `into`.
///
/// Same reason as [`vacate`], one level up and with a filter: the cgroup being
/// emptied is not Zygo's, so only processes running this same binary are
/// moved. A process that exits mid-scan, or one whose `/proc` entry cannot be
/// read, is skipped rather than guessed at.
fn vacate_ours(from: &Path, into: &Path) -> Result<()> {
    let procs = match std::fs::read_to_string(from.join("cgroup.procs")) {
        Ok(text) => text,
        Err(_) => return Ok(()),
    };
    let Ok(ours) = std::fs::read_link("/proc/self/exe") else {
        return Ok(());
    };
    let mut moved = false;
    for pid in procs.split_whitespace() {
        let exe = std::path::PathBuf::from(format!("/proc/{pid}/exe"));
        if std::fs::read_link(&exe).is_ok_and(|target| target == ours) {
            if !moved {
                create(into)?;
                moved = true;
            }
            let _ = std::fs::write(into.join("cgroup.procs"), pid);
        }
    }
    Ok(())
}

/// Enable controllers for children via `cgroup.subtree_control`.
///
/// Best-effort: a controller that is already enabled, or that the parent never
/// delegated, produces an error the caller cannot act on here. Enforcement of
/// "no limits, no sandbox" happens in [`Hierarchy::create_tenant`], which
/// checks that the limit files actually appeared.
fn enable_controllers(dir: &Path, controllers: &[&str]) -> Result<()> {
    let have = available_controllers(dir);
    for c in controllers {
        if have.iter().any(|h| h == *c) {
            // One controller per write: a single rejected entry would
            // otherwise discard the whole line.
            let _ = std::fs::write(dir.join("cgroup.subtree_control"), format!("+{c}"));
        }
    }
    Ok(())
}

fn create(dir: &Path) -> Result<()> {
    match std::fs::create_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(Error::BackendUnavailable {
                backend: "ns",
                reason: format!("cannot create the cgroup {}", dir.display()),
                remedy: "delegate cgroup v2 controllers to your user session; run `zygo doctor`"
                    .into(),
            })
        }
        Err(e) => Err(Error::io(dir, e)),
    }
}

fn write_file(path: &Path, value: &str) -> Result<()> {
    std::fs::write(path, value).at(path)
}

/// Make a name safe as a single path component. Tenant names come from callers
/// that may be passing through end-user input, so `../` must not be a way out
/// of the tenant subtree.
fn sanitise(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // `.` and `..` survive the filter above but are still traversal.
    if cleaned.chars().all(|c| c == '.') {
        return "_".repeat(cleaned.len().max(1));
    }
    if cleaned.is_empty() {
        return "_".to_string();
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Cpu, Duration};

    // --- cleaning up after a dead supervisor -------------------------------
    //
    // These test `is_empty_subtree`, which is where the whole decision lives.
    // The removal itself is not unit tested: `rmdir` on a real cgroup succeeds
    // with its control files in place, and on an ordinary filesystem it does
    // not, so a temporary directory cannot stand in for one. That half is
    // covered end to end in `poc/verify_supervisor.sh`, against a real kernel.

    /// A fake cgroup with the given pids in its `cgroup.procs`.
    fn fake_cgroup(dir: &Path, pids: &str) {
        std::fs::create_dir_all(dir).expect("mkdir");
        std::fs::write(dir.join("cgroup.procs"), pids).expect("procs");
    }

    #[test]
    fn a_cgroup_holding_nothing_is_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("tenant");
        fake_cgroup(&dir, "");
        assert!(is_empty_subtree(&dir));
    }

    #[test]
    fn a_cgroup_holding_a_process_is_not_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("tenant");
        fake_cgroup(&dir, "4242\n");
        assert!(!is_empty_subtree(&dir));
    }

    #[test]
    fn a_process_deep_in_the_tree_still_counts() {
        // The case a check that only looked at the top level would get wrong:
        // the tenant and its zygote are empty and only a request cgroup holds
        // anything, but the tenant is very much in use.
        let tmp = tempfile::tempdir().expect("tempdir");
        let h = Hierarchy::new(tmp.path().join("zygo.slice"));
        let g = h.generation("resize");
        fake_cgroup(&h.tenant("resize"), "");
        fake_cgroup(&g, "");
        fake_cgroup(&Hierarchy::zygote(&g), "");
        fake_cgroup(&Hierarchy::request(&g, "00000001"), "99\n");

        assert!(!is_empty_subtree(&h.tenant("resize")));
        assert!(
            h.clean_stale_tenants().is_empty(),
            "a tenant with a running request was cleaned up"
        );
        assert!(Hierarchy::zygote(&g).exists());
    }

    #[test]
    fn a_whole_empty_tree_is_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let h = Hierarchy::new(tmp.path().join("zygo.slice"));
        let g = h.generation("resize");
        fake_cgroup(&h.tenant("resize"), "");
        fake_cgroup(&g, "");
        fake_cgroup(&Hierarchy::zygote(&g), "");
        fake_cgroup(&Hierarchy::request(&g, "00000001"), "");
        assert!(is_empty_subtree(&h.tenant("resize")));
    }

    #[test]
    fn a_directory_that_is_not_there_is_empty() {
        // Racing with something else's cleanup is not an error.
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(is_empty_subtree(&tmp.path().join("gone")));
    }

    #[test]
    fn a_hierarchy_that_was_never_created_is_not_an_error() {
        // The first supervisor on a fresh machine takes this path.
        let tmp = tempfile::tempdir().expect("tempdir");
        let h = Hierarchy::new(tmp.path().join("zygo.slice"));
        assert!(h.clean_stale_tenants().is_empty());
    }

    fn limits() -> Limits {
        Limits {
            mem: Bytes::from_mib(256),
            mem_high: Bytes::from_mib(230),
            swap: Bytes(0),
            oom_group: true,
            connections: 256,
            bandwidth: None,
            cpu: Cpu(0.5),
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

    /// The path logic of `discover`, exercised without `/proc`.
    fn slice_for(current: &str) -> PathBuf {
        let current = Path::new(current);
        match current
            .ancestors()
            .find(|a| a.file_name().is_some_and(|n| n == "zygo.slice"))
        {
            Some(existing) => existing.to_path_buf(),
            None => current.join("zygo.slice"),
        }
    }

    #[test]
    fn discovery_creates_a_slice_under_the_current_cgroup() {
        assert_eq!(
            slice_for("/sys/fs/cgroup/user.slice/user-1000.slice"),
            Path::new("/sys/fs/cgroup/user.slice/user-1000.slice/zygo.slice")
        );
    }

    /// A second invocation must reuse the first one's slice instead of nesting.
    #[test]
    fn discovery_reuses_an_enclosing_slice_rather_than_nesting() {
        let expected = Path::new("/sys/fs/cgroup/launch/zygo.slice");
        assert_eq!(
            slice_for("/sys/fs/cgroup/launch/zygo.slice/system"),
            expected
        );
        assert_eq!(
            slice_for("/sys/fs/cgroup/launch/zygo.slice/tenants/acme/zygote"),
            expected
        );
        assert_eq!(slice_for("/sys/fs/cgroup/launch/zygo.slice"), expected);
    }

    #[test]
    fn layout_matches_the_design_document() {
        let h = Hierarchy::new("/sys/fs/cgroup/zygo.slice");
        assert_eq!(h.system(), Path::new("/sys/fs/cgroup/zygo.slice/system"));
        assert_eq!(h.tenants(), Path::new("/sys/fs/cgroup/zygo.slice/tenants"));
        assert_eq!(
            h.tenant("tenant-A"),
            Path::new("/sys/fs/cgroup/zygo.slice/tenants/tenant-A")
        );
        let g = h.generation("tenant-A");
        assert_eq!(g.parent().unwrap(), h.tenant("tenant-A"));
        assert!(
            g.file_name().unwrap().to_str().unwrap().starts_with('g'),
            "{}",
            g.display()
        );
        assert_eq!(Hierarchy::zygote(&g), g.join("zygote"));
        assert_eq!(Hierarchy::request(&g, "01f3"), g.join("req-01f3"));
    }

    /// Two generations of one tenant never collide, even in one process.
    #[test]
    fn generations_are_unique() {
        let h = Hierarchy::new("/cg/zygo.slice");
        let a = h.generation("t");
        let b = h.generation("t");
        assert_ne!(a, b);
        assert_eq!(a.parent(), b.parent());
    }

    #[test]
    fn tenant_names_cannot_escape_the_subtree() {
        let h = Hierarchy::new("/cg/zygo.slice");
        for evil in ["../../etc", "..", "a/b", "a\0b", "."] {
            let p = h.tenant(evil);
            assert_eq!(
                p.parent().unwrap(),
                h.tenants(),
                "`{evil}` escaped to {}",
                p.display()
            );
        }
        // And the same for request ids, which come from the same direction.
        let g = h.generation("t");
        let p = Hierarchy::request(&g, "../../x");
        assert_eq!(p.parent().unwrap(), g);
    }

    #[test]
    fn ensure_builds_the_hierarchy_with_a_system_reservation() {
        let tmp = tempfile::tempdir().unwrap();
        let h = Hierarchy::new(tmp.path().join("zygo.slice"));
        h.ensure(Bytes(64 * (1 << 30))).unwrap();

        assert!(h.system().is_dir());
        assert!(h.tenants().is_dir());

        let sys_min = std::fs::read_to_string(h.system().join("memory.min")).unwrap();
        assert_eq!(sys_min, SYSTEM_RESERVE.get().to_string());

        let budget = std::fs::read_to_string(h.tenants().join("memory.max")).unwrap();
        let expected = Bytes(64 * (1 << 30) - SYSTEM_RESERVE.get()).scaled(TENANT_BUDGET_FRACTION);
        assert_eq!(budget, expected.get().to_string());
    }

    /// Stand in for a delegated hierarchy: on a real cgroupfs the kernel
    /// creates the limit files when a controller is enabled.
    fn fake_delegation(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("cgroup.controllers"), "cpu io memory pids\n").unwrap();
        for f in ["memory.max", "pids.max", "cpu.max"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
    }

    #[test]
    fn creating_a_tenant_writes_every_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let h = Hierarchy::new(tmp.path().join("zygo.slice"));
        h.ensure(Bytes(8 * (1 << 30))).unwrap();
        fake_delegation(&h.tenant("acme"));

        let dir = h.create_tenant("acme", &limits()).unwrap();

        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
        assert_eq!(read("memory.max"), (256 * (1 << 20)).to_string());
        assert_eq!(read("memory.swap.max"), "0");
        assert_eq!(read("memory.oom.group"), "1");
        assert_eq!(read("pids.max"), "64");
        assert_eq!(read("cpu.max"), "50000 100000");
    }

    /// Replacing a function must not let the old sandbox's teardown reach the
    /// new one: each gets a generation of its own under the shared tenant, and
    /// removing one leaves the other — and the tenant's limits — in place.
    #[test]
    fn generations_of_one_tenant_are_torn_down_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let h = Hierarchy::new(tmp.path().join("zygo.slice"));
        h.ensure(Bytes(8 * (1 << 30))).unwrap();
        fake_delegation(&h.tenant("acme"));
        h.create_tenant("acme", &limits()).unwrap();

        let old = h.create_generation("acme").unwrap();
        let new = h.create_generation("acme").unwrap();
        assert_ne!(old, new);
        assert!(Hierarchy::zygote(&old).is_dir());
        assert!(Hierarchy::zygote(&new).is_dir());

        Hierarchy::remove(&old).unwrap();
        assert!(!old.exists());
        assert!(Hierarchy::zygote(&new).is_dir(), "the replacement survived");
        assert!(
            h.tenant("acme").join("memory.max").exists(),
            "limits stay on the tenant"
        );
    }

    /// Requirement N4: a sandbox must never start with limits that are not
    /// actually enforced. Without delegation the directory exists and `mkdir`
    /// succeeds, but no limit file does — which is risk R2's exact shape.
    #[test]
    fn a_tenant_without_delegated_controllers_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let h = Hierarchy::new(tmp.path().join("zygo.slice"));
        h.ensure(Bytes(8 * (1 << 30))).unwrap();

        let err = h.create_tenant("acme", &limits()).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("not delegated"), "{text}");
        assert!(text.contains("memory.max"), "{text}");
        assert_eq!(err.exit_code(), 125);
    }

    #[test]
    fn missing_limit_files_are_listed_individually() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(missing_limit_files(tmp.path()), REQUIRED_LIMIT_FILES);

        std::fs::write(tmp.path().join("memory.max"), "").unwrap();
        assert_eq!(missing_limit_files(tmp.path()), ["pids.max", "cpu.max"]);
    }

    /// cgroup v2's "no internal processes" rule: controllers cannot be enabled
    /// for children while the cgroup still holds processes of its own.
    #[test]
    fn ensure_moves_existing_processes_into_the_system_cgroup() {
        let tmp = tempfile::tempdir().unwrap();
        let h = Hierarchy::new(tmp.path().join("zygo.slice"));
        std::fs::create_dir_all(h.root()).unwrap();
        std::fs::write(h.root().join("cgroup.procs"), "111\n222\n").unwrap();

        h.ensure(Bytes(8 * (1 << 30))).unwrap();

        let moved = std::fs::read_to_string(h.system().join("cgroup.procs")).unwrap();
        // Each pid is written separately, so the fake file holds the last one;
        // what matters is that the move happened at all.
        assert!(!moved.trim().is_empty(), "processes were not moved aside");
    }

    #[test]
    fn vacating_an_empty_cgroup_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let from = tmp.path().join("from");
        let into = tmp.path().join("into");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::write(from.join("cgroup.procs"), "\n").unwrap();

        vacate(&from, &into).unwrap();
        assert!(!into.exists(), "nothing to move, nothing to create");
    }

    #[test]
    fn attach_writes_the_pid_to_cgroup_procs() {
        let tmp = tempfile::tempdir().unwrap();
        attach(tmp.path(), 4242).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("cgroup.procs")).unwrap(),
            "4242"
        );
    }

    #[test]
    fn kill_reports_whether_the_fast_path_exists() {
        let tmp = tempfile::tempdir().unwrap();
        // Kernels before 5.14 have no `cgroup.kill`; the caller must fall back.
        assert!(!kill(tmp.path()).unwrap());

        std::fs::write(tmp.path().join("cgroup.kill"), "").unwrap();
        assert!(kill(tmp.path()).unwrap());
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("cgroup.kill")).unwrap(),
            "1"
        );
    }

    #[test]
    fn freeze_and_thaw_write_the_expected_values() {
        let tmp = tempfile::tempdir().unwrap();
        freeze(tmp.path(), true).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("cgroup.freeze")).unwrap(),
            "1"
        );
        freeze(tmp.path(), false).unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("cgroup.freeze")).unwrap(),
            "0"
        );
    }

    #[test]
    fn missing_controllers_are_detected() {
        let tmp = tempfile::tempdir().unwrap();
        // No file at all: everything is missing (risk R2).
        assert_eq!(missing_controllers(tmp.path()), REQUIRED_CONTROLLERS);

        std::fs::write(
            tmp.path().join("cgroup.controllers"),
            "cpu io memory pids\n",
        )
        .unwrap();
        assert!(missing_controllers(tmp.path()).is_empty());

        std::fs::write(tmp.path().join("cgroup.controllers"), "cpu io\n").unwrap();
        assert_eq!(missing_controllers(tmp.path()), ["memory", "pids"]);
    }

    #[test]
    fn remove_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("gone");
        std::fs::create_dir(&dir).unwrap();
        Hierarchy::remove(&dir).unwrap();
        Hierarchy::remove(&dir).unwrap();
    }

    #[test]
    fn peak_memory_is_optional() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(peak_memory(tmp.path()), None);
        std::fs::write(tmp.path().join("memory.peak"), "123456\n").unwrap();
        assert_eq!(peak_memory(tmp.path()), Some(Bytes(123456)));
    }
}
