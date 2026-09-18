//! Isolation backends: where the boundary is drawn (design doc §3.9).
//!
//! `ns`, `gvisor` and `vm` all consume the same [`SandboxConfig`] and speak the
//! same warm-execution protocol. Requirement N8 is that the same spec and the
//! same test suite pass on all three — the trait exists to make that structural
//! rather than aspirational.

use crate::error::{Error, Result};
use crate::sandbox::{SandboxConfig, SandboxState};
use crate::spec::Isolation;

#[cfg(target_os = "linux")]
pub mod ns;

/// A started sandbox.
/// A started sandbox, owned by whoever will outlive it.
///
/// `Sync` as well as `Send` because the supervisor shares one `WarmFn` — and so
/// the handle inside it — across connection threads. Every implementation is
/// plain data (a pid, a state, a cgroup path); the kernel owns the process, not
/// this struct, so there is nothing here that a second thread could tear.
pub trait Sandbox: Send + Sync {
    /// Host pid of the sandbox's init process.
    fn pid(&self) -> u32;

    fn state(&self) -> SandboxState;

    /// Wait for the sandbox to exit and return its exit code.
    fn wait(&mut self) -> Result<i32>;

    /// Kill the whole process tree.
    fn kill(&mut self) -> Result<()>;
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
pub fn for_isolation(isolation: Isolation) -> Result<Box<dyn Backend>> {
    let backend: Box<dyn Backend> = match isolation {
        #[cfg(target_os = "linux")]
        Isolation::Ns => Box::new(ns::NsBackend::new()),
        #[cfg(not(target_os = "linux"))]
        Isolation::Ns => Box::new(Unimplemented {
            name: "ns",
            reason: format!(
                "namespaces are a Linux feature; this host runs {}",
                std::env::consts::OS
            ),
            remedy: "run Zygo inside a Linux VM or container (macOS shim is phase 5)".into(),
        }),
        Isolation::Vm => Box::new(Unimplemented {
            name: "vm",
            reason: "the libkrun backend is not built into this binary yet".into(),
            remedy: "track phase 2 in todo.md; use --isolation ns meanwhile".into(),
        }),
        Isolation::Gvisor => Box::new(Unimplemented {
            name: "gvisor",
            reason: "the gVisor backend is not implemented yet".into(),
            remedy: "track phase 4 in todo.md; use --isolation ns or vm meanwhile".into(),
        }),
    };

    match backend.availability().into_error(backend.name()) {
        Some(e) => Err(e),
        None => Ok(backend),
    }
}

/// Placeholder for a backend that exists in the design but not yet in the
/// binary. It reports *why* rather than being silently missing, so
/// `--isolation vm` gives a straight answer instead of an unknown-flag error.
struct Unimplemented {
    name: &'static str,
    reason: String,
    remedy: String,
}

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
        match for_isolation(isolation) {
            Ok(_) => panic!("{isolation} unexpectedly available"),
            Err(e) => e,
        }
    }

    #[test]
    fn unimplemented_backends_explain_themselves() {
        let err = expect_error(Isolation::Gvisor);
        let msg = err.to_string();
        assert!(msg.contains("gvisor"), "{msg}");
        assert!(msg.contains("phase 4"), "{msg}");
        assert_eq!(err.exit_code(), 125);
    }

    #[test]
    fn vm_says_it_is_pending_rather_than_unknown() {
        let err = expect_error(Isolation::Vm);
        assert!(err.to_string().contains("libkrun"), "{err}");
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
