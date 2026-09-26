// SPDX-License-Identifier: Apache-2.0
//! The mount plan, rendered as an OCI runtime `config.json` (design doc §3.9).
//!
//! The `ns` backend *executes* a [`MountPlan`](crate::sandbox::mount::MountPlan); `gvisor` hands the same plan to
//! `runsc`, which executes it on the other side of a process boundary. That is
//! the whole reason the plan is pure data (see [`crate::sandbox::mount`]): two
//! backends that each derived their own idea of what a sandbox looks like would
//! drift, and requirement N8 says the same spec must mean the same thing on
//! every backend.
//!
//! What is *not* translated, and why, is as much of the contract as what is:
//!
//! * **Device nodes.** The plan binds a fixed set (`null`, `zero`, `full`,
//!   `random`, `urandom`, `tty`) because creating them needs `mknod`. An OCI
//!   runtime creates exactly that set itself unless told otherwise, so the ops
//!   become nothing and the result is the same `/dev`.
//! * **`MakeRootPrivate`.** Mount propagation is the runtime's own business;
//!   the spec has `linux.rootfsPropagation` for it, and `private` is the
//!   default every runtime already applies.
//! * **The overlay root.** `root.path` is one directory, so a
//!   [`RootfsView::Overlay`](crate::sandbox::mount::RootfsView::Overlay) has nowhere to go. The `gvisor` backend asks the
//!   store to flatten instead, which is the fallback the store already has for
//!   old kernels. gVisor's Sentry keeps its own overlay above it anyway.
//!
//! Nothing here touches the filesystem except [`write_bundle`], and nothing
//! here is Linux-only, so the translation is unit-tested everywhere.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::error::{IoContext, Result};
use crate::sandbox::mount::{MountOp, flags};
use crate::sandbox::{SandboxConfig, limits::RlimitKind};
use crate::spec::{MountMode, Network};

/// The runtime spec version this generates. 1.0.2 is what `runsc`, `runc` and
/// `crun` all accept; later versions add fields, none of which are used here.
pub const OCI_VERSION: &str = "1.0.2";

/// What the bundle needs that a [`SandboxConfig`] does not carry.
#[derive(Debug, Clone)]
pub struct BundleOptions<'a> {
    /// Absolute path to the rootfs directory. Must be a single flattened
    /// tree: see the note about the overlay root above.
    pub rootfs: &'a Path,
    /// Host uid and gid the sandbox's uid maps to. In rootless operation
    /// this is the invoking user, which is the only id they may map.
    pub host_uid: u32,
    pub host_gid: u32,
    /// The runtime will be invoked in its own rootless mode, and creates the
    /// user namespace itself.
    ///
    /// Then the bundle must **not** declare one. `runsc --rootless` builds a
    /// user namespace and writes its own id map; a spec that also asks for
    /// one makes the gofer clone with `CLONE_NEWUSER` a second time and fail
    /// with `fork/exec /proc/self/exe: invalid argument` — which is exactly
    /// what the first real run of this backend did, and which reads like
    /// anything but "you asked for two user namespaces". The uid in
    /// `process.user` still applies: gVisor's Sentry implements it itself.
    pub rootless: bool,
    /// Syscall names the seccomp profile allows. Empty emits no seccomp
    /// section at all, which is different from an empty allowlist — an empty
    /// allowlist would deny every syscall and the sandbox would not start.
    pub seccomp_allow: &'a [&'a str],
}

/// Build the `config.json` document for this sandbox.
pub fn config(sandbox: &SandboxConfig, options: &BundleOptions<'_>) -> Value {
    json!({
        "ociVersion": OCI_VERSION,
        "process": process(sandbox, options),
        "root": {
            "path": options.rootfs,
            // The plan's guarantee, expressed where the runtime looks for it.
            "readonly": !sandbox.writable_root,
        },
        // A sandbox that can read the host's hostname can often tell which
        // host it is on; the tenant's own name is both useful and inert.
        "hostname": sandbox.id.name,
        "mounts": mounts(sandbox),
        "linux": linux(sandbox, options),
    })
}

fn process(sandbox: &SandboxConfig, options: &BundleOptions<'_>) -> Value {
    let rlimits: Vec<Value> = sandbox
        .limits
        .rlimits()
        .iter()
        .map(|(kind, value)| {
            json!({
                "type": match kind {
                    RlimitKind::NoFile => "RLIMIT_NOFILE",
                    RlimitKind::FSize => "RLIMIT_FSIZE",
                    RlimitKind::Core => "RLIMIT_CORE",
                    RlimitKind::NProc => "RLIMIT_NPROC",
                },
                "hard": value,
                "soft": value,
            })
        })
        .collect();

    // Every set empty: the sandbox holds no capability, and `ambient` being
    // empty is what stops one being regained across `execve`.
    let no_capabilities = json!({
        "bounding": [],
        "effective": [],
        "inheritable": [],
        "permitted": [],
        "ambient": [],
    });

    json!({
        "terminal": false,
        "user": { "uid": sandbox.uid, "gid": sandbox.gid },
        "args": sandbox.argv,
        "env": sandbox
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<String>>(),
        "cwd": workdir(sandbox, options),
        "capabilities": no_capabilities,
        "rlimits": rlimits,
        "noNewPrivileges": true,
        // Host uid/gid, recorded for the reader rather than for the runtime:
        // the mapping below is what actually places the sandbox on the host.
        "_zygoHostIds": { "uid": options.host_uid, "gid": options.host_gid },
    })
}

/// The working directory, or `/` when the spec's does not exist.
///
/// The `ns` launcher `chdir`s and falls back to `/` on failure, because an
/// image that lacks the configured directory is a poor reason to refuse to
/// start. An OCI runtime instead *creates* the directory, and a read-only
/// root makes that fail — so `zygo run --isolation gvisor alpine:3 echo hi`
/// died with "failed to create process working directory" where `ns` had run
/// it. Requirement N8 is that the same spec means the same thing on every
/// backend, so the fallback is applied here rather than left to the runtime.
///
/// A directory counts as existing if the image has it, or if something is
/// mounted exactly there.
fn workdir(sandbox: &SandboxConfig, options: &BundleOptions<'_>) -> PathBuf {
    let wanted = &sandbox.workdir;
    let root = Path::new("/");
    if wanted == root {
        return wanted.clone();
    }
    let relative = wanted.strip_prefix("/").unwrap_or(wanted);
    if options.rootfs.join(relative).is_dir() {
        return wanted.clone();
    }
    let newroot = &sandbox.mounts.newroot;
    let mounted_here = sandbox.mounts.ops.iter().any(|op| {
        let target = match op {
            MountOp::Bind { target, .. }
            | MountOp::Tmpfs { target, .. }
            | MountOp::Proc { target }
            | MountOp::Sysfs { target }
            | MountOp::DevPts { target } => target,
            _ => return false,
        };
        target.strip_prefix(newroot).unwrap_or(target) == relative
    });
    if mounted_here {
        return wanted.clone();
    }
    root.to_path_buf()
}

/// Translate the mount plan, dropping the ops an OCI runtime does for itself.
fn mounts(sandbox: &SandboxConfig) -> Vec<Value> {
    let newroot = &sandbox.mounts.newroot;
    // Plan targets are host paths under the assembly directory; OCI wants the
    // path *inside* the sandbox.
    let inside = |target: &Path| -> String {
        let rel = target.strip_prefix(newroot).unwrap_or(target);
        let shown = rel.to_string_lossy();
        if shown.is_empty() || shown == "." {
            "/".to_string()
        } else if shown.starts_with('/') {
            shown.into_owned()
        } else {
            format!("/{shown}")
        }
    };

    let mut out = Vec::new();
    for op in &sandbox.mounts.ops {
        match op {
            // Handled by the runtime, or by `root.path`. Listed rather than
            // caught by a wildcard so that a new op cannot be silently
            // dropped here — the match is exhaustive on purpose.
            MountOp::MakeRootPrivate
            | MountOp::Overlay { .. }
            | MountOp::BindRoot { .. }
            | MountOp::DevNode { .. } => {}

            MountOp::Proc { target } => out.push(json!({
                "destination": inside(target),
                "type": "proc",
                "source": "proc",
                "options": ["nosuid", "noexec", "nodev"],
            })),
            MountOp::Sysfs { target } => out.push(json!({
                "destination": inside(target),
                "type": "sysfs",
                "source": "sysfs",
                "options": ["nosuid", "noexec", "nodev", "ro"],
            })),
            MountOp::DevPts { target } => out.push(json!({
                "destination": inside(target),
                "type": "devpts",
                "source": "devpts",
                "options": ["nosuid", "noexec", "newinstance", "ptmxmode=0666", "mode=0620"],
            })),
            MountOp::Tmpfs {
                target,
                options,
                flags: f,
            } => {
                let mut opts: Vec<String> = options
                    .split(',')
                    .map(str::to_string)
                    .filter(|s| !s.is_empty())
                    .collect();
                opts.extend(flag_options(*f));
                out.push(json!({
                    "destination": inside(target),
                    "type": "tmpfs",
                    "source": "tmpfs",
                    "options": opts,
                }));
            }
            MountOp::Bind {
                source,
                target,
                mode,
            } => {
                // `rbind`, so a source that is itself a mount point brings its
                // submounts; `nosuid` and `nodev` because a bind from the host
                // must not carry privilege in.
                let mut opts = vec!["rbind".to_string(), "nosuid".into(), "nodev".into()];
                opts.push(
                    match mode {
                        MountMode::Ro => "ro",
                        MountMode::Rw => "rw",
                    }
                    .to_string(),
                );
                out.push(json!({
                    "destination": inside(target),
                    "type": "bind",
                    "source": source,
                    "options": opts,
                }));
            }
            // These are not mounts in the OCI model; they are the two path
            // lists under `linux`, filled in by `linux()`.
            MountOp::Mask { .. } | MountOp::RemountReadOnly { .. } => {}
        }
    }
    out
}

fn flag_options(value: u64) -> Vec<String> {
    let mut out = Vec::new();
    for (bit, name) in [
        (flags::NOSUID, "nosuid"),
        (flags::NODEV, "nodev"),
        (flags::NOEXEC, "noexec"),
        (flags::RDONLY, "ro"),
    ] {
        if value & bit != 0 {
            out.push(name.to_string());
        }
    }
    out
}

fn linux(sandbox: &SandboxConfig, options: &BundleOptions<'_>) -> Value {
    let newroot = &sandbox.mounts.newroot;
    let inside = |target: &Path| -> String {
        let rel = target.strip_prefix(newroot).unwrap_or(target);
        format!("/{}", rel.to_string_lossy().trim_start_matches('/'))
    };

    let mut masked = Vec::new();
    let mut readonly = Vec::new();
    for op in &sandbox.mounts.ops {
        match op {
            MountOp::Mask { target } => masked.push(inside(target)),
            MountOp::RemountReadOnly { target } => readonly.push(inside(target)),
            _ => {}
        }
    }

    let mut doc = json!({
        "namespaces": namespaces(sandbox.network, options.rootless),
        "maskedPaths": masked,
        "readonlyPaths": readonly,
        "resources": resources(sandbox),
        "rootfsPropagation": "private",
    });

    // Only where the runtime is not making its own: see `rootless`.
    if !options.rootless {
        // One id each: the sandbox's uid maps to the invoking user, and
        // nothing else on the host is reachable through the map. An
        // unprivileged runtime may map only ids it owns.
        doc["uidMappings"] = json!([{
            "containerID": sandbox.uid,
            "hostID": options.host_uid,
            "size": 1,
        }]);
        doc["gidMappings"] = json!([{
            "containerID": sandbox.gid,
            "hostID": options.host_gid,
            "size": 1,
        }]);
    }

    if !options.seccomp_allow.is_empty() {
        doc["seccomp"] = seccomp(options.seccomp_allow);
    }
    doc
}

fn namespaces(network: Network, rootless: bool) -> Vec<Value> {
    let mut out = vec![
        json!({ "type": "pid" }),
        json!({ "type": "ipc" }),
        json!({ "type": "uts" }),
        json!({ "type": "mount" }),
        json!({ "type": "cgroup" }),
    ];
    if !rootless {
        out.push(json!({ "type": "user" }));
    }
    // A network namespace with nothing in it *is* the sandbox's network for
    // every mode but `host`; `egress` and `full` get their connectivity from
    // `pasta` attaching to it afterwards, exactly as on `ns`. Omitting the
    // entry, for `host`, is how OCI says "share the host's".
    if network != Network::Host {
        out.push(json!({ "type": "network" }));
    }
    out
}

fn resources(sandbox: &SandboxConfig) -> Value {
    let limits = &sandbox.limits;
    let mut cpu = json!({ "period": 100_000u64 });
    // `Cpu` is in cores; the OCI quota is microseconds of runtime per period.
    let quota = (limits.cpu.cores() * 100_000.0).round() as i64;
    if quota > 0 {
        cpu["quota"] = json!(quota);
    }

    let mut memory = json!({
        "limit": limits.mem.get(),
        "reservation": limits.mem_high.get(),
        "swap": limits.mem.get() + limits.swap.get(),
        "disableOOMKiller": false,
    });
    if limits.oom_group {
        // Not an OCI field; kept so the bundle records the intent rather than
        // silently losing it, and so a diff against the `ns` cgroup writes is
        // readable.
        memory["_zygoOomGroup"] = json!(true);
    }

    json!({
        "memory": memory,
        "cpu": cpu,
        "pids": { "limit": limits.pids },
    })
}

/// An OCI seccomp section from the profile's allowlist.
///
/// Same direction as the native filter: deny by default, name what is
/// allowed. `runsc` implements the syscall surface itself, so this is defence
/// in depth rather than the only boundary — but a profile that silently meant
/// nothing on one backend would make the `seccomp` field a lie there.
fn seccomp(allow: &[&str]) -> Value {
    json!({
        "defaultAction": "SCMP_ACT_ERRNO",
        "defaultErrnoRet": 1, // EPERM, as the native filter answers.
        "architectures": ["SCMP_ARCH_X86_64", "SCMP_ARCH_AARCH64"],
        "syscalls": [{
            "names": allow,
            "action": "SCMP_ACT_ALLOW",
        }],
    })
}

/// Write `config.json` into `dir`, creating it, and return its path.
///
/// An OCI *bundle* is a directory holding `config.json` and a rootfs. The
/// rootfs here is referenced by absolute path rather than copied, because it
/// is the image store's, shared between every sandbox on the same image.
pub fn write_bundle(
    dir: &Path,
    sandbox: &SandboxConfig,
    options: &BundleOptions<'_>,
) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).at(dir)?;
    let path = dir.join("config.json");
    let document = config(sandbox, options);
    let text = serde_json::to_vec_pretty(&document).map_err(|e| {
        crate::error::Error::primitive(
            "serialise",
            "the OCI bundle could not be encoded",
            std::io::Error::other(e),
        )
    })?;
    std::fs::write(&path, text).at(&path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::limits::Limits;
    use crate::sandbox::mount::{RootfsView, plan};
    use crate::sandbox::{SandboxId, mount};
    use crate::spec::{Isolation, Mount, SeccompProfile};

    fn limits() -> Limits {
        use crate::spec::{Bytes, Cpu, Duration};
        Limits {
            mem: Bytes::from_mib(256),
            mem_high: Bytes::from_mib(256).scaled(0.9),
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

    fn sandbox() -> SandboxConfig {
        let limits = limits();
        let mounts = plan(
            "/tmp/newroot",
            &RootfsView::Flat {
                dir: "/store/flat/abc".into(),
            },
            &limits,
            &[
                Mount {
                    source: "/host/code".into(),
                    target: "/app".into(),
                    mode: MountMode::Ro,
                },
                Mount {
                    source: "/host/out".into(),
                    target: "/out".into(),
                    mode: MountMode::Rw,
                },
            ],
        );
        SandboxConfig {
            id: SandboxId::new("resize"),
            isolation: Isolation::Gvisor,
            seccomp: SeccompProfile::Default,
            network: Network::None,
            limits,
            mounts,
            argv: vec!["python3".into(), "/app/handler.py".into()],
            env: vec![("PATH".into(), "/usr/bin".into())],
            workdir: "/app".into(),
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

    fn options<'a>(allow: &'a [&'a str]) -> BundleOptions<'a> {
        BundleOptions {
            rootfs: Path::new("/store/flat/abc"),
            host_uid: 501,
            host_gid: 20,
            seccomp_allow: allow,
            rootless: false,
        }
    }

    fn rootless<'a>(allow: &'a [&'a str]) -> BundleOptions<'a> {
        BundleOptions {
            rootless: true,
            ..options(allow)
        }
    }

    fn mount_at<'a>(doc: &'a Value, destination: &str) -> Option<&'a Value> {
        doc["mounts"]
            .as_array()?
            .iter()
            .find(|m| m["destination"] == destination)
    }

    fn options_of(m: &Value) -> Vec<String> {
        m["options"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect()
    }

    #[test]
    fn the_document_has_the_shape_a_runtime_expects() {
        let doc = config(&sandbox(), &options(&[]));
        assert_eq!(doc["ociVersion"], OCI_VERSION);
        assert_eq!(doc["root"]["path"], "/store/flat/abc");
        assert_eq!(doc["root"]["readonly"], true);
        assert_eq!(doc["hostname"], "resize");
        assert_eq!(doc["process"]["args"][0], "python3");
        assert_eq!(doc["process"]["cwd"], "/app");
        assert_eq!(doc["process"]["env"][0], "PATH=/usr/bin");
        assert_eq!(doc["process"]["noNewPrivileges"], true);
        assert_eq!(doc["process"]["user"]["uid"], 1000);
    }

    /// The whole point of the backend: a writable root is a property of the
    /// plan, and it has to survive the translation.
    #[test]
    fn a_writable_root_is_carried_across() {
        let mut s = sandbox();
        s.writable_root = true;
        assert_eq!(config(&s, &options(&[]))["root"]["readonly"], false);
    }

    #[test]
    fn every_capability_set_is_empty() {
        let doc = config(&sandbox(), &options(&[]));
        for set in [
            "bounding",
            "effective",
            "inheritable",
            "permitted",
            "ambient",
        ] {
            assert_eq!(
                doc["process"]["capabilities"][set].as_array().map(Vec::len),
                Some(0),
                "{set} is not empty"
            );
        }
    }

    /// Plan targets are host paths under the assembly directory. A bundle
    /// that repeated them would mount the sandbox's `/proc` at
    /// `/tmp/newroot/proc` *inside* the sandbox, which is silently wrong.
    #[test]
    fn mount_destinations_are_paths_inside_the_sandbox() {
        let doc = config(&sandbox(), &options(&[]));
        for m in doc["mounts"].as_array().expect("mounts") {
            let d = m["destination"].as_str().expect("a destination");
            assert!(d.starts_with('/'), "{d}");
            assert!(!d.contains("newroot"), "{d} still carries the host prefix");
        }
        assert!(mount_at(&doc, "/proc").is_some());
        assert!(mount_at(&doc, "/tmp").is_some());
        assert!(mount_at(&doc, "/dev/shm").is_some());
    }

    #[test]
    fn pseudo_filesystems_keep_their_flags() {
        let doc = config(&sandbox(), &options(&[]));
        let proc = mount_at(&doc, "/proc").expect("/proc");
        assert_eq!(proc["type"], "proc");
        assert!(options_of(proc).contains(&"nosuid".to_string()));

        let sys = mount_at(&doc, "/sys").expect("/sys");
        assert!(
            options_of(sys).contains(&"ro".to_string()),
            "sysfs must be read-only"
        );

        let pts = mount_at(&doc, "/dev/pts").expect("/dev/pts");
        assert_eq!(pts["type"], "devpts");
        assert!(options_of(pts).contains(&"newinstance".to_string()));
    }

    /// A tmpfs carries two different things that both arrive as "options" in
    /// the plan: filesystem options (`size`) and mount flags (`nosuid`).
    /// Both have to reach the bundle, and the size is the limit.
    #[test]
    fn a_tmpfs_carries_both_its_size_and_its_flags() {
        let doc = config(&sandbox(), &options(&[]));
        let tmp = mount_at(&doc, "/tmp").expect("/tmp");
        assert_eq!(tmp["type"], "tmpfs");
        let opts = options_of(tmp);
        assert!(opts.iter().any(|o| o.starts_with("size=")), "{opts:?}");
        assert!(opts.contains(&"nosuid".to_string()), "{opts:?}");
        assert!(opts.contains(&"nodev".to_string()), "{opts:?}");
    }

    #[test]
    fn bind_mounts_keep_their_direction_and_never_carry_privilege() {
        let doc = config(&sandbox(), &options(&[]));
        let app = mount_at(&doc, "/app").expect("/app");
        assert_eq!(app["source"], "/host/code");
        assert_eq!(app["type"], "bind");
        let opts = options_of(app);
        assert!(opts.contains(&"ro".to_string()), "{opts:?}");
        assert!(opts.contains(&"rbind".to_string()), "{opts:?}");
        assert!(opts.contains(&"nosuid".to_string()), "{opts:?}");
        assert!(opts.contains(&"nodev".to_string()), "{opts:?}");

        let out = mount_at(&doc, "/out").expect("/out");
        assert!(options_of(out).contains(&"rw".to_string()));
    }

    /// The plan's device nodes and root op must not become mounts: `/dev/null`
    /// as a bind of the host's would be a device the runtime did not vet, and
    /// the root is `root.path`.
    #[test]
    fn device_nodes_and_the_root_are_left_to_the_runtime() {
        let doc = config(&sandbox(), &options(&[]));
        for name in mount::DEV_NODES {
            assert!(
                mount_at(&doc, &format!("/dev/{name}")).is_none(),
                "/dev/{name} should come from the runtime's default set"
            );
        }
        assert!(mount_at(&doc, "/").is_none(), "the root is root.path");
    }

    #[test]
    fn masked_and_readonly_paths_become_the_two_lists() {
        let doc = config(&sandbox(), &options(&[]));
        let masked: Vec<&str> = doc["linux"]["maskedPaths"]
            .as_array()
            .expect("maskedPaths")
            .iter()
            .map(|v| v.as_str().unwrap_or_default())
            .collect();
        let readonly: Vec<&str> = doc["linux"]["readonlyPaths"]
            .as_array()
            .expect("readonlyPaths")
            .iter()
            .map(|v| v.as_str().unwrap_or_default())
            .collect();

        for p in mount::MASKED_PATHS {
            assert!(masked.contains(p), "{p} is not masked");
        }
        for p in mount::READONLY_PATHS {
            assert!(readonly.contains(p), "{p} is not read-only");
        }
        // And they are not *also* mounts, which would be the plan translated
        // twice.
        assert!(mount_at(&doc, "/proc/kcore").is_none());
    }

    #[test]
    fn limits_become_the_resource_block() {
        let s = sandbox();
        let doc = config(&s, &options(&[]));
        let r = &doc["linux"]["resources"];
        assert_eq!(r["memory"]["limit"], s.limits.mem.get());
        assert_eq!(r["memory"]["reservation"], s.limits.mem_high.get());
        assert_eq!(r["pids"]["limit"], s.limits.pids);
        assert_eq!(r["cpu"]["period"], 100_000);
        // `Cpu` is cores; the quota is microseconds per period.
        assert_eq!(
            r["cpu"]["quota"].as_i64(),
            Some((s.limits.cpu.cores() * 100_000.0).round() as i64)
        );
        // Swap is the OCI total (memory + swap), not the swap alone: a
        // `swap: 0` spec must not read as "no memory at all".
        assert_eq!(
            r["memory"]["swap"].as_u64(),
            Some(s.limits.mem.get() + s.limits.swap.get())
        );
    }

    /// What the first real `runsc` run failed on after the namespace fix:
    /// `alpine:3` has no `/app`, an OCI runtime tries to *create* the working
    /// directory, and the root is read-only. `ns` falls back to `/` and runs,
    /// so this must too (requirement N8).
    #[test]
    fn a_working_directory_the_image_lacks_falls_back_to_the_root() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("srv")).expect("mkdir");

        let with_rootfs = |allow: &'static [&'static str]| BundleOptions {
            rootfs: &rootfs,
            host_uid: 501,
            host_gid: 20,
            seccomp_allow: allow,
            rootless: true,
        };

        // `/app` is not in this image and nothing mounts it.
        let mut s = sandbox();
        s.mounts = plan(
            "/tmp/newroot",
            &RootfsView::Flat {
                dir: rootfs.clone(),
            },
            &limits(),
            &[],
        );
        s.workdir = "/app".into();
        assert_eq!(config(&s, &with_rootfs(&[]))["process"]["cwd"], "/");

        // A directory the image does have is kept.
        s.workdir = "/srv".into();
        assert_eq!(config(&s, &with_rootfs(&[]))["process"]["cwd"], "/srv");

        // And one the image lacks but a mount provides is kept too — which is
        // the ordinary case: `mounts = ["./bin:/app:ro"]`.
        s.workdir = "/app".into();
        s.mounts = plan(
            "/tmp/newroot",
            &RootfsView::Flat {
                dir: rootfs.clone(),
            },
            &limits(),
            &[Mount {
                source: "/host/bin".into(),
                target: "/app".into(),
                mode: MountMode::Ro,
            }],
        );
        assert_eq!(config(&s, &with_rootfs(&[]))["process"]["cwd"], "/app");
    }

    #[test]
    fn rlimits_are_named_the_way_the_spec_names_them() {
        let doc = config(&sandbox(), &options(&[]));
        let names: Vec<&str> = doc["process"]["rlimits"]
            .as_array()
            .expect("rlimits")
            .iter()
            .map(|r| r["type"].as_str().unwrap_or_default())
            .collect();
        assert!(names.contains(&"RLIMIT_NOFILE"), "{names:?}");
        assert!(names.contains(&"RLIMIT_FSIZE"), "{names:?}");
    }

    /// Omitting the network namespace is how OCI says "share the host's", so
    /// getting this backwards would silently put a sealed sandbox on the
    /// host's network.
    #[test]
    fn only_host_networking_omits_the_network_namespace() {
        let has_net = |network| {
            let mut s = sandbox();
            s.network = network;
            config(&s, &rootless(&[]))["linux"]["namespaces"]
                .as_array()
                .expect("namespaces")
                .iter()
                .any(|n| n["type"] == "network")
        };
        assert!(
            has_net(Network::None),
            "a sealed sandbox needs its own netns"
        );
        assert!(has_net(Network::Egress));
        assert!(has_net(Network::Full));
        assert!(!has_net(Network::Host), "host networking shares the host's");
    }

    #[test]
    fn the_user_namespace_maps_exactly_one_id() {
        let doc = config(&sandbox(), &options(&[]));
        assert!(
            doc["linux"]["namespaces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["type"] == "user"),
            "a runtime that is not making its own needs to be asked"
        );
        let uid = &doc["linux"]["uidMappings"][0];
        assert_eq!(uid["containerID"], 1000);
        assert_eq!(uid["hostID"], 501);
        assert_eq!(uid["size"], 1);
    }

    /// The bug the first real `runsc` run hit: `--rootless` makes its own
    /// user namespace, and a bundle that asks for a second one fails in the
    /// gofer with `fork/exec /proc/self/exe: invalid argument`. The uid must
    /// still be asked for — the Sentry applies it itself.
    #[test]
    fn a_rootless_runtime_is_not_asked_for_a_user_namespace() {
        let doc = config(&sandbox(), &rootless(&[]));
        assert!(
            !doc["linux"]["namespaces"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["type"] == "user"),
            "the runtime makes this one itself"
        );
        assert!(doc["linux"].get("uidMappings").is_none());
        assert!(doc["linux"].get("gidMappings").is_none());
        assert_eq!(doc["process"]["user"]["uid"], 1000, "still not root");

        // Everything else is unchanged: this is one namespace, not a mode.
        for kind in ["pid", "ipc", "uts", "mount", "cgroup", "network"] {
            assert!(
                doc["linux"]["namespaces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|n| n["type"] == kind),
                "{kind} is missing"
            );
        }
    }

    #[test]
    fn seccomp_is_absent_unless_an_allowlist_is_given() {
        let doc = config(&sandbox(), &options(&[]));
        assert!(
            doc["linux"].get("seccomp").is_none(),
            "an empty allowlist would deny everything, including execve"
        );

        let doc = config(&sandbox(), &options(&["read", "write", "execve"]));
        let s = &doc["linux"]["seccomp"];
        assert_eq!(s["defaultAction"], "SCMP_ACT_ERRNO");
        assert_eq!(
            s["defaultErrnoRet"], 1,
            "EPERM, as the native filter answers"
        );
        assert_eq!(s["syscalls"][0]["action"], "SCMP_ACT_ALLOW");
        assert_eq!(s["syscalls"][0]["names"][2], "execve");
    }

    #[test]
    fn the_bundle_is_written_where_a_runtime_looks_for_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("bundle");
        let path = write_bundle(&dir, &sandbox(), &options(&["read"])).expect("write");
        assert_eq!(path, dir.join("config.json"));

        let text = std::fs::read_to_string(&path).expect("read back");
        let parsed: Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(parsed["ociVersion"], OCI_VERSION);
        assert_eq!(parsed, config(&sandbox(), &options(&["read"])));
    }
}
