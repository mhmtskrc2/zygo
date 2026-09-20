//! OCI images: references, the content-addressed store, and the Distribution
//! client that fills it.
//!
//! Zygo never builds images (ADR-004). It consumes what `docker build`,
//! `buildah` or any registry produces, because compatibility is adoption
//! (principle P4).

pub mod media;
pub mod reference;
pub mod store;

#[cfg(feature = "registry")]
pub mod auth;
#[cfg(feature = "registry")]
pub mod registry;

pub use media::{ImageConfig, Index, LayerCompression, Manifest, Platform};
pub use reference::Reference;
pub use store::{ImageEntry, Store, Whiteouts};

#[cfg(feature = "registry")]
pub use registry::{PullProgress, RegistryClient, Verified};

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("invalid image reference `{input}`: {reason}")]
    Reference { input: String, reason: String },

    #[error("invalid digest `{input}`: {reason}")]
    Digest { input: String, reason: String },

    #[error(
        "digest mismatch: expected {expected}, got {got}\n  → the registry returned corrupted or tampered data"
    )]
    DigestMismatch { expected: String, got: String },

    #[error(
        "unsafe path in image layer: {0}\n  → the layer tries to write outside its own tree; refusing to unpack"
    )]
    UnsafePath(String),

    #[error("cannot unpack layer: {0}")]
    Unpack(String),

    #[error("unsupported layer media type `{0}`\n  → Zygo reads tar, tar+gzip and tar+zstd layers")]
    UnsupportedLayer(String),

    #[error(
        "image `{reference}` is not available for {wanted}\n  → the registry offers: {available}"
    )]
    NoMatchingPlatform {
        reference: String,
        wanted: String,
        available: String,
    },

    #[error("image `{0}` is not in the local store\n  → run `zygo pull {0}`")]
    NotPulled(String),

    #[error("registry error: {0}")]
    Registry(String),

    #[error(
        "registry authentication failed for {registry}: {reason}\n  → run `zygo login {registry}`"
    )]
    Auth { registry: String, reason: String },
}

impl ImageError {
    pub fn reference(input: &str, reason: impl Into<String>) -> Self {
        ImageError::Reference {
            input: input.to_string(),
            reason: reason.into(),
        }
    }

    pub fn digest(input: &str, reason: impl Into<String>) -> Self {
        ImageError::Digest {
            input: input.to_string(),
            reason: reason.into(),
        }
    }
}
