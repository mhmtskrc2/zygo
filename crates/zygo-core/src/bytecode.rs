// SPDX-License-Identifier: Apache-2.0
//! Bytecode for Python images, compiled once, as a layer of its own.
//!
//! The official `python:*-slim` images delete every `.pyc` to save space —
//! `python:3.12-slim` has 1097 `.py` files in its standard library and not one
//! compiled file. A sandbox's root is read-only, so Python cannot cache what it
//! compiles either, and **every run recompiles every module it imports**. That
//! is the whole of a small program's start-up: measured with an embedder's
//! script harness, which imports `re`, `json`, `hmac`, `urllib.request` and a
//! few others, the run took 171 ms without bytecode and 67 ms with it, and a
//! bare `python -c pass` 14 ms either way. `import re` alone is 34 ms from
//! source.
//!
//! So the first time an image is used — or pulled — its standard library is
//! compiled inside a sandbox, and what that wrote becomes one more layer: the
//! `__pycache__` directories beside the sources, exactly where Python looks.
//! The image is then served as `<reference>+bytecode.<key>`, which goes with
//! its base on `zygo image rm` like a derived system image does.
//!
//! **`unchecked-hash`**, not the default timestamp check. A layer never
//! changes, so there is nothing to check a `.pyc` against, and a timestamp
//! check would depend on every copy of the image keeping the sources' mtimes
//! to the second. An unchecked `.pyc` is used as is.
//!
//! **Beside the sources, not `PYTHONPYCACHEPREFIX`.** A prefix would have
//! worked for the standard library and broken everything else: under one,
//! Python looks *only* in the prefix, so the `__pycache__` directories `pip`
//! writes next to installed packages — a venv's, a project's — would be
//! ignored and recompiled on every run instead.
//!
//! An optimisation, so it never fails a run: an image with no Python, or with
//! bytecode already, is returned as it is, and a build that goes wrong is a
//! warning and the original image. `ZYGO_BYTECODE=0` turns it off.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::derive::{RUNTIME_DIRS, copy_preserving, find, write_layer};
use crate::error::{Error, IoContext, Result};
use crate::image::{ImageEntry, LayerCompression, Store};
use crate::sandbox::oneshot::{run_captured, tail};
use crate::sandbox::{DEFAULT_PATH, RootfsView, SandboxConfig};
use crate::spec::{Bytes, Cpu, Duration, Network, ResolvedFn};

/// Bumped when what the build does changes, so an older layer is not reused.
///
/// v2: v1 left the modules `compileall` itself imports — `re`, `enum`,
/// `functools`, `encodings` and thirty more — with the *timestamp* `.pyc` the
/// interpreter cached while starting, which `compileall` then took for up to
/// date. Their timestamps did not survive into the layer, so those modules were
/// recompiled on every run anyway: 108 ms for an embedder's script harness
/// where 67 was measured with every module compiled.
const VERSION: &str = "bytecode-v2";

/// Where Python images keep their standard library.
const LIB_DIRS: &[&str] = &["usr/local/lib", "usr/lib"];

/// The image to serve, and whether this call compiled it.
#[derive(Debug, Clone)]
pub struct Bytecode {
    pub image: ImageEntry,
    pub built: bool,
}

/// One interpreter's standard library in the image: `/usr/local/lib/python3.12`
/// and the tag its `.pyc` files carry, `cpython-312`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stdlib {
    pub dir: String,
    pub interpreter: String,
    pub tag: String,
}

/// How long a failed build is left alone before a run tries it again.
const RETRY_FAILED_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// The image with its Python bytecode compiled, building the layer if needed.
///
/// A build that failed is not tried again by every run for an hour: it copies
/// the image's whole root before it can fail, and a run that paid for that
/// each time — measured at 170–200 ms of every `zygo run` in a container
/// whose disk had filled — was slower than having no bytecode at all.
/// `zygo pull` tries again regardless ([`ensure_now`]).
pub fn ensure(store: &Store, base: &ImageEntry) -> Result<Bytecode> {
    ensure_with(store, base, false)
}

/// [`ensure`], trying a build that failed recently too — what `zygo pull`
/// does, because pulling is when an operator expects the work to be done.
pub fn ensure_now(store: &Store, base: &ImageEntry) -> Result<Bytecode> {
    ensure_with(store, base, true)
}

fn failed_marker(store: &Store, key: &str) -> std::path::PathBuf {
    store.paths().data().join("cache/bytecode-failed").join(key)
}

fn ensure_with(store: &Store, base: &ImageEntry, retry_failed: bool) -> Result<Bytecode> {
    let unchanged = || {
        Ok(Bytecode {
            image: base.clone(),
            built: false,
        })
    };
    if std::env::var_os("ZYGO_BYTECODE").is_some_and(|v| v == "0") {
        return unchanged();
    }
    let missing = stdlibs_without_bytecode(store, &base.layers);
    if missing.is_empty() {
        return unchanged();
    }

    let key = cache_key(&base.manifest, std::env::consts::ARCH);
    let reference = format!("{}+bytecode.{}", base.reference, &key[..12]);
    if let Some(image) = find(store, &reference) {
        return Ok(Bytecode {
            image,
            built: false,
        });
    }
    let marker = failed_marker(store, &key);
    if !retry_failed
        && std::fs::metadata(&marker)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < RETRY_FAILED_AFTER))
    {
        return unchanged();
    }
    let _lock = store.lock(&format!("bytecode-{key}"))?;
    if let Some(image) = find(store, &reference) {
        return Ok(Bytecode {
            image,
            built: false,
        });
    }

    match build(store, base, &missing, &key, reference) {
        Ok(image) => {
            let _ = std::fs::remove_file(&marker);
            Ok(Bytecode { image, built: true })
        }
        Err(e) => {
            if let Some(dir) = marker.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&marker, e.to_string());
            tracing::warn!(
                image = %base.reference,
                "could not compile the image's Python bytecode, so every run compiles what \
                 it imports: {e}"
            );
            unchanged()
        }
    }
}

/// The cache key: the base image, the build's own version, the architecture.
pub fn cache_key(base_manifest: &str, arch: &str) -> String {
    let mut key = Sha256::new();
    for part in [base_manifest, VERSION, arch] {
        key.update(part.as_bytes());
        key.update(b"\n");
    }
    hex::encode(key.finalize())
}

/// Every Python standard library in the image that has no bytecode.
///
/// Read from the unpacked layers rather than from a flattened root, so the
/// common answer — "already done" or "no Python here" — costs a few
/// directory reads and no copy. A library counts as compiled when any layer
/// holds its `os` module's `.pyc`: Debian's own `python3` packages compile
/// theirs at install time, and those images are left alone.
pub fn stdlibs_without_bytecode(store: &Store, layers: &[String]) -> Vec<Stdlib> {
    let dirs: Vec<std::path::PathBuf> = layers
        .iter()
        .filter_map(|d| store.layer_dir(d).ok())
        .collect();
    let mut found: Vec<Stdlib> = Vec::new();
    for layer in &dirs {
        for lib in LIB_DIRS {
            let Ok(entries) = std::fs::read_dir(layer.join(lib)) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some(stdlib) = stdlib_named(lib, &name) else {
                    continue;
                };
                if entry.path().join("os.py").is_file() && !found.contains(&stdlib) {
                    found.push(stdlib);
                }
            }
        }
    }
    found.retain(|s| {
        let pyc = Path::new(s.dir.trim_start_matches('/'))
            .join("__pycache__")
            .join(format!("os.{}.pyc", s.tag));
        !dirs.iter().any(|layer| layer.join(&pyc).is_file())
    });
    found
}

/// `python3.12` under `usr/local/lib` → the library, its interpreter and its
/// tag. Anything else is not a standard library.
fn stdlib_named(lib: &str, name: &str) -> Option<Stdlib> {
    let minor = name.strip_prefix("python3.")?;
    if minor.is_empty() || !minor.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(Stdlib {
        dir: format!("/{lib}/{name}"),
        interpreter: name.to_string(),
        tag: format!("cpython-3{minor}"),
    })
}

/// Copy the image, compile inside the copy, turn the difference into a layer.
fn build(
    store: &Store,
    base: &ImageEntry,
    stdlibs: &[Stdlib],
    key: &str,
    reference: String,
) -> Result<ImageEntry> {
    let started = Instant::now();
    let flat = store.flatten(&base.layers)?;
    let work = store.paths().tmp().join(format!(
        "bytecode-{}-{}",
        &key[..12],
        crate::process_token()
    ));
    let _ = std::fs::remove_dir_all(&work);
    let result = (|| -> Result<(String, u64)> {
        let root = work.join("root");
        copy_preserving(&flat, &root)?;
        for d in RUNTIME_DIRS {
            let p = root.join(d);
            std::fs::create_dir_all(&p).at(&p)?;
        }
        compile(
            &base.reference,
            stdlibs,
            &root,
            &work.join("newroot"),
            store,
        )?;

        let tar_path = work.join("layer.tar");
        let (digest, size) = write_layer(&flat, &root, &tar_path)?;
        store.write_blob(
            &digest,
            BufReader::new(File::open(&tar_path).at(&tar_path)?),
        )?;
        store.unpack_layer(&digest, LayerCompression::None)?;
        Ok((digest, size))
    })();
    let _ = std::fs::remove_dir_all(&work);
    let (digest, size) = result?;

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
        index: None,
        platform: base.platform.clone(),
    };
    store.put(image.clone())?;
    tracing::info!(
        image = %base.reference,
        layer = %digest,
        layer_bytes = size,
        ms = started.elapsed().as_millis() as u64,
        "compiled the image's Python bytecode"
    );
    Ok(image)
}

/// `compileall` for each library, with that library's own interpreter, in a
/// sandbox whose root is the writable copy.
fn compile(
    image: &str,
    stdlibs: &[Stdlib],
    root: &Path,
    newroot: &Path,
    store: &Store,
) -> Result<()> {
    let spec = build_spec(image, stdlibs);
    std::fs::create_dir_all(newroot).at(newroot)?;
    let view = RootfsView::Flat {
        dir: root.to_path_buf(),
    };
    let env = vec![
        ("HOME".to_string(), "/tmp".to_string()),
        ("PATH".to_string(), DEFAULT_PATH.to_string()),
        // The interpreter running `compileall` must not cache what it imports
        // on its own: see `VERSION`.
        ("PYTHONDONTWRITEBYTECODE".to_string(), "1".to_string()),
    ];
    let mut config = SandboxConfig::from_resolved(&spec, &view, newroot, spec.cmd.clone(), &env);
    config.writable_root = true;
    let result = run_captured(&mut config, store.paths());
    let _ = std::fs::remove_dir_all(newroot);
    match result {
        Ok((0, _)) => Ok(()),
        Ok((code, output)) => Err(Error::Build {
            what: "the bytecode layer",
            reason: format!("compileall exited {code}:\n{}", tail(&output, 20)),
            remedy: "set ZYGO_BYTECODE=0 to serve the image without it".into(),
        }),
        Err(e) => Err(e),
    }
}

fn build_spec(image: &str, stdlibs: &[Stdlib]) -> ResolvedFn {
    use crate::spec::{Layer, ResolveOptions, resolve_standalone};

    let mut spec = resolve_standalone(
        "bytecode-build",
        &Layer {
            image: Some(image.to_string()),
            cmd: Some(build_argv(stdlibs)),
            user: Some("0".to_string()),
            mem: Some(Bytes::from_mib(512)),
            cpu: Some(Cpu(2.0)),
            pids: Some(64),
            timeout: Some(Duration::from_secs(300)),
            scratch: Some(Bytes::from_mib(256)),
            ..Default::default()
        },
        &ResolveOptions {
            one_shot: true,
            ..Default::default()
        },
    )
    .expect("the build layer is fixed and valid");
    spec.mounts = Vec::new();
    spec.network = Network::None;
    spec
}

/// One `compileall` per library. A file that does not compile — the standard
/// library's own tests carry deliberately broken ones — makes `compileall`
/// exit 1 while everything else was written, so its status is not the build's:
/// the build fails only when the interpreter itself is missing.
fn build_argv(stdlibs: &[Stdlib]) -> Vec<String> {
    let mut script = String::from("set -e\n");
    for s in stdlibs {
        script.push_str(&format!(
            "command -v {i} >/dev/null\n{i} -m compileall -q -f --invalidation-mode unchecked-hash {d} || true\n",
            i = s.interpreter,
            d = s.dir
        ));
    }
    vec!["sh".to_string(), "-c".to_string(), script]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_python3_minor_directories_are_libraries() {
        let s = stdlib_named("usr/local/lib", "python3.12").unwrap();
        assert_eq!(s.dir, "/usr/local/lib/python3.12");
        assert_eq!(s.interpreter, "python3.12");
        assert_eq!(s.tag, "cpython-312");
        for other in [
            "python3",
            "python3.",
            "python3.12t",
            "python2.7",
            "pkgconfig",
        ] {
            assert!(stdlib_named("usr/lib", other).is_none(), "{other}");
        }
    }

    #[test]
    fn the_key_follows_the_image_and_the_architecture() {
        let a = cache_key("sha256:aaa", "aarch64");
        assert_eq!(a, cache_key("sha256:aaa", "aarch64"));
        assert_ne!(a, cache_key("sha256:bbb", "aarch64"));
        assert_ne!(a, cache_key("sha256:aaa", "x86_64"));
    }

    #[test]
    fn a_library_without_its_os_pyc_is_found_and_one_with_it_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::Paths::rooted(tmp.path());
        let store = Store::new(paths);
        let digest = format!("sha256:{}", "a".repeat(64));
        let layer = store.layer_dir(&digest).unwrap();
        let lib = layer.join("usr/local/lib/python3.12");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("os.py"), "").unwrap();

        let missing = stdlibs_without_bytecode(&store, std::slice::from_ref(&digest));
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].tag, "cpython-312");

        std::fs::create_dir_all(lib.join("__pycache__")).unwrap();
        std::fs::write(lib.join("__pycache__/os.cpython-312.pyc"), "").unwrap();
        assert!(stdlibs_without_bytecode(&store, &[digest]).is_empty());
    }

    #[test]
    fn a_failed_build_is_left_alone_for_a_while_unless_asked() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::new(crate::Paths::rooted(tmp.path()));
        let marker = failed_marker(&store, "k");
        assert!(marker.starts_with(tmp.path()));
        std::fs::create_dir_all(marker.parent().unwrap()).unwrap();
        std::fs::write(&marker, "no space left on device").unwrap();
        let fresh = std::fs::metadata(&marker)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t.elapsed().is_ok_and(|age| age < RETRY_FAILED_AFTER));
        assert!(fresh, "a marker just written is within the retry window");
    }

    #[test]
    fn the_build_names_each_library_with_its_own_interpreter() {
        let argv = build_argv(&[stdlib_named("usr/local/lib", "python3.12").unwrap()]);
        assert_eq!(argv[..2], ["sh", "-c"]);
        assert!(argv[2].contains("python3.12 -m compileall"), "{}", argv[2]);
        assert!(argv[2].contains("--invalidation-mode unchecked-hash"));
        // Forced: a `.pyc` already there is one the build's own interpreter
        // wrote with a timestamp, and would otherwise be kept.
        assert!(argv[2].contains("compileall -q -f"), "{}", argv[2]);
        assert!(argv[2].contains("/usr/local/lib/python3.12"));
    }
}
