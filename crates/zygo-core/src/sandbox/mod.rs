//! Backend-independent description of a sandbox.
//!
//! Everything here is pure data: the mount plan, the limit set and the identity
//! of the sandbox. A backend consumes this and draws the isolation boundary
//! wherever it draws it — namespaces, gVisor's Sentry, or a VM. Requirement N8
//! ("behavioural equivalence across backends") is much easier to hold when the
//! backends share the description rather than each deriving their own.

pub mod limits;
pub mod mount;
pub mod oneshot;

use std::path::PathBuf;

pub use limits::{CgroupWrite, Limits, RlimitKind};
pub use mount::{MountOp, MountPlan, RootfsView};

use crate::spec::{Isolation, Network, ResolvedFn, SeccompProfile};

/// Identity of a running sandbox. `tenant` is the cgroup and uid-range key;
/// `name` is what the user typed.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SandboxId {
    pub tenant: String,
    pub name: String,
}

impl SandboxId {
    pub fn new(name: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            tenant: name.clone(),
            name,
        }
    }

    pub fn with_tenant(tenant: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            name: name.into(),
        }
    }
}

/// Everything a backend needs to start one sandbox.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub id: SandboxId,
    pub isolation: Isolation,
    pub seccomp: SeccompProfile,
    pub network: Network,
    pub limits: Limits,
    pub mounts: MountPlan,
    /// Program and arguments to `execve` inside the sandbox.
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub workdir: PathBuf,
    /// uid/gid inside the sandbox, mapped to an unprivileged host range.
    pub uid: u32,
    pub gid: u32,
    /// A descriptor for the sandbox to use as its stdin, stdout and stderr.
    ///
    /// `None` means the sandbox inherits the caller's — which is what makes a
    /// daemonless `zygo run` feel native, but also hands tenant code a writable
    /// descriptor to the user's terminal. `Some(fd)` replaces all three: the
    /// slave side of a pty the caller allocated (`--tty`), which the sandbox
    /// also adopts as its controlling terminal, or a plain pipe, which is how
    /// the venv builder captures what `pip` says.
    pub stdio: Option<std::os::fd::RawFd>,
    /// A connected socket for the runtime agent, placed at
    /// [`crate::pool::AGENT_FD`] in the sandbox.
    ///
    /// Passing a descriptor rather than a socket path means nothing has to
    /// exist in the sandbox's filesystem for the agent to reach the supervisor.
    pub agent_fd: Option<std::os::fd::RawFd>,
    /// Build the sandbox and then *hold* it instead of running `argv`.
    ///
    /// Warm-exec (design doc §3.4, layer 1): the namespaces, mounts, cgroup
    /// and hardening are set up once and kept; each request is a fresh process
    /// entered into them by the supervisor. The init process stays as Zygo's
    /// own code — a reaping loop, never `execve` — so nothing from the image
    /// has to exist for a sandbox to stay up.
    pub hold: bool,
    /// Leave the root writable instead of remounting it read-only.
    ///
    /// Only the derived-layer builder sets this, on a private copy of the
    /// image that exists to be written to and is diffed afterwards. A sandbox
    /// running anyone's code keeps the read-only root: the plan's `bind ro`
    /// is a guarantee, not a default.
    pub writable_root: bool,
    /// The egress allowlist, as the spec wrote it, with every hostname already
    /// resolved to addresses ([`crate::net`]).
    ///
    /// Resolution happens on the host, before the sandbox exists, so nothing
    /// inside it can influence what the filter permits.
    pub allow: Vec<crate::spec::AllowRule>,
    pub allow_resolved: crate::net::Allowed,
    /// Whether the private and link-local rejects are lifted.
    pub allow_private_net: bool,
    /// Where `pasta` writes its pid, so the sandbox can take it down again.
    /// `None` for a sandbox that needs no `pasta`.
    pub pasta_pid_file: Option<PathBuf>,
}

/// `PATH` inside a sandbox whose image sets none.
///
/// What a stock Debian or Alpine image exports, so `python3` and `sh` resolve
/// the way they would under `docker run`. Shared by the launcher's `execve`
/// candidate search and by the venv cache, which has to put `/venv/bin` in
/// front of exactly this.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The state machine a warm sandbox moves through (design doc appendix A).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxState {
    Starting,
    /// In RAM and running.
    Warm,
    /// In RAM but frozen with `cgroup.freeze`.
    Paused,
    /// Shut down; the next request pays a cold start.
    Cold,
    Failed,
}

impl SandboxState {
    pub const fn as_str(self) -> &'static str {
        match self {
            SandboxState::Starting => "starting",
            SandboxState::Warm => "warm",
            SandboxState::Paused => "paused",
            SandboxState::Cold => "cold",
            SandboxState::Failed => "failed",
        }
    }
}

impl std::fmt::Display for SandboxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl SandboxConfig {
    /// Derive a config from a resolved function and a prepared rootfs view.
    ///
    /// `argv` is supplied by the caller because it differs by mode: warm-exec
    /// runs the function's `cmd`, an agent runtime runs the agent with the
    /// handler path.
    ///
    /// `image_env` is the image's own environment. It goes in underneath the
    /// spec's, the way `docker run -e` layers over a Dockerfile `ENV`: images
    /// set `PATH` there, and without it a bare command like `python3` cannot be
    /// found inside the sandbox.
    pub fn from_resolved(
        f: &ResolvedFn,
        rootfs: &RootfsView,
        newroot: impl Into<PathBuf>,
        argv: Vec<String>,
        image_env: &[(String, String)],
    ) -> Self {
        let mounts = mount::plan(newroot, rootfs, &f.limits, &f.mounts);
        let uid = f.user.parse().unwrap_or(1000);

        let mut merged: std::collections::BTreeMap<String, String> =
            image_env.iter().cloned().collect();
        merged.extend(f.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        merged.insert("ZYGO_TENANT".to_string(), f.name.clone());
        let env: Vec<(String, String)> = merged.into_iter().collect();

        Self {
            id: SandboxId::new(&f.name),
            isolation: f.isolation,
            seccomp: f.seccomp,
            network: f.network,
            limits: f.limits.clone(),
            mounts,
            argv,
            env,
            workdir: f.workdir.clone(),
            uid,
            gid: uid,
            stdio: None,
            agent_fd: None,
            hold: false,
            writable_root: false,
            allow: f.allow.clone(),
            // Resolution reaches the network, so it is the caller's to do —
            // and to decide when. An empty set with a non-empty `allow` means
            // the caller has not done it yet, which the launcher refuses.
            allow_resolved: crate::net::Allowed::default(),
            allow_private_net: f.allow_private_net,
            pasta_pid_file: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Layer, ResolveOptions, resolve_standalone};

    #[test]
    fn config_carries_limits_and_plan_through() {
        let f = resolve_standalone(
            "demo",
            &Layer {
                image: Some("alpine".into()),
                cmd: Some(vec!["/bin/true".into()]),
                ..Default::default()
            },
            &ResolveOptions::default(),
        )
        .unwrap();

        let view = RootfsView::Flat {
            dir: PathBuf::from("/data/flat/x"),
        };
        let cfg =
            SandboxConfig::from_resolved(&f, &view, "/newroot", vec!["/bin/true".into()], &[]);

        assert_eq!(cfg.id.tenant, "demo");
        assert_eq!(cfg.limits.pids, 64);
        assert_eq!(cfg.uid, 1000);
        assert!(
            cfg.env
                .iter()
                .any(|(k, v)| k == "ZYGO_TENANT" && v == "demo")
        );
        assert!(!cfg.mounts.ops.is_empty());
    }

    /// Docker layers `-e` over the Dockerfile's `ENV`; so does this.
    #[test]
    fn the_spec_environment_layers_over_the_images() {
        let f = resolve_standalone(
            "demo",
            &Layer {
                image: Some("alpine".into()),
                cmd: Some(vec!["/bin/true".into()]),
                env: Some(
                    [("LANG".to_string(), "tr_TR".to_string())]
                        .into_iter()
                        .collect(),
                ),
                ..Default::default()
            },
            &ResolveOptions::default(),
        )
        .unwrap();
        let view = RootfsView::Flat {
            dir: PathBuf::from("/x"),
        };
        let image_env = [
            ("PATH".to_string(), "/usr/bin:/bin".to_string()),
            ("LANG".to_string(), "C.UTF-8".to_string()),
        ];
        let cfg = SandboxConfig::from_resolved(
            &f,
            &view,
            "/newroot",
            vec!["/bin/true".into()],
            &image_env,
        );

        let get = |k: &str| {
            cfg.env
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("PATH").as_deref(), Some("/usr/bin:/bin"), "inherited");
        assert_eq!(get("LANG").as_deref(), Some("tr_TR"), "the spec wins");
        assert_eq!(get("ZYGO_TENANT").as_deref(), Some("demo"));
    }

    #[test]
    fn states_render_for_zygo_ps() {
        assert_eq!(SandboxState::Warm.to_string(), "warm");
        assert_eq!(SandboxState::Paused.to_string(), "paused");
    }
}
