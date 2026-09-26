// SPDX-License-Identifier: Apache-2.0
//! A one-shot sandbox, started by the supervisor for a client.
//!
//! What `zygo run` does for itself, done here on a client's behalf: this
//! process already sits in a delegated, built `zygo.slice`, and a client
//! on an ordinary systemd session cannot get into one. The client sends
//! its three streams over the control socket ([`ClientStreams`]); the
//! sandbox is started and handed back unwaited ([`Oneshot`]), because the
//! launcher thread must not wait for anybody's program.

use std::path::PathBuf;

use super::Pool;
#[cfg(target_os = "linux")]
use super::request::next_request_id;
#[cfg(target_os = "linux")]
use crate::error::IoContext;
use crate::error::{Error, Result};
use crate::spec::ResolvedFn;

/// What a one-shot started for another process carries of that process:
/// its three streams, whether they are one terminal, and the signals it
/// ignores (see [`SandboxConfig::ignored_signals`](crate::sandbox::SandboxConfig::ignored_signals)).
pub struct ClientStreams {
    pub stdio: [std::os::fd::OwnedFd; 3],
    pub tty: bool,
    pub ignored_signals: u64,
}

/// A one-shot sandbox the supervisor started for a client, not yet waited on.
///
/// The `pivot_root` target is carried so the waiter can remove it once the
/// sandbox is gone — `remove_dir`, not `remove_dir_all`, for the reason
/// `zygo run` gives: a mount that outlived its namespace makes the removal
/// fail and the directory stay, which is the right way round.
pub struct Oneshot {
    pub sandbox: Box<dyn crate::backend::Sandbox>,
    pub newroot: PathBuf,
}

impl Pool {
    /// Start a one-shot sandbox with somebody else's standard streams.
    ///
    /// What `zygo run` does for itself, done here on a client's behalf —
    /// because this process already sits in a delegated, built `zygo.slice`
    /// and the client, on an ordinary systemd session, cannot get into one:
    /// cgroup delegation containment forbids it (`zygo-cli/src/scope.rs`).
    /// Measured at 45 ms for a `zygo run` from a login shell against 11 ms
    /// from a cgroup like this one, and the difference is the whole point.
    ///
    /// Started and **not waited on**: this runs on the launcher thread, and
    /// a launcher that waited would hold every other start on the machine
    /// for as long as the program ran. The caller waits, on its own thread.
    ///
    /// The descriptors are the client's own stdin, stdout and stderr, and
    /// they stay three distinct things; `tty` says they are one terminal the
    /// sandbox should adopt as its controlling terminal. Either way they are
    /// numbered 3 or above — they arrived over `SCM_RIGHTS` — which the
    /// launcher's child relies on when it `dup2`s them into place.
    #[cfg(target_os = "linux")]
    pub fn start_oneshot(&self, f: &ResolvedFn, client: ClientStreams) -> Result<Oneshot> {
        use std::os::fd::AsRawFd;

        use crate::image::{Reference, Store};
        use crate::sandbox::{SandboxConfig, mount};

        for fd in &client.stdio {
            if fd.as_raw_fd() < 3 {
                return Err(Error::primitive(
                    "stdio",
                    "internal supervisor error",
                    std::io::Error::other(
                        "a received descriptor landed on 0, 1 or 2, which the child cannot \
                         dup2 over safely",
                    ),
                ));
            }
        }

        let store = Store::new(self.config.paths.clone());
        let reference: Reference = f.image.parse()?;
        // In the store already, like `serve`: pulling is a network operation
        // with output of its own, and it is the client's.
        let entry = store
            .get(&reference)
            .ok_or_else(|| Error::BackendUnavailable {
                backend: "pool",
                reason: format!("image `{}` is not in the local store", f.image),
                remedy: format!("run `zygo pull {}`", f.image),
            })?;
        let entry = if f.system.is_empty() {
            entry
        } else {
            crate::derive::ensure(&store, &entry, &f.system)?.image
        };

        let mut f = f.clone();
        let venv = match &f.requirements {
            Some(requirements) => {
                if !requirements.is_file() {
                    return Err(Error::Spec(crate::spec::SpecError::invalid(
                        "requirements",
                        format!("{} does not exist", requirements.display()),
                    )));
                }
                let venv = crate::venv::ensure(&store, &entry, requirements)?;
                f.mounts.push(venv.mount());
                Some(venv)
            }
            None => None,
        };
        let entry = crate::bytecode::ensure(&store, &entry)?.image;

        let net = crate::net::setup(&self.config.paths, &f.name, &f)?;
        if let Some(mount) = net.mount.clone() {
            f.mounts.push(mount);
        }
        for w in &net.warnings {
            tracing::warn!("{w}");
        }

        // The image's own config: its default command, and — just as
        // importantly — its `PATH`, without which a bare `python3` cannot be
        // resolved. Read from the store's blob, never fetched.
        let image_config: crate::image::ImageConfig =
            serde_json::from_slice(&store.read_blob(&entry.config)?).map_err(|e| {
                Error::primitive(
                    "image config",
                    "the image's config blob is malformed",
                    std::io::Error::other(e),
                )
            })?;
        let argv = if f.cmd.is_empty() {
            let argv = image_config.default_argv();
            if argv.is_empty() {
                return Err(Error::Spec(crate::spec::SpecError::invalid(
                    "cmd",
                    format!("`{}` declares no entrypoint or cmd", f.image),
                )));
            }
            argv
        } else {
            f.cmd.clone()
        };
        let mut env = image_config.env_pairs();
        if venv.is_some() {
            let image_path = env
                .iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            env.retain(|(k, _)| k != "PATH" && k != "VIRTUAL_ENV");
            env.extend(crate::venv::Venv::env_over(&image_path));
        }

        let mount_points = mount::required_mount_points(&f.mounts);
        let overlay = crate::doctor::cached(&self.config.paths)
            .checks
            .iter()
            .any(|c| c.name == "overlayfs (userns)" && c.status == crate::doctor::Status::Ok);
        let view = store.rootfs_view(&entry.layers, overlay, &mount_points)?;
        let newroot = self.config.paths.tmp().join(format!(
            "run-{}-{}",
            crate::process_token(),
            next_request_id()
        ));
        std::fs::create_dir_all(&newroot).at(&newroot)?;

        let mut config = SandboxConfig::from_resolved(&f, &view, &newroot, argv, &env);
        config.allow_resolved = net.allowed;
        config.pasta_pid_file = net.pid_file;
        config.stdio_streams = Some([
            client.stdio[0].as_raw_fd(),
            client.stdio[1].as_raw_fd(),
            client.stdio[2].as_raw_fd(),
        ]);
        let _ = client.tty; // the child adopts a terminal by trying; see `adopt_streams`
        // The client's dispositions, not this process's: see the field.
        config.ignored_signals = Some(client.ignored_signals);

        let backend = crate::backend::for_isolation(f.isolation, &self.config.paths)?;
        let sandbox = backend.start(&config);
        // Held open across the start and no longer: the child has its own
        // copies now, and these must not outlive the request in a process
        // that spawns other things.
        drop(client.stdio);
        let sandbox = match sandbox {
            Ok(s) => s,
            Err(e) => {
                let _ = std::fs::remove_dir(&newroot);
                return Err(e);
            }
        };
        Ok(Oneshot { sandbox, newroot })
    }

    /// Off Linux there is nothing to start in; the same answer
    /// `Pool::serve_exec` gives, so a supervisor built for macOS still compiles
    /// and still says why.
    #[cfg(not(target_os = "linux"))]
    pub fn start_oneshot(&self, _f: &ResolvedFn, _client: ClientStreams) -> Result<Oneshot> {
        Err(Error::BackendUnavailable {
            backend: "pool",
            reason: "a one-shot sandbox enters Linux namespaces".into(),
            remedy: "run Zygo inside a Linux VM or container; on macOS the `zygo` \
                 binary normally forwards into one it manages"
                .into(),
        })
    }
}
