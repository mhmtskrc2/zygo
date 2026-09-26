// SPDX-License-Identifier: Apache-2.0
//! The content-addressed stores, over the control socket.
//!
//! Scripts and blobs: bytes a caller registers once and names by digest
//! afterwards. The supervisor owns both stores for the reason it owns the
//! sandboxes — it is the process that hands the bytes to a child, and a
//! second writer would be a second opinion about what a digest means.
//! Dependency sets, the third store, have a build to run and live in
//! [`deps`](super::deps).

use super::{ControlError, Response, Supervisor};

impl Supervisor {
    /// Register a script and answer with the name the store gave it.
    ///
    /// The supervisor owns the store for the same reason it owns the
    /// sandboxes: it is the process that will hand a script to a child, and a
    /// second writer would be a second opinion about what a digest means.
    /// Content-addressed, so this is idempotent — the same bytes from two
    /// tenants are one file, and `existed` is how the caller finds that out.
    ///
    /// `tenant` records *whose* script it is. The file is shared; the
    /// reference is not, and it is what lets deleting a tenant take its code
    /// and leave everybody else's.
    pub fn put_script(
        &self,
        source: &str,
        tenant: Option<&str>,
    ) -> std::result::Result<Response, Response> {
        let store = crate::scripts::ScriptStore::new(&self.paths);
        let digest = crate::scripts::ScriptDigest::of(source);
        let existed = store.contains(&digest);
        let digest = store
            .put(source)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        if let Some(tenant) = tenant {
            crate::tenants::Tenants::new(&self.paths)
                .add_script(tenant, digest.as_str())
                .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        }
        Ok(Response::Script {
            digest: digest.to_string(),
            size: source.len() as u64,
            existed,
        })
    }

    /// Whether this host holds a script, and how big it is.
    pub fn get_script(&self, digest: &str) -> std::result::Result<Response, Response> {
        let store = crate::scripts::ScriptStore::new(&self.paths);
        let digest = crate::scripts::ScriptDigest::parse(digest)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        let size = std::fs::metadata(store.path(&digest))
            .map(|m| m.len())
            .map_err(|_| Response::error(ControlError::NotFound, format!("no script {digest}")))?;
        Ok(Response::Script {
            digest: digest.to_string(),
            size,
            existed: true,
        })
    }

    /// Forget a script.
    ///
    /// Nothing checks whether anything still refers to it, because until
    /// tenants exist nothing *can* refer to it durably: a request
    /// already in flight has the bytes, and a caller that removes a script it
    /// is about to run has made that call fail on purpose.
    pub fn delete_script(&self, digest: &str) -> std::result::Result<Response, Response> {
        let store = crate::scripts::ScriptStore::new(&self.paths);
        let digest = crate::scripts::ScriptDigest::parse(digest)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        match store.remove(&digest) {
            Ok(true) => Ok(Response::Ok),
            Ok(false) => Err(Response::error(
                ControlError::NotFound,
                format!("no script {digest}"),
            )),
            Err(e) => Err(Response::error(ControlError::CallFailed, e)),
        }
    }

    /// Store a blob a caller will name by digest later.
    pub fn put_blob(&self, encoded: &str) -> std::result::Result<Response, Response> {
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, encoded)
            .map_err(|e| {
                Response::error(
                    ControlError::BadSpec,
                    format!("the body is not base64: {e}"),
                )
            })?;
        let size = bytes.len() as u64;
        let (digest, existed) = crate::blobs::BlobStore::new(&self.paths)
            .put(&bytes)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        Ok(Response::Script {
            digest: digest.to_string(),
            size,
            existed,
        })
    }

    /// Whether the store holds this blob, and how big it is.
    pub fn get_blob(&self, digest: &str) -> std::result::Result<Response, Response> {
        let parsed = crate::scripts::ScriptDigest::parse(digest)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        let store = crate::blobs::BlobStore::new(&self.paths);
        match store.size(&parsed) {
            Some(size) => Ok(Response::Script {
                digest: parsed.to_string(),
                size,
                existed: true,
            }),
            None => Err(Response::error(
                ControlError::NotFound,
                format!("no blob {parsed}"),
            )),
        }
    }

    pub fn delete_blob(&self, digest: &str) -> std::result::Result<Response, Response> {
        let parsed = crate::scripts::ScriptDigest::parse(digest)
            .map_err(|e| Response::error(ControlError::BadSpec, e))?;
        match crate::blobs::BlobStore::new(&self.paths).remove(&parsed) {
            Ok(true) => Ok(Response::Ok),
            Ok(false) => Err(Response::error(
                ControlError::NotFound,
                format!("no blob {parsed}"),
            )),
            Err(e) => Err(Response::error(ControlError::CallFailed, e)),
        }
    }
}
