//! # zygo-core
//!
//! Core library behind the `zygo` CLI, the Python/TypeScript bindings and any
//! platform embedding Zygo directly (see ADR-008: the library is the product,
//! the CLI is a thin client over it).
//!
//! The crate is organised around the lifecycle of a sandbox:
//!
//! - [`spec`] — the declarative `sandbox.toml` surface and the layering rules
//!   that turn it, together with CLI flags, into a [`spec::ResolvedFn`].
//! - [`image`] — OCI reference parsing, the content-addressed store and the
//!   Distribution client that fills it.
//! - [`sandbox`] — backend-independent descriptions of what a sandbox *is*: the
//!   mount plan and the resource limits.
//! - [`cgroup`] — the two-level cgroup v2 hierarchy (`zygo.slice/{system,tenants}`).
//! - [`backend`] — where the isolation boundary is drawn: `ns`, `gvisor`, `vm`.
//! - [`pool`] — the warm pool: sandboxes that are already built when the
//!   request arrives. This is what the whole crate exists to make possible.
//! - [`protocol`] — the language-independent warm execution wire protocol.
//! - [`supervisor`] — the process that owns the warm pool between commands,
//!   and the control protocol the CLI reaches it over.
//! - [`venv`] — the dependency cache: `requirements.txt` built once, inside
//!   the image it will run in, and shared by every function that lists the
//!   same one.
//! - [`doctor`] — environment probing, shared by `zygo doctor` and by backend
//!   preflight checks.
//!
//! Everything except [`backend`]'s Linux implementations is platform
//! independent and unit tested on any host.

// Linked for its symbols, never named in Rust.
//
// libkrun exports a C ABI from a Rust crate: `backend::vm` declares the
// functions in an `extern "C"` block and the linker resolves them against this
// rlib. Nothing here *calls* a Rust item from it, so without this line the
// crate is dropped and every `krun_*` symbol is undefined. It has to be at the
// crate root — the same line inside a nested module is a different resolution
// and fails with "can't find crate".
//
// The crate is `krun`; the *package* is `libkrun`. `[lib] name = "krun"` in
// its manifest, which is why `extern crate libkrun` fails with exactly the
// same message as a missing dependency.
#[cfg(feature = "vm")]
extern crate krun;

pub mod backend;
pub mod cgroup;
pub mod derive;
pub mod doctor;
pub mod error;
pub mod image;
pub mod lock;
pub mod net;
pub mod oci;
pub mod paths;
pub mod pool;
pub mod protocol;
pub mod sandbox;
pub mod scripts;
pub mod spec;
pub mod supervisor;
pub mod venv;

pub use error::{Error, Result};
pub use paths::Paths;
pub use spec::{ResolvedFn, Spec};

/// Version of this crate, surfaced in `zygo --version` and in the `READY`
/// handshake so agents can detect a supervisor mismatch.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
