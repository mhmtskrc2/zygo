// SPDX-License-Identifier: Apache-2.0
//! The content-addressed image store.
//!
//! Blobs land under `images/blobs/sha256/<digest>`, verified on the way in.
//! Layers are unpacked once into `images/layers/<digest>/` and shared by every
//! image that references them, which is what makes page cache sharing between
//! tenants fall out for free.
//!
//! Layer extraction is a security boundary: the tar comes from a registry and
//! may be hostile. [`safe_join`] and the symlink check in
//! [`Store::unpack_layer`] are the two places that matter.

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::media::LayerCompression;
use super::{ImageError, Reference};
use crate::error::{IoContext, Result};
use crate::paths::{Lock, Paths};
use crate::sandbox::RootfsView;
use crate::sandbox::mount::{MountPoint, MountPointKind};

/// OCI whiteout marker prefix: `.wh.<name>` deletes `<name>` from lower layers.
const WHITEOUT_PREFIX: &str = ".wh.";
/// `.wh..wh..opq` in a directory hides every lower-layer entry in it.
const OPAQUE_MARKER: &str = ".wh..wh..opq";
/// Sidecar recording the whiteouts found while unpacking a layer.
const WHITEOUT_FILE: &str = ".zygo-whiteouts.json";
/// Written once a layer directory is complete, so a crash mid-unpack is not
/// mistaken for a usable layer.
const LAYER_DONE: &str = ".zygo-complete";

/// Whiteouts recorded for a layer.
///
/// Rootless extraction cannot create the char device 0:0 that overlayfs uses
/// for whiteouts, so they are recorded here instead and applied by
/// [`Store::flatten`]. Until the overlay path handles them natively, an image
/// whose upper layer deletes a file is only fully correct in flattened mode —
/// [`Store::rootfs_view`] takes that into account.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Whiteouts {
    /// Paths deleted by this layer, relative to the rootfs.
    pub removed: Vec<PathBuf>,
    /// Directories whose lower-layer contents are hidden entirely.
    pub opaque: Vec<PathBuf>,
}

impl Whiteouts {
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.opaque.is_empty()
    }
}

/// An entry of the local image index.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageEntry {
    /// The reference as the user wrote it.
    pub reference: String,
    /// Digest of the platform-specific manifest.
    pub manifest: String,
    /// Digest of the image config blob.
    pub config: String,
    /// Layer digests, base first.
    pub layers: Vec<String>,
    /// Total size of the layer blobs.
    pub size: u64,
    /// Unix seconds of the last pull.
    pub pulled_at: u64,
    /// Digest of the multi-platform index the manifest was selected from,
    /// when the registry served one. This is what `zygo.lock` pins, because
    /// it names the same image on every architecture; `manifest` names this
    /// host's build of it. `None` for a single-platform image, for images
    /// pulled before this field existed, and for derived images.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// `os/arch` the manifest was selected for. `None` where `index` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// The on-disk image store.
#[derive(Debug, Clone)]
pub struct Store {
    paths: Paths,
}

impl Store {
    pub fn new(paths: Paths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Validate a `sha256:<64 hex>` digest and return its hex part.
    pub fn parse_digest(digest: &str) -> std::result::Result<&str, ImageError> {
        let hex = digest
            .strip_prefix("sha256:")
            .ok_or_else(|| ImageError::digest(digest, "only sha256 digests are supported"))?;
        if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(ImageError::digest(digest, "not a 64-character hex sha256"));
        }
        // Lower case only. `write_blob` compares against a freshly computed
        // digest, which `digest_of` renders in lower case, so an upper-case
        // reference named a blob that could be written and never verified.
        // Refusing it here says so once, where the
        // reference was written.
        if hex.chars().any(|c| c.is_ascii_uppercase()) {
            return Err(ImageError::digest(
                digest,
                "sha256 digests are written in lower-case hex",
            ));
        }
        Ok(hex)
    }

    pub fn blob_path(&self, digest: &str) -> std::result::Result<PathBuf, ImageError> {
        Ok(self.paths.blobs().join(Self::parse_digest(digest)?))
    }

    pub fn has_blob(&self, digest: &str) -> bool {
        self.blob_path(digest).is_ok_and(|p| p.is_file())
    }

    /// Stream a blob in, verifying its digest, and only then publish it.
    ///
    /// Written to a temporary file and renamed, so a concurrent reader never
    /// sees a partial blob and a failed download leaves nothing behind that
    /// looks complete.
    pub fn write_blob<R: Read>(&self, digest: &str, mut reader: R) -> Result<u64> {
        let hex = Self::parse_digest(digest)?;
        let final_path = self.paths.blobs().join(hex);
        if final_path.is_file() {
            return Ok(final_path.metadata().at(&final_path)?.len());
        }

        std::fs::create_dir_all(self.paths.blobs()).at(self.paths.blobs())?;
        std::fs::create_dir_all(self.paths.tmp()).at(self.paths.tmp())?;
        // Unique per *attempt*, not per process. The name used to be
        // `blob-{hex}-{pid}`, which two threads of one process pulling the
        // same blob share — so they wrote into the same file, and whichever
        // renamed second published a mixture of the two. A counter
        // makes every attempt its own file at no cost.
        static ATTEMPT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let attempt = ATTEMPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp_path = self
            .paths
            .tmp()
            .join(format!("blob-{hex}-{}-{attempt}", crate::process_token()));

        // And the file is removed on every way out but the successful one.
        // Before, only the digest-mismatch branch cleaned up, so a read error,
        // a full disk or a dropped connection each left a part-written blob in
        // `tmp/` for ever.
        let _cleanup = TempFile(&tmp_path);

        let mut hasher = Sha256::new();
        let mut written = 0u64;
        {
            let mut out = File::create(&tmp_path).at(&tmp_path)?;
            let mut buf = vec![0u8; 128 * 1024];
            loop {
                let n = reader.read(&mut buf).at(&tmp_path)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n]).at(&tmp_path)?;
                written += n as u64;
            }
            out.flush().at(&tmp_path)?;
        }

        let got = hex::encode(hasher.finalize());
        if got != hex {
            return Err(ImageError::DigestMismatch {
                expected: digest.to_string(),
                got: format!("sha256:{got}"),
            }
            .into());
        }

        std::fs::rename(&tmp_path, &final_path).at(&final_path)?;
        // Renamed, so there is nothing left to remove.
        _cleanup.keep();
        Ok(written)
    }

    pub fn read_blob(&self, digest: &str) -> Result<Vec<u8>> {
        let p = self.blob_path(digest)?;
        std::fs::read(&p).at(&p)
    }

    pub fn layer_dir(&self, digest: &str) -> std::result::Result<PathBuf, ImageError> {
        Ok(self.paths.layers().join(Self::parse_digest(digest)?))
    }

    /// Whether a layer is unpacked *and* was finished.
    pub fn has_layer(&self, digest: &str) -> bool {
        self.layer_dir(digest)
            .is_ok_and(|d| d.join(LAYER_DONE).is_file())
    }

    /// Unpack a layer blob into the layer store. Idempotent.
    pub fn unpack_layer(&self, digest: &str, compression: LayerCompression) -> Result<PathBuf> {
        let dir = self.layer_dir(digest)?;
        if self.has_layer(digest) {
            return Ok(dir);
        }

        let _lock = self.lock(&format!("layer-{}", Self::parse_digest(digest)?))?;
        // Another process may have finished while we waited for the lock.
        if self.has_layer(digest) {
            return Ok(dir);
        }
        // A previous attempt may have died partway through.
        if dir.exists() {
            std::fs::remove_dir_all(&dir).at(&dir)?;
        }
        std::fs::create_dir_all(&dir).at(&dir)?;

        let blob = self.blob_path(digest)?;
        let file = BufReader::new(File::open(&blob).at(&blob)?);
        let whiteouts = match compression {
            LayerCompression::None => extract_tar(file, &dir),
            LayerCompression::Gzip => extract_tar(flate2::read::GzDecoder::new(file), &dir),
            LayerCompression::Zstd => {
                let dec = ruzstd::decoding::StreamingDecoder::new(file)
                    .map_err(|e| ImageError::unpack_with("zstd", e))?;
                extract_tar(dec, &dir)
            }
        };

        let whiteouts = match whiteouts {
            Ok(w) => w,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };

        if !whiteouts.is_empty() {
            let p = dir.join(WHITEOUT_FILE);
            std::fs::write(
                &p,
                serde_json::to_vec_pretty(&whiteouts).unwrap_or_default(),
            )
            .at(&p)?;
        }
        let done = dir.join(LAYER_DONE);
        std::fs::write(&done, b"").at(&done)?;
        Ok(dir)
    }

    /// Whiteouts recorded for a layer, if any.
    pub fn whiteouts(&self, digest: &str) -> Whiteouts {
        let Ok(dir) = self.layer_dir(digest) else {
            return Whiteouts::default();
        };
        std::fs::read(dir.join(WHITEOUT_FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// How to present an image's layers as a root filesystem.
    ///
    /// Overlay is preferred, but only when no layer deletes anything: rootless
    /// extraction cannot record whiteouts in the form overlayfs reads, so an
    /// image that relies on them must be flattened to be correct. Silently
    /// showing a file the image deleted would be worse than being slower.
    ///
    /// `mount_points` are the directories the launcher needs to mount over.
    /// The sandbox root is read-only, so they cannot be created once it is
    /// assembled: for an overlay they come from an extra bottom layer, and for
    /// a flattened rootfs they are created in the cache directory, which the
    /// store owns.
    pub fn rootfs_view(
        &self,
        layers: &[String],
        overlay_supported: bool,
        mount_points: &[MountPoint],
    ) -> Result<RootfsView> {
        let needs_flatten = layers.iter().any(|d| !self.whiteouts(d).is_empty());

        if overlay_supported && !needs_flatten {
            // Overlayfs wants the top layer first; manifests list base first.
            let mut lower = layers
                .iter()
                .rev()
                .map(|d| self.layer_dir(d))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            // Last, so the image's own version of any of these directories
            // wins: the skeleton only fills in what the image lacks.
            lower.push(self.skeleton(mount_points)?);
            return Ok(RootfsView::Overlay { lower });
        }

        // The mount points are part of the key, not something written into a
        // shared directory afterwards. Without that, two functions on the same
        // image share one flattened rootfs and overwrite each other's mount
        // points: measured with `zygo up` on a three-function spec, where a
        // handler that did not exist created `/zygo/handler.py` as a directory
        // and the next two functions failed with ENOTDIR trying to bind a file
        // onto it. Identical shapes still share, which is the common case.
        let dir = self.flatten_with_mount_points(layers, mount_points)?;
        Ok(RootfsView::Flat { dir })
    }

    /// A directory holding nothing but the mount points, used as the lowest
    /// overlay layer. Shared by every sandbox that needs the same set.
    pub fn skeleton(&self, mount_points: &[MountPoint]) -> Result<PathBuf> {
        let mut key = Sha256::new();
        let mut sorted: Vec<MountPoint> = mount_points.to_vec();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        sorted.dedup_by(|a, b| a.path == b.path);
        for p in &sorted {
            key.update(p.path.as_os_str().as_encoded_bytes());
            // The kind is part of the key: the same path as a file and as a
            // directory are different skeletons.
            key.update(match p.kind {
                MountPointKind::Directory => b"/d\n".as_slice(),
                MountPointKind::File => b"/f\n".as_slice(),
            });
        }
        let dir = self
            .paths
            .data()
            .join("cache/skeleton")
            .join(hex::encode(key.finalize()));

        if dir.join(LAYER_DONE).is_file() {
            return Ok(dir);
        }
        let _lock = self.lock("skeleton")?;
        create_mount_points(&dir, &sorted)?;
        let done = dir.join(LAYER_DONE);
        std::fs::write(&done, b"").at(&done)?;
        Ok(dir)
    }

    /// Materialise layers into a single directory, applying whiteouts. Cached
    /// under `cache/flat/<key>` and shared by every image with the same stack.
    pub fn flatten(&self, layers: &[String]) -> Result<PathBuf> {
        self.flatten_with_mount_points(layers, &[])
    }

    /// Flatten, with the mount points a sandbox needs baked in.
    ///
    /// Keyed on both, because the mount points are created *inside* the result:
    /// a directory keyed on the layers alone would be shared by every function
    /// using that image, and the first one to run would decide whether
    /// `/zygo/handler.py` is a file or a directory for all of them.
    pub fn flatten_with_mount_points(
        &self,
        layers: &[String],
        mount_points: &[MountPoint],
    ) -> Result<PathBuf> {
        let mut key = Sha256::new();
        for d in layers {
            key.update(d.as_bytes());
            key.update(b"\n");
        }
        let mut sorted: Vec<MountPoint> = mount_points.to_vec();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        sorted.dedup_by(|a, b| a.path == b.path);
        for p in &sorted {
            key.update(p.path.as_os_str().as_encoded_bytes());
            key.update(match p.kind {
                MountPointKind::Directory => b"/d\n".as_slice(),
                MountPointKind::File => b"/f\n".as_slice(),
            });
        }
        let key = hex::encode(key.finalize());
        let dir = self.paths.flat_cache().join(&key);
        if dir.join(LAYER_DONE).is_file() {
            touch(&dir.join(LAYER_DONE));
            return Ok(dir);
        }

        let _lock = self.lock(&format!("flat-{key}"))?;
        if dir.join(LAYER_DONE).is_file() {
            return Ok(dir);
        }
        let mount_points = sorted;
        if dir.exists() {
            std::fs::remove_dir_all(&dir).at(&dir)?;
        }
        std::fs::create_dir_all(&dir).at(&dir)?;

        for digest in layers {
            let src = self.layer_dir(digest)?;
            let w = self.whiteouts(digest);
            for opaque in &w.opaque {
                let target = safe_join(&dir, opaque)?;
                if target.is_dir() {
                    std::fs::remove_dir_all(&target).at(&target)?;
                    std::fs::create_dir_all(&target).at(&target)?;
                }
            }
            for removed in &w.removed {
                let target = safe_join(&dir, removed)?;
                if target.is_dir() {
                    let _ = std::fs::remove_dir_all(&target);
                } else {
                    let _ = std::fs::remove_file(&target);
                }
            }
            copy_tree(&src, &dir)?;
        }

        // After the layers, so the image's own version of a path wins, and
        // before the done marker, so nobody sees a rootfs whose mount points
        // are only half there.
        create_mount_points(&dir, &mount_points)?;

        // The marker carries the layer digests rather than being empty. This
        // directory is named after a hash of those digests *and* the mount
        // points, so nothing can invert it, and `zygo image prune` has to be
        // able to ask whether an image that still exists could have produced
        // this rootfs. Written last, so a half-built directory has no marker
        // and makes no claim to be live.
        let done = dir.join(LAYER_DONE);
        let mut record = String::new();
        for digest in layers {
            record.push_str(digest);
            record.push('\n');
        }
        std::fs::write(&done, record.as_bytes()).at(&done)?;
        Ok(dir)
    }

    /// Flattened rootfs directories that no image in the index could produce.
    ///
    /// A directory is kept when every layer it records is still referenced.
    /// One that records nothing — built by a Zygo from before the record, or
    /// interrupted — cannot be shown to be live, and a cache that cannot be
    /// shown to be live is what prune is for: dropping it costs one
    /// re-flatten on the next run and nothing else.
    pub fn unreferenced_flat(&self) -> Result<Vec<PathBuf>> {
        let referenced: std::collections::BTreeSet<String> = self
            .read_index()
            .into_iter()
            .flat_map(|e| e.layers)
            .collect();

        let dir = self.paths.flat_cache();
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).at(&dir)? {
            let path = entry.at(&dir)?.path();
            if !path.is_dir() {
                continue;
            }
            let record = std::fs::read_to_string(path.join(LAYER_DONE)).unwrap_or_default();
            let mut built_from = record.lines().filter(|l| !l.trim().is_empty()).peekable();
            let live = built_from.peek().is_some() && built_from.all(|d| referenced.contains(d));
            if !live {
                out.push(path);
            }
        }
        out.sort();
        Ok(out)
    }

    // --- index -------------------------------------------------------------

    /// The index, or an empty one — but never an empty one because the file
    /// was *unreadable*.
    ///
    /// "No file yet" and "the file is corrupt" both used to answer with an
    /// empty list, and the next `put` then wrote that empty list back: one bad
    /// parse and the store forgot every image it had, silently. They
    /// are different situations and only the first is ordinary.
    ///
    /// A corrupt index is moved aside rather than deleted, so it can be looked
    /// at, and the loss is announced. The images themselves are still on disk;
    /// what is lost is the record of their references, which a `pull` rebuilds.
    fn read_index(&self) -> Vec<ImageEntry> {
        let path = self.paths.image_index();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            // Nothing written yet: the ordinary first-run case.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "the image index could not be read; continuing with an empty one, \
                     and it will be overwritten by the next change"
                );
                return Vec::new();
            }
        };
        match serde_json::from_slice(&bytes) {
            Ok(entries) => entries,
            Err(e) => {
                let aside = path.with_extension("json.corrupt");
                let moved = std::fs::rename(&path, &aside).is_ok();
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    kept = moved,
                    "the image index is not valid JSON; it has been set aside and the \
                     store will rebuild it. Pulled images are still on disk; their \
                     references are not, so `zygo images` will look empty until they \
                     are pulled again"
                );
                Vec::new()
            }
        }
    }

    fn write_index(&self, entries: &[ImageEntry]) -> Result<()> {
        let path = self.paths.image_index();
        std::fs::create_dir_all(self.paths.images()).at(self.paths.images())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(entries).unwrap_or_default()).at(&tmp)?;
        std::fs::rename(&tmp, &path).at(&path)
    }

    /// Record a pulled image, replacing any previous entry for the reference.
    pub fn put(&self, entry: ImageEntry) -> Result<()> {
        let _lock = self.lock("index")?;
        let mut entries = self.read_index();
        entries.retain(|e| e.reference != entry.reference);
        entries.push(entry);
        entries.sort_by(|a, b| a.reference.cmp(&b.reference));
        self.write_index(&entries)
    }

    /// Forget an image, and every derived image built on it.
    ///
    /// Only the index changes here. The layers, and the caches keyed on the
    /// image's liveness, become collectable and are collected by the same
    /// pass `zygo image prune` runs — which `zygo image rm` runs immediately,
    /// because `rmi` frees disk and a removal that leaves the bytes behind
    /// until a second command is a surprise.
    ///
    /// Derived images (`<reference>+system.<key>`, `<reference>+bytecode.<key>`)
    /// go with their base: they are that base plus one layer, named after it,
    /// and nothing asks for one by its own name. Matched on `<reference>+`,
    /// which only a derived name can start with: an OCI tag has no `+`.
    /// Returns what was removed, base first, and nothing when the reference
    /// was not in the index.
    pub fn remove(&self, reference: &Reference) -> Result<Vec<ImageEntry>> {
        let _lock = self.lock("index")?;
        let key = reference.to_string();
        let derived_prefix = format!("{key}+");
        let (removed, kept): (Vec<ImageEntry>, Vec<ImageEntry>) = self
            .read_index()
            .into_iter()
            .partition(|e| e.reference == key || e.reference.starts_with(&derived_prefix));
        if removed.is_empty() {
            return Ok(removed);
        }
        self.write_index(&kept)?;
        let mut removed = removed;
        removed.sort_by_key(|e| e.reference != key);
        Ok(removed)
    }

    /// Compressed layer blobs that nothing needs until a layer is lost.
    ///
    /// A blob is read once, to unpack its layer; after that every path —
    /// overlay, flatten, derive, `pull` itself — works from the unpacked
    /// directory, and `pull` treats a present layer as cached without looking
    /// for the blob. Keeping both is what containerd does and roughly doubles
    /// the store; dropping them is `prune --blobs`, and costs one download if
    /// a layer directory is ever removed by hand. Manifest and config blobs
    /// are not layers and are never offered here.
    pub fn droppable_blobs(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut out = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for entry in self.read_index() {
            for digest in entry.layers {
                if !seen.insert(digest.clone()) || !self.has_layer(&digest) {
                    continue;
                }
                if let Ok(blob) = self.blob_path(&digest)
                    && blob.is_file()
                {
                    out.push((digest, blob));
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// Flattened rootfs directories whose image is still here but that have
    /// not been used since `cutoff`. Use is recorded on the completion
    /// marker's modification time, refreshed on every hit.
    pub fn flat_unused_since(&self, cutoff: std::time::SystemTime) -> Result<Vec<PathBuf>> {
        let stale: std::collections::BTreeSet<PathBuf> =
            self.unreferenced_flat()?.into_iter().collect();
        let dir = self.paths.flat_cache();
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).at(&dir)? {
            let path = entry.at(&dir)?.path();
            if !path.is_dir() || stale.contains(&path) {
                continue;
            }
            if last_used(&path.join(LAYER_DONE)).is_some_and(|t| t < cutoff) {
                out.push(path);
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn get(&self, reference: &Reference) -> Option<ImageEntry> {
        let key = reference.to_string();
        self.read_index().into_iter().find(|e| e.reference == key)
    }

    pub fn list(&self) -> Vec<ImageEntry> {
        self.read_index()
    }

    /// Layer digests referenced by no image in the index — the GC candidates.
    pub fn unreferenced_layers(&self) -> Result<Vec<String>> {
        let referenced: std::collections::BTreeSet<String> = self
            .read_index()
            .into_iter()
            .flat_map(|e| e.layers)
            .collect();
        let dir = self.paths.layers();
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).at(&dir)? {
            let entry = entry.at(&dir)?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let digest = format!("sha256:{name}");
            if !referenced.contains(&digest) {
                out.push(digest);
            }
        }
        out.sort();
        Ok(out)
    }

    // --- locking -----------------------------------------------------------

    /// Take an exclusive lock, so two `zygo pull`s of the same image do not
    /// unpack into the same directory at once.
    ///
    /// The same mechanism the rest of Zygo uses to serialise slow work; see
    /// [`Paths::lock`].
    pub fn lock(&self, key: &str) -> Result<Lock> {
        self.paths.lock(key)
    }
}

/// Record that a cache entry was used now.
///
/// The marker's modification time is the only record of use a cache entry
/// has, and it is what `prune --unused-for` reads. Best effort: a cache that
/// cannot be stamped is still a cache, and the next hit tries again.
pub(crate) fn touch(marker: &Path) {
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(marker) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

/// When a cache entry was last used, from its marker.
pub(crate) fn last_used(marker: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(marker).ok()?.modified().ok()
}

/// Join `rel` under `root`, refusing anything that could escape.
///
/// Absolute paths, `..` and prefixes are rejected rather than normalised: a
/// layer entry that needs them is malformed or hostile, and quietly rewriting
/// it would hide the fact.
pub fn safe_join(root: &Path, rel: &Path) -> std::result::Result<PathBuf, ImageError> {
    let mut out = root.to_path_buf();
    for component in rel.components() {
        match component {
            Component::Normal(c) => {
                let s = c.to_string_lossy();
                if s.contains('\0') {
                    return Err(ImageError::UnsafePath(rel.display().to_string()));
                }
                out.push(c);
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(ImageError::UnsafePath(rel.display().to_string()));
            }
        }
    }
    Ok(out)
}

/// Extract a tar stream into `dir`, recording whiteouts instead of writing them.
fn extract_tar<R: Read>(reader: R, dir: &Path) -> Result<Whiteouts> {
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(false);
    archive.set_unpack_xattrs(false);
    archive.set_overwrite(true);

    let mut whiteouts = Whiteouts::default();

    for entry in archive.entries().map_err(unpack_err)? {
        let mut entry = entry.map_err(unpack_err)?;
        let path = entry.path().map_err(unpack_err)?.into_owned();

        // Whiteouts are metadata, not content.
        if let Some(file_name) = path.file_name().and_then(|n| n.to_str()) {
            if file_name == OPAQUE_MARKER {
                if let Some(parent) = path.parent() {
                    whiteouts.opaque.push(parent.to_path_buf());
                }
                continue;
            }
            if let Some(stripped) = file_name.strip_prefix(WHITEOUT_PREFIX) {
                let target = path.with_file_name(stripped);
                whiteouts.removed.push(target);
                continue;
            }
        }

        let dest = safe_join(dir, &path)?;

        // Refuse to write through a symlink planted by an earlier entry: that
        // is the classic tar escape, and `create_dir_all` would follow it.
        if let Some(parent) = dest.parent() {
            ensure_real_directory(dir, parent)?;
        }

        match entry.header().entry_type() {
            // Hard links must be resolved against the layer root. `unpack`
            // resolves them against the process working directory, which is
            // wrong and fails on any image where one binary hard-links to
            // another (`usr/bin/perl` → `usr/bin/perl5.40.1` in python:slim).
            tar::EntryType::Link => {
                let target = entry
                    .link_name()
                    .map_err(unpack_err)?
                    .ok_or_else(|| {
                        ImageError::unpack(format!("hard link {} has no target", path.display()))
                    })?
                    .into_owned();
                link_or_copy(&safe_join(dir, &target)?, &dest)?;
            }

            // Device nodes and FIFOs need privileges Zygo does not have when
            // rootless — and would be ignored anyway: `/dev` inside a sandbox
            // is synthesised by the mount plan, never taken from the image.
            tar::EntryType::Char | tar::EntryType::Block | tar::EntryType::Fifo => {
                tracing::debug!(path = %path.display(), "skipping device node in layer");
            }

            _ => {
                entry.unpack(&dest).map_err(unpack_err)?;
            }
        }
    }

    Ok(whiteouts)
}

/// Create what a sandbox mounts over. Idempotent.
///
/// A bind mount needs its target to be the same kind of object as its source,
/// so a file mount point is an empty file rather than a directory — mounting a
/// file onto a directory fails with `ENOTDIR`.
fn create_mount_points(dir: &Path, mount_points: &[MountPoint]) -> Result<()> {
    for point in mount_points {
        // Targets are absolute paths inside the sandbox.
        let relative = point.path.strip_prefix("/").unwrap_or(&point.path);
        let path = safe_join(dir, relative)?;

        match point.kind {
            MountPointKind::Directory => std::fs::create_dir_all(&path).at(&path)?,
            MountPointKind::File => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).at(parent)?;
                }
                // An existing directory here would come from the image and is
                // not ours to replace; the mount will fail with a clear error.
                if !path.exists() {
                    std::fs::write(&path, b"").at(&path)?;
                }
            }
        }
    }
    Ok(())
}

/// Reproduce a hard link, falling back to a copy.
///
/// The copy fallback matters for the flattened rootfs, where layers are copied
/// across what may be different filesystems, and for stores on filesystems that
/// do not support hard links at all. It costs disk, not correctness.
fn link_or_copy(source: &Path, dest: &Path) -> Result<()> {
    if std::fs::symlink_metadata(source).is_err() {
        return Err(ImageError::unpack(format!(
            "hard link target {} is missing from the layer",
            source.display()
        ))
        .into());
    }
    // `symlink_metadata`, and a removal that does not follow a link: a
    // dangling symlink at `dest` does not "exist", so `copy` created the
    // link's *target* — on the host, if the link pointed outside the layer.
    remove_whatever_is_there(dest)?;
    match std::fs::hard_link(source, dest) {
        Ok(()) => Ok(()),
        // Narrow on purpose. The fallback exists for filesystems that have no
        // hard links and for a link across a mount point; catching every error
        // turned a full disk or a permission problem into a silent copy, and
        // then into a failure somewhere else.
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::CrossesDevices
                    | std::io::ErrorKind::Unsupported
                    | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            std::fs::copy(source, dest).at(dest).map(|_| ())
        }
        Err(e) => Err(crate::Error::io(dest, e)),
    }
}

/// Create every directory between `root` and `path`, failing if any existing
/// component is a symlink rather than a real directory.
fn ensure_real_directory(root: &Path, path: &Path) -> Result<()> {
    let rel = path.strip_prefix(root).unwrap_or(Path::new(""));
    let mut current = root.to_path_buf();
    for component in rel.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(md) if md.is_dir() => {}
            Ok(md) if md.file_type().is_symlink() => {
                return Err(ImageError::UnsafePath(format!(
                    "{} traverses a symlink",
                    current.display()
                ))
                .into());
            }
            Ok(_) => {
                // A regular file where a directory must be: the later layer
                // replaces it, which is legitimate.
                std::fs::remove_file(&current).at(&current)?;
                std::fs::create_dir(&current).at(&current)?;
            }
            Err(_) => std::fs::create_dir_all(&current).at(&current)?,
        }
    }
    Ok(())
}

fn unpack_err(e: std::io::Error) -> crate::Error {
    ImageError::unpack_source(e).into()
}

/// Recursive copy used by [`Store::flatten`]. Later layers overwrite earlier
/// ones, which is exactly overlay semantics.
/// Whether a directory entry is the store's own bookkeeping rather than part
/// of an image: the done marker and the whiteout sidecar.
pub(crate) fn is_internal_entry(name: &std::ffi::OsStr) -> bool {
    name == LAYER_DONE || name == WHITEOUT_FILE
}

/// Copy one layer's tree over what earlier layers left.
///
/// Every decision about the destination is taken with `symlink_metadata`, and
/// anything of the wrong kind is removed before it is written through. That is
/// not a refinement: the unpack path was hardened against hostile tars
/// (`safe_join`, `ensure_real_directory`) and this one was not, so a lower
/// layer could plant `etc` as a symlink to `/etc` and an upper layer's
/// `etc/passwd` would be created **on the host**.
/// `create_dir_all` follows a symlink, and `Path::exists` is false for a
/// dangling one — so both branches wrote through it.
/// Give `path` the access and modification times in `md`, without following
/// a symlink.
///
/// A flattened rootfs used to have every file stamped with the moment it was
/// flattened, and the image's own timestamps matter to what runs in it. apt
/// asks the mirror "changed since <mtime of my cached InRelease>?": with the
/// flatten's "now" the answer was always "no", apt kept the index the image
/// shipped with, and once that passed its Valid-Until every system-layer
/// build failed with "Release file … is expired". Python's bytecode checks
/// the same way.
///
/// Best effort: a timestamp that cannot be set leaves a working rootfs, which
/// a failed flatten would not.
fn keep_times(path: &Path, md: &std::fs::Metadata) {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return;
    };
    let times = [
        libc::timespec {
            tv_sec: md.atime() as libc::time_t,
            tv_nsec: md.atime_nsec() as _,
        },
        libc::timespec {
            tv_sec: md.mtime() as libc::time_t,
            tv_nsec: md.mtime_nsec() as _,
        },
    ];
    // SAFETY: a valid C string and a two-element array, as utimensat takes.
    unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            c.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src).at(src)? {
        let entry = entry.at(src)?;
        let name = entry.file_name();
        // Internal bookkeeping never reaches the rootfs.
        if is_internal_entry(&name) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let md = std::fs::symlink_metadata(&from).at(&from)?;
        // `symlink_metadata`, never `exists`: a dangling symlink does not
        // exist and is exactly the thing that has to be removed.
        let existing = std::fs::symlink_metadata(&to);

        if md.is_dir() {
            // A directory over anything that is not a real directory: the
            // symlink or file goes first, or `create_dir_all` resolves it.
            match &existing {
                Ok(there) if there.is_dir() => {}
                Ok(_) => remove_whatever_is_there(&to)?,
                Err(_) => {}
            }
            std::fs::create_dir_all(&to).at(&to)?;
            copy_tree(&from, &to)?;
            // After its contents, whose arrival changed it.
            keep_times(&to, &md);
        } else {
            if existing.is_ok() {
                remove_whatever_is_there(&to)?;
            }
            if md.file_type().is_symlink() {
                #[cfg(unix)]
                {
                    let target = std::fs::read_link(&from).at(&from)?;
                    std::os::unix::fs::symlink(&target, &to).at(&to)?;
                }
            } else {
                std::fs::copy(&from, &to).at(&to)?;
            }
            keep_times(&to, &md);
        }
    }
    Ok(())
}

/// Remove whatever is at `path`, whether it is a file, a symlink or a
/// directory, without following a link.
fn remove_whatever_is_there(path: &Path) -> Result<()> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(_) => return Ok(()),
    };
    // A real directory is removed recursively; a *symlink* to one is removed
    // as a link, which is why the kind is read without following it.
    if md.is_dir() {
        std::fs::remove_dir_all(path).at(path)
    } else {
        std::fs::remove_file(path).at(path)
    }
}

/// Removes a temporary file unless it was published.
///
/// A `?` is an early return, and an early return between "create the temp
/// file" and "rename it" used to leave that file behind. There were four such
/// returns and one of them cleaned up. A guard cannot miss a path the
/// way a list of cleanup calls can.
struct TempFile<'a>(&'a std::path::Path);

impl TempFile<'_> {
    /// The file reached its final name; there is nothing to remove.
    fn keep(self) {
        std::mem::forget(self);
    }
}

impl Drop for TempFile<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The directories the launcher needs to mount over.
    fn mount_points() -> Vec<MountPoint> {
        ["/proc", "/sys", "/dev", "/tmp"]
            .iter()
            .map(|p| MountPoint {
                path: PathBuf::from(p),
                kind: MountPointKind::Directory,
            })
            .collect()
    }

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(tmp.path());
        paths.ensure().unwrap();
        (tmp, Store::new(paths))
    }

    fn sha256_of(bytes: &[u8]) -> String {
        format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
    }

    /// Build a tar in memory from `(path, contents)` pairs.
    fn tar_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, contents) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, path, *contents).unwrap();
        }
        builder.into_inner().unwrap()
    }

    /// Build a tar with an arbitrary entry name, bypassing the `tar` crate's
    /// own validation.
    ///
    /// A hostile registry is under no obligation to use a well-behaved writer,
    /// so the traversal defence has to be tested against a tar that a polite
    /// builder would refuse to produce.
    fn hostile_tar(name: &str, contents: &[u8]) -> Vec<u8> {
        let mut header = [0u8; 512];
        let write = |h: &mut [u8; 512], at: usize, bytes: &[u8]| {
            h[at..at + bytes.len()].copy_from_slice(bytes);
        };

        write(&mut header, 0, name.as_bytes()); // name
        write(&mut header, 100, b"0000644\0"); // mode
        write(&mut header, 108, b"0000000\0"); // uid
        write(&mut header, 116, b"0000000\0"); // gid
        write(
            &mut header,
            124,
            format!("{:011o}\0", contents.len()).as_bytes(),
        ); // size
        write(&mut header, 136, b"00000000000\0"); // mtime
        header[156] = b'0'; // typeflag: regular file
        write(&mut header, 257, b"ustar\0"); // magic
        write(&mut header, 263, b"00"); // version

        // Checksum is computed with the checksum field read as spaces.
        header[148..156].fill(b' ');
        let sum: u32 = header.iter().map(|b| *b as u32).sum();
        write(&mut header, 148, format!("{sum:06o}\0 ").as_bytes());

        let mut out = header.to_vec();
        out.extend_from_slice(contents);
        out.resize(out.len().div_ceil(512) * 512, 0); // pad to a block
        out.extend_from_slice(&[0u8; 1024]); // two empty blocks end the archive
        out
    }

    #[test]
    fn digest_validation_rejects_the_obvious_attacks() {
        assert!(Store::parse_digest("sha256:../../etc/passwd").is_err());
        assert!(Store::parse_digest("md5:abc").is_err());
        assert!(Store::parse_digest(&format!("sha256:{}", "a".repeat(63))).is_err());
        assert!(Store::parse_digest(&format!("sha256:{}", "a".repeat(64))).is_ok());
    }

    #[test]
    fn blobs_are_verified_on_the_way_in() {
        let (_t, s) = store();
        let data = b"hello zygo";
        let digest = sha256_of(data);

        let n = s.write_blob(&digest, &data[..]).unwrap();
        assert_eq!(n, data.len() as u64);
        assert!(s.has_blob(&digest));
        assert_eq!(s.read_blob(&digest).unwrap(), data);
    }

    #[test]
    fn a_blob_whose_content_does_not_match_is_rejected_and_not_stored() {
        let (_t, s) = store();
        let claimed = sha256_of(b"expected");
        let err = s
            .write_blob(&claimed, &b"actually something else"[..])
            .unwrap_err();
        assert!(err.to_string().contains("digest mismatch"), "{err}");
        assert!(
            !s.has_blob(&claimed),
            "a corrupt blob must not be published"
        );
        // And nothing is left behind in tmp.
        let leftovers: Vec<_> = std::fs::read_dir(s.paths().tmp())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("blob-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn writing_an_existing_blob_is_a_no_op() {
        let (_t, s) = store();
        let data = b"x";
        let digest = sha256_of(data);
        s.write_blob(&digest, &data[..]).unwrap();
        // A second write with a *wrong* reader still succeeds, because the
        // content is already verified and present.
        s.write_blob(&digest, &b"junk"[..]).unwrap();
        assert_eq!(s.read_blob(&digest).unwrap(), data);
    }

    #[test]
    fn layers_unpack_and_are_marked_complete() {
        let (_t, s) = store();
        let tar = tar_of(&[("etc/hosts", b"127.0.0.1"), ("bin/sh", b"#!")]);
        let digest = sha256_of(&tar);
        s.write_blob(&digest, &tar[..]).unwrap();

        let dir = s.unpack_layer(&digest, LayerCompression::None).unwrap();
        assert!(s.has_layer(&digest));
        assert_eq!(std::fs::read(dir.join("etc/hosts")).unwrap(), b"127.0.0.1");
        // Unpacking again is a no-op, not an error.
        s.unpack_layer(&digest, LayerCompression::None).unwrap();
    }

    #[test]
    fn gzip_layers_unpack() {
        use flate2::{Compression, write::GzEncoder};
        let (_t, s) = store();
        let tar = tar_of(&[("a.txt", b"hello")]);
        let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(&tar).unwrap();
        let gz = enc.finish().unwrap();

        let digest = sha256_of(&gz);
        s.write_blob(&digest, &gz[..]).unwrap();
        let dir = s.unpack_layer(&digest, LayerCompression::Gzip).unwrap();
        assert_eq!(std::fs::read(dir.join("a.txt")).unwrap(), b"hello");
    }

    /// Real base images hard-link binaries to each other (`python:3.12-slim`
    /// links `usr/bin/perl` to `usr/bin/perl5.40.1`). The link target is
    /// relative to the layer root, not to the process working directory.
    #[test]
    fn hard_links_resolve_against_the_layer_root() {
        let (_t, s) = store();
        let mut builder = tar::Builder::new(Vec::new());

        let contents = b"#!/usr/bin/perl";
        let mut file = tar::Header::new_gnu();
        file.set_size(contents.len() as u64);
        file.set_mode(0o755);
        file.set_cksum();
        builder
            .append_data(&mut file, "usr/bin/perl5.40.1", &contents[..])
            .unwrap();

        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Link);
        link.set_size(0);
        link.set_mode(0o755);
        builder
            .append_link(&mut link, "usr/bin/perl", "usr/bin/perl5.40.1")
            .unwrap();
        let tar = builder.into_inner().unwrap();

        let digest = sha256_of(&tar);
        s.write_blob(&digest, &tar[..]).unwrap();
        let dir = s.unpack_layer(&digest, LayerCompression::None).unwrap();

        assert_eq!(std::fs::read(dir.join("usr/bin/perl")).unwrap(), contents);
        assert_eq!(
            std::fs::read(dir.join("usr/bin/perl5.40.1")).unwrap(),
            contents
        );
    }

    #[test]
    fn a_hard_link_cannot_point_outside_the_layer() {
        let (_t, s) = store();
        let mut builder = tar::Builder::new(Vec::new());
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Link);
        link.set_size(0);
        // `append_link` validates the *entry* name but not the target.
        builder
            .append_link(&mut link, "grab", "/etc/shadow")
            .unwrap();
        let tar = builder.into_inner().unwrap();

        let digest = sha256_of(&tar);
        s.write_blob(&digest, &tar[..]).unwrap();
        let err = s.unpack_layer(&digest, LayerCompression::None).unwrap_err();
        assert!(err.to_string().contains("unsafe path"), "{err}");
    }

    #[test]
    fn device_nodes_are_skipped_rather_than_failing_the_layer() {
        let (_t, s) = store();
        let mut builder = tar::Builder::new(Vec::new());

        let mut dev = tar::Header::new_gnu();
        dev.set_entry_type(tar::EntryType::Char);
        dev.set_size(0);
        dev.set_mode(0o666);
        dev.set_device_major(1).unwrap();
        dev.set_device_minor(3).unwrap();
        dev.set_cksum();
        builder.append_data(&mut dev, "dev/null", &[][..]).unwrap();

        let contents = b"ok";
        let mut file = tar::Header::new_gnu();
        file.set_size(contents.len() as u64);
        file.set_mode(0o644);
        file.set_cksum();
        builder
            .append_data(&mut file, "etc/motd", &contents[..])
            .unwrap();
        let tar = builder.into_inner().unwrap();

        let digest = sha256_of(&tar);
        s.write_blob(&digest, &tar[..]).unwrap();
        let dir = s.unpack_layer(&digest, LayerCompression::None).unwrap();

        assert!(
            !dir.join("dev/null").exists(),
            "device nodes are not extracted"
        );
        assert_eq!(std::fs::read(dir.join("etc/motd")).unwrap(), contents);
    }

    #[test]
    fn whiteouts_are_recorded_rather_than_written() {
        let (_t, s) = store();
        let tar = tar_of(&[
            ("etc/.wh.hosts", b""),
            ("var/.wh..wh..opq", b""),
            ("etc/passwd", b"root"),
        ]);
        let digest = sha256_of(&tar);
        s.write_blob(&digest, &tar[..]).unwrap();
        let dir = s.unpack_layer(&digest, LayerCompression::None).unwrap();

        assert!(
            !dir.join("etc/.wh.hosts").exists(),
            "marker must not be extracted"
        );
        assert!(dir.join("etc/passwd").exists());

        let w = s.whiteouts(&digest);
        assert_eq!(w.removed, [PathBuf::from("etc/hosts")]);
        assert_eq!(w.opaque, [PathBuf::from("var")]);
    }

    /// A hostile layer must not be able to write outside the layer directory.
    #[test]
    fn path_traversal_in_a_layer_is_refused() {
        for name in ["../../escaped", "/etc/passwd", "a/../../../escaped"] {
            let (_t, s) = store();
            let tar = hostile_tar(name, b"pwned");
            let digest = sha256_of(&tar);
            s.write_blob(&digest, &tar[..]).unwrap();

            let err = s.unpack_layer(&digest, LayerCompression::None).unwrap_err();
            assert!(err.to_string().contains("unsafe path"), "{name}: {err}");
            // And the half-written layer is cleaned up rather than left usable.
            assert!(!s.has_layer(&digest), "{name}");
        }
    }

    /// The hostile-tar helper must produce an archive a reader accepts, or the
    /// test above would pass for the wrong reason.
    #[test]
    fn hostile_tar_helper_produces_a_readable_archive() {
        let bytes = hostile_tar("harmless.txt", b"hello");
        let mut archive = tar::Archive::new(std::io::Cursor::new(bytes));
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap().to_str().unwrap(), "harmless.txt");
        let mut body = String::new();
        std::io::Read::read_to_string(&mut entry, &mut body).unwrap();
        assert_eq!(body, "hello");
    }

    #[test]
    fn absolute_paths_in_a_layer_are_refused() {
        assert!(safe_join(Path::new("/root"), Path::new("/etc/passwd")).is_err());
        assert!(safe_join(Path::new("/root"), Path::new("a/../../b")).is_err());
        assert_eq!(
            safe_join(Path::new("/root"), Path::new("./a/b")).unwrap(),
            PathBuf::from("/root/a/b")
        );
    }

    /// The classic escape: plant a symlink, then write "through" it.
    #[test]
    fn writing_through_a_planted_symlink_is_refused() {
        let (_t, s) = store();
        let mut builder = tar::Builder::new(Vec::new());
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        builder
            .append_link(&mut link, "escape", "/tmp/zygo-should-not-exist")
            .unwrap();
        let mut file = tar::Header::new_gnu();
        file.set_size(5);
        file.set_mode(0o644);
        file.set_cksum();
        builder
            .append_data(&mut file, "escape/x", &b"pwned"[..])
            .unwrap();
        let tar = builder.into_inner().unwrap();

        let digest = sha256_of(&tar);
        s.write_blob(&digest, &tar[..]).unwrap();
        let err = s.unpack_layer(&digest, LayerCompression::None).unwrap_err();
        assert!(err.to_string().contains("symlink"), "{err}");
        assert!(!Path::new("/tmp/zygo-should-not-exist").exists());
    }

    /// The same escape as above, on the *flatten* path.
    ///
    /// The unpack path has been hardened since the first pass; the flatten path
    /// had not. A lower layer plants a directory name as a symlink
    /// pointing outside the store, and an upper layer then writes a file under
    /// that name — `create_dir_all` follows the link and the file lands on the
    /// host.
    ///
    /// Attempted rather than inspected: the assertion is that the path outside
    /// the store does not exist afterwards.
    #[cfg(unix)]
    #[test]
    fn flattening_does_not_write_through_a_symlink_a_lower_layer_planted() {
        let (t, s) = store();
        let outside = t.path().join("outside");
        std::fs::create_dir(&outside).unwrap();

        // The lower layer: `etc` is a symlink to somewhere else entirely.
        let mut builder = tar::Builder::new(Vec::new());
        let mut link = tar::Header::new_gnu();
        link.set_entry_type(tar::EntryType::Symlink);
        link.set_size(0);
        link.set_mode(0o777);
        builder
            .append_link(&mut link, "etc", outside.to_str().unwrap())
            .unwrap();
        let lower = builder.into_inner().unwrap();
        let lower_d = sha256_of(&lower);
        s.write_blob(&lower_d, &lower[..]).unwrap();
        s.unpack_layer(&lower_d, LayerCompression::None).unwrap();

        // The upper layer writes under that name.
        let upper = tar_of(&[("etc/passwd", b"pwned")]);
        let upper_d = sha256_of(&upper);
        s.write_blob(&upper_d, &upper[..]).unwrap();
        s.unpack_layer(&upper_d, LayerCompression::None).unwrap();

        // Flattening is what a host without unprivileged overlayfs does on
        // every run, so this is not an unusual path.
        let _ = s.rootfs_view(&[lower_d, upper_d], false, &mount_points());

        assert!(
            !outside.join("passwd").exists(),
            "the flatten wrote through a planted symlink, onto the host"
        );
    }

    /// A dangling symlink left by a lower layer is replaced, not written
    /// through.
    ///
    /// `Path::exists` is false for a dangling link, which is why both the
    /// copy branch and the hard-link branch skipped their removal and created
    /// the link's target instead.
    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_in_the_destination_is_replaced() {
        let t = tempfile::tempdir().unwrap();
        let from = t.path().join("from");
        let to = t.path().join("to");
        std::fs::create_dir_all(&from).unwrap();
        std::fs::create_dir_all(&to).unwrap();
        std::fs::write(from.join("f"), b"real").unwrap();

        let elsewhere = t.path().join("elsewhere");
        std::os::unix::fs::symlink(&elsewhere, to.join("f")).unwrap();
        assert!(!to.join("f").exists(), "the link really is dangling");

        copy_tree(&from, &to).expect("copy over a dangling link");

        assert!(
            !elsewhere.exists(),
            "the copy created the symlink's target instead of replacing the link"
        );
        assert_eq!(std::fs::read(to.join("f")).unwrap(), b"real");
        assert!(
            !std::fs::symlink_metadata(to.join("f"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "the link is still there"
        );
    }

    /// A flattened file keeps the time the layer gave it, not the time it was
    /// copied — apt's cache check and Python's bytecode both read it.
    #[test]
    fn flattening_keeps_the_layers_timestamps() {
        let (_t, s) = store();
        let mut layer = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(5);
        header.set_mode(0o644);
        header.set_mtime(1_758_215_626); // 2025-09-18
        header.set_cksum();
        layer
            .append_data(&mut header, "etc/stamp", &b"hello"[..])
            .unwrap();
        let bytes = layer.into_inner().unwrap();
        let digest = sha256_of(&bytes);
        s.write_blob(&digest, &bytes[..]).unwrap();
        s.unpack_layer(&digest, LayerCompression::None).unwrap();
        let flat = s.flatten(&[digest]).expect("flatten");
        let mtime = std::fs::metadata(flat.join("etc/stamp"))
            .unwrap()
            .modified()
            .unwrap();
        assert_eq!(
            mtime
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            1_758_215_626
        );
    }

    #[test]
    fn flatten_applies_whiteouts_across_layers() {
        let (_t, s) = store();

        let base = tar_of(&[
            ("etc/hosts", b"base"),
            ("etc/passwd", b"root"),
            ("var/log/a", b"1"),
        ]);
        let base_d = sha256_of(&base);
        s.write_blob(&base_d, &base[..]).unwrap();
        s.unpack_layer(&base_d, LayerCompression::None).unwrap();

        let upper = tar_of(&[
            ("etc/.wh.hosts", b""),
            ("var/.wh..wh..opq", b""),
            ("etc/motd", b"hi"),
        ]);
        let upper_d = sha256_of(&upper);
        s.write_blob(&upper_d, &upper[..]).unwrap();
        s.unpack_layer(&upper_d, LayerCompression::None).unwrap();

        let flat = s.flatten(&[base_d, upper_d]).unwrap();
        assert!(
            !flat.join("etc/hosts").exists(),
            "deleted by the upper layer"
        );
        assert!(
            flat.join("etc/passwd").exists(),
            "untouched by the upper layer"
        );
        assert!(flat.join("etc/motd").exists(), "added by the upper layer");
        assert!(
            !flat.join("var/log/a").exists(),
            "hidden by the opaque marker"
        );
        // Bookkeeping files never reach the rootfs.
        assert!(!flat.join(WHITEOUT_FILE).exists());
    }

    #[test]
    fn later_layers_overwrite_earlier_ones() {
        let (_t, s) = store();
        let a = tar_of(&[("f", b"old")]);
        let a_d = sha256_of(&a);
        s.write_blob(&a_d, &a[..]).unwrap();
        s.unpack_layer(&a_d, LayerCompression::None).unwrap();

        let b = tar_of(&[("f", b"new")]);
        let b_d = sha256_of(&b);
        s.write_blob(&b_d, &b[..]).unwrap();
        s.unpack_layer(&b_d, LayerCompression::None).unwrap();

        let flat = s.flatten(&[a_d, b_d]).unwrap();
        assert_eq!(std::fs::read(flat.join("f")).unwrap(), b"new");
    }

    /// Put one layer in the store and return its digest.
    fn write_layer(s: &Store, files: &[(&str, &[u8])]) -> String {
        let tar = tar_of(files);
        let digest = sha256_of(&tar);
        s.write_blob(&digest, &tar[..]).unwrap();
        s.unpack_layer(&digest, LayerCompression::None).unwrap();
        digest
    }

    #[test]
    fn two_functions_on_one_image_do_not_share_a_mount_point_kind() {
        // Found by `zygo up` on a three-function spec: the flattened rootfs was
        // keyed on the layers alone, so the first function to run decided
        // whether `/zygo/handler.py` was a file or a directory and the next two
        // failed with ENOTDIR trying to bind a file onto it.
        let (_t, s) = store();
        let layer = write_layer(&s, &[("bin/sh", b"#!/bin/sh")]);
        let layers = vec![layer];

        let as_file = [MountPoint {
            path: "zygo/handler.py".into(),
            kind: MountPointKind::File,
        }];
        let as_dir = [MountPoint {
            path: "zygo/handler.py".into(),
            kind: MountPointKind::Directory,
        }];

        let file_root = s.flatten_with_mount_points(&layers, &as_file).unwrap();
        let dir_root = s.flatten_with_mount_points(&layers, &as_dir).unwrap();

        assert_ne!(file_root, dir_root, "the two shapes shared a rootfs");
        assert!(file_root.join("zygo/handler.py").is_file());
        assert!(dir_root.join("zygo/handler.py").is_dir());
        // And the first one is still intact after the second was built.
        assert!(
            file_root.join("zygo/handler.py").is_file(),
            "building the second rootfs changed the first"
        );
    }

    #[test]
    fn the_same_shape_still_shares_one_rootfs() {
        // The saving that made a shared directory tempting in the first place:
        // every function of the same image with the same mounts is the common
        // case, and it must not become a copy each.
        let (_t, s) = store();
        let layer = write_layer(&s, &[("bin/sh", b"#!/bin/sh")]);
        let layers = vec![layer];
        let points = [MountPoint {
            path: "zygo/handler.py".into(),
            kind: MountPointKind::File,
        }];

        let first = s.flatten_with_mount_points(&layers, &points).unwrap();
        let second = s.flatten_with_mount_points(&layers, &points).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn mount_points_are_there_before_the_rootfs_is_declared_ready() {
        // Half-built rootfs directories are what the done marker exists to
        // prevent, and the mount points are part of being built.
        let (_t, s) = store();
        let layer = write_layer(&s, &[("bin/sh", b"#!/bin/sh")]);
        let points = [
            MountPoint {
                path: "zygo/agent.py".into(),
                kind: MountPointKind::File,
            },
            MountPoint {
                path: "tmp".into(),
                kind: MountPointKind::Directory,
            },
        ];
        let root = s.flatten_with_mount_points(&[layer], &points).unwrap();
        assert!(root.join(LAYER_DONE).is_file());
        assert!(root.join("zygo/agent.py").is_file());
        assert!(root.join("tmp").is_dir());
    }

    #[test]
    fn rootfs_view_prefers_overlay_but_flattens_when_whiteouts_exist() {
        let (_t, s) = store();
        let clean = tar_of(&[("a", b"1")]);
        let clean_d = sha256_of(&clean);
        s.write_blob(&clean_d, &clean[..]).unwrap();
        s.unpack_layer(&clean_d, LayerCompression::None).unwrap();

        match s
            .rootfs_view(std::slice::from_ref(&clean_d), true, &mount_points())
            .unwrap()
        {
            RootfsView::Overlay { lower } => {
                assert_eq!(lower.len(), 2, "the layer plus the mount-point skeleton");
                assert_eq!(
                    lower[1],
                    s.skeleton(&mount_points()).unwrap(),
                    "the skeleton must be the lowest layer, so the image wins"
                );
            }
            other => panic!("expected overlay, got {other:?}"),
        }
        // Without overlay support, the same stack is flattened.
        assert!(matches!(
            s.rootfs_view(std::slice::from_ref(&clean_d), false, &mount_points())
                .unwrap(),
            RootfsView::Flat { .. }
        ));

        let wh = tar_of(&[("etc/.wh.x", b"")]);
        let wh_d = sha256_of(&wh);
        s.write_blob(&wh_d, &wh[..]).unwrap();
        s.unpack_layer(&wh_d, LayerCompression::None).unwrap();
        assert!(
            matches!(
                s.rootfs_view(&[clean_d, wh_d], true, &mount_points())
                    .unwrap(),
                RootfsView::Flat { .. }
            ),
            "a layer with whiteouts cannot be shown as a rootless overlay"
        );
    }

    #[test]
    fn overlay_lowerdirs_are_ordered_top_layer_first() {
        let (_t, s) = store();
        let mut digests = Vec::new();
        for name in ["base", "mid", "top"] {
            let t = tar_of(&[(name, b"x")]);
            let d = sha256_of(&t);
            s.write_blob(&d, &t[..]).unwrap();
            s.unpack_layer(&d, LayerCompression::None).unwrap();
            digests.push(d);
        }
        match s.rootfs_view(&digests, true, &mount_points()).unwrap() {
            RootfsView::Overlay { lower } => {
                assert_eq!(
                    lower[0],
                    s.layer_dir(&digests[2]).unwrap(),
                    "top layer first"
                );
                assert_eq!(
                    lower[2],
                    s.layer_dir(&digests[0]).unwrap(),
                    "base layer last"
                );
            }
            other => panic!("expected overlay, got {other:?}"),
        }
    }

    #[test]
    fn the_index_round_trips_and_deduplicates() {
        let (_t, s) = store();
        let entry = ImageEntry {
            reference: "python:3.12".into(),
            manifest: sha256_of(b"m"),
            config: sha256_of(b"c"),
            layers: vec![sha256_of(b"l")],
            size: 10,
            pulled_at: 1,
            index: None,
            platform: None,
        };
        s.put(entry.clone()).unwrap();
        s.put(ImageEntry {
            size: 20,
            pulled_at: 2,
            ..entry.clone()
        })
        .unwrap();

        let list = s.list();
        assert_eq!(list.len(), 1, "same reference replaces, not appends");
        assert_eq!(list[0].size, 20);

        let r: Reference = "python:3.12".parse().unwrap();
        assert_eq!(s.get(&r).unwrap().size, 20);
    }

    #[test]
    fn unreferenced_layers_are_reported_for_gc() {
        let (_t, s) = store();
        let kept = tar_of(&[("a", b"1")]);
        let kept_d = sha256_of(&kept);
        s.write_blob(&kept_d, &kept[..]).unwrap();
        s.unpack_layer(&kept_d, LayerCompression::None).unwrap();

        let orphan = tar_of(&[("b", b"2")]);
        let orphan_d = sha256_of(&orphan);
        s.write_blob(&orphan_d, &orphan[..]).unwrap();
        s.unpack_layer(&orphan_d, LayerCompression::None).unwrap();

        s.put(ImageEntry {
            reference: "img:1".into(),
            manifest: sha256_of(b"m"),
            config: sha256_of(b"c"),
            layers: vec![kept_d],
            size: 1,
            pulled_at: 1,
            index: None,
            platform: None,
        })
        .unwrap();

        assert_eq!(s.unreferenced_layers().unwrap(), [orphan_d]);
    }

    /// The flattened rootfs is bigger than the layers it comes from, and for
    /// three small images it was most of the data directory. Prune could not
    /// see it at all until the directory recorded what it was built from.
    #[test]
    fn a_flattened_rootfs_is_collected_once_its_image_is_gone() {
        let (_t, s) = store();
        let tar = tar_of(&[("a", b"1")]);
        let digest = sha256_of(&tar);
        s.write_blob(&digest, &tar[..]).unwrap();
        s.unpack_layer(&digest, LayerCompression::None).unwrap();

        s.put(ImageEntry {
            reference: "img:1".into(),
            manifest: sha256_of(b"m"),
            config: sha256_of(b"c"),
            layers: vec![digest.clone()],
            size: 1,
            pulled_at: 1,
            index: None,
            platform: None,
        })
        .unwrap();

        let flat = s
            .flatten_with_mount_points(std::slice::from_ref(&digest), &mount_points())
            .unwrap();
        assert!(flat.is_dir());
        assert!(
            s.unreferenced_flat().unwrap().is_empty(),
            "the image that produced it is still here"
        );

        // The image goes; the rootfs built from its layer goes with it.
        std::fs::write(s.paths().image_index(), b"[]").unwrap();
        assert_eq!(s.unreferenced_flat().unwrap(), [flat]);
    }

    /// Until `rm` existed, nothing a user decided they were finished with
    /// could become unreferenced: `prune` collected only what a crash had
    /// orphaned. This is the whole of that fix.
    #[test]
    fn removing_an_image_makes_its_layers_collectable() {
        let (_t, s) = store();
        let tar = tar_of(&[("a", b"1")]);
        let digest = sha256_of(&tar);
        s.write_blob(&digest, &tar[..]).unwrap();
        s.unpack_layer(&digest, LayerCompression::None).unwrap();

        let base = ImageEntry {
            reference: "img:1".into(),
            manifest: sha256_of(b"m"),
            config: sha256_of(b"c"),
            layers: vec![digest.clone()],
            size: 1,
            pulled_at: 1,
            index: None,
            platform: None,
        };
        s.put(base.clone()).unwrap();
        // A derived image of that base, and an unrelated one that must stay.
        s.put(ImageEntry {
            reference: "img:1+system.abcdef123456".into(),
            ..base.clone()
        })
        .unwrap();
        s.put(ImageEntry {
            reference: "other:1".into(),
            ..base.clone()
        })
        .unwrap();

        assert!(
            s.unreferenced_layers().unwrap().is_empty(),
            "everything is referenced while the images are here"
        );

        let removed = s.remove(&"img:1".parse().unwrap()).unwrap();
        let names: Vec<&str> = removed.iter().map(|e| e.reference.as_str()).collect();
        assert_eq!(
            names,
            ["img:1", "img:1+system.abcdef123456"],
            "the derived image goes with its base, base first"
        );
        assert_eq!(
            s.list().len(),
            1,
            "the unrelated image is untouched: {:?}",
            s.list()
        );
        // Still referenced — by `other:1`, which shares the layer.
        assert!(s.unreferenced_layers().unwrap().is_empty());

        s.remove(&"other:1".parse().unwrap()).unwrap();
        assert_eq!(
            s.unreferenced_layers().unwrap(),
            [digest],
            "the last reference is gone, so the layer is collectable"
        );
    }

    #[test]
    fn removing_an_image_that_is_not_here_removes_nothing() {
        let (_t, s) = store();
        assert!(s.remove(&"ghost:1".parse().unwrap()).unwrap().is_empty());
    }

    /// A venv or a rootfs keyed on a live image is never collectable by
    /// liveness alone, so age is the only thing that can retire it — and age
    /// needs the hit path to leave a mark.
    #[test]
    fn using_a_flattened_rootfs_records_that_it_was_used() {
        let (_t, s) = store();
        let tar = tar_of(&[("a", b"1")]);
        let digest = sha256_of(&tar);
        s.write_blob(&digest, &tar[..]).unwrap();
        s.unpack_layer(&digest, LayerCompression::None).unwrap();
        s.put(ImageEntry {
            reference: "img:1".into(),
            manifest: sha256_of(b"m"),
            config: sha256_of(b"c"),
            layers: vec![digest.clone()],
            size: 1,
            pulled_at: 1,
            index: None,
            platform: None,
        })
        .unwrap();

        let dir = s
            .flatten_with_mount_points(std::slice::from_ref(&digest), &mount_points())
            .unwrap();

        // Backdate the marker, as an unused cache would be.
        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 86_400);
        std::fs::File::options()
            .write(true)
            .open(dir.join(LAYER_DONE))
            .unwrap()
            .set_modified(long_ago)
            .unwrap();

        let week = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 86_400);
        assert_eq!(
            s.flat_unused_since(week).unwrap(),
            std::slice::from_ref(&dir),
            "ninety days idle is older than a week"
        );

        // A hit moves it out of reach of the same cutoff.
        let again = s
            .flatten_with_mount_points(std::slice::from_ref(&digest), &mount_points())
            .unwrap();
        assert_eq!(again, dir, "same key, same directory");
        assert!(
            s.flat_unused_since(week).unwrap().is_empty(),
            "using it is what keeps it"
        );

        // Age never collects what liveness already condemns: no double count.
        std::fs::write(s.paths().image_index(), b"[]").unwrap();
        assert_eq!(s.unreferenced_flat().unwrap(), [dir]);
        assert!(
            s.flat_unused_since(std::time::SystemTime::now())
                .unwrap()
                .is_empty(),
            "already collectable as unreferenced; counting it twice would \
             double the reported size"
        );
    }

    /// The blob is dead weight once its layer is unpacked, and it is half the
    /// store. Offered, never taken without `--blobs`.
    #[test]
    fn a_blob_is_droppable_once_its_layer_is_unpacked() {
        let (_t, s) = store();
        let tar = tar_of(&[("a", b"1")]);
        let digest = sha256_of(&tar);
        s.write_blob(&digest, &tar[..]).unwrap();

        assert!(
            s.droppable_blobs().unwrap().is_empty(),
            "not referenced by any image yet"
        );

        s.unpack_layer(&digest, LayerCompression::None).unwrap();
        s.put(ImageEntry {
            reference: "img:1".into(),
            manifest: sha256_of(b"m"),
            config: sha256_of(b"c"),
            layers: vec![digest.clone(), digest.clone()],
            size: 1,
            pulled_at: 1,
            index: None,
            platform: None,
        })
        .unwrap();

        let droppable = s.droppable_blobs().unwrap();
        assert_eq!(droppable.len(), 1, "a shared layer is offered once");
        assert_eq!(droppable[0].0, digest);
        assert!(droppable[0].1.is_file());
    }

    /// A directory from before the record, or from an interrupted build,
    /// cannot prove it is live — and a cache that cannot is what prune takes.
    #[test]
    fn a_flat_directory_with_no_record_is_collected() {
        let (_t, s) = store();
        let dir = s.paths().flat_cache().join("deadbeef");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(LAYER_DONE), b"").unwrap();
        assert_eq!(s.unreferenced_flat().unwrap(), [dir]);
    }

    #[test]
    fn locks_are_reentrant_across_drops() {
        let (_t, s) = store();
        {
            let _a = s.lock("thing").unwrap();
        }
        let _b = s.lock("thing").unwrap();
    }
}
