//! The dependency cache (design doc §3.7, todo 2.7).
//!
//! `requirements.txt` → `cache/venvs/<hash>/`, built once and bound read-only
//! at `/venv` in every sandbox that lists the same file against the same image.
//!
//! Built **inside a sandbox**, with the image's own `pip`. The alternative —
//! running `pip` on the host — would build for the host's Python, and a wheel
//! compiled against the wrong interpreter or the wrong libc fails at import
//! time in the sandbox with a message that points nowhere useful. Building
//! where the code will run is also what makes the cache key honest: the same
//! requirements against a different image are a different venv.
//!
//! The design says "an embedded `uv`". That was not done: `uv` is ~30 MB, and
//! requirement N6 caps the whole binary at 15 MB. The image's `pip` is slower
//! and already there.
//!
//! The build sandbox is given **host networking**. This is the one place Zygo
//! grants it without `--allow-host-net`, and it is justified narrowly: the
//! operation is installing the user's own requirements file, once, before any
//! tenant code exists to abuse it. It never applies to the warm sandbox, which
//! keeps the spec's `network`.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Error, IoContext, Result};
use crate::image::{ImageEntry, Store};
use crate::sandbox::oneshot::{run_captured, tail};
use crate::sandbox::{DEFAULT_PATH, SandboxConfig};
use crate::spec::{Bytes, Cpu, Duration, Mount, MountMode, Network, ResolvedFn};

/// Where the venv is mounted inside the sandbox.
pub const VENV_IN_SANDBOX: &str = "/venv";

/// Where the requirements file is mounted for the build.
const REQUIREMENTS_IN_SANDBOX: &str = "/req.txt";

/// Written into a finished venv; its absence means "half built, rebuild".
const DONE_MARKER: &str = ".zygo-venv-done";

/// A built venv, ready to be mounted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Venv {
    /// Host directory holding the venv.
    pub dir: PathBuf,
    /// Whether this call built it or found it.
    pub built: bool,
}

impl Venv {
    /// The mount that puts this venv into a sandbox.
    pub fn mount(&self) -> Mount {
        Mount {
            source: self.dir.clone(),
            target: PathBuf::from(VENV_IN_SANDBOX),
            mode: MountMode::Ro,
        }
    }

    /// Environment that makes the venv the interpreter's default.
    ///
    /// `/venv/bin` first on `PATH` is what does it: `python3` then resolves to
    /// the venv's, which finds `pyvenv.cfg` beside itself and uses the venv's
    /// site-packages. `VIRTUAL_ENV` is a courtesy for tools that read it.
    pub fn env() -> Vec<(String, String)> {
        Venv::env_over(DEFAULT_PATH)
    }

    /// The same, in front of a `PATH` that came from somewhere else.
    ///
    /// `zygo run` knows the image's own `PATH` — it reads the image config to
    /// resolve a bare `python3` — and discarding it would break every other
    /// program the image ships. The warm path has no image config to hand and
    /// uses the built-in default.
    pub fn env_over(base_path: &str) -> Vec<(String, String)> {
        let base = if base_path.is_empty() {
            DEFAULT_PATH
        } else {
            base_path
        };
        vec![
            ("VIRTUAL_ENV".to_string(), VENV_IN_SANDBOX.to_string()),
            ("PATH".to_string(), venv_path(base)),
        ]
    }
}

/// `/venv/bin` in front of `base`.
pub fn venv_path(base: &str) -> String {
    format!("{VENV_IN_SANDBOX}/bin:{base}")
}

/// The cache key: this image, these requirements.
///
/// The manifest digest rather than the reference: `python:3.12-slim` moves,
/// and a venv built against last month's image is not the venv for today's.
/// The file's bytes rather than its path, so two projects with identical
/// requirements share, and an edit invalidates.
pub fn cache_key(image_manifest: &str, requirements: &[u8]) -> String {
    let mut key = Sha256::new();
    key.update(image_manifest.as_bytes());
    key.update(b"\n");
    key.update(requirements);
    hex::encode(key.finalize())
}

/// Get the venv for `requirements` against `image`, building it if needed.
///
/// Serialised per key with the store's lock, so two functions that list the
/// same requirements and start together build once. The lock is taken only
/// when the marker is missing; the common case is a single `stat`.
pub fn ensure(store: &Store, image: &ImageEntry, requirements: &Path) -> Result<Venv> {
    let bytes = std::fs::read(requirements).at(requirements)?;
    let key = cache_key(&image.manifest, &bytes);
    let dir = store.paths().venv_cache().join(&key);

    if dir.join(DONE_MARKER).is_file() {
        // The only record of use this venv has; `prune --unused-for` reads it.
        crate::image::store::touch(&dir.join(DONE_MARKER));
        return Ok(Venv { dir, built: false });
    }

    let _lock = store.lock(&format!("venv-{key}"))?;
    if dir.join(DONE_MARKER).is_file() {
        return Ok(Venv { dir, built: false });
    }

    // A directory without a marker is a build that did not finish. Start over
    // rather than trust it: `pip` resumes nothing.
    if dir.exists() {
        std::fs::remove_dir_all(&dir).at(&dir)?;
    }
    std::fs::create_dir_all(&dir).at(&dir)?;

    build(store, image, requirements, &dir)?;

    let marker = dir.join(DONE_MARKER);
    std::fs::write(
        &marker,
        format!(
            "image {}\nrequirements sha256 {}\n",
            image.manifest,
            hex::encode(Sha256::digest(&bytes))
        ),
    )
    .at(&marker)?;
    Ok(Venv { dir, built: true })
}

/// Cached venvs built against an image that is no longer in the store.
///
/// The cache key is a hash of the image manifest and the requirements file,
/// so the directory name says nothing; the done marker already records the
/// manifest, which is what makes this answerable. A venv with no marker is a
/// build that did not finish and is never usable, so it goes too.
///
/// Nothing here consults the specs that asked for these venvs. A venv is
/// rebuilt on demand, and one belonging to a function that is merely not
/// running right now is still keyed on an image that is still present, so it
/// survives.
pub fn unreferenced(store: &Store) -> Result<Vec<PathBuf>> {
    let live: std::collections::BTreeSet<String> =
        store.list().into_iter().map(|e| e.manifest).collect();

    let dir = store.paths().venv_cache();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).at(&dir)? {
        let path = entry.at(&dir)?.path();
        if !path.is_dir() {
            continue;
        }
        let marker = std::fs::read_to_string(path.join(DONE_MARKER)).unwrap_or_default();
        let built_against = marker
            .lines()
            .find_map(|l| l.strip_prefix("image "))
            .map(str::trim);
        match built_against {
            Some(manifest) if live.contains(manifest) => {}
            _ => out.push(path),
        }
    }
    out.sort();
    Ok(out)
}

/// Venvs whose image is still here but that no request has used since
/// `cutoff`.
///
/// This is the only way a venv retires while its image stays: editing one
/// pin in a requirements file builds a new venv under a new key and says
/// nothing about the old one, because nothing can — no spec is consulted
/// and none has to be. Use is the marker's modification time, refreshed on
/// every hit by [`ensure`].
pub fn unused_since(store: &Store, cutoff: std::time::SystemTime) -> Result<Vec<PathBuf>> {
    let stale: std::collections::BTreeSet<PathBuf> = unreferenced(store)?.into_iter().collect();
    let dir = store.paths().venv_cache();
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).at(&dir)? {
        let path = entry.at(&dir)?.path();
        if !path.is_dir() || stale.contains(&path) {
            continue;
        }
        if crate::image::store::last_used(&path.join(DONE_MARKER)).is_some_and(|t| t < cutoff) {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// Run `python3 -m venv` and `pip install` inside a one-shot sandbox.
fn build(store: &Store, image: &ImageEntry, requirements: &Path, dir: &Path) -> Result<()> {
    let spec = build_spec(&image.reference, requirements, dir);

    let mount_points = crate::sandbox::mount::required_mount_points(&spec.mounts);
    let overlay = crate::doctor::cached(store.paths())
        .checks
        .iter()
        .any(|c| c.name == "overlayfs (userns)" && c.status == crate::doctor::Status::Ok);
    // The build runs on the image with its bytecode, so `pip` itself starts
    // from compiled code rather than recompiling a few hundred of its own
    // modules. The venv stays keyed on `image`, the one it is served with.
    let with_bytecode = crate::bytecode::ensure(store, image)?.image;
    let view = store.rootfs_view(&with_bytecode.layers, overlay, &mount_points)?;

    let newroot = store
        .paths()
        .tmp()
        .join(format!("venv-build-{}", crate::process_token()));
    std::fs::create_dir_all(&newroot).at(&newroot)?;

    let mut config =
        SandboxConfig::from_resolved(&spec, &view, &newroot, build_argv(), &build_env());

    tracing::info!(
        image = %image.reference,
        requirements = %requirements.display(),
        "building a venv inside the image (host networking, once)"
    );
    let started = std::time::Instant::now();
    let outcome = run_captured(&mut config, store.paths());
    let _ = std::fs::remove_dir_all(&newroot);

    match outcome {
        Ok((0, _)) => {
            tracing::info!(elapsed_s = started.elapsed().as_secs(), "venv built");
            Ok(())
        }
        Ok((code, output)) => Err(Error::Build {
            what: "the venv build",
            reason: format!(
                "pip exited {code} building {}:\n{}",
                requirements.display(),
                tail(&output, 40)
            ),
            remedy: "fix the requirements file; the build runs with the image's own pip and host networking"
                .into(),
        }),
        Err(e) => Err(Error::BackendUnavailable {
            backend: "venv",
            reason: format!("the venv build sandbox failed: {e}"),
            remedy: "run `zygo doctor`; the build needs the same primitives as any sandbox".into(),
        }),
    }
}

/// The one-shot function that performs the build.
///
/// Derived from nothing but the image: the requirements file's own function is
/// deliberately not the template, because its limits are sized for a handler
/// and its `network` is sized for tenant code. A `pip install` is neither.
fn build_spec(image: &str, requirements: &Path, dir: &Path) -> ResolvedFn {
    use crate::spec::{Layer, ResolveOptions, resolve_standalone};

    let mut spec = resolve_standalone(
        "venv-build",
        &Layer {
            image: Some(image.to_string()),
            cmd: Some(build_argv()),
            // `pip` resolving and compiling wheels is not a 256 MB job.
            mem: Some(Bytes::from_mib(1024)),
            cpu: Some(Cpu(2.0)),
            pids: Some(256),
            timeout: Some(Duration::from_secs(600)),
            scratch: Some(Bytes::from_mib(512)),
            ..Default::default()
        },
        &ResolveOptions {
            one_shot: true,
            ..Default::default()
        },
    )
    .expect("the build layer is fixed and valid");

    spec.mounts = vec![
        Mount {
            source: dir.to_path_buf(),
            target: PathBuf::from(VENV_IN_SANDBOX),
            mode: MountMode::Rw,
        },
        Mount {
            source: requirements.to_path_buf(),
            target: PathBuf::from(REQUIREMENTS_IN_SANDBOX),
            mode: MountMode::Ro,
        },
    ];
    // Set after resolution on purpose, bypassing `--allow-host-net`: see the
    // module documentation for why this one build is allowed what tenant code
    // is not.
    spec.network = Network::Host;
    spec
}

fn build_argv() -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        // Three steps rather than one, so a failure says which. `python3 -m
        // venv` runs `ensurepip` as a subprocess and swallows its output,
        // reporting only `Command '[...]' returned non-zero exit status 1` —
        // which is what a CI runner said, and it names nothing anyone can act
        // on. Splitting it puts `ensurepip`'s own stderr in the sandbox's
        // output, where the error message already carries it.
        //
        // The image's own `pip` installs into the venv when it can (`--python`,
        // pip 22.3+). `ensurepip` put a second pip into every venv first, and
        // that alone was 2.0 s of every build — measured, before a single
        // package — for a tool the venv never uses once it is built.
        format!(
            "python3 -m venv --without-pip {VENV_IN_SANDBOX} && \
             if python3 -m pip --python {VENV_IN_SANDBOX}/bin/python3 --version >/dev/null 2>&1; then \
               python3 -m pip --python {VENV_IN_SANDBOX}/bin/python3 install --no-cache-dir \
               --disable-pip-version-check --timeout 60 --retries 5 -r {REQUIREMENTS_IN_SANDBOX}; \
             else \
               {VENV_IN_SANDBOX}/bin/python3 -m ensurepip --upgrade --default-pip && \
               {VENV_IN_SANDBOX}/bin/pip install --no-cache-dir --disable-pip-version-check \
               --timeout 60 --retries 5 -r {REQUIREMENTS_IN_SANDBOX}; \
             fi"
        ),
    ]
}

fn build_env() -> Vec<(String, String)> {
    vec![
        // `pip` wants somewhere writable for its own state; `/tmp` is the
        // sandbox's scratch and the only writable place besides the venv.
        ("HOME".to_string(), "/tmp".to_string()),
        ("PATH".to_string(), DEFAULT_PATH.to_string()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_changes_with_the_image_and_with_the_requirements() {
        let a = cache_key("sha256:aaa", b"requests==2.32\n");
        let same = cache_key("sha256:aaa", b"requests==2.32\n");
        let other_image = cache_key("sha256:bbb", b"requests==2.32\n");
        let other_reqs = cache_key("sha256:aaa", b"requests==2.31\n");
        assert_eq!(a, same, "identical inputs share a venv");
        assert_ne!(
            a, other_image,
            "the same requirements against another image are another venv"
        );
        assert_ne!(a, other_reqs, "an edit to the file invalidates");
    }

    #[test]
    fn the_venv_goes_first_on_path_and_the_rest_is_the_default() {
        let path = venv_path(DEFAULT_PATH);
        assert!(path.starts_with("/venv/bin:"), "{path}");
        assert!(path.ends_with(DEFAULT_PATH), "{path}");
        let env = Venv::env();
        assert!(env.iter().any(|(k, v)| k == "VIRTUAL_ENV" && v == "/venv"));
        assert!(
            env.iter()
                .any(|(k, v)| k == "PATH" && v.starts_with("/venv/bin:"))
        );
    }

    #[test]
    fn the_venv_is_mounted_read_only() {
        // A tenant must not be able to modify a venv that other tenants share.
        let venv = Venv {
            dir: PathBuf::from("/cache/venvs/abc"),
            built: false,
        };
        let m = venv.mount();
        assert_eq!(m.target, PathBuf::from("/venv"));
        assert_eq!(m.mode, MountMode::Ro);
    }

    #[test]
    fn the_build_gets_host_networking_and_its_own_limits() {
        // Two things the requirements file's own function must not inherit:
        // the tenant's `network = "none"` (pip could not fetch) and its
        // handler-sized limits (pip would be OOM-killed compiling a wheel).
        let spec = build_spec(
            "python:3.12-slim",
            Path::new("/p/requirements.txt"),
            Path::new("/c/venv"),
        );
        assert_eq!(spec.network, Network::Host);
        assert_eq!(spec.limits.mem, Bytes::from_mib(1024));
        assert!(spec.limits.timeout.get() >= std::time::Duration::from_secs(600));
        assert_eq!(spec.cmd[0], "sh");
        // Each step on its own, so a failure names the one that failed.
        assert!(spec.cmd[2].contains("python3 -m venv --without-pip /venv"));
        assert!(spec.cmd[2].contains("/venv/bin/python3 -m ensurepip"));
        assert!(spec.cmd[2].contains("pip install"));
    }

    #[test]
    fn the_build_mounts_the_venv_writable_and_the_requirements_read_only() {
        let spec = build_spec(
            "python:3.12-slim",
            Path::new("/p/requirements.txt"),
            Path::new("/c/venv"),
        );
        let venv = spec
            .mounts
            .iter()
            .find(|m| m.target == Path::new("/venv"))
            .expect("venv mount");
        assert_eq!(venv.mode, MountMode::Rw, "the build has to write it");
        assert_eq!(venv.source, Path::new("/c/venv"));
        let req = spec
            .mounts
            .iter()
            .find(|m| m.target == Path::new("/req.txt"))
            .expect("req mount");
        assert_eq!(req.mode, MountMode::Ro);
    }

    #[test]
    fn a_finished_venv_is_found_without_taking_the_lock() {
        // The marker is the whole contract: present means built and complete.
        let tmp = tempfile::tempdir().expect("tempdir");
        let paths = crate::Paths::rooted(tmp.path());
        paths.ensure().expect("ensure");
        let store = Store::new(paths.clone());
        let reqs = tmp.path().join("requirements.txt");
        std::fs::write(&reqs, "six\n").expect("write");
        let image = ImageEntry {
            reference: "python:3.12-slim".into(),
            manifest: "sha256:abc".into(),
            config: "sha256:cfg".into(),
            layers: vec![],
            pulled_at: 0,
            size: 0,
            index: None,
            platform: None,
        };
        let dir = paths.venv_cache().join(cache_key("sha256:abc", b"six\n"));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(DONE_MARKER), "").expect("marker");

        let venv = ensure(&store, &image, &reqs).expect("found");
        assert_eq!(venv.dir, dir);
        assert!(!venv.built, "nothing was built");
    }

    /// Editing one pin builds a venv under a new key and says nothing about
    /// the old one — nothing can, because no spec is consulted. Age is the
    /// only thing that retires it while the image stays.
    #[test]
    fn a_venv_nothing_has_used_is_collectable_by_age() {
        let tmp = tempfile::tempdir().expect("tmp");
        let paths = crate::paths::Paths::rooted(tmp.path());
        paths.ensure().expect("ensure");
        let store = Store::new(paths.clone());

        let image = ImageEntry {
            reference: "python:3.12-slim".into(),
            manifest: "sha256:abc".into(),
            config: "sha256:cfg".into(),
            layers: vec![],
            pulled_at: 0,
            size: 0,
            index: None,
            platform: None,
        };
        store.put(image.clone()).expect("put");

        let reqs = tmp.path().join("requirements.txt");
        std::fs::write(&reqs, "six==1.16.0\n").expect("write");
        let old = paths
            .venv_cache()
            .join(cache_key("sha256:abc", b"six==1.16.0\n"));
        std::fs::create_dir_all(&old).expect("mkdir");
        std::fs::write(old.join(DONE_MARKER), "image sha256:abc\n").expect("marker");

        let long_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(60 * 86_400);
        std::fs::File::options()
            .write(true)
            .open(old.join(DONE_MARKER))
            .expect("open")
            .set_modified(long_ago)
            .expect("backdate");

        assert!(
            unreferenced(&store).expect("scan").is_empty(),
            "its image is still here, so liveness will never collect it"
        );
        let week = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 86_400);
        assert_eq!(
            unused_since(&store, week).expect("age"),
            std::slice::from_ref(&old)
        );

        // A request that uses it puts it back out of reach.
        let found = ensure(&store, &image, &reqs).expect("hit");
        assert_eq!(found.dir, old);
        assert!(!found.built, "found, not rebuilt");
        assert!(
            unused_since(&store, week).expect("age").is_empty(),
            "the hit path records the use"
        );
    }

    /// A venv outlives the image it was built against unless something
    /// collects it, and it is larger than the layers that image is made of.
    #[test]
    fn a_venv_is_collected_once_its_image_leaves_the_store() {
        let tmp = tempfile::tempdir().expect("tmp");
        let paths = crate::paths::Paths::rooted(tmp.path());
        paths.ensure().expect("ensure");
        let store = Store::new(paths.clone());

        let image = ImageEntry {
            reference: "python:3.12-slim".into(),
            manifest: "sha256:abc".into(),
            config: "sha256:cfg".into(),
            layers: vec![],
            pulled_at: 0,
            size: 0,
            index: None,
            platform: None,
        };
        store.put(image.clone()).expect("put");

        let mine = paths.venv_cache().join(cache_key("sha256:abc", b"six\n"));
        std::fs::create_dir_all(&mine).expect("mkdir");
        std::fs::write(mine.join(DONE_MARKER), "image sha256:abc\n").expect("marker");

        // Built against an image that is not in the index any more.
        let stale = paths.venv_cache().join(cache_key("sha256:old", b"six\n"));
        std::fs::create_dir_all(&stale).expect("mkdir");
        std::fs::write(stale.join(DONE_MARKER), "image sha256:old\n").expect("marker");

        // Half built: no marker, never usable.
        let partial = paths.venv_cache().join("partial");
        std::fs::create_dir_all(&partial).expect("mkdir");

        let mut found = unreferenced(&store).expect("scan");
        found.sort();
        let mut want = vec![partial, stale];
        want.sort();
        assert_eq!(found, want, "the live venv stays");
    }
}
