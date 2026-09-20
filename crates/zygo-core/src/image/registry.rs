//! OCI Distribution client.
//!
//! Pulls a reference into the local [`Store`]: resolve the manifest (following
//! a multi-platform index), fetch the config and the layers, verify every blob
//! against its digest, unpack each layer once.

use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::Mutex;

use super::auth::{Challenge, CredentialStore, TokenResponse};
use super::media::{self, Index, LayerCompression, Manifest, Platform};
use super::store::{ImageEntry, Store};
use super::{ImageConfig, ImageError, Reference};
use crate::error::Result;

/// Progress callback events, so a CLI can draw a bar and a library embedder can
/// ignore the whole thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullProgress {
    Resolving {
        reference: String,
    },
    /// A layer is about to be downloaded. `size` is 0 when unknown.
    LayerStart {
        digest: String,
        size: u64,
    },
    /// A layer was already present.
    LayerCached {
        digest: String,
    },
    LayerDone {
        digest: String,
    },
    Unpacking {
        digest: String,
    },
    Done {
        layers: usize,
        bytes: u64,
    },
}

/// A Distribution API client.
pub struct RegistryClient {
    http: reqwest::Client,
    store: Store,
    credentials: CredentialStore,
    platform: Platform,
    /// Bearer tokens keyed by `registry/repository`, reused across the many
    /// blob requests a single pull makes.
    tokens: Arc<Mutex<std::collections::HashMap<String, String>>>,
}

impl RegistryClient {
    pub fn new(store: Store) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(format!("zygo/{}", crate::VERSION))
            .build()
            .map_err(|e| ImageError::Registry(e.to_string()))?;

        Ok(Self {
            http,
            store,
            credentials: CredentialStore::load_default(),
            platform: Platform::host(),
            tokens: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Override the platform to pull for. Used by `--platform` and by tests.
    pub fn with_platform(mut self, platform: Platform) -> Self {
        self.platform = platform;
        self
    }

    pub fn with_credentials(mut self, credentials: CredentialStore) -> Self {
        self.credentials = credentials;
        self
    }

    fn base_url(&self, reference: &Reference) -> String {
        let scheme = if reference.is_insecure_local() {
            "http"
        } else {
            "https"
        };
        format!("{scheme}://{}/v2", reference.endpoint())
    }

    /// Pull an image into the store and record it in the index.
    ///
    /// Idempotent: blobs and layers already present are not re-fetched, which
    /// is what makes a warm `zygo run` cheap.
    pub async fn pull(
        &self,
        reference: &Reference,
        mut progress: impl FnMut(PullProgress),
    ) -> Result<ImageEntry> {
        progress(PullProgress::Resolving {
            reference: reference.to_string(),
        });

        let (manifest, manifest_digest, index_digest) = self.resolve_manifest(reference).await?;

        // Config first: it is small, and a failure here is a cheap failure.
        let config_bytes = self
            .fetch_blob_bytes(reference, &manifest.config.digest)
            .await?;
        self.store
            .write_blob(&manifest.config.digest, config_bytes.as_slice())?;

        let mut total = 0u64;
        let mut layer_digests = Vec::with_capacity(manifest.layers.len());

        for layer in &manifest.layers {
            let compression = LayerCompression::from_media_type(&layer.media_type)
                .ok_or_else(|| ImageError::UnsupportedLayer(layer.media_type.clone()))?;

            if self.store.has_layer(&layer.digest) {
                progress(PullProgress::LayerCached {
                    digest: layer.digest.clone(),
                });
            } else {
                if !self.store.has_blob(&layer.digest) {
                    progress(PullProgress::LayerStart {
                        digest: layer.digest.clone(),
                        size: layer.size,
                    });
                    self.download_blob(reference, &layer.digest).await?;
                    progress(PullProgress::LayerDone {
                        digest: layer.digest.clone(),
                    });
                }
                progress(PullProgress::Unpacking {
                    digest: layer.digest.clone(),
                });
                self.store.unpack_layer(&layer.digest, compression)?;
            }

            total += layer.size;
            layer_digests.push(layer.digest.clone());
        }

        let entry = ImageEntry {
            reference: reference.to_string(),
            manifest: manifest_digest,
            config: manifest.config.digest.clone(),
            layers: layer_digests,
            size: total,
            pulled_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            index: index_digest,
            platform: Some(format!(
                "{}/{}",
                self.platform.os, self.platform.architecture
            )),
        };
        self.store.put(entry.clone())?;

        progress(PullProgress::Done {
            layers: entry.layers.len(),
            bytes: total,
        });
        Ok(entry)
    }

    /// Fetch the manifest, resolving a multi-platform index to the entry for
    /// this platform.
    ///
    /// Returns the manifest, its digest, and the digest of the index it was
    /// selected from when there was one — the name that means the same image
    /// on every architecture, which is what a lock file has to record.
    async fn resolve_manifest(
        &self,
        reference: &Reference,
    ) -> Result<(Manifest, String, Option<String>)> {
        let url = format!(
            "{}/{}/manifests/{}",
            self.base_url(reference),
            reference.repository,
            reference.version()
        );

        let response = self
            .authorised_get(reference, &url, media::MANIFEST_ACCEPT)
            .await?;
        let digest = response
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let body = response
            .bytes()
            .await
            .map_err(|e| ImageError::Registry(e.to_string()))?;

        // The media type in the document is authoritative; the `Content-Type`
        // header is not always set correctly by proxying registries.
        let probe: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| ImageError::Registry(format!("malformed manifest: {e}")))?;
        let media_type = probe
            .get("mediaType")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let looks_like_index = probe.get("manifests").is_some()
            || media_type == media::MEDIA_OCI_INDEX
            || media_type == media::MEDIA_DOCKER_LIST;

        if looks_like_index {
            let index: Index = serde_json::from_slice(&body)
                .map_err(|e| ImageError::Registry(format!("malformed index: {e}")))?;
            let descriptor =
                index
                    .select(&self.platform)
                    .ok_or_else(|| ImageError::NoMatchingPlatform {
                        reference: reference.to_string(),
                        wanted: format!("{}/{}", self.platform.os, self.platform.architecture),
                        available: {
                            let a = index.available();
                            if a.is_empty() {
                                "nothing usable".into()
                            } else {
                                a.join(", ")
                            }
                        },
                    })?;

            let index_digest = digest.unwrap_or_else(|| {
                use sha2::{Digest, Sha256};
                format!("sha256:{}", hex::encode(Sha256::digest(&body)))
            });
            // Recurse once, pinned to the selected manifest digest.
            let pinned = Reference {
                digest: Some(descriptor.digest.clone()),
                ..reference.clone()
            };
            let (manifest, manifest_digest, _) = Box::pin(self.resolve_manifest(&pinned)).await?;
            return Ok((manifest, manifest_digest, Some(index_digest)));
        }

        let manifest: Manifest = serde_json::from_slice(&body)
            .map_err(|e| ImageError::Registry(format!("malformed manifest: {e}")))?;

        let digest = digest.unwrap_or_else(|| {
            use sha2::{Digest, Sha256};
            format!("sha256:{}", hex::encode(Sha256::digest(&body)))
        });

        self.store.write_blob(&digest, body.as_ref())?;
        Ok((manifest, digest, None))
    }

    /// Read the image config, for default argv and environment.
    pub async fn image_config(&self, entry: &ImageEntry) -> Result<ImageConfig> {
        let bytes = self.store.read_blob(&entry.config)?;
        serde_json::from_slice(&bytes)
            .map_err(|e| ImageError::Registry(format!("malformed image config: {e}")).into())
    }

    async fn fetch_blob_bytes(&self, reference: &Reference, digest: &str) -> Result<Vec<u8>> {
        if self.store.has_blob(digest) {
            return self.store.read_blob(digest);
        }
        let url = format!(
            "{}/{}/blobs/{digest}",
            self.base_url(reference),
            reference.repository
        );
        let response = self.authorised_get(reference, &url, "*/*").await?;
        Ok(response
            .bytes()
            .await
            .map_err(|e| ImageError::Registry(e.to_string()))?
            .to_vec())
    }

    /// Stream a blob straight into the store so a 900 MB layer never has to be
    /// held in memory.
    async fn download_blob(&self, reference: &Reference, digest: &str) -> Result<()> {
        let url = format!(
            "{}/{}/blobs/{digest}",
            self.base_url(reference),
            reference.repository
        );
        let response = self.authorised_get(reference, &url, "*/*").await?;

        let mut stream = response.bytes_stream();
        let (tx, rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(4);

        // The store's verifying writer is synchronous; run it on the blocking
        // pool and feed it from the async stream.
        let store = self.store.clone();
        let digest_owned = digest.to_string();
        let writer = tokio::task::spawn_blocking(move || {
            store.write_blob(
                &digest_owned,
                ChannelReader {
                    rx,
                    current: Vec::new(),
                    pos: 0,
                },
            )
        });

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| ImageError::Registry(e.to_string()))?;
            if tx.send(Ok(chunk.to_vec())).is_err() {
                break; // the writer stopped; its error is the real one
            }
        }
        drop(tx);

        writer
            .await
            .map_err(|e| ImageError::Registry(format!("blob writer panicked: {e}")))??;
        Ok(())
    }

    /// GET with Bearer/Basic auth, retrying once after a token challenge.
    async fn authorised_get(
        &self,
        reference: &Reference,
        url: &str,
        accept: &str,
    ) -> Result<reqwest::Response> {
        let key = format!("{}/{}", reference.endpoint(), reference.repository);

        let cached = self.tokens.lock().await.get(&key).cloned();
        let response = self.send(url, accept, cached.as_deref(), reference).await?;
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return check_status(response, reference).await;
        }

        // Challenge → token → retry. One retry only: a second 401 with a fresh
        // token means the credentials are wrong, not that the token is stale.
        let challenge = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .and_then(Challenge::parse);

        let Some(challenge) = challenge else {
            return Err(ImageError::Auth {
                registry: reference.registry.clone(),
                reason: "registry refused the request and offered no Bearer challenge".into(),
            }
            .into());
        };

        let token = self.fetch_token(&challenge, reference).await?;
        self.tokens.lock().await.insert(key, token.clone());

        let retried = self.send(url, accept, Some(&token), reference).await?;
        check_status(retried, reference).await
    }

    async fn send(
        &self,
        url: &str,
        accept: &str,
        token: Option<&str>,
        reference: &Reference,
    ) -> Result<reqwest::Response> {
        let mut req = self.http.get(url).header(reqwest::header::ACCEPT, accept);
        if let Some(t) = token {
            req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {t}"));
        } else if let Some(c) = self.credentials.get(&reference.registry) {
            req = req.header(reqwest::header::AUTHORIZATION, c.basic_header());
        }
        req.send()
            .await
            .map_err(|e| ImageError::Registry(format!("{url}: {e}")).into())
    }

    async fn fetch_token(&self, challenge: &Challenge, reference: &Reference) -> Result<String> {
        let url = challenge.token_url(&reference.repository);
        let mut req = self.http.get(&url);
        if let Some(c) = self.credentials.get(&reference.registry) {
            req = req.header(reqwest::header::AUTHORIZATION, c.basic_header());
        }

        let response = req
            .send()
            .await
            .map_err(|e| ImageError::Registry(format!("{url}: {e}")))?;

        if !response.status().is_success() {
            return Err(ImageError::Auth {
                registry: reference.registry.clone(),
                reason: format!("token endpoint returned {}", response.status()),
            }
            .into());
        }

        let token: TokenResponse = response
            .json()
            .await
            .map_err(|e| ImageError::Registry(format!("malformed token response: {e}")))?;
        Ok(token.bearer()?)
    }
}

/// Turn a non-success status into an error that says what to do about it.
async fn check_status(
    response: reqwest::Response,
    reference: &Reference,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    Err(match status {
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN => ImageError::Auth {
            registry: reference.registry.clone(),
            reason: format!("registry returned {status}"),
        },
        reqwest::StatusCode::NOT_FOUND => ImageError::Registry(format!(
            "`{reference}` not found on {}\n  → check the name and tag",
            reference.registry
        )),
        reqwest::StatusCode::TOO_MANY_REQUESTS => ImageError::Registry(format!(
            "{} is rate limiting this pull\n  → authenticate with `zygo login {}`",
            reference.registry, reference.registry
        )),
        other => ImageError::Registry(format!("{reference}: registry returned {other}")),
    }
    .into())
}

/// Adapts the async download into the synchronous `Read` the verifying store
/// writer expects.
struct ChannelReader {
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    current: Vec<u8>,
    pos: usize,
}

impl std::io::Read for ChannelReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.current.len() {
            match self.rx.recv() {
                Ok(Ok(chunk)) => {
                    self.current = chunk;
                    self.pos = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(0), // sender dropped: end of stream
            }
        }
        let n = (self.current.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.current[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use std::io::Read;

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(tmp.path());
        paths.ensure().unwrap();
        (tmp, Store::new(paths))
    }

    #[test]
    fn insecure_scheme_is_used_only_for_loopback_registries() {
        let (_t, s) = store();
        let c = RegistryClient::new(s).unwrap();
        assert!(
            c.base_url(&"localhost:5000/x".parse().unwrap())
                .starts_with("http://")
        );
        assert!(
            c.base_url(&"ghcr.io/a/b".parse().unwrap())
                .starts_with("https://")
        );
        assert!(
            c.base_url(&"alpine".parse().unwrap())
                .starts_with("https://")
        );
    }

    #[test]
    fn base_url_targets_hubs_real_endpoint() {
        let (_t, s) = store();
        let c = RegistryClient::new(s).unwrap();
        assert_eq!(
            c.base_url(&"python:3.12".parse().unwrap()),
            "https://registry-1.docker.io/v2"
        );
    }

    #[test]
    fn channel_reader_reassembles_chunks_across_read_boundaries() {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        tx.send(Ok(b"hello ".to_vec())).unwrap();
        tx.send(Ok(b"zygo".to_vec())).unwrap();
        drop(tx);

        let mut r = ChannelReader {
            rx,
            current: Vec::new(),
            pos: 0,
        };
        let mut out = String::new();
        r.read_to_string(&mut out).unwrap();
        assert_eq!(out, "hello zygo");
    }

    #[test]
    fn channel_reader_propagates_transport_errors() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(Err(std::io::Error::other("connection reset")))
            .unwrap();
        drop(tx);

        let mut r = ChannelReader {
            rx,
            current: Vec::new(),
            pos: 0,
        };
        let mut buf = [0u8; 8];
        assert!(r.read(&mut buf).is_err());
    }

    #[test]
    fn channel_reader_respects_a_small_destination_buffer() {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        tx.send(Ok(b"abcdef".to_vec())).unwrap();
        drop(tx);

        let mut r = ChannelReader {
            rx,
            current: Vec::new(),
            pos: 0,
        };
        let mut buf = [0u8; 2];
        assert_eq!(r.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"ab");
        assert_eq!(r.read(&mut buf).unwrap(), 2);
        assert_eq!(&buf, b"cd");
    }
}
