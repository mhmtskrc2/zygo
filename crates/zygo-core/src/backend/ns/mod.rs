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
pub mod idmap;
pub mod landlock;
pub mod prepare;
pub mod seccomp;
pub mod syscalls;

use std::io::Read;
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

    /// Kill everything in the sandbox.
    ///
    /// Prefers `cgroup.kill`, which takes down the whole subtree in one write.
    /// On kernels before 5.14 it falls back to killing the pid namespace's
    /// init, which the kernel turns into a SIGKILL for every member.
    fn kill_tree(&mut self) -> Result<()> {
        let killed_via_cgroup = match &self.cgroup_dir {
            Some(dir) => cgroup::kill(dir).unwrap_or(false),
            None => false,
        };
        if !killed_via_cgroup {
            let _ = self.signal(libc::SIGKILL);
        }
        Ok(())
    }

    fn reap(&mut self, block: bool) -> Result<Option<i32>> {
        if let Some(code) = self.exit_code {
            return Ok(Some(code));
        }

        let mut status: libc::c_int = 0;
        let flags = if block { 0 } else { libc::WNOHANG };
        let rc = unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, flags) };

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
        if let Some(dir) = &self.cgroup_dir {
            let _ = cgroup::Hierarchy::remove(dir);
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
    fn pid(&self) -> u32 {
        self.pid
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
                return Err(Error::Primitive {
                    operation: "timeout",
                    remedy: format!(
                        "the sandbox exceeded its {:?} budget and was killed; \
                         raise `timeout` if the work legitimately takes longer",
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
    let plan = prepare::prepare(config).map_err(|e| Error::Primitive {
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

    let tenant_cgroup = match hierarchy {
        Some(h) => {
            // Builds `zygo.slice/{system,tenants}` and moves this process into
            // `system`, without which the controllers below cannot be
            // delegated. Idempotent, so every launch may call it.
            h.ensure(cgroup::Hierarchy::host_ram())?;
            Some(h.create_tenant(&config.id.tenant, &config.limits)?)
        }
        None => None,
    };

    let (ready_read, ready_write) = pipe()?;
    let (err_read, err_write) = pipe_cloexec()?;

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
            unsafe { child::child_main(&plan, ready_read.as_raw_fd(), err_write.as_raw_fd()) }
        }

        clone::CloneResult::Parent { child: pid } => {
            drop(ready_read);
            drop(err_write);

            let mut sandbox = NsSandbox {
                pid,
                state: SandboxState::Starting,
                cgroup_dir: tenant_cgroup.clone(),
                timeout: config.limits.timeout.get(),
                exit_code: None,
            };

            // The child is blocked waiting for its identity. Give it one, then
            // put it in its cgroup — both must happen before it runs any code.
            let identity = write_id_maps(pid, &uid_map, &gid_map);
            if let Some(dir) = &tenant_cgroup
                && identity.is_ok()
                && let Err(e) = cgroup::attach(&dir.join("zygote"), pid)
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
            let mut payload = Vec::new();
            let mut reader = std::fs::File::from(err_read);
            reader.read_to_end(&mut payload).map_err(|e| {
                Error::primitive("read launch status", "internal launcher error", e)
            })?;

            if !payload.is_empty() {
                let _ = sandbox.reap(true);
                return Err(launch_failure(&payload));
            }

            sandbox.state = SandboxState::Warm;
            Ok(sandbox)
        }
    }
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

    let single = |map: &str| map.lines().next().unwrap_or("").to_string();
    let (uid_line, gid_line) = if multi_line {
        (single(uid_map), single(gid_map))
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

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    rustix::pipe::pipe().map_err(|e| Error::primitive("pipe", "internal launcher error", e.into()))
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
