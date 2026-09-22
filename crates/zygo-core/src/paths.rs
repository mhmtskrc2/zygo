//! On-disk layout (design doc §3.13).
//!
//! ```text
//! $XDG_DATA_HOME/zygo/
//! ├── images/blobs/sha256/…       OCI blobs
//! ├── images/layers/<digest>/     unpacked layers
//! ├── images/index.json           reference → manifest digest
//! ├── cache/venvs/<hash>/         dependency cache
//! ├── cache/flat/<digest>/        flattened rootfs (overlayfs fallback)
//! ├── tenants/<name>/data.img     optional persistent tenant space
//! └── runsc/, krun/               optional backend binaries
//! $XDG_RUNTIME_DIR/zygo/
//! ├── supervisor.sock             CLI ↔ supervisor
//! ├── tenants/<name>/agent.sock   supervisor ↔ runtime agent
//! └── supervisor.pid
//! ```

use std::path::{Path, PathBuf};

use crate::error::{IoContext, Result};

/// Resolved directory layout. Construct once and pass down; tests build one
/// rooted at a `tempfile::TempDir`.
#[derive(Debug, Clone)]
pub struct Paths {
    data: PathBuf,
    runtime: PathBuf,
}

impl Paths {
    /// Layout derived from the XDG environment, with the usual fallbacks.
    pub fn from_env() -> Self {
        Self::from_vars(|key| std::env::var_os(key).filter(|v| !v.is_empty()))
    }

    /// The same, against any set of variables.
    ///
    /// Split out so the rule can be tested rather than described. The two
    /// halves are derived **independently**, which is the part worth pinning:
    /// a caller that reproduces one of them and lets the other fall back gets
    /// a layout that looks right and points somewhere else. That is how the
    /// supervisor came to listen on one socket while its own client waited on
    /// another.
    pub fn from_vars(var: impl Fn(&str) -> Option<std::ffi::OsString>) -> Self {
        let path = |key: &str| var(key).map(PathBuf::from);

        let data = path("ZYGO_DATA_HOME")
            .or_else(|| path("XDG_DATA_HOME").map(|p| p.join("zygo")))
            .or_else(|| path("HOME").map(|h| h.join(".local/share/zygo")))
            .unwrap_or_else(|| PathBuf::from("/tmp/zygo"));

        let runtime = path("ZYGO_RUNTIME_DIR")
            .or_else(|| path("XDG_RUNTIME_DIR").map(|p| p.join("zygo")))
            .unwrap_or_else(|| {
                // macOS has no XDG_RUNTIME_DIR; per-uid /tmp keeps the socket
                // owned by the caller, which `SO_PEERCRED` checks rely on.
                PathBuf::from(format!("/tmp/zygo-{}", current_uid()))
            });

        Self { data, runtime }
    }

    /// The variables that reproduce this layout in a child process.
    ///
    /// What `supervisor::client` hands the supervisor it starts. Both halves
    /// or neither: passing one and leaving the other to a fallback is the bug
    /// this exists to prevent.
    pub fn as_vars(&self) -> [(&'static str, &Path); 2] {
        [
            ("ZYGO_DATA_HOME", self.data.as_path()),
            ("ZYGO_RUNTIME_DIR", self.runtime.as_path()),
        ]
    }

    /// Replace the runtime directory, keeping the data directory.
    ///
    /// `--data-root` (and so `ZYGO_DATA_HOME`, which clap binds to it) selects
    /// a self-contained tree with the socket at `<root>/run`. That is what the
    /// verification suites want. It is *not* what an ordinary session wants,
    /// where the data lives under `~/.local/share` and the socket belongs in
    /// `$XDG_RUNTIME_DIR`.
    ///
    /// Both are legitimate, and the one thing that must never happen is a
    /// process reproducing half of one and half of the other: that is how the
    /// supervisor came to listen on `<data>/run/supervisor.sock` while its own
    /// client waited on `$XDG_RUNTIME_DIR/zygo/supervisor.sock`. So the runtime
    /// half is settable on its own, and [`Paths::as_vars`] hands both halves
    /// to a child together.
    pub fn with_runtime(self, runtime: impl Into<PathBuf>) -> Self {
        Self {
            runtime: runtime.into(),
            ..self
        }
    }

    /// Layout rooted under `root`, for tests and for `--data-root`.
    pub fn rooted(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            runtime: root.join("run"),
            data: root,
        }
    }

    pub fn data(&self) -> &Path {
        &self.data
    }
    pub fn runtime(&self) -> &Path {
        &self.runtime
    }

    pub fn images(&self) -> PathBuf {
        self.data.join("images")
    }
    pub fn blobs(&self) -> PathBuf {
        self.images().join("blobs/sha256")
    }
    pub fn layers(&self) -> PathBuf {
        self.images().join("layers")
    }
    pub fn image_index(&self) -> PathBuf {
        self.images().join("index.json")
    }
    pub fn tmp(&self) -> PathBuf {
        self.data.join("tmp")
    }
    /// Scripts that arrive with requests, by content digest (protocol 1.1).
    pub fn scripts(&self) -> PathBuf {
        self.data.join("scripts")
    }
    pub fn venv_cache(&self) -> PathBuf {
        self.data.join("cache/venvs")
    }
    pub fn flat_cache(&self) -> PathBuf {
        self.data.join("cache/flat")
    }
    /// Records of derived system layers, by cache key: which packages, at
    /// which versions. The layers themselves live in the image store.
    pub fn system_cache(&self) -> PathBuf {
        self.data.join("cache/system")
    }
    pub fn tenant_data(&self, name: &str) -> PathBuf {
        self.data.join("tenants").join(name)
    }
    /// Where the `vm` backend's guest kernel lives, as the layout above
    /// reserves.
    ///
    /// A downloaded artefact rather than a linked one: the kernel is GPL and
    /// this binary is Apache-2.0, and it is ten to twenty megabytes against a
    /// fifteen megabyte budget. `zygo backend install vm` writes it here and
    /// verifies a pinned digest, the way `install gvisor` verifies `runsc`.
    pub fn krun(&self) -> PathBuf {
        self.backends().join("krun")
    }

    pub fn backends(&self) -> PathBuf {
        self.data.join("backends")
    }

    pub fn supervisor_sock(&self) -> PathBuf {
        self.runtime.join("supervisor.sock")
    }
    pub fn supervisor_pid(&self) -> PathBuf {
        self.runtime.join("supervisor.pid")
    }
    pub fn agent_sock(&self, name: &str) -> PathBuf {
        self.runtime.join("tenants").join(name).join("agent.sock")
    }

    /// Take an exclusive, cross-process lock named `key`.
    ///
    /// Advisory `flock` on a file under `tmp/`, released when the returned
    /// guard drops — including when the process dies, which is what makes it
    /// safe to hold across something slow. Two Zygo processes that would
    /// otherwise build the same thing twice take the same key and one waits.
    ///
    /// It **blocks**. Every caller is in the position of having found work
    /// that somebody else may be part-way through, and waiting for their
    /// answer beats racing them to a half-built result.
    pub fn lock(&self, key: &str) -> Result<Lock> {
        let dir = self.tmp();
        std::fs::create_dir_all(&dir).at(&dir)?;
        let path = dir.join(format!(
            "{}.lock",
            key.chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                })
                .collect::<String>()
        ));
        let file = std::fs::File::create(&path).at(&path)?;
        lock_exclusive(&file).at(&path)?;
        Ok(Lock { _file: file })
    }

    /// Whether `key` is locked by somebody else right now.
    ///
    /// Only ever used to decide whether to *say* something before blocking on
    /// [`Paths::lock`] — "another command is already doing this" is worth a
    /// line when the wait is a minute long. Racy by nature, which is why it
    /// decides nothing else.
    pub fn is_locked(&self, key: &str) -> bool {
        let path = self.tmp().join(format!(
            "{}.lock",
            key.chars()
                .map(|c| if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                })
                .collect::<String>()
        ));
        let Ok(file) = std::fs::File::open(&path) else {
            return false;
        };
        try_lock_exclusive(&file).is_err()
    }

    /// Create the directories needed before anything is written. Cheap enough
    /// to call on every command; `create_dir_all` on an existing tree is a
    /// handful of `stat`s.
    pub fn ensure(&self) -> Result<()> {
        for dir in [
            self.blobs(),
            self.layers(),
            self.tmp(),
            self.venv_cache(),
            self.flat_cache(),
            self.system_cache(),
            self.runtime.clone(),
        ] {
            std::fs::create_dir_all(&dir).at(&dir)?;
        }
        self.restrict_runtime_dir()
    }

    /// Make the runtime directory owner-only.
    ///
    /// It holds the control socket, and anyone who can reach that socket can
    /// run code as this user. The socket itself is also chmodded, but there is
    /// a window between `bind` and `chmod`; a `0700` parent closes it, and it is
    /// the check that still holds if a future socket is created somewhere that
    /// forgets its own mode.
    fn restrict_runtime_dir(&self) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&self.runtime, mode).at(&self.runtime)?;
        }
        Ok(())
    }
}

impl Default for Paths {
    fn default() -> Self {
        Self::from_env()
    }
}

fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: getuid is always successful and has no preconditions.
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    // The bug this file's `from_vars`/`as_vars` pair exists to prevent: the
    // supervisor listened on `<data>/run/supervisor.sock` while its client
    // waited on `$XDG_RUNTIME_DIR/zygo/supervisor.sock`, and the timeout said
    // only that it "did not answer".
    #[test]
    fn a_child_given_these_variables_lands_on_the_same_socket() {
        let parent = Paths::from_vars(|key| match key {
            "HOME" => Some("/home/m".into()),
            "XDG_RUNTIME_DIR" => Some("/run/user/501".into()),
            _ => None,
        });
        // What the parent worked out: two directories from two sources.
        assert_eq!(parent.data(), Path::new("/home/m/.local/share/zygo"));
        assert_eq!(
            parent.supervisor_sock(),
            Path::new("/run/user/501/zygo/supervisor.sock")
        );

        let vars = parent.as_vars();
        let child = Paths::from_vars(|key| {
            vars.iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.as_os_str().to_owned())
        });
        assert_eq!(child.supervisor_sock(), parent.supervisor_sock());
        assert_eq!(child.data(), parent.data());
    }

    // Reproducing only the data half is what `--data-root` did.
    #[test]
    fn half_the_layout_gives_a_different_socket() {
        let parent = Paths::from_vars(|key| match key {
            "HOME" => Some("/home/m".into()),
            "XDG_RUNTIME_DIR" => Some("/run/user/501".into()),
            _ => None,
        });
        let half = Paths::rooted(parent.data());
        assert_ne!(half.supervisor_sock(), parent.supervisor_sock());
    }

    use super::*;

    #[test]
    fn rooted_layout_is_self_contained() {
        let p = Paths::rooted("/x");
        assert_eq!(p.blobs(), Path::new("/x/images/blobs/sha256"));
        assert_eq!(p.layers(), Path::new("/x/images/layers"));
        assert_eq!(p.supervisor_sock(), Path::new("/x/run/supervisor.sock"));
        assert_eq!(p.agent_sock("a"), Path::new("/x/run/tenants/a/agent.sock"));
    }

    #[test]
    fn ensure_creates_every_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::rooted(tmp.path());
        p.ensure().unwrap();
        assert!(p.blobs().is_dir());
        assert!(p.layers().is_dir());
        assert!(p.runtime().is_dir());
    }
    /// The guard that stops two `zygo` commands from starting the Linux VM at
    /// once. Creating the instance takes about a minute, and a second
    /// `limactl start` inside that minute fails against the half-made
    /// directory rather than waiting for it.
    #[test]
    fn a_lock_is_exclusive_and_released_on_drop() {
        let tmp = tempfile::tempdir().expect("tmp");
        let paths = Paths::rooted(tmp.path());
        paths.ensure().expect("ensure");

        assert!(!paths.is_locked("vm-start"), "nothing holds it yet");

        let held = paths.lock("vm-start").expect("lock");
        assert!(paths.is_locked("vm-start"), "held now");
        // A different job is not blocked by this one.
        assert!(!paths.is_locked("vm-guest-binary"));

        drop(held);
        assert!(!paths.is_locked("vm-start"), "released on drop");
    }

    /// The key becomes a filename, so it must not be able to leave `tmp/`.
    #[test]
    fn a_lock_key_cannot_escape_the_data_directory() {
        let tmp = tempfile::tempdir().expect("tmp");
        let paths = Paths::rooted(tmp.path());
        paths.ensure().expect("ensure");

        let _held = paths.lock("../../etc/passwd").expect("lock");
        let files: Vec<String> = std::fs::read_dir(paths.tmp())
            .expect("read")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files, ["______etc_passwd.lock"], "every separator replaced");
    }
}

/// Held for as long as the caller needs exclusivity; released on drop.
#[derive(Debug)]
pub struct Lock {
    _file: std::fs::File,
}

#[cfg(unix)]
fn lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::LockExclusive)?;
    Ok(())
}

#[cfg(not(unix))]
fn lock_exclusive(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn try_lock_exclusive(file: &std::fs::File) -> std::io::Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive)?;
    // Taken and immediately given back: the question was whether it was free.
    let _ = rustix::fs::flock(file, rustix::fs::FlockOperation::Unlock);
    Ok(())
}
#[cfg(not(unix))]
fn try_lock_exclusive(_file: &std::fs::File) -> std::io::Result<()> {
    Ok(())
}
