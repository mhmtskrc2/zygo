// SPDX-License-Identifier: Apache-2.0
//! Error types.
//!
//! Every variant carries enough context to print an actionable message: which
//! primitive failed, what was attempted, and — where one exists — the doc
//! anchor explaining how to fix the environment. Phase 1 requires that no
//! failing kernel primitive surfaces as a bare `errno`.

use std::path::PathBuf;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Spec(#[from] crate::spec::SpecError),

    #[error(transparent)]
    Image(#[from] crate::image::ImageError),

    #[error(transparent)]
    Protocol(#[from] crate::protocol::ProtocolError),

    /// A kernel primitive the sandbox depends on failed or is unavailable.
    /// `remedy` is shown to the user verbatim.
    ///
    /// `operation` is written to read as the subject of "failed", so it is
    /// usually a syscall name — but not always: a wall-clock timeout reads
    /// better as "the sandbox failed: Connection timed out" than as the
    /// syscall that noticed.
    #[error("{operation} failed: {source}\n  → {remedy}")]
    Primitive {
        operation: &'static str,
        remedy: String,
        #[source]
        source: std::io::Error,
    },

    #[error("{backend} backend is not available on this host: {reason}\n  → {remedy}")]
    BackendUnavailable {
        backend: &'static str,
        reason: String,
        remedy: String,
    },

    /// A dependency build ran and failed: `pip` could not resolve a
    /// requirement, `apt` could not find a package.
    ///
    /// Its own variant because of the exit code (E-02). These used to be
    /// `BackendUnavailable`, which is exit **125** — "this host cannot run
    /// sandboxes". A CI job reading that code is told to try another machine,
    /// when what actually happened is that a line in `requirements.txt` names
    /// a package that does not exist. The host is fine; the input is wrong,
    /// and wrong input is exit 1.
    ///
    /// A build that could not *start* is still `BackendUnavailable`: that one
    /// really is about the host.
    #[error("{what} failed: {reason}\n  → {remedy}")]
    Build {
        what: &'static str,
        reason: String,
        remedy: String,
    },

    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// An I/O failure with no path attached.
    ///
    /// Deliberately **not** `#[from]` (E-03). With the conversion derived, a
    /// bare `?` on any `io::Result` compiled and produced "No such file or
    /// directory" with nothing to say which file — in a mount plan with
    /// thirty entries, that is not a diagnosis. Without it the compiler asks
    /// for [`IoContext::at`], which carries the path, and the few places that
    /// genuinely have no path say so by naming this variant.
    #[error(transparent)]
    Bare(std::io::Error),
}

impl Error {
    /// Wrap an I/O failure with the path it happened on. Bare `io::Error` has
    /// no path, which makes "No such file or directory" useless in a mount plan
    /// with thirty entries.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub fn primitive(
        operation: &'static str,
        remedy: impl Into<String>,
        source: std::io::Error,
    ) -> Self {
        Error::Primitive {
            operation,
            remedy: remedy.into(),
            source,
        }
    }

    /// Process exit code for this error, following the CLI convention:
    /// 1 = generic failure, 2 = usage/spec error, 125 = environment cannot run
    /// the sandbox (mirrors `docker run`'s "daemon error" code), 137 = the
    /// sandbox was killed.
    ///
    /// A sandbox that overran its wall-clock budget reports **137**, not 125.
    /// 125 means "the host could not run this"; a timeout means the host ran it
    /// and then killed it, which is a different thing for anything reading exit
    /// codes. 137 is `128 + SIGKILL`, the same answer `docker run` gives and
    /// literally what happened — the deadline is enforced with `cgroup.kill` or
    /// a `SIGKILL` to every member of the request's cgroup.
    /// Whether this is the launcher's own deadline, rather than any other
    /// failure that also ends in a `SIGKILL`.
    ///
    /// The wait status cannot say: a deadline kill and an out-of-memory kill
    /// are both exit 137, and only the side that enforced the deadline knows
    /// which it was. The warm path records the same thing in
    /// `Outcome::timed_out`; this is the one-shot path's version of it.
    pub fn timed_out(&self) -> bool {
        matches!(
            self,
            Error::Primitive { source, .. } if source.raw_os_error() == Some(libc::ETIMEDOUT)
        )
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Spec(_) => 2,
            Error::Primitive { source, .. } if source.raw_os_error() == Some(libc::ETIMEDOUT) => {
                128 + libc::SIGKILL
            }
            Error::BackendUnavailable { .. } | Error::Primitive { .. } => 125,
            // A build that ran and failed is the caller's input, not the
            // host: exit 1, like any other "what you asked for is wrong".
            Error::Build { .. } => 1,
            _ => 1,
        }
    }
}

/// Convenience for `std::fs` calls that should carry their path on failure.
pub trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::result::Result<T, std::io::Error> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|e| Error::io(path, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primitive(errno: i32) -> Error {
        Error::primitive(
            "timeout",
            "raise `timeout` if the work legitimately takes longer",
            std::io::Error::from_raw_os_error(errno),
        )
    }

    #[test]
    fn a_timed_out_sandbox_reports_that_it_was_killed() {
        // 137 = 128 + SIGKILL, the same answer `docker run` gives — and what
        // actually happened, since the deadline is enforced by killing the
        // request's cgroup.
        assert_eq!(primitive(libc::ETIMEDOUT).exit_code(), 137);
    }

    #[test]
    fn a_host_that_cannot_run_the_sandbox_is_a_different_code() {
        // The distinction a script branches on: 125 is "this host could not
        // run it", 137 is "it ran and was killed".
        assert_eq!(primitive(libc::ENOSYS).exit_code(), 125);
        assert_eq!(
            Error::BackendUnavailable {
                backend: "ns",
                reason: "no cgroup v2".into(),
                remedy: "delegate controllers".into(),
            }
            .exit_code(),
            125
        );
    }

    #[test]
    fn a_spec_problem_is_a_usage_error() {
        assert_eq!(
            Error::Spec(crate::spec::SpecError::Read {
                file: "sandbox.toml".into(),
                message: "no such file".into(),
            })
            .exit_code(),
            2
        );
    }
}
