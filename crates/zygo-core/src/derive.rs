// SPDX-License-Identifier: Apache-2.0
//! Derived system layers: apt packages as an OCI layer of their own.
//!
//! `system = ["libpq5", "jq"]` in a function's spec → those packages installed
//! once, on top of the image, as a new OCI layer in the content-addressed
//! store. Every function naming the same packages against the same image
//! shares it; a different version list is a different layer; nothing is ever
//! installed into an image another tenant sees.
//!
//! The root of a sandbox is read-only, so a tenant cannot `apt install`. The
//! alternative is asking them to write a Dockerfile, which is the thing Zygo
//! exists to avoid. So the install runs in a one-shot sandbox whose root is a
//! private, writable copy of the image, and what the install changed becomes
//! the layer.
//!
//! **Copy and diff, not overlayfs.** An overlay `upperdir` was the plan. An
//! unprivileged overlay mount needs kernel 5.11 and is refused on the host this
//! was first verified on (5.10); and even where it works, turning overlay
//! whiteouts — character devices and `trusted.*` xattrs — into OCI ones needs
//! privileges a rootless build does not have. Copying the flattened image and
//! diffing afterwards works everywhere, costs one copy of the image per build
//! (a second or two for a slim image, once per key), and produces the layer
//! from ordinary files. Overlay would be an optimisation of the same result.
//!
//! **Host networking**, as for the venv cache and for the same narrow reason:
//! the operation is installing distribution packages the user named, once,
//! before any tenant code runs. A package-repository allowlist
//! belongs to `egress` networking, which does not exist yet; until it does this
//! is the same grant `pip` gets.
//!
//! **Root inside the user namespace**, because `apt` and `dpkg` insist on it.
//! That is the user's own uid on the host under a single-id map, so files a
//! package wants owned by `_apt` or `man` come out owned by root — which is
//! also what a rootless `docker build` produces. Where `newuidmap` and a
//! subordinate range are available the launcher maps them and ownership is
//! kept.
//!
//! `nix = [...]` is declared in the spec and not implemented here.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::error::{Error, IoContext, Result};
use crate::image::store::is_internal_entry;
use crate::image::{ImageEntry, LayerCompression, Store};
use crate::sandbox::oneshot::{run_captured, tail};
use crate::sandbox::{DEFAULT_PATH, RootfsView, SandboxConfig};
use crate::spec::{Bytes, Cpu, Duration, Network, ResolvedFn, SeccompProfile, SpecError};

/// Top-level directories every sandbox mounts over, and so never part of a
/// layer: whatever is under them at build time is the kernel's or the host's,
/// not the image's.
pub(crate) const RUNTIME_DIRS: &[&str] = &["proc", "sys", "dev", "tmp", "run"];

/// The record of a build under `cache/system/<key>/`: the packages at the
/// versions `apt` chose, one per line.
const RECORD: &str = "packages";

/// A derived image, ready to be served from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Derived {
    /// The base image plus one layer; registered in the store's index.
    pub image: ImageEntry,
    /// Whether this call built it or found it.
    pub built: bool,
    /// `name=version` as installed, for every package asked for.
    pub versions: Vec<String>,
}

/// Check one `system` entry: `name` or `name=version`.
///
/// The name rules are Debian policy §5.6.1 — lowercase letters, digits, `+`,
/// `-`, `.`, at least two characters, starting alphanumeric. Strict on purpose:
/// these strings reach `apt-get` as arguments, and while they are passed as
/// positional parameters rather than through a shell, a package name is a
/// small, known alphabet and there is no reason to accept anything else.
pub fn validate_package(spec: &str) -> std::result::Result<(), String> {
    let (name, version) = match spec.split_once('=') {
        Some((n, v)) => (n, Some(v)),
        None => (spec, None),
    };
    let name_char =
        |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'+' || b == b'-' || b == b'.';
    let name_ok = name.len() >= 2
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.bytes().all(name_char);
    if !name_ok {
        return Err(format!(
            "`{spec}` is not a package name (lowercase letters, digits, `+`, `-`, `.`; \
             at least two characters, starting with a letter or digit)"
        ));
    }
    if let Some(v) = version {
        let version_ok = !v.is_empty()
            && v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".+~:-*".contains(&b));
        if !version_ok {
            return Err(format!(
                "`{spec}`: the version after `=` may contain letters, digits, \
                 `.`, `+`, `~`, `:`, `-` and `*`"
            ));
        }
    }
    Ok(())
}

/// Sorted and de-duplicated: the form the key, the record and `apt` all see.
fn normalise(packages: &[String]) -> Vec<String> {
    let mut p = packages.to_vec();
    p.sort();
    p.dedup();
    p
}

/// The cache key: this image, this architecture, these packages.
///
/// The manifest digest rather than the reference, as for the venv cache: a tag
/// moves, and packages installed on last month's image are not a layer for
/// today's. The architecture because a layer holds compiled code. The
/// packages in normalised order, so `["b", "a"]` and `["a", "b"]` share.
pub fn cache_key(base_manifest: &str, packages: &[String], arch: &str) -> String {
    let mut key = Sha256::new();
    key.update(base_manifest.as_bytes());
    key.update(b"\n");
    key.update(arch.as_bytes());
    key.update(b"\n");
    for p in normalise(packages) {
        key.update(p.as_bytes());
        key.update(b"\n");
    }
    hex::encode(key.finalize())
}

/// What the derived image is called in the store's index.
///
/// Not a reference anyone types: `zygo image ls` shows it, and the suffix says
/// where it came from. The base reference is kept in front so the listing
/// sorts derived images next to their base.
pub fn derived_reference(base: &str, key: &str) -> String {
    format!("{base}+system.{}", &key[..12])
}

/// Get the image with `packages` installed on `base`, building it if needed.
///
/// Serialised per key with the store's lock, so two functions that name the
/// same packages and start together build once. The common case is a scan of
/// the image index.
pub fn ensure(store: &Store, base: &ImageEntry, packages: &[String]) -> Result<Derived> {
    for p in packages {
        validate_package(p).map_err(|m| Error::Spec(SpecError::invalid("system", m)))?;
    }
    let packages = normalise(packages);
    if packages.is_empty() {
        return Ok(Derived {
            image: base.clone(),
            built: false,
            versions: Vec::new(),
        });
    }

    let key = cache_key(&base.manifest, &packages, std::env::consts::ARCH);
    let reference = derived_reference(&base.reference, &key);
    if let Some(image) = find(store, &reference) {
        return Ok(Derived {
            image,
            built: false,
            versions: read_record(store, &key),
        });
    }

    let _lock = store.lock(&format!("system-{key}"))?;
    if let Some(image) = find(store, &reference) {
        return Ok(Derived {
            image,
            built: false,
            versions: read_record(store, &key),
        });
    }
    build(store, base, &packages, &key, reference)
}

/// The `name=version` list recorded when `packages` were last built on
/// `base` — what `zygo.lock` records. Empty when nothing was asked for or
/// nothing has been built yet.
pub fn recorded_versions(store: &Store, base: &ImageEntry, packages: &[String]) -> Vec<String> {
    let packages = normalise(packages);
    if packages.is_empty() {
        return Vec::new();
    }
    read_record(
        store,
        &cache_key(&base.manifest, &packages, std::env::consts::ARCH),
    )
}

/// A derived image that is fully present: indexed, and every layer unpacked.
pub(crate) fn find(store: &Store, reference: &str) -> Option<ImageEntry> {
    store
        .list()
        .into_iter()
        .find(|e| e.reference == reference)
        .filter(|e| e.layers.iter().all(|d| store.has_layer(d)))
}

/// Copy the image, install into the copy, turn the difference into a layer.
fn build(
    store: &Store,
    base: &ImageEntry,
    packages: &[String],
    key: &str,
    reference: String,
) -> Result<Derived> {
    let flat = store.flatten(&base.layers)?;
    let work =
        store
            .paths()
            .tmp()
            .join(format!("system-{}-{}", &key[..12], crate::process_token()));
    let _ = std::fs::remove_dir_all(&work);
    let root = work.join("root");
    copy_preserving(&flat, &root)?;
    // The plan mounts over these; they must exist. Never part of the diff.
    for d in RUNTIME_DIRS {
        let p = root.join(d);
        std::fs::create_dir_all(&p).at(&p)?;
    }

    tracing::info!(
        image = %base.reference,
        packages = ?packages,
        "installing system packages inside the image (host networking, once)"
    );
    let started = Instant::now();
    if let Err(e) = install(
        &base.reference,
        packages,
        &root,
        &work.join("newroot"),
        store.paths(),
    ) {
        let _ = std::fs::remove_dir_all(&work);
        return Err(e);
    }
    let installed_s = started.elapsed().as_secs();

    let versions = installed_versions(&root.join("var/lib/dpkg/status"), packages);
    let tar_path = work.join("layer.tar");
    let (digest, size) = write_layer(&flat, &root, &tar_path)?;
    store.write_blob(
        &digest,
        BufReader::new(File::open(&tar_path).at(&tar_path)?),
    )?;
    store.unpack_layer(&digest, LayerCompression::None)?;
    let _ = std::fs::remove_dir_all(&work);

    let mut layers = base.layers.clone();
    layers.push(digest.clone());
    let image = ImageEntry {
        reference,
        manifest: format!("sha256:{key}"),
        config: base.config.clone(),
        layers,
        size: base.size + size,
        pulled_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        // A derived image is this host's build on this host's base; there
        // is no index for it to be selected from.
        index: None,
        platform: base.platform.clone(),
    };
    store.put(image.clone())?;
    write_record(store, key, base, &versions)?;

    tracing::info!(
        install_s = installed_s,
        total_s = started.elapsed().as_secs(),
        layer = %digest,
        layer_bytes = size,
        versions = ?versions,
        "system layer built"
    );
    Ok(Derived {
        image,
        built: true,
        versions,
    })
}

/// Run `apt-get install` in a sandbox whose root is `root`, writable.
fn install(
    image: &str,
    packages: &[String],
    root: &Path,
    newroot: &Path,
    paths: &crate::Paths,
) -> Result<()> {
    let spec = build_spec(image, packages);
    std::fs::create_dir_all(newroot).at(newroot)?;
    let view = RootfsView::Flat {
        dir: root.to_path_buf(),
    };
    let mut config =
        SandboxConfig::from_resolved(&spec, &view, newroot, spec.cmd.clone(), &build_env());
    config.writable_root = true;

    let result = run_captured(&mut config, paths);
    let _ = std::fs::remove_dir_all(newroot);
    match result {
        Ok((0, _)) => Ok(()),
        Ok((code, output)) => Err(Error::Build {
            what: "the system layer build",
            reason: format!(
                "apt-get exited {code} installing {}:\n{}",
                packages.join(" "),
                tail(&output, 40)
            ),
            remedy: "check the package names against the image's distribution; the build \
                     runs `apt-get install` as root inside the image, with host networking"
                .into(),
        }),
        Err(e) => Err(Error::BackendUnavailable {
            backend: "system",
            reason: format!("the package build sandbox failed: {e}"),
            remedy: "run `zygo doctor`; the build needs the same primitives as any sandbox".into(),
        }),
    }
}

/// The one-shot function that performs the install.
///
/// Its own limits and its own profile, not the tenant's: `apt` is not a
/// handler. `permissive` seccomp because `dpkg` uses the legacy `chown`,
/// `chmod` and `mknod` calls the tighter profiles remove — it is what `docker
/// build` runs under, and nothing in it is a tenant's code.
fn build_spec(image: &str, packages: &[String]) -> ResolvedFn {
    use crate::spec::{Layer, ResolveOptions, resolve_standalone};

    let mut spec = resolve_standalone(
        "system-build",
        &Layer {
            image: Some(image.to_string()),
            cmd: Some(build_argv(packages)),
            user: Some("0".to_string()),
            seccomp: Some(SeccompProfile::Permissive),
            mem: Some(Bytes::from_mib(1024)),
            cpu: Some(Cpu(2.0)),
            pids: Some(512),
            timeout: Some(Duration::from_secs(900)),
            scratch: Some(Bytes::from_mib(512)),
            ..Default::default()
        },
        &ResolveOptions {
            one_shot: true,
            ..Default::default()
        },
    )
    .expect("the build layer is fixed and valid");

    spec.mounts = Vec::new();
    // Set after resolution on purpose, bypassing `--allow-host-net`: see the
    // module documentation.
    spec.network = Network::Host;
    spec
}

/// `sh -c '<script>' sh <packages...>`: the packages are positional
/// parameters, never interpolated into the script.
///
/// `APT::Sandbox::User=root` on *both* commands: `apt` drops its download
/// methods to `_apt`, and under a single-id map that uid is not mapped —
/// `seteuid(42)` fails with EINVAL and the http method dies. The first run
/// had the option on `install` only, and `update` is where the download is.
fn build_argv(packages: &[String]) -> Vec<String> {
    // The image's own package lists go first. Official images ship the ones
    // they were built with, and apt revalidates a cached `InRelease` with
    // If-Modified-Since: a mirror that answers "not modified" leaves apt on
    // an index whose Valid-Until has passed, and `update` fails with "Release
    // file … is expired" — for every build, until the image is rebuilt. An
    // index is a few seconds to fetch and this build fetches one anyway.
    let script = "set -e\n\
                  export DEBIAN_FRONTEND=noninteractive\n\
                  rm -rf /var/lib/apt/lists/*\n\
                  apt-get -o APT::Sandbox::User=root update\n\
                  apt-get -o APT::Sandbox::User=root install -y --no-install-recommends \"$@\"\n\
                  apt-get clean\n\
                  rm -rf /var/lib/apt/lists/*\n";
    let mut argv = vec![
        "sh".to_string(),
        "-c".to_string(),
        script.to_string(),
        "sh".to_string(),
    ];
    argv.extend(packages.iter().cloned());
    argv
}

fn build_env() -> Vec<(String, String)> {
    vec![
        ("HOME".to_string(), "/root".to_string()),
        ("PATH".to_string(), DEFAULT_PATH.to_string()),
        ("DEBIAN_FRONTEND".to_string(), "noninteractive".to_string()),
        ("LC_ALL".to_string(), "C".to_string()),
    ]
}

/// `name=version` for each requested package, from the dpkg database the
/// install left behind. A package the database does not list as installed —
/// a virtual name `apt` resolved to something else — is reported as asked for.
fn installed_versions(status: &Path, packages: &[String]) -> Vec<String> {
    let text = std::fs::read_to_string(status).unwrap_or_default();
    let mut installed: BTreeMap<String, String> = BTreeMap::new();
    for stanza in text.split("\n\n") {
        let (mut name, mut version, mut ok) = (None, None, false);
        for line in stanza.lines() {
            if let Some(v) = line.strip_prefix("Package:") {
                name = Some(v.trim());
            } else if let Some(v) = line.strip_prefix("Version:") {
                version = Some(v.trim());
            } else if let Some(v) = line.strip_prefix("Status:") {
                ok = v.trim().ends_with(" installed");
            }
        }
        if ok && let (Some(n), Some(v)) = (name, version) {
            installed.insert(n.to_string(), v.to_string());
        }
    }
    packages
        .iter()
        .map(|p| {
            let name = p.split_once('=').map_or(p.as_str(), |(n, _)| n);
            match installed.get(name) {
                Some(v) => format!("{name}={v}"),
                None => p.clone(),
            }
        })
        .collect()
}

fn record_path(store: &Store, key: &str) -> PathBuf {
    store.paths().system_cache().join(key).join(RECORD)
}

fn write_record(store: &Store, key: &str, base: &ImageEntry, versions: &[String]) -> Result<()> {
    let path = record_path(store, key);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).at(dir)?;
    }
    let mut text = format!("# {} {}\n", base.reference, base.manifest);
    for v in versions {
        text.push_str(v);
        text.push('\n');
    }
    std::fs::write(&path, text).at(&path)
}

/// Records of derived system layers whose derived image is gone.
///
/// The record is only ever read to answer "which versions did `apt` choose",
/// so it is dead the moment its image leaves the index. The image is the
/// authority here and the record is the footnote: the layer itself lives in
/// the image store and is pruned as an ordinary unreferenced layer.
pub fn unreferenced(store: &Store) -> Result<Vec<PathBuf>> {
    let live: std::collections::BTreeSet<String> =
        store.list().into_iter().map(|e| e.reference).collect();

    let dir = store.paths().system_cache();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).at(&dir)? {
        let entry = entry.at(&dir)?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(key) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // First line of the record: `# <base reference> <base manifest>`.
        let record = std::fs::read_to_string(path.join(RECORD)).unwrap_or_default();
        let base = record
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("# "))
            .and_then(|l| l.split_whitespace().next());
        match base {
            Some(base) if live.contains(&derived_reference(base, &key)) => {}
            _ => out.push(path),
        }
    }
    out.sort();
    Ok(out)
}

fn read_record(store: &Store, key: &str) -> Vec<String> {
    std::fs::read_to_string(record_path(store, key))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(str::to_string)
        .collect()
}

// --- the copy ---------------------------------------------------------------

/// Copy a tree keeping modes and modification times.
///
/// The times matter: the diff afterwards treats a file whose size, mode and
/// mtime are unchanged as unchanged, and that is only sound if the copy did
/// not touch them. `dpkg` writes files with their package's timestamps, so a
/// file it replaced with an identical one is identical.
pub(crate) fn copy_preserving(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).at(dst)?;
    for entry in std::fs::read_dir(src).at(src)? {
        let entry = entry.at(src)?;
        let name = entry.file_name();
        if is_internal_entry(&name) {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        let md = std::fs::symlink_metadata(&from).at(&from)?;
        let kind = md.file_type();
        if kind.is_dir() {
            copy_preserving(&from, &to)?;
            // After the contents: a read-only directory has to be filled first.
            std::fs::set_permissions(&to, std::fs::Permissions::from_mode(md.mode() & 0o7777))
                .at(&to)?;
            set_mtime(&to, &md)?;
        } else if kind.is_symlink() {
            let target = std::fs::read_link(&from).at(&from)?;
            std::os::unix::fs::symlink(&target, &to).at(&to)?;
        } else if kind.is_file() {
            std::fs::copy(&from, &to).at(&to)?;
            set_mtime(&to, &md)?;
        }
        // Device nodes and FIFOs: the store never unpacks them.
    }
    Ok(())
}

fn set_mtime(path: &Path, md: &std::fs::Metadata) -> Result<()> {
    let mtime =
        UNIX_EPOCH + StdDuration::new(md.mtime().max(0) as u64, md.mtime_nsec().max(0) as u32);
    File::open(path)
        .at(path)?
        .set_times(std::fs::FileTimes::new().set_modified(mtime))
        .at(path)
}

use std::os::unix::fs::PermissionsExt;

// --- the diff ---------------------------------------------------------------

/// One entry of the layer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Dir {
        mode: u32,
        mtime: i64,
    },
    File {
        mode: u32,
        mtime: i64,
        size: u64,
        source: PathBuf,
    },
    Symlink {
        target: PathBuf,
        mtime: i64,
    },
    /// `.wh.<name>`: the base's entry at this path is gone.
    Whiteout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Change {
    /// Relative to the root, no leading slash.
    path: PathBuf,
    kind: Kind,
}

impl Change {
    /// Whiteouts sort before an entry at the same path, so a path that changed
    /// kind reads as "deleted, then added" in the archive.
    fn rank(&self) -> u8 {
        u8::from(!matches!(self.kind, Kind::Whiteout))
    }
}

/// Everything in `result` that is not in `base`, plus a whiteout for
/// everything in `base` that is not in `result`. Sorted, so the layer's bytes
/// are a function of its contents.
fn diff(base: &Path, result: &Path) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    additions(base, result, Path::new(""), &mut changes)?;
    deletions(base, result, Path::new(""), &mut changes)?;
    changes.sort_by(|a, b| (&a.path, a.rank()).cmp(&(&b.path, b.rank())));
    Ok(changes)
}

/// Entries the diff never looks at: the store's bookkeeping, and the
/// top-level directories a sandbox mounts over.
fn skipped(rel: &Path, name: &OsStr) -> bool {
    is_internal_entry(name)
        || (rel.as_os_str().is_empty() && RUNTIME_DIRS.iter().any(|d| name == *d))
}

fn sorted_entries(dir: &Path) -> Result<Vec<std::fs::DirEntry>> {
    let mut entries = std::fs::read_dir(dir)
        .at(dir)?
        .collect::<std::io::Result<Vec<_>>>()
        .at(dir)?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    Ok(entries)
}

fn additions(base: &Path, result: &Path, rel: &Path, out: &mut Vec<Change>) -> Result<()> {
    for entry in sorted_entries(&result.join(rel))? {
        let name = entry.file_name();
        if skipped(rel, &name) {
            continue;
        }
        let child = rel.join(&name);
        let path = entry.path();
        let after = std::fs::symlink_metadata(&path).at(&path)?;
        let before = std::fs::symlink_metadata(base.join(&child)).ok();
        let kind = after.file_type();

        // A path that changed kind is deleted and re-added: the layer format
        // has no "replace", and flattening a directory over a file would fail.
        let kind_changed = before.as_ref().is_some_and(|b| {
            let b = b.file_type();
            b.is_dir() != kind.is_dir()
                || b.is_symlink() != kind.is_symlink()
                || b.is_file() != kind.is_file()
        });
        if kind_changed {
            out.push(Change {
                path: child.clone(),
                kind: Kind::Whiteout,
            });
        }

        if kind.is_dir() {
            if before.is_none() || kind_changed {
                out.push(Change {
                    path: child.clone(),
                    kind: Kind::Dir {
                        mode: after.mode() & 0o7777,
                        mtime: after.mtime(),
                    },
                });
            }
            additions(base, result, &child, out)?;
        } else if kind.is_symlink() {
            let target = std::fs::read_link(&path).at(&path)?;
            let same = !kind_changed
                && before.is_some()
                && std::fs::read_link(base.join(&child)).ok().as_deref() == Some(target.as_path());
            if !same {
                out.push(Change {
                    path: child,
                    kind: Kind::Symlink {
                        target,
                        mtime: after.mtime(),
                    },
                });
            }
        } else if kind.is_file() {
            let same = !kind_changed
                && before.as_ref().is_some_and(|b| {
                    b.len() == after.len()
                        && b.mtime() == after.mtime()
                        && b.mtime_nsec() == after.mtime_nsec()
                        && (b.mode() & 0o7777) == (after.mode() & 0o7777)
                });
            if !same {
                out.push(Change {
                    path: child,
                    kind: Kind::File {
                        mode: after.mode() & 0o7777,
                        mtime: after.mtime(),
                        size: after.len(),
                        source: path,
                    },
                });
            }
        }
    }
    Ok(())
}

fn deletions(base: &Path, result: &Path, rel: &Path, out: &mut Vec<Change>) -> Result<()> {
    for entry in sorted_entries(&base.join(rel))? {
        let name = entry.file_name();
        if skipped(rel, &name) {
            continue;
        }
        let child = rel.join(&name);
        let path = entry.path();
        let before = std::fs::symlink_metadata(&path).at(&path)?;
        let after_path = result.join(&child);
        match std::fs::symlink_metadata(&after_path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => out.push(Change {
                path: child,
                kind: Kind::Whiteout,
            }),
            Err(e) => return Err(e).at(&after_path),
            // Inside a directory that is still a directory. A kind change was
            // handled as a whiteout by `additions`.
            Ok(after) if before.is_dir() && after.is_dir() => {
                deletions(base, result, &child, out)?;
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// `.wh.<name>` beside where `<name>` was.
fn whiteout_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    path.with_file_name(format!(".wh.{name}"))
}

/// Counts and hashes what passes through it, so the layer's digest is known
/// without a second read.
struct Hashing<W: Write> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Write the difference between `base` and `result` to `out` as an OCI layer
/// tar, returning its digest and size.
///
/// Ownership is root throughout: on disk everything belongs to the user who
/// ran the build, and a layer that recorded that uid would be wrong on every
/// other machine.
pub(crate) fn write_layer(base: &Path, result: &Path, out: &Path) -> Result<(String, u64)> {
    let changes = diff(base, result)?;
    let file = File::create(out).at(out)?;
    let mut builder = tar::Builder::new(Hashing {
        inner: BufWriter::new(file),
        hasher: Sha256::new(),
        written: 0,
    });

    for change in &changes {
        let mut header = tar::Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        match &change.kind {
            Kind::Dir { mode, mtime } => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(*mode);
                header.set_mtime(*mtime as u64);
                header.set_size(0);
                builder
                    .append_data(&mut header, &change.path, std::io::empty())
                    .at(out)?;
            }
            Kind::File {
                mode,
                mtime,
                size,
                source,
            } => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(*mode);
                header.set_mtime(*mtime as u64);
                header.set_size(*size);
                let data = File::open(source).at(source)?;
                builder
                    .append_data(&mut header, &change.path, data)
                    .at(out)?;
            }
            Kind::Symlink { target, mtime } => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                header.set_mtime(*mtime as u64);
                header.set_size(0);
                builder
                    .append_link(&mut header, &change.path, target)
                    .at(out)?;
            }
            Kind::Whiteout => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0);
                header.set_mtime(0);
                header.set_size(0);
                builder
                    .append_data(&mut header, whiteout_path(&change.path), std::io::empty())
                    .at(out)?;
            }
        }
    }

    let mut hashing = builder.into_inner().at(out)?;
    hashing.flush().at(out)?;
    let digest = format!("sha256:{}", hex::encode(hashing.hasher.finalize()));
    Ok((digest, hashing.written))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_names_follow_debian_policy() {
        for ok in [
            "libpq5",
            "jq",
            "g++",
            "libssl3=3.0.14-1~deb12u2",
            "python3.12",
            "libssl3=3.5.*",
            "ca-certificates",
        ] {
            assert!(validate_package(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", "x", "Foo", "a;b", "-x", "a b", "=1", "jq=", "jq=1;2", "../x", "a=b=c",
        ] {
            assert!(validate_package(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_key_is_order_insensitive_and_follows_every_input() {
        let a = cache_key("sha256:aaa", &["jq".into(), "libpq5".into()], "aarch64");
        assert_eq!(
            a,
            cache_key(
                "sha256:aaa",
                &["libpq5".into(), "jq".into(), "jq".into()],
                "aarch64"
            )
        );
        assert_ne!(
            a,
            cache_key("sha256:bbb", &["jq".into(), "libpq5".into()], "aarch64")
        );
        assert_ne!(a, cache_key("sha256:aaa", &["jq".into()], "aarch64"));
        assert_ne!(
            a,
            cache_key("sha256:aaa", &["jq".into(), "libpq5".into()], "x86_64")
        );
        assert_ne!(
            a,
            cache_key("sha256:aaa", &["jq=1.6".into(), "libpq5".into()], "aarch64")
        );
        assert_eq!(
            derived_reference("python:3.12-slim", &a),
            format!("python:3.12-slim+system.{}", &a[..12])
        );
    }

    #[test]
    fn versions_come_from_the_dpkg_database() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let status = tmp.path().join("status");
        std::fs::write(
            &status,
            "Package: jq\nStatus: install ok installed\nVersion: 1.6-2.1\n\n\
             Package: gone\nStatus: deinstall ok config-files\nVersion: 9\n\n\
             Package: libpq5\nStatus: install ok installed\nArchitecture: arm64\nVersion: 15.8-0+deb12u1\n",
        )
        .expect("write");
        let got = installed_versions(
            &status,
            &[
                "jq".into(),
                "libpq5=15.8-0+deb12u1".into(),
                "gone".into(),
                "never".into(),
            ],
        );
        assert_eq!(
            got,
            ["jq=1.6-2.1", "libpq5=15.8-0+deb12u1", "gone", "never"]
        );
    }

    #[test]
    fn the_build_runs_as_root_with_host_networking_and_its_own_profile() {
        let spec = build_spec("python:3.12-slim", &["jq".into(), "libpq5".into()]);
        assert_eq!(spec.user, "0", "apt insists on root");
        assert_eq!(spec.network, Network::Host);
        assert_eq!(spec.seccomp, SeccompProfile::Permissive);
        assert!(
            spec.mounts.is_empty(),
            "nothing of the user's is in the box"
        );
        assert_eq!(spec.limits.mem, Bytes::from_mib(1024));
        // Packages are positional parameters after the script, never in it.
        assert_eq!(&spec.cmd[..2], ["sh", "-c"]);
        assert!(spec.cmd[2].contains("install -y"));
        assert!(!spec.cmd[2].contains("jq"));
        assert_eq!(&spec.cmd[3..], ["sh", "jq", "libpq5"]);
        // Both apt invocations keep root: the download methods would otherwise
        // switch to `_apt`, which a single-id map cannot represent.
        assert_eq!(
            spec.cmd[2].matches("-o APT::Sandbox::User=root").count(),
            2,
            "{}",
            spec.cmd[2]
        );
    }

    fn write(path: &Path, content: &str, mtime_s: i64) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, content).expect("write");
        let t = UNIX_EPOCH + StdDuration::from_secs(mtime_s as u64);
        File::open(path)
            .expect("open")
            .set_times(std::fs::FileTimes::new().set_modified(t))
            .expect("set mtime");
    }

    #[test]
    fn the_copy_keeps_modes_and_times_and_drops_bookkeeping() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let src = tmp.path().join("src");
        write(&src.join("bin/tool"), "#!/bin/sh\n", 1_600_000_000);
        std::fs::set_permissions(src.join("bin/tool"), std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        std::os::unix::fs::symlink("tool", src.join("bin/alias")).expect("symlink");
        std::fs::write(src.join(".zygo-complete"), "").expect("marker");

        let dst = tmp.path().join("dst");
        copy_preserving(&src, &dst).expect("copy");

        let md = std::fs::metadata(dst.join("bin/tool")).expect("stat");
        assert_eq!(md.mode() & 0o7777, 0o755);
        assert_eq!(md.mtime(), 1_600_000_000);
        assert_eq!(
            std::fs::read_link(dst.join("bin/alias")).expect("readlink"),
            Path::new("tool")
        );
        assert!(!dst.join(".zygo-complete").exists());
    }

    /// The base and the result of an "install" that touched a bit of everything.
    fn trees(tmp: &Path) -> (PathBuf, PathBuf) {
        let base = tmp.join("base");
        write(&base.join("etc/unchanged"), "same\n", 1_600_000_000);
        write(&base.join("etc/modified"), "old\n", 1_600_000_000);
        write(&base.join("etc/deleted"), "bye\n", 1_600_000_000);
        write(&base.join("usr/share/gone/inner"), "x\n", 1_600_000_000);
        write(&base.join("usr/share/kept/inner"), "x\n", 1_600_000_000);
        write(&base.join("var/becomes-dir"), "file\n", 1_600_000_000);
        std::os::unix::fs::symlink("unchanged", base.join("etc/link")).expect("symlink");
        std::os::unix::fs::symlink("old", base.join("etc/relinked")).expect("symlink");
        std::fs::create_dir_all(base.join("proc")).expect("proc");
        std::fs::write(base.join(".zygo-complete"), "").expect("marker");

        let result = tmp.join("result");
        copy_preserving(&base, &result).expect("copy");
        write(&result.join("etc/modified"), "new\n", 1_700_000_000);
        std::fs::remove_file(result.join("etc/deleted")).expect("rm");
        std::fs::remove_dir_all(result.join("usr/share/gone")).expect("rm -r");
        std::fs::remove_file(result.join("var/becomes-dir")).expect("rm");
        write(&result.join("var/becomes-dir/child"), "c\n", 1_700_000_000);
        write(&result.join("usr/bin/jq"), "ELF\n", 1_700_000_000);
        std::fs::set_permissions(
            result.join("usr/bin/jq"),
            std::fs::Permissions::from_mode(0o755),
        )
        .expect("chmod");
        std::fs::remove_file(result.join("etc/relinked")).expect("rm");
        std::os::unix::fs::symlink("new", result.join("etc/relinked")).expect("symlink");
        std::os::unix::fs::symlink("jq", result.join("usr/bin/jq-alias")).expect("symlink");
        // Runtime state that a sandbox's mounts would have hidden.
        write(&result.join("proc/1/status"), "nope", 1_700_000_000);
        write(&result.join("tmp/scratch"), "nope", 1_700_000_000);
        (base, result)
    }

    #[test]
    fn the_diff_is_exactly_what_the_install_changed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (base, result) = trees(tmp.path());
        let changes = diff(&base, &result).expect("diff");
        let listed: Vec<(String, &'static str)> = changes
            .iter()
            .map(|c| {
                let kind = match c.kind {
                    Kind::Dir { .. } => "dir",
                    Kind::File { .. } => "file",
                    Kind::Symlink { .. } => "symlink",
                    Kind::Whiteout => "whiteout",
                };
                (c.path.to_string_lossy().into_owned(), kind)
            })
            .collect();
        assert_eq!(
            listed,
            [
                ("etc/deleted".to_string(), "whiteout"),
                ("etc/modified".to_string(), "file"),
                ("etc/relinked".to_string(), "symlink"),
                ("usr/bin".to_string(), "dir"),
                ("usr/bin/jq".to_string(), "file"),
                ("usr/bin/jq-alias".to_string(), "symlink"),
                ("usr/share/gone".to_string(), "whiteout"),
                ("var/becomes-dir".to_string(), "whiteout"),
                ("var/becomes-dir".to_string(), "dir"),
                ("var/becomes-dir/child".to_string(), "file"),
            ]
        );
    }

    #[test]
    fn the_layer_applied_over_the_base_reproduces_the_result() {
        // The real check: through the store's own unpack and flatten, the way
        // a sandbox will see it.
        let tmp = tempfile::tempdir().expect("tempdir");
        let (base, result) = trees(tmp.path());

        let paths = crate::Paths::rooted(tmp.path().join("store"));
        paths.ensure().expect("ensure");
        let store = Store::new(paths);

        // The base as a layer, so flatten has something to apply over.
        let base_tar = tmp.path().join("base.tar");
        {
            let mut b = tar::Builder::new(File::create(&base_tar).expect("create"));
            b.follow_symlinks(false);
            b.append_dir_all(".", &base).expect("append");
            b.finish().expect("finish");
        }
        let base_bytes = std::fs::read(&base_tar).expect("read");
        let base_digest = format!("sha256:{}", hex::encode(Sha256::digest(&base_bytes)));
        store
            .write_blob(&base_digest, &base_bytes[..])
            .expect("blob");
        store
            .unpack_layer(&base_digest, LayerCompression::None)
            .expect("unpack");

        let layer_tar = tmp.path().join("layer.tar");
        let (digest, size) = write_layer(&base, &result, &layer_tar).expect("layer");
        assert_eq!(size, std::fs::metadata(&layer_tar).expect("stat").len());
        store
            .write_blob(&digest, File::open(&layer_tar).expect("open"))
            .expect("blob verifies against the digest we computed");
        store
            .unpack_layer(&digest, LayerCompression::None)
            .expect("unpack");

        let w = store.whiteouts(&digest);
        assert_eq!(
            w.removed,
            [
                PathBuf::from("etc/deleted"),
                PathBuf::from("usr/share/gone"),
                PathBuf::from("var/becomes-dir")
            ]
        );

        let flat = store.flatten(&[base_digest, digest]).expect("flatten");
        let read = |p: &str| std::fs::read_to_string(flat.join(p)).ok();
        assert_eq!(read("etc/unchanged").as_deref(), Some("same\n"));
        assert_eq!(read("etc/modified").as_deref(), Some("new\n"));
        assert_eq!(read("etc/deleted"), None);
        assert!(!flat.join("usr/share/gone").exists());
        assert_eq!(read("usr/share/kept/inner").as_deref(), Some("x\n"));
        assert_eq!(read("usr/bin/jq").as_deref(), Some("ELF\n"));
        assert_eq!(
            std::fs::metadata(flat.join("usr/bin/jq"))
                .expect("stat")
                .mode()
                & 0o7777,
            0o755
        );
        assert_eq!(read("var/becomes-dir/child").as_deref(), Some("c\n"));
        assert_eq!(
            std::fs::read_link(flat.join("etc/relinked")).expect("readlink"),
            Path::new("new")
        );
        assert_eq!(
            std::fs::read_link(flat.join("usr/bin/jq-alias")).expect("readlink"),
            Path::new("jq")
        );
        assert!(
            !flat.join("proc/1").exists(),
            "runtime directories are not in the layer"
        );
        assert!(!flat.join("tmp/scratch").exists());
    }

    #[test]
    fn an_empty_package_list_is_the_base_image() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = crate::Paths::rooted(tmp.path());
        paths.ensure().expect("ensure");
        let store = Store::new(paths);
        let base = ImageEntry {
            reference: "python:3.12-slim".into(),
            manifest: "sha256:abc".into(),
            config: "sha256:cfg".into(),
            layers: vec![],
            pulled_at: 0,
            size: 0,
            index: None,
            platform: None,
        };
        let d = ensure(&store, &base, &[]).expect("nothing to do");
        assert_eq!(d.image, base);
        assert!(!d.built);
    }

    #[test]
    fn a_bad_package_name_is_a_spec_error_before_any_copy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = Store::new(crate::Paths::rooted(tmp.path()));
        let base = ImageEntry {
            reference: "x".into(),
            manifest: "sha256:abc".into(),
            config: "sha256:cfg".into(),
            layers: vec![],
            pulled_at: 0,
            size: 0,
            index: None,
            platform: None,
        };
        let err = ensure(&store, &base, &["jq; rm -rf /".into()]).expect_err("rejected");
        assert!(matches!(err, Error::Spec(_)), "{err}");
    }

    /// The record is only ever read to answer which versions `apt` chose, so
    /// it is dead the moment its derived image leaves the index.
    #[test]
    fn a_system_record_is_collected_once_its_derived_image_is_gone() {
        let tmp = tempfile::tempdir().expect("tmp");
        let paths = crate::paths::Paths::rooted(tmp.path());
        paths.ensure().expect("ensure");
        let store = Store::new(paths.clone());

        let base = "debian:12";
        let live_key = cache_key("sha256:base", &["jq".into()], std::env::consts::ARCH);
        let dead_key = cache_key("sha256:base", &["curl".into()], std::env::consts::ARCH);

        for key in [&live_key, &dead_key] {
            let dir = paths.system_cache().join(key);
            std::fs::create_dir_all(&dir).expect("mkdir");
            std::fs::write(dir.join(RECORD), "# debian:12 sha256:base\njq=1.6\n").expect("write");
        }

        // Only one of the two derived images is in the index.
        store
            .put(ImageEntry {
                reference: derived_reference(base, &live_key),
                manifest: "sha256:derived".into(),
                config: "sha256:cfg".into(),
                layers: vec![],
                size: 0,
                pulled_at: 0,
                index: None,
                platform: None,
            })
            .expect("put");

        assert_eq!(
            unreferenced(&store).expect("scan"),
            [paths.system_cache().join(&dead_key)]
        );
    }
}
