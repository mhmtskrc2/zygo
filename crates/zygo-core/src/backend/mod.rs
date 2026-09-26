// SPDX-License-Identifier: Apache-2.0
//! Isolation backends: where the boundary is drawn (design doc §3.9).
//!
//! `ns`, `gvisor` and `vm` all consume the same [`SandboxConfig`] and speak the
//! same warm-execution protocol. Requirement N8 is that the same spec and the
//! same test suite pass on all three — the trait exists to make that structural
//! rather than aspirational.

use crate::error::{Error, Result};
use crate::sandbox::{SandboxConfig, SandboxState};
use crate::spec::Isolation;

pub mod gvisor;
#[cfg(target_os = "linux")]
pub mod ns;
pub mod vm;

/// What the kernel had to say about a sandbox, after it ended.
///
/// Everything here is a *reason*, which the wait status cannot carry: a
/// deadline kill and an out-of-memory kill are both `SIGKILL`, so both are
/// exit 137.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SandboxOutcome {
    /// Processes the kernel killed here for running out of memory.
    pub oom_kills: u64,
    /// Peak resident memory, where the kernel reports one (5.19+).
    pub peak_rss_kb: u64,
}

/// A started sandbox, owned by whoever will outlive it.
///
/// `Sync` as well as `Send` because the supervisor shares one `WarmFn` — and so
/// the handle inside it — across connection threads. Every implementation is
/// plain data (a pid, a state, a cgroup path); the kernel owns the process, not
/// this struct, so there is nothing here that a second thread could tear.
pub trait Sandbox: Send + Sync {
    /// Host pid of the sandbox's init process.
    fn pid(&self) -> u32;

    /// What the kernel recorded about this sandbox, read while it still had a
    /// cgroup to read it from.
    ///
    /// Sampled by the backend during teardown rather than by the caller
    /// afterwards, because the cgroup directory is removed as part of reaping
    /// and the counters go with it — a caller that read them after `wait`
    /// found an empty directory and reported zero, which reads exactly like
    /// "it was not killed". Found by `tests/linux/verify_api.sh` against a real
    /// kernel.
    fn outcome(&self) -> SandboxOutcome {
        SandboxOutcome::default()
    }

    /// The sandbox's namespaces, held open, for a held (warm-exec) sandbox.
    ///
    /// `None` for a sandbox that ran a program: nothing needs to enter it.
    fn namespaces(&self) -> Option<&NamespaceFds> {
        None
    }

    /// The cgroup this sandbox — and everything it forks — lives in, for a
    /// backend that has one.
    ///
    /// Per-request cgroups go under it, and it is what tearing the sandbox
    /// down kills. Nothing outside it belongs to this sandbox, which is what
    /// lets a replacement start while this one is still running.
    fn cgroup(&self) -> Option<&std::path::Path> {
        None
    }

    /// The sandbox's `/run/secrets`, for a backend whose sandboxes hand one
    /// out.
    ///
    /// A descriptor rather than a path because a held sandbox cannot be
    /// reached through `/proc` at all: it is not dumpable, and nothing an
    /// unprivileged supervisor does changes that. `None` where the supervisor
    /// can still use a path — an agent, which `execve` made readable again.
    fn secrets_dir(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        None
    }

    fn state(&self) -> SandboxState;

    /// Wait for the sandbox to exit and return its exit code.
    fn wait(&mut self) -> Result<i32>;

    /// Kill the whole process tree.
    fn kill(&mut self) -> Result<()>;
}

/// Open descriptors on a sandbox's namespaces, in the order `setns` wants
/// them: user first, because it is what grants the capability to enter the
/// rest.
///
/// Held by the supervisor for the life of a warm-exec sandbox. A descriptor
/// pins its namespace, so these also keep the namespaces alive independently
/// of which processes happen to be in them at any moment.
#[derive(Debug)]
pub struct NamespaceFds {
    pub user: std::os::fd::OwnedFd,
    pub pid: std::os::fd::OwnedFd,
    pub net: std::os::fd::OwnedFd,
    pub ipc: std::os::fd::OwnedFd,
    pub uts: std::os::fd::OwnedFd,
    pub cgroup: std::os::fd::OwnedFd,
    /// Entered last, by the request process itself, after everything that
    /// needs the host's view of the filesystem is done.
    pub mnt: std::os::fd::OwnedFd,
}

impl NamespaceFds {
    /// Open every namespace of `pid` from `/proc`.
    ///
    /// Needs ptrace-read access to the process. From the supervisor that is
    /// always granted for a sandbox it created: same host uid, and full
    /// capabilities in the child user namespace it owns.
    pub fn open(pid: u32) -> crate::error::Result<NamespaceFds> {
        use crate::error::IoContext;
        let open = |kind: &str| {
            let path = std::path::PathBuf::from(format!("/proc/{pid}/ns/{kind}"));
            std::fs::File::open(&path)
                .at(&path)
                .map(std::os::fd::OwnedFd::from)
        };
        Ok(NamespaceFds {
            user: open("user")?,
            pid: open("pid")?,
            net: open("net")?,
            ipc: open("ipc")?,
            uts: open("uts")?,
            cgroup: open("cgroup")?,
            mnt: open("mnt")?,
        })
    }
}

/// Starts sandboxes.
pub trait Backend: Send + Sync {
    fn name(&self) -> &'static str;

    /// Whether this host can run this backend, with the reason when it cannot.
    fn availability(&self) -> Availability;

    fn start(&self, config: &SandboxConfig) -> Result<Box<dyn Sandbox>>;
}

/// Why a backend can or cannot be used here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unavailable { reason: String, remedy: String },
}

impl Availability {
    pub fn is_available(&self) -> bool {
        matches!(self, Availability::Available)
    }

    pub fn unavailable(reason: impl Into<String>, remedy: impl Into<String>) -> Self {
        Availability::Unavailable {
            reason: reason.into(),
            remedy: remedy.into(),
        }
    }

    /// Turn an unavailable backend into the error a user should see.
    pub fn into_error(self, backend: &'static str) -> Option<Error> {
        match self {
            Availability::Available => None,
            Availability::Unavailable { reason, remedy } => Some(Error::BackendUnavailable {
                backend,
                reason,
                remedy,
            }),
        }
    }
}

/// Pick the backend for an isolation level on this host.
///
/// `paths` is passed rather than re-derived, and that is the whole point of
/// the argument. `VmBackend` looks up its guest kernel under the data
/// directory, and when it called `Paths::from_env()` for itself a caller that
/// had been given `--data-root` got a backend pointing at the *default* data
/// directory: `zygo --data-root /tmp/x run --isolation vm` found a kernel that
/// was not in `/tmp/x`, and `doctor` reported one that was not there either.
/// Same failure as `Paths::with_runtime` guards against, one layer down — two
/// halves of a layout derived independently agree until the day they do not.
pub fn for_isolation(isolation: Isolation, paths: &crate::Paths) -> Result<Box<dyn Backend>> {
    let backend: Box<dyn Backend> = match isolation {
        #[cfg(target_os = "linux")]
        Isolation::Ns => Box::new(ns::NsBackend::with_paths(paths)),
        #[cfg(not(target_os = "linux"))]
        Isolation::Ns => Box::new(Unimplemented {
            name: "ns",
            reason: format!(
                "namespaces are a Linux feature; this host runs {}",
                std::env::consts::OS
            ),
            remedy: "run Zygo inside a Linux VM or container; on macOS the `zygo` \
                     binary normally forwards into one it manages"
                .into(),
        }),
        Isolation::Vm => Box::new(vm::VmBackend::with_paths(paths)),
        Isolation::Gvisor => Box::new(gvisor::GvisorBackend::new()),
    };

    match backend.availability().into_error(backend.name()) {
        Some(e) => Err(e),
        None => Ok(backend),
    }
}

/// Placeholder for a backend that exists in the design but cannot exist in
/// this binary — a Linux-only backend compiled for something else. It reports
/// *why* rather than being silently missing, so the flag gives a straight
/// answer instead of an unknown-backend error.
#[cfg(not(target_os = "linux"))]
struct Unimplemented {
    name: &'static str,
    reason: String,
    remedy: String,
}

#[cfg(not(target_os = "linux"))]
impl Backend for Unimplemented {
    fn name(&self) -> &'static str {
        self.name
    }

    fn availability(&self) -> Availability {
        Availability::unavailable(self.reason.clone(), self.remedy.clone())
    }

    fn start(&self, _config: &SandboxConfig) -> Result<Box<dyn Sandbox>> {
        Err(Error::BackendUnavailable {
            backend: self.name,
            reason: self.reason.clone(),
            remedy: self.remedy.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Box<dyn Backend>` is not `Debug`, so `unwrap_err` is unavailable.
    fn expect_error(isolation: Isolation) -> Error {
        match for_isolation(isolation, &crate::Paths::from_env()) {
            Ok(_) => panic!("{isolation} unexpectedly available"),
            Err(e) => e,
        }
    }

    /// `gvisor` is built now, so the only way it is unavailable is a host
    /// that cannot run it — and then it must say which of the two reasons it
    /// is, because "install runsc" and "you are on macOS" have different
    /// answers. A backend that is merely absent must never read as a bug.
    #[test]
    fn gvisor_is_unavailable_only_for_a_reason_the_user_can_act_on() {
        match for_isolation(Isolation::Gvisor, &crate::Paths::from_env()) {
            // A host with runsc on its PATH: nothing to assert but that it
            // resolved to the real backend rather than a placeholder.
            Ok(backend) => assert_eq!(backend.name(), "gvisor"),
            Err(e) => {
                let msg = e.to_string();
                assert!(msg.contains("gvisor"), "{msg}");
                if cfg!(target_os = "linux") {
                    assert!(msg.contains("runsc"), "{msg}");
                    assert!(msg.contains("zygo backend install gvisor"), "{msg}");
                } else {
                    assert!(msg.contains("Linux"), "{msg}");
                }
                assert_eq!(e.exit_code(), 125);
            }
        }
    }

    #[test]
    /// The `vm` backend says *which* of its preconditions is missing, and what
    /// to do about it.
    ///
    /// There are three, and they fail in a deliberate order: a binary built
    /// without the feature has no monitor at all, a host without a usable KVM
    /// cannot run one, and a host with both still needs a guest kernel. A
    /// single "not available" would send somebody to the wrong one — which is
    /// what the placeholder this replaced did, by naming libkrun on a host
    /// that had no KVM either.
    fn vm_says_which_of_its_preconditions_is_missing() {
        let err = expect_error(Isolation::Vm).to_string();
        let expected = if cfg!(not(feature = "vm")) {
            "without the `vm` feature"
        } else if cfg!(not(target_os = "linux")) {
            "KVM is a Linux feature"
        } else {
            // On a Linux host with the feature the answer depends on the
            // machine, so what is asserted is that it named one of them.
            for candidate in ["/dev/kvm", "guest kernel"] {
                if err.contains(candidate) {
                    return;
                }
            }
            panic!("the vm backend refused without naming a precondition: {err}");
        };
        assert!(err.contains(expected), "{err}");
    }

    #[test]
    fn availability_converts_to_an_actionable_error() {
        assert!(Availability::Available.into_error("ns").is_none());
        let e = Availability::unavailable("no kvm", "use ns")
            .into_error("vm")
            .unwrap();
        assert!(e.to_string().contains("no kvm"));
        assert!(e.to_string().contains("use ns"));
    }
}
