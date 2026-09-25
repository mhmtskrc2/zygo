// SPDX-License-Identifier: Apache-2.0
//! `zygo.lock` — what a project's functions resolved to (design doc §3.7).
//!
//! `sandbox.toml` says `image = "python:3.12-slim"`; a tag is a pointer, and
//! the registry moves it. `system = ["libpq5"]` names a package, and `apt`
//! chooses the version. The lock file, written beside the spec by `zygo up`,
//! records what those resolved to *here*, so that a second machine — or this
//! one next month — can tell whether it is about to run the same thing.
//!
//! The semantics are `Cargo.lock`'s, because they are the ones nobody argues
//! with:
//!
//! * absent → written;
//! * the spec changed (a different reference, a different package list, an
//!   edited requirements file) → that entry is rewritten, silently, because
//!   the user just asked for the change;
//! * the spec is the same but the image the host holds is not the one
//!   recorded → **refused**, with the two digests and a remedy. Nothing here
//!   ever changes what runs; it only stops a silent change;
//! * the same packages resolved to different versions → recorded, with a
//!   warning. `apt`'s archive does not keep old versions, so refusing would
//!   strand every fresh host.
//!
//! What it does not do yet, on purpose, until the shape has been discussed:
//! pin pip's transitive resolution (only the requirements file's hash is
//! recorded), pull the locked digest itself, or offer a `--frozen` for CI.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::image::{ImageEntry, Reference, Store};

/// The file's name, beside `sandbox.toml`.
pub const LOCK_FILE: &str = "zygo.lock";

/// Bumped when a field's meaning changes, not when one is added.
pub const LOCK_VERSION: u32 = 1;

const HEADER: &str = "\
# zygo.lock — what each function resolved to when `zygo up` last ran.
# Commit it. `zygo up` refuses to bring a function up on an image that has
# moved from what is recorded here; `zygo up --relock` accepts the move.
";

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "{path} is not a lock file: {message}\n  → delete it and run `zygo up` to write a fresh one"
    )]
    Parse { path: PathBuf, message: String },
    #[error(
        "{path} is version {found}; this build reads version {LOCK_VERSION}\n  → \
         upgrade zygo, or delete the file and run `zygo up` to write a fresh one"
    )]
    Version { path: PathBuf, found: u32 },
}

/// The whole file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockFile {
    pub version: u32,
    /// One entry per function, keyed by name. `[fn.<name>]` on disk.
    #[serde(rename = "fn", default, skip_serializing_if = "BTreeMap::is_empty")]
    pub functions: BTreeMap<String, LockedFn>,
}

impl Default for LockFile {
    fn default() -> Self {
        LockFile {
            version: LOCK_VERSION,
            functions: BTreeMap::new(),
        }
    }
}

/// What one function resolved to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedFn {
    /// The image reference as the spec wrote it.
    pub image: String,
    /// What it resolved to: the multi-platform index's digest when the
    /// registry served one — the same image on every architecture — else
    /// the platform manifest's, in which case `platform` says whose.
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// `name=version` for every `system` package, as `apt` installed them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirements: Option<LockedFile>,
}

/// A project file the function depends on, by content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockedFile {
    /// Relative to the spec's directory, or absolute when it is outside it.
    pub path: String,
    pub sha256: String,
}

impl LockedFile {
    pub fn of(path: &Path, base_dir: &Path) -> std::io::Result<LockedFile> {
        let bytes = std::fs::read(path)?;
        let shown = path
            .strip_prefix(base_dir)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| path.to_path_buf());
        Ok(LockedFile {
            path: shown.to_string_lossy().into_owned(),
            sha256: hex::encode(Sha256::digest(&bytes)),
        })
    }
}

/// How one function's lock entry relates to what the host has now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Comparison {
    /// The spec asked for something different. Each is a short sentence.
    pub changes: Vec<String>,
    /// The spec is the same and the world is not.
    pub drifts: Vec<Drift>,
}

impl Comparison {
    pub fn is_same(&self) -> bool {
        self.changes.is_empty() && self.drifts.is_empty()
    }

    /// The drift that refuses an `up`: the image is not the one recorded.
    pub fn image_drift(&self) -> Option<&Drift> {
        self.drifts
            .iter()
            .find(|d| matches!(d, Drift::Image { .. }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drift {
    /// Same reference, different content.
    Image {
        image: String,
        locked: String,
        locked_platform: Option<String>,
        actual: String,
        actual_platform: Option<String>,
    },
    /// Same packages, different versions.
    System {
        locked: Vec<String>,
        actual: Vec<String>,
    },
}

impl std::fmt::Display for Drift {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Drift::Image {
                image,
                locked,
                locked_platform,
                actual,
                actual_platform,
            } => {
                let platform = |p: &Option<String>| match p {
                    Some(p) => format!(" ({p})"),
                    None => String::new(),
                };
                write!(
                    f,
                    "zygo.lock pins {image} to {locked}{}; this host has {actual}{}",
                    platform(locked_platform),
                    platform(actual_platform)
                )?;
                if locked_platform.is_some()
                    && actual_platform.is_some()
                    && locked_platform != actual_platform
                {
                    write!(
                        f,
                        " — the image was pulled as a single platform, so the lock cannot \
                         name it across architectures"
                    )?;
                }
                Ok(())
            }
            Drift::System { locked, actual } => {
                let moved: Vec<String> = actual
                    .iter()
                    .filter(|a| !locked.contains(a))
                    .map(|a| {
                        let name = a.split('=').next().unwrap_or(a);
                        match locked.iter().find(|l| l.split('=').next() == Some(name)) {
                            Some(l) => format!("{l} → {a}"),
                            None => a.clone(),
                        }
                    })
                    .collect();
                write!(
                    f,
                    "system packages resolved differently: {}",
                    moved.join(", ")
                )
            }
        }
    }
}

impl LockedFn {
    /// What `image`, `system` and `requirements` resolve to on this host,
    /// or `None` when the image is not in the store — in which case nothing
    /// can be said and the serve that follows will say why.
    pub fn observe(
        store: &Store,
        image: &str,
        system: &[String],
        requirements: Option<&Path>,
        base_dir: &Path,
    ) -> std::io::Result<Option<LockedFn>> {
        let Ok(reference) = image.parse::<Reference>() else {
            return Ok(None);
        };
        let Some(entry) = store.get(&reference) else {
            return Ok(None);
        };
        let requirements = match requirements {
            Some(path) => Some(LockedFile::of(path, base_dir)?),
            None => None,
        };
        Ok(Some(LockedFn::from_entry(
            image,
            &entry,
            crate::derive::recorded_versions(store, &entry, system),
            requirements,
        )))
    }

    /// Build an entry from what the store holds for `image`.
    pub fn from_entry(
        image: &str,
        entry: &ImageEntry,
        system: Vec<String>,
        requirements: Option<LockedFile>,
    ) -> LockedFn {
        let (digest, platform) = match &entry.index {
            Some(index) => (index.clone(), None),
            None => (
                entry.manifest.clone(),
                Some(entry.platform.clone().unwrap_or_else(host_platform)),
            ),
        };
        LockedFn {
            image: image.to_string(),
            digest,
            platform,
            system,
            requirements,
        }
    }

    /// `self` is what the lock recorded; `actual` is what the host has now.
    pub fn compare(&self, actual: &LockedFn) -> Comparison {
        let mut c = Comparison::default();

        if self.image != actual.image {
            c.changes
                .push(format!("image {} → {}", self.image, actual.image));
        } else if self.digest != actual.digest {
            c.drifts.push(Drift::Image {
                image: self.image.clone(),
                locked: self.digest.clone(),
                locked_platform: self.platform.clone(),
                actual: actual.digest.clone(),
                actual_platform: actual.platform.clone(),
            });
        }

        let names = |v: &[String]| -> Vec<String> {
            let mut n: Vec<String> = v
                .iter()
                .map(|p| p.split('=').next().unwrap_or(p).to_string())
                .collect();
            n.sort();
            n
        };
        if names(&self.system) != names(&actual.system) {
            c.changes.push(format!(
                "system packages [{}] → [{}]",
                names(&self.system).join(", "),
                names(&actual.system).join(", ")
            ));
        } else if self.system != actual.system {
            let mut locked = self.system.clone();
            let mut now = actual.system.clone();
            locked.sort();
            now.sort();
            if locked != now {
                c.drifts.push(Drift::System {
                    locked: self.system.clone(),
                    actual: actual.system.clone(),
                });
            }
        }

        match (&self.requirements, &actual.requirements) {
            (Some(a), Some(b)) if a.path != b.path => {
                c.changes
                    .push(format!("requirements {} → {}", a.path, b.path));
            }
            (Some(a), Some(b)) if a.sha256 != b.sha256 => {
                c.changes.push(format!("requirements {} edited", a.path));
            }
            (Some(a), None) => c.changes.push(format!("requirements {} removed", a.path)),
            (None, Some(b)) => c.changes.push(format!("requirements {} added", b.path)),
            _ => {}
        }
        c
    }
}

fn host_platform() -> String {
    let p = crate::image::Platform::host();
    format!("{}/{}", p.os, p.architecture)
}

impl LockFile {
    /// Where the lock lives for a spec in `base_dir`.
    pub fn beside(base_dir: &Path) -> PathBuf {
        base_dir.join(LOCK_FILE)
    }

    /// `None` when there is no file yet.
    pub fn load(path: &Path) -> Result<Option<LockFile>, LockError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(LockError::Read {
                    path: path.to_path_buf(),
                    source: e,
                });
            }
        };
        let lock: LockFile = toml::from_str(&text).map_err(|e| LockError::Parse {
            path: path.to_path_buf(),
            message: e.message().to_string(),
        })?;
        if lock.version != LOCK_VERSION {
            return Err(LockError::Version {
                path: path.to_path_buf(),
                found: lock.version,
            });
        }
        Ok(Some(lock))
    }

    /// The file's text, header included.
    pub fn to_toml(&self) -> String {
        let body = toml::to_string(self).expect("a lock file is serialisable");
        format!("{HEADER}\n{body}")
    }

    /// Written whole and renamed into place, so a reader never sees half.
    pub fn save(&self, path: &Path) -> Result<(), LockError> {
        let tmp = path.with_extension("lock.tmp");
        let write = || -> std::io::Result<()> {
            std::fs::write(&tmp, self.to_toml())?;
            std::fs::rename(&tmp, path)
        };
        write().map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            LockError::Write {
                path: path.to_path_buf(),
                source: e,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(index: Option<&str>) -> ImageEntry {
        ImageEntry {
            reference: "python:3.12-slim".into(),
            manifest: "sha256:manifest-arm64".into(),
            config: "sha256:cfg".into(),
            layers: vec![],
            size: 0,
            pulled_at: 0,
            index: index.map(str::to_string),
            platform: Some("linux/arm64".into()),
        }
    }

    fn locked() -> LockedFn {
        LockedFn {
            image: "python:3.12-slim".into(),
            digest: "sha256:index".into(),
            platform: None,
            system: vec!["jq=1.7.1-2".into(), "libpq5=16.4-1".into()],
            requirements: Some(LockedFile {
                path: "requirements.txt".into(),
                sha256: "aa".into(),
            }),
        }
    }

    #[test]
    fn the_file_round_trips_and_reads_as_the_documented_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let path = LockFile::beside(tmp.path());
        assert!(path.ends_with("zygo.lock"));

        let mut lock = LockFile::default();
        lock.functions.insert("api".into(), locked());
        lock.save(&path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# zygo.lock"), "{text}");
        assert!(text.contains("version = 1\n"), "{text}");
        assert!(text.contains("[fn.api]\n"), "{text}");
        assert!(text.contains("digest = \"sha256:index\""), "{text}");
        assert!(text.contains("[fn.api.requirements]\n"), "{text}");
        assert!(
            !text.contains("platform"),
            "an index digest has no platform"
        );
        assert!(
            !path.with_extension("lock.tmp").exists(),
            "no temp file left"
        );

        assert_eq!(LockFile::load(&path).unwrap(), Some(lock));
    }

    #[test]
    fn a_missing_file_is_none_and_a_broken_one_names_itself() {
        let tmp = tempfile::tempdir().unwrap();
        let path = LockFile::beside(tmp.path());
        assert_eq!(LockFile::load(&path).unwrap(), None);

        std::fs::write(&path, "this is = not [ toml").unwrap();
        let err = LockFile::load(&path).unwrap_err();
        assert!(matches!(err, LockError::Parse { .. }), "{err}");
        assert!(err.to_string().contains("zygo.lock"), "{err}");

        std::fs::write(&path, "version = 7\n").unwrap();
        let err = LockFile::load(&path).unwrap_err();
        assert!(matches!(err, LockError::Version { found: 7, .. }), "{err}");
    }

    #[test]
    fn an_indexed_image_pins_the_index_and_a_single_platform_one_says_whose() {
        let multi = LockedFn::from_entry(
            "python:3.12-slim",
            &entry(Some("sha256:index")),
            vec![],
            None,
        );
        assert_eq!(multi.digest, "sha256:index");
        assert_eq!(multi.platform, None);

        let single = LockedFn::from_entry("python:3.12-slim", &entry(None), vec![], None);
        assert_eq!(single.digest, "sha256:manifest-arm64");
        assert_eq!(single.platform.as_deref(), Some("linux/arm64"));
    }

    #[test]
    fn the_same_thing_is_the_same() {
        assert!(locked().compare(&locked()).is_same());
    }

    #[test]
    fn a_different_reference_is_a_change_not_a_drift() {
        let mut now = locked();
        now.image = "python:3.13-slim".into();
        now.digest = "sha256:other".into();
        let c = locked().compare(&now);
        assert!(c.drifts.is_empty(), "{c:?}");
        assert_eq!(c.changes, ["image python:3.12-slim → python:3.13-slim"]);
    }

    #[test]
    fn a_moved_tag_is_the_drift_that_refuses() {
        let mut now = locked();
        now.digest = "sha256:moved".into();
        let c = locked().compare(&now);
        assert!(c.changes.is_empty(), "{c:?}");
        let drift = c.image_drift().expect("an image drift");
        let text = drift.to_string();
        assert!(text.contains("sha256:index"), "{text}");
        assert!(text.contains("sha256:moved"), "{text}");
        assert!(!text.contains("single platform"), "{text}");
    }

    #[test]
    fn a_lock_from_another_architecture_says_so() {
        let locked = LockedFn {
            digest: "sha256:manifest-amd64".into(),
            platform: Some("linux/amd64".into()),
            ..locked()
        };
        let now = LockedFn {
            digest: "sha256:manifest-arm64".into(),
            platform: Some("linux/arm64".into()),
            ..locked.clone()
        };
        let text = locked.compare(&now).image_drift().unwrap().to_string();
        assert!(text.contains("linux/amd64"), "{text}");
        assert!(text.contains("single platform"), "{text}");
    }

    #[test]
    fn moved_versions_are_a_warning_and_a_different_package_list_is_a_change() {
        let mut now = locked();
        now.system = vec!["jq=1.7.1-2".into(), "libpq5=16.6-1".into()];
        let c = locked().compare(&now);
        assert!(c.changes.is_empty());
        assert!(c.image_drift().is_none());
        assert_eq!(c.drifts.len(), 1);
        let text = c.drifts[0].to_string();
        assert!(text.contains("libpq5=16.4-1 → libpq5=16.6-1"), "{text}");

        let mut now = locked();
        now.system = vec!["jq=1.7.1-2".into()];
        let c = locked().compare(&now);
        assert!(c.drifts.is_empty());
        assert_eq!(c.changes, ["system packages [jq, libpq5] → [jq]"]);

        // Order does not matter.
        let mut now = locked();
        now.system.reverse();
        assert!(locked().compare(&now).is_same());
    }

    #[test]
    fn requirements_edits_are_changes() {
        let mut now = locked();
        now.requirements.as_mut().unwrap().sha256 = "bb".into();
        assert_eq!(
            locked().compare(&now).changes,
            ["requirements requirements.txt edited"]
        );
        now.requirements = None;
        assert_eq!(
            locked().compare(&now).changes,
            ["requirements requirements.txt removed"]
        );
    }

    #[test]
    fn observe_reads_the_store_and_hashes_the_requirements() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::Paths::rooted(tmp.path());
        paths.ensure().unwrap();
        let store = Store::new(paths);
        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let reqs = project.join("requirements.txt");
        std::fs::write(&reqs, "six\n").unwrap();

        // Not pulled: nothing to say.
        assert_eq!(
            LockedFn::observe(&store, "python:3.12-slim", &[], Some(&reqs), &project).unwrap(),
            None
        );

        store.put(entry(Some("sha256:index"))).unwrap();
        let seen = LockedFn::observe(&store, "python:3.12-slim", &[], Some(&reqs), &project)
            .unwrap()
            .expect("pulled now");
        assert_eq!(seen.digest, "sha256:index");
        let r = seen.requirements.unwrap();
        assert_eq!(r.path, "requirements.txt", "relative to the spec");
        assert_eq!(r.sha256, hex::encode(Sha256::digest(b"six\n")));
    }
}
