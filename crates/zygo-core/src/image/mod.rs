// SPDX-License-Identifier: Apache-2.0
//! OCI images: references, the content-addressed store, and the Distribution
//! client that fills it.
//!
//! Zygo never builds images (rule P4 in `docs/book/08-principles.md`). It
//! consumes what `docker build`,
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

    /// A layer could not be unpacked. `source` is the underlying error when
    /// there is one — a decoder, the filesystem — so a caller can walk the
    /// chain; `message` is what a person reads, and already names the cause.
    #[error("cannot unpack layer: {message}")]
    Unpack {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

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

    /// The registry, or the way to it, failed. As with [`Unpack`]: the
    /// message is complete on its own, and `source` is there for code that
    /// wants to ask what kind of failure it was — a connection reset is worth
    /// a retry where a malformed manifest is not.
    ///
    /// [`Unpack`]: ImageError::Unpack
    #[error("registry error: {message}")]
    Registry {
        message: String,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    #[error(
        "registry authentication failed for {registry}: {reason}\n  → run `zygo login {registry}`"
    )]
    Auth { registry: String, reason: String },
}

impl ImageError {
    /// An unpack failure with no underlying error to point at.
    pub fn unpack(message: impl Into<String>) -> Self {
        ImageError::Unpack {
            message: message.into(),
            source: None,
        }
    }

    /// An unpack failure caused by `source`, whose text is the message.
    pub fn unpack_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        ImageError::Unpack {
            message: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    /// An unpack failure caused by `source`, read as "`context`: `source`".
    pub fn unpack_with(
        context: impl std::fmt::Display,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        ImageError::Unpack {
            message: format!("{context}: {source}"),
            source: Some(Box::new(source)),
        }
    }

    /// A registry failure with no underlying error to point at.
    pub fn registry(message: impl Into<String>) -> Self {
        ImageError::Registry {
            message: message.into(),
            source: None,
        }
    }

    /// A registry failure caused by `source`, whose text is the message.
    pub fn registry_source(source: impl std::error::Error + Send + Sync + 'static) -> Self {
        ImageError::Registry {
            message: source.to_string(),
            source: Some(Box::new(source)),
        }
    }

    /// A registry failure caused by `source`, read as "`context`: `source`".
    pub fn registry_with(
        context: impl std::fmt::Display,
        source: impl std::error::Error + Send + Sync + 'static,
    ) -> Self {
        ImageError::Registry {
            message: format!("{context}: {source}"),
            source: Some(Box::new(source)),
        }
    }
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

/// The `sha256:<hex>` name of a byte string.
///
/// One place, because the spelling is load-bearing: `Store::parse_digest`
/// compares lower-case hex, and a second implementation that produced upper
/// case would write a blob nothing could ever find.
pub fn digest_of(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}
