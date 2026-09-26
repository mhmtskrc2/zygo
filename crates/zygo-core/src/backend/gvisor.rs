// SPDX-License-Identifier: Apache-2.0
//! The `gvisor` backend: the same sandbox, drawn by `runsc`.
//!
//! `ns` builds the sandbox itself; this hands the same [`MountPlan`](crate::sandbox::mount::MountPlan) to gVisor
//! as an OCI bundle and lets its Sentry — a userspace kernel — serve the
//! tenant's syscalls. The boundary moves from "the host kernel, narrowed by
//! seccomp" to "a Go program that implements Linux", which is why it is
//! offered for hosts that have no KVM and do not want to trust `ns` alone.
//!
//! The translation is [`crate::oci`], which is pure and tested everywhere.
//! What is here is only the process handling: write a bundle, run `runsc`,
//! and take it down again.
//!
//! ## What this backend does not do yet, and why it says so instead
//!
//! * **Warm mode.** A warm sandbox is held open and entered per request with
//!   `setns` on namespaces the supervisor holds descriptors for. gVisor's
//!   process boundary is not those namespaces: entering a running one is
//!   `runsc exec`, a different mechanism with a different cost. Until that is
//!   built and measured, `hold` is refused rather than silently downgraded.
//! * **The agent.** The runtime agent receives its control socket as an
//!   inherited descriptor ([`crate::pool::AGENT_FD`]). An OCI runtime closes
//!   everything but stdio, so the agent would come up with nothing to talk
//!   to; it needs a socket bound into the bundle instead. Refused for now.
//! * **Networking.** `egress` and `full` work by attaching `pasta` to a
//!   network namespace the launcher made. gVisor's netstack is its own, and
//!   pointing `pasta` at it is separate work. Only `network = "none"` is
//!   accepted.
//! * **cgroup limits.** A rootless `runsc` cannot write a cgroup, so it is run
//!   with `--ignore-cgroups` and the limits in the bundle are advisory. This
//!   is the gap that keeps the backend experimental, and it is reported as a
//!   warning on every start rather than left for someone to discover.

use std::path::{Path, PathBuf};
use std::process::Child;

use super::{Availability, Backend, Sandbox};
use crate::error::{Error, Result};
use crate::sandbox::{MountOp, SandboxConfig, SandboxState};
use crate::spec::Network;

/// Binary name, looked for on `$PATH` and under the data directory.
pub const RUNSC: &str = "runsc";

pub struct GvisorBackend {
    runsc: Option<PathBuf>,
}

impl Default for GvisorBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl GvisorBackend {
    pub fn new() -> Self {
        Self {
            runsc: find_runsc(&crate::Paths::from_env()),
        }
    }

    /// For tests and for an embedder that keeps its state elsewhere.
    pub fn with_paths(paths: &crate::Paths) -> Self {
        Self {
            runsc: find_runsc(paths),
        }
    }
}

/// `runsc`, from the data directory first and `$PATH` second.
///
/// The data directory wins because `zygo backend install gvisor` puts it
/// there, and a Zygo that installed its own copy should use that one rather
/// than whatever version a distribution happens to ship.
pub fn find_runsc(paths: &crate::Paths) -> Option<PathBuf> {
    // What `zygo backend install gvisor` writes: the binary with its sidecar
    // directory beside it, which is the only layout that actually runs.
    let installed = paths.backends().join(INSTALL_DIR).join(RUNSC);
    if installed.is_file() {
        return Some(installed);
    }
    // A binary dropped in by hand, kept working because someone will.
    let loose = paths.backends().join(RUNSC);
    if loose.is_file() {
        return Some(loose);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(RUNSC))
        .find(|candidate| candidate.is_file())
}

impl Backend for GvisorBackend {
    fn name(&self) -> &'static str {
        "gvisor"
    }

    fn availability(&self) -> Availability {
        if !cfg!(target_os = "linux") {
            return Availability::unavailable(
                format!(
                    "gVisor runs on Linux; this host runs {}",
                    std::env::consts::OS
                ),
                "run Zygo inside a Linux VM or container; on macOS the `zygo` binary \
                 normally forwards into one it manages",
            );
        }
        match &self.runsc {
            Some(_) => Availability::Available,
            None => {
                Availability::unavailable("runsc is not installed", "zygo backend install gvisor")
            }
        }
    }

    fn start(&self, config: &SandboxConfig) -> Result<Box<dyn Sandbox>> {
        let runsc = match &self.runsc {
            Some(p) => p.clone(),
            None => {
                return Err(self.availability().into_error("gvisor").unwrap_or_else(|| {
                    Error::primitive(
                        "runsc",
                        "zygo backend install gvisor",
                        std::io::Error::from(std::io::ErrorKind::NotFound),
                    )
                }));
            }
        };
        let unsupported = |reason: &str, remedy: &str| Error::BackendUnavailable {
            backend: "gvisor",
            reason: reason.to_string(),
            remedy: remedy.to_string(),
        };

        // Not "yet". Warm functions are an `ns` feature by decision, recorded
        // in docs/book/adr/0002-warm-paths-stay-on-ns.md: a second warm path would
        // be a second thing to keep correct, benchmark and defend, against
        // the one the product rests on. The refusals below are the design.
        if config.hold {
            return Err(unsupported(
                "warm functions are an `ns` feature: entering a running gVisor sandbox \
                 is `runsc exec`, not `setns` (docs/book/adr/0002)",
                "use --isolation ns for warm functions; `gvisor` is for one-shot runs",
            ));
        }
        if config.agent_fd.is_some() {
            return Err(unsupported(
                "the runtime agent is handed its control socket as an inherited \
                 descriptor, and an OCI runtime closes everything but stdio \
                 (docs/book/adr/0002)",
                "use --isolation ns for agent runtimes; `gvisor` is for one-shot runs",
            ));
        }
        if config.network != Network::None {
            return Err(unsupported(
                &format!(
                    "the gvisor backend has only `network = \"none\"`; this function asks \
                     for `{}`, which needs pasta attached to gVisor's own netstack \
                     (docs/book/adr/0002)",
                    config.network
                ),
                "use --isolation ns for a networked sandbox",
            ));
        }
        let rootfs = flat_rootfs(config).ok_or_else(|| {
            unsupported(
                "an OCI bundle's root is one directory, and this sandbox's rootfs is an \
                 overlay of image layers",
                "the store can flatten instead; this is the backend's own bug if you see it",
            )
        })?;

        // Said on every start, because a limit that is written down and not
        // enforced is worse than one that was never promised.
        tracing::warn!(
            "the gvisor backend is experimental: a rootless runsc cannot write cgroups, \
             so mem, cpu and pids limits are advisory here"
        );

        let bundle = bundle_dir(config);
        let allow = seccomp_allowlist(config);
        let allow_refs: Vec<&str> = allow.iter().map(String::as_str).collect();
        crate::oci::write_bundle(
            &bundle,
            config,
            &crate::oci::BundleOptions {
                rootfs: &rootfs,
                host_uid: current_uid(),
                host_gid: current_gid(),
                seccomp_allow: &allow_refs,
                // `run_args` passes `--rootless`, so runsc builds the user
                // namespace and its id map; asking for a second one is what
                // broke the gofer on the first real run.
                rootless: true,
            },
        )?;

        let id = container_id(config);
        let args = run_args(&bundle, &id);
        let mut command = std::process::Command::new(&runsc);
        command.args(&args);
        if let Some(fd) = config.stdio {
            attach_stdio(&mut command, fd);
        }
        let child = command.spawn().map_err(|e| {
            Error::primitive("spawn", format!("could not start {}", runsc.display()), e)
        })?;

        Ok(Box::new(GvisorSandbox {
            runsc,
            id,
            bundle,
            child: Some(child),
            state: SandboxState::Warm,
        }))
    }
}

/// Directory of helper binaries `runsc` execs, beside it.
///
/// Not optional and not obvious: this release refuses to start a sandbox
/// without `gvisor-bin/gvisor_sentry`, because its default
/// `--sidecar-usage-policy` is `STRICT`. Installing the 100 MB `runsc` alone
/// produces a runtime that passes `--version` and cannot run anything, which
/// is exactly what the first attempt here did.
pub const SIDECAR_DIR: &str = "gvisor-bin";

/// Where an installed gVisor lives under the data directory's `backends/`.
pub const INSTALL_DIR: &str = "gvisor";

/// Whether a release-archive member is one Zygo needs.
///
/// `runsc` and its sidecars, and nothing else: the archive also carries
/// `containerd-shim-runsc-v1`, 41 MB of a shim for a runtime Zygo never
/// speaks to.
pub fn wanted_member(path: &Path) -> bool {
    path == Path::new(RUNSC) || path.starts_with(SIDECAR_DIR)
}

/// Unpack a gVisor release archive into `dir`, returning the path of `runsc`.
///
/// The archive is a zstd-compressed tar of around 300 MB unpacked, so it is
/// *streamed* rather than held in memory. The decoder is the one the image
/// store already uses for zstd layers, with the same window limit: the
/// archive is written with a 128 MiB window, over ruzstd's own default.
pub fn extract_release(archive: impl std::io::Read, dir: &Path) -> Result<PathBuf> {
    let decoder = crate::image::media::zstd_decoder(archive)
        .map_err(|e| unpack_error(format!("the archive is not zstd: {e}")))?;
    extract_release_from_tar(decoder, dir)
}

/// The same, from an already-decompressed tar.
///
/// Split out so the member selection is testable: `tar` can write an archive
/// and `ruzstd` decodes but cannot encode, so a test can build the input for
/// this half and not for the other.
pub fn extract_release_from_tar(tar: impl std::io::Read, dir: &Path) -> Result<PathBuf> {
    use crate::error::IoContext;

    std::fs::create_dir_all(dir).at(dir)?;
    let mut archive = tar::Archive::new(tar);
    let entries = archive
        .entries()
        .map_err(|e| unpack_error(format!("the archive could not be read: {e}")))?;

    let mut runsc = None;
    for entry in entries {
        let mut entry =
            entry.map_err(|e| unpack_error(format!("the archive is truncated: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| unpack_error(format!("an archive entry has no path: {e}")))?
            .into_owned();
        if !wanted_member(&path) {
            continue;
        }
        // The archive is checksum-verified before it reaches here, but a
        // member that escapes the directory is refused anyway: the check
        // costs nothing and the alternative is trusting a bucket with paths.
        let target = crate::image::store::safe_join(dir, &path)
            .map_err(|e| unpack_error(format!("{}: {e}", path.display())))?;
        if entry.header().entry_type().is_dir() {
            std::fs::create_dir_all(&target).at(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).at(parent)?;
        }
        entry
            .unpack(&target)
            .map_err(|e| unpack_error(format!("{} could not be written: {e}", path.display())))?;
        set_executable(&target)?;
        if path == Path::new(RUNSC) {
            let size = std::fs::metadata(&target).at(&target)?.len();
            if size == 0 {
                return Err(unpack_error("the archive's runsc is empty"));
            }
            runsc = Some(target);
        }
    }

    runsc.ok_or_else(|| {
        unpack_error("the archive holds no `runsc` — gVisor may have changed its release layout")
    })
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<()> {
    use crate::error::IoContext;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).at(path)
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<()> {
    Ok(())
}

fn unpack_error(message: impl Into<String>) -> Error {
    Error::Image(crate::image::ImageError::unpack(message.into()))
}

/// The flattened rootfs the bundle's `root.path` needs, from the plan.
///
/// `None` for an overlay view: see the module note.
pub fn flat_rootfs(config: &SandboxConfig) -> Option<PathBuf> {
    config.mounts.ops.iter().find_map(|op| match op {
        MountOp::BindRoot { source, .. } => Some(source.clone()),
        _ => None,
    })
}

/// `runsc` arguments for a one-shot run.
///
/// Separated from `start` so the flags can be asserted without a `runsc` to
/// run: they are the difference between a sandbox and a container with the
/// host's network.
pub fn run_args(bundle: &Path, id: &str) -> Vec<String> {
    vec![
        // Rootless: no daemon, no root, which is the whole premise (P2).
        "--rootless".to_string(),
        // gVisor's own netstack, with nothing attached to it. Not `host`,
        // which would hand the sandbox the host's interfaces.
        "--network=none".to_string(),
        // A rootless runsc cannot write a cgroup; without this it fails
        // rather than running unlimited, which is at least honest — but the
        // supervisor is the one that owns limits on this backend today.
        "--ignore-cgroups".to_string(),
        "run".to_string(),
        "--bundle".to_string(),
        bundle.to_string_lossy().into_owned(),
        id.to_string(),
    ]
}

/// A container id unique to this sandbox and this process.
///
/// `runsc` keeps state per id; a second run under a live id fails, and two
/// Zygo processes must not collide.
pub fn container_id(config: &SandboxConfig) -> String {
    format!(
        "zygo-{}-{}",
        config
            .id
            .name
            .replace(|c: char| !c.is_ascii_alphanumeric(), "-"),
        crate::process_token()
    )
}

fn bundle_dir(config: &SandboxConfig) -> PathBuf {
    crate::Paths::from_env()
        .tmp()
        .join(format!("gvisor-{}", container_id(config)))
}

/// The seccomp names for this profile, on a host that has the tables.
#[cfg(target_os = "linux")]
fn seccomp_allowlist(config: &SandboxConfig) -> Vec<String> {
    super::ns::seccomp::allowed_names(config.seccomp)
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[cfg(not(target_os = "linux"))]
fn seccomp_allowlist(_config: &SandboxConfig) -> Vec<String> {
    Vec::new()
}

#[cfg(unix)]
fn attach_stdio(command: &mut std::process::Command, fd: std::os::fd::RawFd) {
    use std::os::fd::FromRawFd;
    // Three independent descriptions of the same open file, so that a close
    // on one does not take the others with it.
    let dup = |fd: std::os::fd::RawFd| -> std::process::Stdio {
        // SAFETY: `fd` is owned by the caller for the life of the spawn, and
        // `dup` gives this process a descriptor of its own.
        unsafe {
            let copy = libc::dup(fd);
            if copy < 0 {
                std::process::Stdio::null()
            } else {
                std::process::Stdio::from_raw_fd(copy)
            }
        }
    };
    command.stdin(dup(fd)).stdout(dup(fd)).stderr(dup(fd));
}

#[cfg(not(unix))]
fn attach_stdio(_command: &mut std::process::Command, _fd: std::os::fd::RawFd) {}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: `getuid` cannot fail and touches no memory.
    unsafe { libc::getuid() }
}

#[cfg(unix)]
fn current_gid() -> u32 {
    // SAFETY: as above.
    unsafe { libc::getgid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(not(unix))]
fn current_gid() -> u32 {
    0
}

/// A sandbox `runsc` is running.
pub struct GvisorSandbox {
    runsc: PathBuf,
    id: String,
    bundle: PathBuf,
    /// `None` once it has been waited for: a second `wait` on a reaped child
    /// would block or error.
    child: Option<Child>,
    state: SandboxState,
}

impl Sandbox for GvisorSandbox {
    /// The `runsc` process on the host.
    ///
    /// Not the sandboxed program's pid, which lives in gVisor's own pid space
    /// and is not a host pid at all. Everything Zygo does with a pid on this
    /// backend — waiting, signalling — is done to `runsc`, which passes it on.
    fn pid(&self) -> u32 {
        self.child.as_ref().map(Child::id).unwrap_or(0)
    }

    fn state(&self) -> SandboxState {
        self.state
    }

    fn wait(&mut self) -> Result<i32> {
        let Some(child) = self.child.as_mut() else {
            return Ok(0);
        };
        let status = child
            .wait()
            .map_err(|e| Error::primitive("wait", "waiting for runsc failed", e))?;
        self.child = None;
        self.state = SandboxState::Cold;
        self.cleanup();
        // A signalled `runsc` is reported the way a shell reports one, which
        // is what the rest of Zygo already does with an exit status.
        Ok(exit_code(status))
    }

    fn kill(&mut self) -> Result<()> {
        // `runsc kill` first: it takes down the sandboxed process tree inside
        // the Sentry. Killing the `runsc` process alone can leave the sandbox
        // running, which is the whole reason this is not just a `SIGKILL`.
        let _ = std::process::Command::new(&self.runsc)
            .args(["--rootless", "kill", &self.id, "KILL"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.child = None;
        self.state = SandboxState::Cold;
        self.cleanup();
        Ok(())
    }
}

impl GvisorSandbox {
    /// Remove the container's state and its bundle.
    ///
    /// `runsc delete` and not just the directory: the runtime keeps state of
    /// its own under its root, and an id that is never deleted is one that
    /// cannot be reused.
    fn cleanup(&self) {
        let _ = std::process::Command::new(&self.runsc)
            .args(["--rootless", "delete", "--force", &self.id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let _ = std::fs::remove_dir_all(&self.bundle);
    }
}

impl Drop for GvisorSandbox {
    fn drop(&mut self) {
        if self.child.is_some() {
            let _ = self.kill();
        }
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::SandboxId;
    use crate::sandbox::limits::Limits;
    use crate::sandbox::mount::{RootfsView, plan};
    use crate::spec::{Bytes, Cpu, Duration, Isolation, SeccompProfile};

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

    fn sandbox(view: RootfsView) -> SandboxConfig {
        let limits = limits();
        let mounts = plan("/tmp/newroot", &view, &limits, &[]);
        SandboxConfig {
            id: SandboxId::new("resize"),
            isolation: Isolation::Gvisor,
            seccomp: SeccompProfile::Default,
            network: Network::None,
            limits,
            mounts,
            argv: vec!["/bin/true".into()],
            env: Vec::new(),
            workdir: "/".into(),
            uid: 1000,
            gid: 1000,
            stdio: None,
            stdio_streams: None,
            ignored_signals: None,
            agent_fd: None,
            hold: false,
            writable_root: false,
            allow: Vec::new(),
            allow_resolved: Default::default(),
            allow_private_net: false,
            pasta_pid_file: None,
            own_limits: true,
        }
    }

    fn flat() -> SandboxConfig {
        sandbox(RootfsView::Flat {
            dir: "/store/flat/abc".into(),
        })
    }

    /// The flags are the backend's security posture, and none of them needs a
    /// `runsc` to assert. `--network=host` slipping in here would hand the
    /// sandbox the host's interfaces with nothing else changing.
    #[test]
    fn the_runsc_invocation_is_rootless_and_has_no_network() {
        let args = run_args(Path::new("/tmp/bundle"), "zygo-resize-1");
        assert!(args.contains(&"--rootless".to_string()), "{args:?}");
        assert!(args.contains(&"--network=none".to_string()), "{args:?}");
        assert!(!args.iter().any(|a| a.contains("host")), "{args:?}");

        // `run --bundle <dir> <id>`, in that order: runsc takes the id last.
        let run = args.iter().position(|a| a == "run").expect("a run verb");
        let bundle = args.iter().position(|a| a == "--bundle").expect("--bundle");
        assert!(bundle > run, "flags for `run` come after it");
        assert_eq!(args[bundle + 1], "/tmp/bundle");
        assert_eq!(args.last().map(String::as_str), Some("zygo-resize-1"));
    }

    #[test]
    fn the_container_id_is_unique_to_this_process_and_safe_as_a_path() {
        let mut config = flat();
        config.id = SandboxId::new("weird/name with spaces");
        let id = container_id(&config);
        assert!(id.starts_with("zygo-"), "{id}");
        assert!(id.ends_with(crate::process_token()), "{id}");
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "{id} is not safe as a directory name"
        );
    }

    #[test]
    fn the_rootfs_comes_from_the_plan_and_an_overlay_has_none() {
        assert_eq!(
            flat_rootfs(&flat()),
            Some(PathBuf::from("/store/flat/abc")),
            "the flat view is what the bundle can use"
        );
        let overlay = sandbox(RootfsView::Overlay {
            lower: vec!["/store/layers/a".into(), "/store/layers/b".into()],
        });
        assert_eq!(
            flat_rootfs(&overlay),
            None,
            "an overlay has no single directory to be root.path"
        );
    }

    /// Every one of these is a thing the backend cannot do. Refusing with a
    /// reason is the contract; silently running something weaker is what a
    /// backend must never do.
    #[test]
    fn what_the_backend_cannot_do_is_refused_by_name() {
        // A backend with no runsc refuses before anything else, so these are
        // asserted against a backend that has one.
        let backend = GvisorBackend {
            runsc: Some(PathBuf::from("/nonexistent/runsc")),
        };
        let message = |config: &SandboxConfig| match backend.start(config) {
            Ok(_) => panic!("unexpectedly started"),
            Err(e) => e.to_string(),
        };

        let mut held = flat();
        held.hold = true;
        let m = message(&held);
        assert!(m.contains("warm"), "{m}");
        assert!(m.contains("runsc exec"), "{m}");

        let mut agent = flat();
        agent.agent_fd = Some(3);
        let m = message(&agent);
        assert!(m.contains("descriptor"), "{m}");

        let mut networked = flat();
        networked.network = Network::Egress;
        let m = message(&networked);
        assert!(m.contains("egress"), "{m}");
        assert!(m.contains("none"), "{m}");

        let overlay = sandbox(RootfsView::Overlay {
            lower: vec!["/store/layers/a".into()],
        });
        let m = message(&overlay);
        assert!(m.contains("overlay"), "{m}");
    }

    #[test]
    fn a_missing_runsc_is_reported_with_the_command_that_installs_it() {
        let backend = GvisorBackend { runsc: None };
        match backend.availability() {
            Availability::Available => panic!("no runsc, yet available"),
            Availability::Unavailable { reason, remedy } => {
                if cfg!(target_os = "linux") {
                    assert!(reason.contains("runsc"), "{reason}");
                    assert!(remedy.contains("zygo backend install gvisor"), "{remedy}");
                } else {
                    assert!(reason.contains("Linux"), "{reason}");
                }
            }
        }
    }

    #[test]
    fn an_installed_runsc_is_preferred_over_the_one_on_the_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = crate::Paths::rooted(tmp.path());
        assert_eq!(
            find_runsc(&paths),
            None,
            "nothing installed, nothing on PATH here"
        );

        std::fs::create_dir_all(paths.backends()).expect("backends dir");
        let installed = paths.backends().join(RUNSC);
        std::fs::write(&installed, b"#!/bin/sh\n").expect("write");
        assert_eq!(find_runsc(&paths), Some(installed));
    }

    fn archive(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, body) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, name, std::io::Cursor::new(*body))
                .expect("append");
        }
        builder.into_inner().expect("finish")
    }

    /// The bug the first real install had: `runsc` alone passes `--version`
    /// and then refuses to start a sandbox, because this release execs
    /// `gvisor-bin/gvisor_sentry` and its sidecar policy is STRICT. The
    /// sidecars are not optional and the shim is not wanted.
    #[test]
    fn the_release_is_unpacked_with_its_sidecars_and_without_the_shim() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("gvisor");
        let tar = archive(&[
            ("containerd-shim-runsc-v1", b"shim"),
            ("runsc", b"the runtime"),
            ("gvisor-bin/gvisor_sentry", b"sentry"),
            ("gvisor-bin/runsc-fd-parking", b"helper"),
        ]);

        let runsc = extract_release_from_tar(std::io::Cursor::new(tar), &dir).expect("unpacked");
        assert_eq!(runsc, dir.join("runsc"));
        assert_eq!(std::fs::read(&runsc).expect("read"), b"the runtime");
        assert!(
            dir.join("gvisor-bin/gvisor_sentry").is_file(),
            "without the sentry runsc cannot start a sandbox"
        );
        assert!(dir.join("gvisor-bin/runsc-fd-parking").is_file());
        assert!(
            !dir.join("containerd-shim-runsc-v1").exists(),
            "the containerd shim is 41 MB Zygo never calls"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&runsc)
                .expect("stat")
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "runsc must be executable");
        }
    }

    #[test]
    fn a_layout_change_is_reported_rather_than_half_installed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let tar = archive(&[("gvisor-bin/runsc-fd-parking", b"helper")]);
        let err =
            extract_release_from_tar(std::io::Cursor::new(tar), tmp.path()).expect_err("no runsc");
        assert!(err.to_string().contains("release layout"), "{err}");

        let tar = archive(&[("runsc", b"")]);
        let err =
            extract_release_from_tar(std::io::Cursor::new(tar), tmp.path()).expect_err("empty");
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn a_member_that_escapes_the_directory_is_refused() {
        assert!(wanted_member(Path::new("runsc")));
        assert!(wanted_member(Path::new("gvisor-bin/gvisor_sentry")));
        assert!(!wanted_member(Path::new("containerd-shim-runsc-v1")));

        // The `tar` builder refuses to *write* a `..` path, so the header is
        // filled in by hand. A test that cannot construct the attack proves
        // nothing about the defence.
        let mut builder = tar::Builder::new(Vec::new());
        let body: &[u8] = b"nope";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        {
            let raw = header.as_mut_bytes();
            let name = b"gvisor-bin/../../escaped";
            raw[..name.len()].copy_from_slice(name);
            raw[name.len()..100].fill(0);
        }
        header.set_cksum();
        builder
            .append(&header, std::io::Cursor::new(body))
            .expect("a hand-built header is not validated");
        let mut runsc_header = tar::Header::new_gnu();
        runsc_header.set_size(2);
        runsc_header.set_mode(0o755);
        runsc_header.set_cksum();
        builder
            .append_data(&mut runsc_header, "runsc", std::io::Cursor::new(&b"rt"[..]))
            .expect("append runsc");
        let tar = builder.into_inner().expect("finish");

        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("nested").join("gvisor");
        let escaped = tmp.path().join("escaped");
        let _ = extract_release_from_tar(std::io::Cursor::new(tar), &dir);
        assert!(
            !escaped.exists(),
            "a `..` member was written outside the directory, to {}",
            escaped.display()
        );
    }

    #[test]
    fn a_signalled_runsc_is_reported_the_way_a_shell_reports_one() {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(exit_code(std::process::ExitStatus::from_raw(9)), 128 + 9);
            assert_eq!(exit_code(std::process::ExitStatus::from_raw(0)), 0);
        }
    }
}
