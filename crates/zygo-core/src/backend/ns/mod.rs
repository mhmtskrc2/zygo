//! The `ns` backend: namespaces + cgroups + seccomp + Landlock
//! (design doc §3.3).
//!
//! This is the backend the efficiency argument rests on: a sandbox is an
//! ordinary process the kernel has been told to constrain (principle P1), built
//! with no daemon, no RPC and no mount orchestration on the request path.
//!
//! The launch is split across three modules because the middle of it runs under
//! async-signal-safety rules:
//!
//! - [`prepare`] — the parent renders the whole plan into `CString`s and raw
//!   pointers while it is still allowed to allocate.
//! - [`clone`] — `clone3`, which unlike `unshare` puts the child *in* the new
//!   pid namespace so it can mount its own `/proc`.
//! - [`child`] — the child's syscall-only sequence, ending in `execve`.
//!
//! The parent keeps the two jobs the child cannot do for itself: writing
//! `uid_map`/`gid_map` (a process cannot map itself into a subordinate range)
//! and placing the child in its cgroup before it runs any tenant code.

pub mod child;
pub mod clone;
pub mod enter;
pub mod idmap;
pub mod landlock;
pub mod prepare;
pub mod seccomp;
pub mod syscalls;

use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{Availability, Backend, Sandbox};
use crate::cgroup;
use crate::doctor;
use crate::error::{Error, Result};
use crate::sandbox::{SandboxConfig, SandboxState};
use crate::spec::Isolation;

pub use idmap::{IdMapEntry, NamespaceSet, SubIdRange, id_map, parse_subid, render_id_map};

/// How long a sandbox has to reach `execve` — or say why it could not.
///
/// Everything before that point is mounts, a `pivot_root` and hardening:
/// milliseconds on a laptop, and seconds at worst on a small board with a
/// cold page cache. Finite because the launcher is one thread: a child that
/// hangs here is a supervisor that never serves anything again.
pub const START_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a killed sandbox has to actually exit.
///
/// Generous for a process that has been sent `SIGKILL`: anything not in an
/// uninterruptible sleep goes within milliseconds. What the number is really
/// for is the case where the signal reached nothing — see [`NsSandbox::
/// wait_within`].
const REAP_TIMEOUT: Duration = Duration::from_secs(10);

/// Read a pipe to end of file, giving up after `budget`.
///
/// `std::fs::File` has no read timeout, so the descriptor is polled and then
/// read; `poll` is also the only way to tell "nothing yet" from "nothing ever"
/// on a pipe whose writer is still open.
fn read_to_end_within(fd: OwnedFd, budget: Duration) -> Result<Vec<u8>> {
    use rustix::event::{PollFd, PollFlags, poll};

    let deadline = Instant::now() + budget;
    let mut payload = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Error::BackendUnavailable {
                backend: "ns",
                reason: format!(
                    "the sandbox did not start within {}s: it neither ran its \
                     program nor reported a failure",
                    budget.as_secs()
                ),
                remedy: "run `zygo doctor`; if this repeats, please report it with \
                         the spec — a sandbox should reach `execve` in milliseconds"
                    .into(),
            });
        }
        let mut fds = [PollFd::new(&fd, PollFlags::IN)];
        let left = rustix::fs::Timespec {
            tv_sec: left.as_secs() as _,
            tv_nsec: left.subsec_nanos() as _,
        };
        match poll(&mut fds, Some(&left)) {
            Ok(0) => continue, // timed out; the deadline check above ends it
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                return Err(Error::primitive(
                    "poll launch status",
                    "internal launcher error",
                    e.into(),
                ));
            }
        }
        match rustix::io::read(&fd, &mut buf) {
            Ok(0) => return Ok(payload), // end of file: the child exec'd
            Ok(n) => payload.extend_from_slice(&buf[..n]),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                return Err(Error::primitive(
                    "read launch status",
                    "internal launcher error",
                    e.into(),
                ));
            }
        }
    }
}

/// The `ns` backend.
#[derive(Debug, Default)]
pub struct NsBackend {
    /// Where per-tenant cgroups are created. `None` means limits are not
    /// applied — only reachable when the caller explicitly accepted that.
    hierarchy: Option<cgroup::Hierarchy>,
}

impl NsBackend {
    pub fn new() -> Self {
        Self {
            hierarchy: cgroup::Hierarchy::discover().ok(),
        }
    }

    /// Use a specific cgroup hierarchy instead of discovering one. For tests.
    pub fn with_hierarchy(hierarchy: cgroup::Hierarchy) -> Self {
        Self {
            hierarchy: Some(hierarchy),
        }
    }
}

impl Backend for NsBackend {
    fn name(&self) -> &'static str {
        "ns"
    }

    fn availability(&self) -> Availability {
        if !clone::is_available() {
            return Availability::unavailable(
                "clone3 is unavailable (kernel < 5.3, or a host seccomp policy blocks it)",
                "run on Linux 5.11+, or use --isolation vm",
            );
        }

        let report = doctor::run();
        if report.supports(Isolation::Ns) {
            return Availability::Available;
        }
        // Report the first blocking check with its own remedy, rather than a
        // generic "unsupported" the user cannot act on.
        match report
            .checks
            .iter()
            .find(|c| c.status == doctor::Status::Failed)
        {
            Some(c) => Availability::unavailable(
                format!("{}: {}", c.name, c.detail),
                c.remedy
                    .clone()
                    .unwrap_or_else(|| "run `zygo doctor`".into()),
            ),
            None => Availability::unavailable(
                "the environment does not meet the requirements for `ns`",
                "run `zygo doctor` for the details",
            ),
        }
    }

    fn start(&self, config: &SandboxConfig) -> Result<Box<dyn Sandbox>> {
        let sandbox = launch(config, self.hierarchy.as_ref())?;
        Ok(Box::new(sandbox))
    }
}

/// A running `ns` sandbox.
pub struct NsSandbox {
    pid: u32,
    state: SandboxState,
    /// Removed when the sandbox is reaped.
    cgroup_dir: Option<PathBuf>,
    /// Wall-clock budget, enforced by [`NsSandbox::wait`].
    timeout: Duration,
    exit_code: Option<i32>,
    /// Open only for a held (warm-exec) sandbox: what a request enters.
    namespaces: Option<crate::backend::NamespaceFds>,
    /// Where the `pasta` serving this sandbox wrote its pid, when it has one.
    /// `pasta` holds the network namespace open, so it outlives the sandbox
    /// unless it is told to stop.
    pasta_pid_file: Option<PathBuf>,
    /// The egress resolver, for a sandbox that has one. Dropped with the
    /// sandbox, which stops its thread.
    dns: Option<crate::net::DnsProxy>,
    /// `/run/secrets` inside this sandbox, handed out by the child before it
    /// hardened. The only way in: see `child::hand_out_secrets_dir`.
    secrets_dir: Option<std::os::fd::OwnedFd>,
}

impl NsSandbox {
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Send a signal to the sandbox's init process.
    ///
    /// Signalling pid 1 of a pid namespace is how `SIGINT` and `SIGTERM` are
    /// forwarded from the CLI; the kernel tears down the whole namespace when
    /// its init dies.
    pub fn signal(&self, signal: i32) -> Result<()> {
        let rc = unsafe { libc::kill(self.pid as libc::pid_t, signal) };
        if rc != 0 {
            return Err(Error::primitive(
                "kill",
                "the sandbox may have already exited",
                std::io::Error::last_os_error(),
            ));
        }
        Ok(())
    }

    /// `waitpid`, with an end to it.
    ///
    /// The blocking form has no deadline, and the thread that calls it is the
    /// launcher — the one thread that starts sandboxes, one at a time. A
    /// child that will not die therefore does not fail one request; it stops
    /// the supervisor from serving anything, ever, while still answering
    /// `ping` and looking healthy.
    ///
    /// So it polls, sends one more `SIGKILL` halfway through in case the
    /// first went somewhere that no longer existed, and gives up with an
    /// error naming the pid. Giving up leaks a process, which is bad; parking
    /// the launcher leaks the whole supervisor, which is worse, and says
    /// nothing while it does it.
    fn wait_within(&self, status: &mut libc::c_int, budget: Duration) -> Result<libc::pid_t> {
        let pid = self.pid as libc::pid_t;
        let deadline = Instant::now() + budget;
        let mut nudged = false;
        loop {
            // SAFETY: `status` is a live local; `WNOHANG` cannot block.
            let rc = unsafe { libc::waitpid(pid, status, libc::WNOHANG) };
            if rc != 0 {
                return Ok(rc);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::primitive(
                    "waitpid",
                    format!(
                        "sandbox process {pid} did not exit within {}s of being killed; \
                         it has been left running rather than blocking the launcher",
                        budget.as_secs()
                    ),
                    std::io::Error::from(std::io::ErrorKind::TimedOut),
                ));
            }
            if !nudged && left < budget / 2 {
                nudged = true;
                let _ = self.signal(libc::SIGKILL);
            }
            std::thread::sleep(Duration::from_millis(10).min(left));
        }
    }

    /// Kill everything in the sandbox.
    ///
    /// Prefers `cgroup.kill`, which takes down the whole subtree in one write.
    /// On kernels before 5.14 it falls back to killing the pid namespace's
    /// init, which the kernel turns into a SIGKILL for every member.
    fn kill_tree(&mut self) -> Result<()> {
        if let Some(dir) = &self.cgroup_dir {
            let _ = cgroup::kill(dir);
        }
        // *And* the process itself, not "or". `cgroup::kill` reports that the
        // write to `cgroup.kill` succeeded, which it does on an empty cgroup
        // too — and a sandbox that failed early has not been attached to one
        // yet. Taking that as proof of death meant no signal was ever sent,
        // and the blocking `waitpid` below then parked the launcher thread
        // for ever. Every later `serve` queued behind it: a Raspberry Pi
        // supervisor sat like that for fourteen minutes with a live child it
        // had already decided was dead.
        //
        // Sending both is free and always safe: the child has not been reaped,
        // so its pid is still ours and cannot have been reused.
        let _ = self.signal(libc::SIGKILL);
        Ok(())
    }

    fn reap(&mut self, block: bool) -> Result<Option<i32>> {
        if let Some(code) = self.exit_code {
            return Ok(Some(code));
        }

        let mut status: libc::c_int = 0;
        let rc = if block {
            self.wait_within(&mut status, REAP_TIMEOUT)?
        } else {
            // SAFETY: `status` is a live local; `WNOHANG` cannot block.
            unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, libc::WNOHANG) }
        };

        if rc == 0 {
            return Ok(None); // still running
        }
        if rc < 0 {
            return Err(Error::primitive(
                "waitpid",
                "the sandbox process could not be reaped",
                std::io::Error::last_os_error(),
            ));
        }

        let code = exit_code_of(status);
        self.exit_code = Some(code);
        self.state = SandboxState::Cold;
        // The resolver first: its thread may be mid-way through handing an
        // address to `nft` inside a namespace that is about to go.
        drop(self.dns.take());
        // Before the cgroup, because `pasta` is not in it: it runs on the host
        // as an ordinary process of this user, and nothing else would end it.
        if let Some(file) = &self.pasta_pid_file {
            crate::net::stop_pasta(file);
        }
        if let Some(dir) = &self.cgroup_dir {
            let _ = cgroup::Hierarchy::remove(dir);
            // The tenant above it is left for its other generations; if this
            // was the last, the now-empty directory goes too. `remove_dir` is
            // not recursive, so a tenant that still holds a generation — a
            // replacement — stays, ENOTEMPTY and all.
            if let Some(tenant) = dir.parent() {
                let _ = std::fs::remove_dir(tenant);
            }
        }
        Ok(Some(code))
    }
}

/// Convert a `wait` status into a shell-style exit code.
fn exit_code_of(status: libc::c_int) -> i32 {
    if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        libc::WEXITSTATUS(status)
    }
}

impl Sandbox for NsSandbox {
    fn cgroup(&self) -> Option<&std::path::Path> {
        self.cgroup_dir.as_deref()
    }

    fn pid(&self) -> u32 {
        self.pid
    }

    fn namespaces(&self) -> Option<&crate::backend::NamespaceFds> {
        self.namespaces.as_ref()
    }

    fn secrets_dir(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.secrets_dir.as_ref().map(std::os::fd::AsFd::as_fd)
    }

    fn state(&self) -> SandboxState {
        self.state
    }

    fn wait(&mut self) -> Result<i32> {
        if self.timeout.is_zero() {
            return self.reap(true).map(|c| c.unwrap_or(0));
        }

        // Poll rather than block, so the wall-clock limit can be enforced.
        // The supervisor will replace this with a timerfd in phase 2; for a
        // one-shot `run` the polling cost is irrelevant next to the program.
        let deadline = Instant::now() + self.timeout;
        let mut backoff = Duration::from_micros(200);
        loop {
            if let Some(code) = self.reap(false)? {
                return Ok(code);
            }
            if Instant::now() >= deadline {
                self.kill_tree()?;
                let _ = self.reap(true);
                // `ETIMEDOUT` is not decoration: `Error::exit_code` reads it to
                // report 137 rather than 125, so a caller can tell "killed for
                // running too long" from "this host cannot run sandboxes".
                return Err(Error::Primitive {
                    operation: "the sandbox",
                    remedy: format!(
                        "raise `timeout` if {:?} is not long enough for this work",
                        self.timeout
                    ),
                    source: std::io::Error::from_raw_os_error(libc::ETIMEDOUT),
                });
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(Duration::from_millis(20));
        }
    }

    fn kill(&mut self) -> Result<()> {
        self.kill_tree()?;
        let _ = self.reap(true);
        Ok(())
    }
}

impl Drop for NsSandbox {
    fn drop(&mut self) {
        // A sandbox whose handle is dropped must not outlive it. `PDEATHSIG`
        // covers the launcher dying, but not the handle simply going out of
        // scope while the process lives on.
        if self.exit_code.is_none() {
            let _ = self.kill_tree();
            let _ = self.reap(true);
        }
    }
}

/// Start a sandbox.
pub fn launch(config: &SandboxConfig, hierarchy: Option<&cgroup::Hierarchy>) -> Result<NsSandbox> {
    // Everything that can allocate happens here, before the clone.
    let mut plan = prepare::prepare(config).map_err(|e| Error::Primitive {
        operation: "prepare launch",
        remedy: "the sandbox configuration contains a path that cannot be passed to the kernel"
            .into(),
        source: std::io::Error::other(e.to_string()),
    })?;

    let namespaces = NamespaceSet::for_network(config.network);
    let outer_uid = unsafe { libc::getuid() };
    let outer_gid = unsafe { libc::getgid() };
    // Captured *before* the clone: inside a user namespace with no map yet,
    // `getuid()` returns the overflow uid and mapping that is rejected. PoC 1
    // found this the hard way.
    let sub = idmap::subuid_range_for_current_user();
    let uid_map = idmap::render_id_map(&idmap::id_map(config.uid, outer_uid, sub));
    let gid_map = idmap::render_id_map(&idmap::id_map(config.gid, outer_gid, sub));

    // The tenant carries the limits and outlives this sandbox; the generation
    // is what this one launch attaches to, kills and removes, so retiring it
    // cannot reach a replacement started under the same name.
    let generation = match hierarchy {
        Some(h) => {
            // Builds `zygo.slice/{system,tenants}` and moves this process into
            // `system`, without which the controllers below cannot be
            // delegated. Idempotent, so every launch may call it.
            h.ensure(cgroup::Hierarchy::host_ram())?;
            h.create_tenant(&config.id.tenant, &config.limits)?;
            Some(h.create_generation(&config.id.tenant)?)
        }
        None => None,
    };

    // Close-on-exec, like every other pipe here. It stays open across the
    // spawns of `newuidmap`, `nft` and the long-lived `pasta` below, and a
    // `pasta` holding the write end means the parent never sees end of file
    // on it — quite apart from handing an unrelated process a descriptor into
    // this launch (S-03, 2026-09-21 review). The child gets its copy through
    // `clone`, which does not exec, so the flag costs it nothing.
    let (ready_read, ready_write) = pipe_cloexec()?;
    let (err_read, err_write) = pipe_cloexec()?;

    // A held sandbox hands `/run/secrets` back over this pair. Only a held
    // one: an agent is `execve`d, which resets `PR_SET_DUMPABLE`, so the
    // supervisor can still reach its `/proc`. The init of a warm-exec sandbox
    // never execs and is deliberately unreadable, which is why a descriptor
    // is the only route in.
    let secrets_pair = if config.hold {
        Some(
            std::os::unix::net::UnixStream::pair()
                .map_err(|e| Error::primitive("socketpair", "internal launcher error", e))?,
        )
    } else {
        None
    };
    if let Some((_, theirs)) = &secrets_pair {
        plan.secrets_fd = Some(theirs.as_raw_fd());
    }

    // SAFETY: `plan` is fully materialised, the child touches only syscalls,
    // and both pipe ends are owned here.
    let result = unsafe { clone::clone3(namespaces.clone_flags()) }.map_err(|e| {
        Error::primitive(
            "clone3",
            "could not create the sandbox namespaces; run `zygo doctor`",
            e,
        )
    })?;

    match result {
        clone::CloneResult::Child => {
            // Nothing below may allocate. `child_main` never returns.
            drop(ready_write);
            drop(err_read);
            if let Some((ours, _)) = secrets_pair {
                drop(ours);
            }
            unsafe { child::child_main(&plan, ready_read.as_raw_fd(), err_write.as_raw_fd()) }
        }

        clone::CloneResult::Parent { child: pid } => {
            drop(ready_read);
            drop(err_write);
            // The child's end belongs to the child; holding a copy here would
            // stop `recv` ever reporting the child's failure as end of file.
            let secrets_sock = secrets_pair.map(|(ours, theirs)| {
                drop(theirs);
                ours
            });

            let mut sandbox = NsSandbox {
                pid,
                state: SandboxState::Starting,
                cgroup_dir: generation.clone(),
                timeout: config.limits.timeout.get(),
                exit_code: None,
                namespaces: None,
                pasta_pid_file: None,
                dns: None,
                secrets_dir: None,
            };

            // For a held sandbox, open its namespaces now, while the child is
            // parked waiting for its id maps. A namespace is the same object
            // before and after the child pivots into it, so nothing is lost by
            // taking the descriptors this early — and this is the one moment
            // the child is guaranteed still dumpable, so no capability
            // argument is needed for `/proc/<pid>/ns` to be readable.
            if config.hold {
                match crate::backend::NamespaceFds::open(pid) {
                    Ok(fds) => sandbox.namespaces = Some(fds),
                    Err(e) => {
                        let _ = sandbox.kill();
                        return Err(e);
                    }
                }
            }

            // The child is blocked waiting for its identity. Give it one, then
            // put it in its cgroup — both must happen before it runs any code.
            let identity = write_id_maps(pid, &uid_map, &gid_map);
            if let Some(dir) = &generation
                && identity.is_ok()
                && let Err(e) = cgroup::attach(&cgroup::Hierarchy::zygote(dir), pid)
            {
                let _ = sandbox.kill();
                return Err(e);
            }

            // The network, while the child is still parked. `pasta` enters the
            // user namespace, so it has to come after the id maps; the ruleset
            // has to come before the child is released, or a sandbox would be
            // briefly reachable with no allowlist at all. Any failure here
            // takes the sandbox down rather than starting it unconfined.
            if identity.is_ok()
                && crate::net::needs_configuration(config.network)
                && let Err(e) = configure_network(config, pid, &mut sandbox)
            {
                let _ = sandbox.kill();
                return Err(e);
            }

            let signal = if identity.is_ok() {
                child::READY_OK
            } else {
                child::READY_FAILED
            };
            let _ = write_all(ready_write.as_raw_fd(), &[signal]);
            drop(ready_write);

            if let Err(e) = identity {
                let _ = sandbox.kill();
                return Err(e);
            }

            // The write end is CLOEXEC, so a successful execve closes it and
            // this read sees EOF. Anything else is the child's failure report.
            //
            // Bounded: the launcher thread runs one sandbox at a time, so a
            // child that neither execs nor reports leaves every later `serve`
            // queued behind it for ever. That is what a wedged supervisor on
            // a Raspberry Pi turned out to be — this thread parked in
            // `pipe_read` with no end in sight.
            let payload = match read_to_end_within(err_read, START_TIMEOUT) {
                Ok(payload) => payload,
                Err(e) => {
                    let _ = sandbox.reap(true);
                    return Err(e);
                }
            };

            if !payload.is_empty() {
                let _ = sandbox.reap(true);
                return Err(launch_failure(&payload));
            }

            // The sandbox is up, so the child sent this before it hardened and
            // it is sitting in the socket buffer. Bounded anyway: an unbounded
            // read here would be one more way for a launcher thread to stop
            // for ever, which is a lesson this file has already learned once.
            if let Some(sock) = secrets_sock {
                let _ = sock.set_read_timeout(Some(Duration::from_secs(10)));
                match crate::net::linux::recv_fd(sock.as_raw_fd()) {
                    Ok(dir) => sandbox.secrets_dir = Some(dir),
                    Err(e) => {
                        let _ = sandbox.reap(true);
                        return Err(Error::primitive(
                            "recvmsg",
                            "the sandbox did not hand back /run/secrets; \
                             secrets cannot be delivered to it",
                            e,
                        ));
                    }
                }
            }

            sandbox.state = SandboxState::Warm;
            Ok(sandbox)
        }
    }
}

/// Hand the sandbox's network namespace to `pasta` and install its allowlist.
///
/// The pid file is recorded on the sandbox before `pasta` is started, so a
/// `pasta` that came up and then failed the ruleset step is still stopped when
/// the sandbox is torn down.
fn configure_network(config: &SandboxConfig, pid: u32, sandbox: &mut NsSandbox) -> Result<()> {
    let pid_file = config
        .pasta_pid_file
        .clone()
        .ok_or_else(|| Error::BackendUnavailable {
            backend: "network",
            reason: format!(
                "`network = \"{}\"` needs somewhere to record pasta's pid",
                config.network
            ),
            remedy: "this is a Zygo bug: the caller did not set `pasta_pid_file`".into(),
        })?;
    sandbox.pasta_pid_file = Some(pid_file.clone());
    sandbox.dns = crate::net::configure(
        pid,
        config.network,
        &config.allow,
        &config.allow_resolved,
        config.allow_private_net,
        &config.limits,
        &pid_file,
    )?;
    Ok(())
}

/// Turn the child's `(step, errno)` report into a message that names the
/// primitive and, where one exists, the fix.
fn launch_failure(payload: &[u8]) -> Error {
    let Some((step, errno)) = child::decode_failure(payload) else {
        return Error::Primitive {
            operation: "sandbox startup",
            remedy: "the sandbox failed to start and reported nothing usable".into(),
            source: std::io::Error::other("unrecognised launch failure"),
        };
    };

    Error::Primitive {
        operation: leak_step(step),
        remedy: step
            .remedy(errno)
            .unwrap_or("run `zygo doctor` to check this host's requirements")
            .to_string(),
        source: std::io::Error::from_raw_os_error(errno),
    }
}

/// `Error::Primitive` wants a `&'static str`; the step descriptions already are.
fn leak_step(step: prepare::Step) -> &'static str {
    step.describe()
}

/// Write `uid_map` and `gid_map` for the child.
///
/// A process may map only its own uid unless it holds `CAP_SETUID` in the
/// parent namespace, so a subordinate range has to go through the setuid
/// helpers `newuidmap`/`newgidmap`. Without them the map degenerates to a
/// single identity entry: the sandbox still runs, but every tenant shares one
/// host uid and uid-level separation between tenants is lost.
fn write_id_maps(pid: u32, uid_map: &str, gid_map: &str) -> Result<()> {
    let multi_line = uid_map.lines().count() > 1;

    if multi_line && helper_available("newuidmap") {
        run_id_helper("newuidmap", pid, uid_map)?;
        run_id_helper("newgidmap", pid, gid_map)?;
        return Ok(());
    }

    // `setgroups` must be denied before `gid_map` can be written by an
    // unprivileged process.
    let _ = std::fs::write(format!("/proc/{pid}/setgroups"), "deny");

    // Without the helpers exactly one entry may be written: the caller's own.
    // Which line that is depends on the sandbox user — see `identity_line`.
    let (uid_line, gid_line) = if multi_line {
        (
            idmap::identity_line(uid_map, unsafe { libc::getuid() }),
            idmap::identity_line(gid_map, unsafe { libc::getgid() }),
        )
    } else {
        (uid_map.to_string(), gid_map.to_string())
    };

    std::fs::write(format!("/proc/{pid}/uid_map"), &uid_line).map_err(|e| {
        Error::primitive(
            "write uid_map",
            "the kernel refused the user namespace mapping; run `zygo doctor`",
            e,
        )
    })?;
    std::fs::write(format!("/proc/{pid}/gid_map"), &gid_line).map_err(|e| {
        Error::primitive(
            "write gid_map",
            "the kernel refused the group namespace mapping; run `zygo doctor`",
            e,
        )
    })?;
    Ok(())
}

fn helper_available(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join(name).is_file()))
}

fn run_id_helper(helper: &str, pid: u32, map: &str) -> Result<()> {
    let mut args: Vec<String> = vec![pid.to_string()];
    for line in map.lines() {
        args.extend(line.split_whitespace().map(str::to_string));
    }
    let output = std::process::Command::new(helper)
        .args(&args)
        .output()
        .map_err(|e| {
            Error::primitive(
                "run newuidmap",
                "install the uidmap package, or run without a subordinate id range",
                e,
            )
        })?;
    if !output.status.success() {
        return Err(Error::Primitive {
            operation: "run newuidmap",
            remedy: format!(
                "{helper} refused the mapping: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            source: std::io::Error::from_raw_os_error(libc::EPERM),
        });
    }
    Ok(())
}

/// A pipe whose write end closes on `execve`, which is how a successful launch
/// is signalled without the child sending anything.
fn pipe_cloexec() -> Result<(OwnedFd, OwnedFd)> {
    rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
        .map_err(|e| Error::primitive("pipe", "internal launcher error", e.into()))
}

fn write_all(fd: i32, buf: &[u8]) -> std::io::Result<()> {
    let mut written = 0;
    while written < buf.len() {
        let n = unsafe {
            libc::write(
                fd,
                buf[written..].as_ptr() as *const libc::c_void,
                buf.len() - written,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        written += n as usize;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_shell_convention() {
        // Normal exit with status 3.
        assert_eq!(exit_code_of(3 << 8), 3);
        // Killed by SIGKILL (9).
        assert_eq!(exit_code_of(libc::SIGKILL), 128 + 9);
    }

    #[test]
    fn an_unrecognised_failure_payload_still_produces_an_error() {
        let err = launch_failure(&[0xff; 8]);
        assert!(err.to_string().contains("failed to start"), "{err}");
    }

    #[test]
    fn a_decoded_failure_names_the_step_and_the_fix() {
        let mut payload = [0u8; 8];
        payload[..4].copy_from_slice(&(prepare::Step::MountRoot as u32).to_ne_bytes());
        payload[4..].copy_from_slice(&libc::EINVAL.to_ne_bytes());

        let err = launch_failure(&payload);
        let text = err.to_string();
        assert!(text.contains("mounting the image"), "{text}");
        assert!(text.contains("5.11"), "{text}");
        assert_eq!(err.exit_code(), 125);
    }
}
