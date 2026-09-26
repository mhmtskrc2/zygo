// SPDX-License-Identifier: Apache-2.0
//! Backend-independent description of a sandbox.
//!
//! Everything here is pure data: the mount plan, the limit set and the identity
//! of the sandbox. A backend consumes this and draws the isolation boundary
//! wherever it draws it — namespaces, gVisor's Sentry, or a VM. Behavioural
//! equivalence across backends is much easier to hold when the
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
    /// Three descriptors for the sandbox's stdin, stdout and stderr, kept
    /// distinct.
    ///
    /// The other shape `stdio` has: a caller that is not this process — a
    /// `zygo run` client whose sandbox the *supervisor* is starting on its
    /// behalf — hands over its own three streams, and they are three
    /// different things (a pipe on stdin, a terminal on stdout, a file on
    /// stderr) that must stay three different things. `stdio` collapses them
    /// into one, which is right for a pty and wrong for everything else.
    ///
    /// Takes precedence over `stdio` when both are set; the launcher applies
    /// exactly one of them.
    pub stdio_streams: Option<[std::os::fd::RawFd; 3]>,
    /// Signal dispositions for the program: `Some(mask)` resets every signal
    /// to its default and then ignores those in `mask` (bit `n - 1` for
    /// signal `n`); `None` leaves what this process would pass on.
    ///
    /// Set for a sandbox started on another process's behalf, with *that*
    /// process's ignored set: dispositions survive `clone3` and `SIG_IGN`
    /// survives `execve`, so the program would otherwise get the
    /// supervisor's — and a supervisor a script put in the background with
    /// `&` has `SIGINT` ignored.
    pub ignored_signals: Option<u64>,
    /// A connected socket for the runtime agent, placed at
    /// [`crate::pool::AGENT_FD`] in the sandbox.
    ///
    /// Passing a descriptor rather than a socket path means nothing has to
    /// exist in the sandbox's filesystem for the agent to reach the supervisor.
    pub agent_fd: Option<std::os::fd::RawFd>,
    /// Build the sandbox and then *hold* it instead of running `argv`.
    ///
    /// Warm-exec: the namespaces, mounts, cgroup
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
    /// The limits belong to this sandbox alone.
    ///
    /// Set for a one-shot ([`ResolvedFn::one_shot`](crate::spec::ResolvedFn)):
    /// the launcher then gives it a function cgroup of its own instead of the
    /// one every sandbox of the same name shares, so `--mem` bounds this
    /// sandbox and not everything running under that name at once. See
    /// [`crate::cgroup::Hierarchy::function_name`].
    pub own_limits: bool,
}

impl SandboxConfig {
    /// An ordinary one-shot sandbox, for tests.
    ///
    /// The same literal was written out in several test modules; a field added
    /// to `SandboxConfig` had to be added to every one of them before anything
    /// compiled. Here it is written once, and
    /// a test that cares about one field changes that field.
    #[doc(hidden)]
    pub fn for_tests() -> SandboxConfig {
        let limits = Limits::for_tests();
        let view = crate::sandbox::RootfsView::Flat {
            dir: "/store/flat/for-tests".into(),
        };
        let mounts = crate::sandbox::mount::plan("/tmp/newroot", &view, &limits, &[]);
        SandboxConfig {
            id: SandboxId::new("for-tests"),
            isolation: Isolation::Ns,
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
}

/// `PATH` inside a sandbox whose image sets none.
///
/// What a stock Debian or Alpine image exports, so `python3` and `sh` resolve
/// the way they would under `docker run`. Shared by the launcher's `execve`
/// candidate search and by the venv cache, which has to put `/venv/bin` in
/// front of exactly this.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The state machine a warm sandbox moves through.
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
        // The *function*, not the tenant, and the name is now wrong for what
        // it holds — but it is a variable tenant code reads, so changing it
        // would break handlers to correct a word. `ZYGO_FUNCTION` is the same
        // value under the right name; both are set until the next release
        // that may break a handler.
        merged.insert("ZYGO_TENANT".to_string(), f.name.clone());
        merged.insert("ZYGO_FUNCTION".to_string(), f.name.clone());
        // A home that can be written to, unless the image or the spec named
        // one. Docker fills `HOME` in from the image's passwd entry for the
        // user; the sandbox's uid rarely has one, and then everything that
        // expands `~` — pip's cache, npm's, git's config — lands on the
        // read-only root and fails with a sentence about a directory nobody
        // asked for. `/tmp` is the scratch mount: the one place every sandbox
        // can write, sized by `scratch`, gone with the run.
        merged
            .entry("HOME".to_string())
            .or_insert_with(|| "/tmp".to_string());
        // Thread pools sized to the quota, not to the host. OpenBLAS, OpenMP,
        // MKL, polars and Rayon count the host's CPUs, start that many
        // spinning workers, and `cpu.max` then throttles all of them together:
        // on a 5-CPU host with `cpu = 2`, eight warm numpy requests at once
        // ran at 24–38 requests a second, and at 155–173 with one BLAS thread
        // each. Only filled in where the image and the spec said nothing.
        for (key, value) in thread_hints(f.limits.cpu) {
            merged.entry(key.to_string()).or_insert(value);
        }
        let env: Vec<(String, String)> = merged.into_iter().collect();

        Self {
            id: SandboxId::with_tenant(&f.tenant, &f.name),
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
            stdio_streams: None,
            ignored_signals: None,
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
            own_limits: f.one_shot,
        }
    }
}

/// The variables that size a runtime's worker pool, set to the CPU quota
/// rounded up, and never above what the host has.
///
/// `PYTHON_CPU_COUNT` is what `os.cpu_count()` returns from Python 3.13 on,
/// which `concurrent.futures` and `multiprocessing` size their pools from;
/// older interpreters ignore it. `GOMAXPROCS` is for a Go program before 1.25,
/// which does not read `cpu.max` itself.
pub fn thread_hints(cpu: crate::spec::Cpu) -> Vec<(&'static str, String)> {
    let host = std::thread::available_parallelism().map_or(1, |n| n.get());
    let n = (cpu.cores().ceil() as usize)
        .clamp(1, host.max(1))
        .to_string();
    [
        "OMP_NUM_THREADS",
        "OPENBLAS_NUM_THREADS",
        "MKL_NUM_THREADS",
        "NUMEXPR_NUM_THREADS",
        "VECLIB_MAXIMUM_THREADS",
        "RAYON_NUM_THREADS",
        "POLARS_MAX_THREADS",
        "PYTHON_CPU_COUNT",
        "GOMAXPROCS",
    ]
    .into_iter()
    .map(|k| (k, n.clone()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Layer, ResolveOptions, resolve_standalone};

    #[test]
    fn thread_pools_follow_the_quota_unless_the_spec_says_otherwise() {
        let mut f = resolve_standalone(
            "demo",
            &Layer {
                image: Some("alpine".into()),
                cmd: Some(vec!["/bin/true".into()]),
                cpu: Some(crate::spec::Cpu(0.5)),
                ..Default::default()
            },
            &ResolveOptions::default(),
        )
        .unwrap();
        f.env.insert("OMP_NUM_THREADS".into(), "7".into());
        let view = RootfsView::Flat {
            dir: PathBuf::from("/data/flat/x"),
        };
        let cfg = SandboxConfig::from_resolved(&f, &view, "/newroot", vec!["x".into()], &[]);
        let get = |k: &str| {
            cfg.env
                .iter()
                .find(|(a, _)| a == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("OPENBLAS_NUM_THREADS"), Some("1"));
        assert_eq!(get("PYTHON_CPU_COUNT"), Some("1"));
        assert_eq!(
            get("OMP_NUM_THREADS"),
            Some("7"),
            "the spec's own value wins"
        );
    }

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

        assert_eq!(
            cfg.id.tenant,
            crate::spec::DEFAULT_TENANT,
            "nobody named a tenant, so it is the operator's own"
        );
        assert_eq!(cfg.id.name, "demo");
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
        assert_eq!(
            get("HOME").as_deref(),
            Some("/tmp"),
            "a writable home by default"
        );
    }

    /// `HOME` is a default, not an override: an image that sets one, or a
    /// spec that does, keeps it.
    #[test]
    fn a_home_the_image_or_the_spec_names_is_kept() {
        let view = RootfsView::Flat {
            dir: PathBuf::from("/x"),
        };
        let cfg = |spec_env: Option<(&str, &str)>, image_env: &[(String, String)]| {
            let f = resolve_standalone(
                "demo",
                &Layer {
                    image: Some("alpine".into()),
                    cmd: Some(vec!["/bin/true".into()]),
                    env: spec_env
                        .map(|(k, v)| [(k.to_string(), v.to_string())].into_iter().collect()),
                    ..Default::default()
                },
                &ResolveOptions::default(),
            )
            .unwrap();
            let cfg = SandboxConfig::from_resolved(
                &f,
                &view,
                "/newroot",
                vec!["/bin/true".into()],
                image_env,
            );
            cfg.env
                .iter()
                .find(|(k, _)| k == "HOME")
                .map(|(_, v)| v.clone())
        };

        let image_home = [("HOME".to_string(), "/home/app".to_string())];
        assert_eq!(
            cfg(None, &image_home).as_deref(),
            Some("/home/app"),
            "the image's"
        );
        assert_eq!(
            cfg(Some(("HOME", "/work")), &[]).as_deref(),
            Some("/work"),
            "the spec's"
        );
        assert_eq!(
            cfg(Some(("HOME", "/work")), &image_home).as_deref(),
            Some("/work"),
            "the spec over the image"
        );
    }

    #[test]
    fn states_render_for_zygo_ps() {
        assert_eq!(SandboxState::Warm.to_string(), "warm");
        assert_eq!(SandboxState::Paused.to_string(), "paused");
    }
}
