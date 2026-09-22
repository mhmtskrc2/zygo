//! A content-addressed store for the scripts that arrive with requests.
//!
//! Protocol 1.1 lets an `EXEC` carry the code to run, so that one warm
//! interpreter — a *runtime pool* — serves ten thousand scripts instead of ten
//! thousand zygotes serving one each (`docs/bench-embed.md` has the number:
//! 9.98 MB of proportional memory per warm script, which is 97 GiB at that
//! count). An embedder registers a script once and calls it ten thousand
//! times; this is where the "once" goes.
//!
//! Content-addressed, because that is what makes it safe to share: a script's
//! name is the SHA-256 of its bytes, so it can never change under a request
//! that was queued against it, two tenants who upload the same script hold one
//! file, and a request that names a digest gets exactly those bytes or an
//! error — never somebody's newer version.
//!
//! What is deliberately *not* here: delivery into the sandbox. This store is
//! on the host and nothing in a sandbox can see it. A request's script is
//! copied in for the length of that request, by the same machinery that
//! delivers secrets — `pool::place_script`, and [`SCRIPT_DIR_IN_SANDBOX`] is
//! where it lands. Mounting the store itself would make every tenant's script
//! listable by every other tenant's code, which is the one thing a shared
//! runtime pool must not allow.
//!
//! [`SCRIPT_DIR_IN_SANDBOX`]: crate::pool::SCRIPT_DIR_IN_SANDBOX

use std::fmt;
use std::path::PathBuf;

use sha2::{Digest as _, Sha256};

use crate::error::{IoContext, Result};
use crate::paths::Paths;
use crate::spec::SpecError;

/// The most a script may be.
///
/// Well under the 32 MiB frame limit, because the script travels *inside* a
/// frame that also carries the event — and because four megabytes of Python
/// is not a script, it is a package, and packages are what `requirements`
/// and `system` are for.
pub const MAX_SCRIPT_BYTES: usize = 4 * 1024 * 1024;

/// `sha256:<64 hex digits>` — a script's name, derived from its bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ScriptDigest(String);

impl ScriptDigest {
    /// The digest of these exact bytes.
    pub fn of(source: &str) -> ScriptDigest {
        ScriptDigest(format!(
            "sha256:{}",
            hex::encode(Sha256::digest(source.as_bytes()))
        ))
    }

    /// Accept a digest somebody else wrote down, strictly.
    ///
    /// Strict because it becomes a path component: exactly the algorithm
    /// prefix, exactly sixty-four lowercase hex digits, nothing that could be
    /// a `..` or a `/`.
    pub fn parse(text: &str) -> Result<ScriptDigest> {
        let hex = text.strip_prefix("sha256:").ok_or_else(|| {
            SpecError::invalid(
                "script.digest",
                format!("`{text}` does not start with `sha256:`"),
            )
        })?;
        let well_formed = hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !well_formed {
            return Err(SpecError::invalid(
                "script.digest",
                format!("`{text}` is not sha256: followed by 64 lowercase hex digits"),
            )
            .into());
        }
        Ok(ScriptDigest(text.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The hex half, which is the file name.
    ///
    /// Public because it is the name a script has in two places outside this
    /// module: the file in the store, and the file the supervisor writes into
    /// the sandbox at `/run/script/<hex>` for the child to load.
    pub fn hex(&self) -> &str {
        &self.0["sha256:".len()..]
    }
}

impl fmt::Display for ScriptDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ScriptDigest {
    type Error = String;
    fn try_from(text: String) -> std::result::Result<Self, String> {
        ScriptDigest::parse(&text).map_err(|e| e.to_string())
    }
}

impl From<ScriptDigest> for String {
    fn from(d: ScriptDigest) -> String {
        d.0
    }
}

/// The scripts on this host, by digest.
///
/// Immutable by construction: a file's name is the hash of its contents, so
/// there is nothing to update, only things to add and — one day, with a
/// reference count nobody has needed yet — things to remove.
pub struct ScriptStore {
    dir: PathBuf,
}

impl ScriptStore {
    pub fn new(paths: &Paths) -> ScriptStore {
        ScriptStore {
            dir: paths.scripts(),
        }
    }

    /// Where a digest's bytes live, whether or not they do yet.
    pub fn path(&self, digest: &ScriptDigest) -> PathBuf {
        self.dir.join(digest.hex())
    }

    pub fn contains(&self, digest: &ScriptDigest) -> bool {
        self.path(digest).is_file()
    }

    /// Store a script and return its name.
    ///
    /// Idempotent: the same bytes are the same digest and the same file, and
    /// a second `put` of them touches nothing — which is what lets two
    /// tenants, or one tenant twice, register a script without a race.
    /// Written whole to a temporary name and renamed into place, so a reader
    /// that finds the file finds all of it.
    pub fn put(&self, source: &str) -> Result<ScriptDigest> {
        if source.len() > MAX_SCRIPT_BYTES {
            return Err(SpecError::invalid(
                "script",
                format!(
                    "{} bytes is over the {} MiB limit for one script; a dependency \
                     that size belongs in `requirements` or `system`",
                    source.len(),
                    MAX_SCRIPT_BYTES / 1024 / 1024
                ),
            )
            .into());
        }
        let digest = ScriptDigest::of(source);
        let path = self.path(&digest);
        if path.is_file() {
            return Ok(digest);
        }

        std::fs::create_dir_all(&self.dir).at(&self.dir)?;
        // The pid in the name keeps two concurrent `put`s of the same script
        // from truncating each other's temporary file; the rename at the end
        // is atomic, and whichever lands second replaces identical bytes.
        let temporary = self
            .dir
            .join(format!(".{}.{}.incoming", digest.hex(), std::process::id()));
        std::fs::write(&temporary, source).at(&temporary)?;
        std::fs::rename(&temporary, &path).at(&path)?;
        Ok(digest)
    }

    /// The script, or `None` when this host has never seen that digest.
    ///
    /// The bytes are re-hashed on the way out. A file whose contents do not
    /// match its name is corruption — a disk error, or something writing into
    /// the store by hand — and handing it to a sandbox as the script somebody
    /// asked for would be the one thing a content-addressed store must never
    /// do.
    pub fn get(&self, digest: &ScriptDigest) -> Result<Option<String>> {
        let path = self.path(digest);
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(crate::error::Error::io(&path, e)),
        };
        if ScriptDigest::of(&source) != *digest {
            return Err(crate::error::Error::primitive(
                "script store",
                "remove the file and register the script again",
                std::io::Error::other(format!(
                    "{} does not hash to its own name; the store is corrupt",
                    path.display()
                )),
            ));
        }
        Ok(Some(source))
    }

    /// Forget a script. `Ok(false)` when it was not there.
    ///
    /// Nothing calls this on a request path: a script can be in flight, and
    /// the supervisor reads it before the fork, so removing it under a running
    /// request harms nothing — but a caller that removes one still referenced
    /// by a queued job has made that job fail, and that is the caller's
    /// decision to make.
    pub fn remove(&self, digest: &ScriptDigest) -> Result<bool> {
        match std::fs::remove_file(self.path(digest)) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(crate::error::Error::io(self.path(digest), e)),
        }
    }

    /// Every digest this host holds, for `zygo scripts` and for a GC.
    pub fn list(&self) -> Result<Vec<ScriptDigest>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(crate::error::Error::io(&self.dir, e)),
        };
        let mut digests = Vec::new();
        for entry in entries {
            let entry = entry.at(&self.dir)?;
            if let Some(name) = entry.file_name().to_str()
                && let Ok(d) = ScriptDigest::parse(&format!("sha256:{name}"))
            {
                digests.push(d);
            }
        }
        digests.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(digests)
    }

    #[cfg(test)]
    fn dir(&self) -> &std::path::Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ScriptStore) {
        let root = tempfile::tempdir().expect("a temp dir");
        let paths = Paths::rooted(root.path());
        (root, ScriptStore::new(&paths))
    }

    #[test]
    fn a_script_comes_back_as_it_went_in_under_the_hash_of_its_bytes() {
        let (_root, s) = store();
        let src = "def handler(event):\n    return {'n': 1}\n";
        let digest = s.put(src).unwrap();
        assert_eq!(digest, ScriptDigest::of(src));
        assert!(digest.as_str().starts_with("sha256:"));
        assert_eq!(digest.as_str().len(), "sha256:".len() + 64);
        assert_eq!(s.get(&digest).unwrap().as_deref(), Some(src));
        assert!(s.contains(&digest));
    }

    #[test]
    fn the_same_bytes_are_one_file_and_a_second_put_touches_nothing() {
        let (_root, s) = store();
        let a = s.put("x = 1\n").unwrap();
        let before = std::fs::metadata(s.path(&a)).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = s.put("x = 1\n").unwrap();
        assert_eq!(a, b);
        assert_eq!(
            std::fs::metadata(s.path(&a)).unwrap().modified().unwrap(),
            before
        );
        assert_eq!(
            s.list().unwrap().len(),
            1,
            "one script, however many times registered"
        );
    }

    #[test]
    fn different_bytes_are_different_names() {
        let (_root, s) = store();
        let a = s.put("x = 1\n").unwrap();
        let b = s.put("x = 2\n").unwrap();
        assert_ne!(a, b);
        assert_eq!(s.list().unwrap(), {
            let mut both = vec![a, b];
            both.sort_by(|x, y| x.as_str().cmp(y.as_str()));
            both
        });
    }

    #[test]
    fn a_digest_nobody_registered_is_none_not_an_error() {
        let (_root, s) = store();
        let ghost = ScriptDigest::of("never stored");
        assert_eq!(s.get(&ghost).unwrap(), None);
        assert!(!s.contains(&ghost));
        assert!(!s.remove(&ghost).unwrap());
    }

    #[test]
    fn a_digest_is_parsed_strictly_because_it_becomes_a_path() {
        for bad in [
            "",
            "sha256:",
            "sha256:abc",
            "md5:d41d8cd98f00b204e9800998ecf8427e",
            &format!("sha256:{}", "A".repeat(64)),
            &format!("sha256:{}", "../".repeat(21) + "a"),
            &format!("sha256:{}/x", "a".repeat(61)),
        ] {
            assert!(ScriptDigest::parse(bad).is_err(), "{bad:?} was accepted");
        }
        let good = ScriptDigest::of("ok");
        assert_eq!(ScriptDigest::parse(good.as_str()).unwrap(), good);
    }

    #[test]
    fn the_digest_round_trips_through_serde_as_a_string() {
        let d = ScriptDigest::of("serde");
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(json, format!("\"{d}\""));
        assert_eq!(serde_json::from_str::<ScriptDigest>(&json).unwrap(), d);
        assert!(serde_json::from_str::<ScriptDigest>("\"sha256:nope\"").is_err());
    }

    #[test]
    fn a_script_over_the_limit_is_refused_and_nothing_is_written() {
        let (_root, s) = store();
        let huge = "#".repeat(MAX_SCRIPT_BYTES + 1);
        let err = s.put(&huge).unwrap_err().to_string();
        assert!(err.contains("MiB limit"), "{err}");
        assert!(!s.dir().exists() || std::fs::read_dir(s.dir()).unwrap().next().is_none());
    }

    #[test]
    fn a_file_that_does_not_hash_to_its_name_is_corruption_not_a_script() {
        let (_root, s) = store();
        let digest = s.put("honest = True\n").unwrap();
        std::fs::write(s.path(&digest), "tampered = True\n").unwrap();
        let err = s.get(&digest).unwrap_err().to_string();
        assert!(err.contains("corrupt"), "{err}");
    }

    #[test]
    fn no_temporary_file_survives_a_put() {
        let (_root, s) = store();
        s.put("a\n").unwrap();
        s.put("b\n").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(s.dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".incoming"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn remove_forgets_a_script() {
        let (_root, s) = store();
        let d = s.put("gone\n").unwrap();
        assert!(s.remove(&d).unwrap());
        assert_eq!(s.get(&d).unwrap(), None);
    }
}
