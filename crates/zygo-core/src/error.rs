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

    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Bare(#[from] std::io::Error),
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
    /// the sandbox (mirrors `docker run`'s "daemon error" code).
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Spec(_) => 2,
            Error::BackendUnavailable { .. } | Error::Primitive { .. } => 125,
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
