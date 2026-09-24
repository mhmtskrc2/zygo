//! Blobs: bytes an embedder sends once and names many times.
//!
//! A workspace tar can be sent with every call, and for a one-off that is the
//! right shape. For the case an embedder actually has — the same fixture, the
//! same model weights, the same input document across a thousand calls —
//! sending it a thousand times is a thousand copies over the wire and a
//! thousand base64 encodings of the same megabytes.
//!
//! So: `PUT /blobs` once, `{"blob": "sha256:…"}` on every call after. Exactly
//! the bargain [`crate::scripts`] makes for code, for the same reason, and
//! deliberately a **separate store**: a script is text with a size limit
//! measured in kilobytes and a blob is arbitrary bytes measured in hundreds of
//! megabytes, and a store that held both would have to take the looser of
//! every rule.

use std::path::PathBuf;

use sha2::{Digest as _, Sha256};

use crate::error::{IoContext, Result};
use crate::paths::Paths;
use crate::scripts::ScriptDigest;
use crate::spec::SpecError;

/// Largest blob this host will store.
///
/// The same number [`crate::workspace::MAX_WORKSPACE_BYTES`] uses, because a
/// blob's only use is to become a workspace and a blob that could not be
/// unpacked would be a blob nobody could use.
pub const MAX_BLOB_BYTES: usize = 256 * 1024 * 1024;

/// The blobs on this host, by content digest.
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    pub fn new(paths: &Paths) -> BlobStore {
        BlobStore {
            dir: paths.blob_store(),
        }
    }

    pub fn path(&self, digest: &ScriptDigest) -> PathBuf {
        self.dir.join(digest.hex())
    }

    pub fn contains(&self, digest: &ScriptDigest) -> bool {
        self.path(digest).is_file()
    }

    /// The digest these bytes would have.
    pub fn digest_of(bytes: &[u8]) -> ScriptDigest {
        let hex = hex::encode(Sha256::digest(bytes));
        ScriptDigest::parse(&format!("sha256:{hex}")).expect("a digest this built")
    }

    /// Store bytes and return their name. Idempotent, like a script's.
    pub fn put(&self, bytes: &[u8]) -> Result<(ScriptDigest, bool)> {
        if bytes.len() > MAX_BLOB_BYTES {
            return Err(SpecError::invalid(
                "blob",
                format!(
                    "{} bytes is over the {} MiB limit for one blob",
                    bytes.len(),
                    MAX_BLOB_BYTES / 1024 / 1024
                ),
            )
            .into());
        }
        let digest = BlobStore::digest_of(bytes);
        if self.contains(&digest) {
            return Ok((digest, true));
        }
        std::fs::create_dir_all(&self.dir).at(&self.dir)?;
        // Whole to a temporary name, then renamed: a reader that finds the
        // file finds all of it, and two writers of the same bytes cannot
        // interleave into something that is neither.
        let temporary = self.dir.join(format!(
            ".{}.{}.incoming",
            digest.hex(),
            crate::process_token()
        ));
        std::fs::write(&temporary, bytes).at(&temporary)?;
        let path = self.path(&digest);
        std::fs::rename(&temporary, &path).at(&path)?;
        Ok((digest, false))
    }

    /// Read a blob back, or say it is not here.
    ///
    /// **Verified on the way out.** A store that handed back whatever was at
    /// the path would let a host with a damaged disk — or anything that could
    /// write into the store — substitute bytes under a digest a tenant is
    /// about to unpack into their own sandbox. The check costs one hash of
    /// something that is about to be unpacked anyway.
    pub fn get(&self, digest: &ScriptDigest) -> Result<Option<Vec<u8>>> {
        let path = self.path(digest);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(crate::error::Error::io(&path, e)),
        };
        let found = BlobStore::digest_of(&bytes);
        if found != *digest {
            return Err(crate::error::Error::primitive(
                "blob store",
                format!("{path:?} does not hash to its own name; remove it"),
                std::io::Error::other("digest mismatch"),
            ));
        }
        Ok(Some(bytes))
    }

    pub fn remove(&self, digest: &ScriptDigest) -> Result<bool> {
        let path = self.path(digest);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(crate::error::Error::io(&path, e)),
        }
    }

    pub fn size(&self, digest: &ScriptDigest) -> Option<u64> {
        std::fs::metadata(self.path(digest)).ok().map(|m| m.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let root = tempfile::tempdir().expect("a temp dir");
        let blobs = BlobStore::new(&Paths::rooted(root.path()));
        (root, blobs)
    }

    #[test]
    fn the_same_bytes_are_one_blob() {
        let (_root, blobs) = store();
        let (first, existed) = blobs.put(b"the same bytes").expect("put");
        assert!(!existed);
        let (again, existed) = blobs.put(b"the same bytes").expect("put again");
        assert_eq!(first, again);
        assert!(existed, "a second put wrote a second file");

        assert_eq!(
            blobs.get(&first).expect("get").as_deref(),
            Some(&b"the same bytes"[..])
        );
        assert_eq!(blobs.size(&first), Some(14));
    }

    #[test]
    fn a_digest_nobody_stored_is_absent_rather_than_an_error() {
        let (_root, blobs) = store();
        let digest = BlobStore::digest_of(b"never stored");
        assert!(!blobs.contains(&digest));
        assert!(blobs.get(&digest).expect("get").is_none());
        assert!(!blobs.remove(&digest).expect("remove"));
    }

    /// The reason `get` hashes rather than trusting the path.
    #[test]
    fn bytes_that_do_not_hash_to_their_name_are_refused() {
        let (_root, blobs) = store();
        let (digest, _) = blobs.put(b"honest").expect("put");
        std::fs::write(blobs.path(&digest), b"substituted").expect("tamper");

        let err = blobs.get(&digest).expect_err("a substituted blob");
        assert!(
            format!("{err}").contains("does not hash to its own name"),
            "{err}"
        );
    }

    #[test]
    fn a_blob_past_the_limit_is_refused_rather_than_stored() {
        let (_root, blobs) = store();
        let big = vec![0u8; MAX_BLOB_BYTES + 1];
        let err = blobs.put(&big).expect_err("over the limit");
        assert!(format!("{err}").contains("over the"), "{err}");
        assert!(
            !blobs.dir.exists() || std::fs::read_dir(&blobs.dir).expect("read").count() == 0,
            "a refused blob left a file"
        );
    }
}
