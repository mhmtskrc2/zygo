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
        let data = env_path("ZYGO_DATA_HOME")
            .or_else(|| env_path("XDG_DATA_HOME").map(|p| p.join("zygo")))
            .or_else(|| home().map(|h| h.join(".local/share/zygo")))
            .unwrap_or_else(|| PathBuf::from("/tmp/zygo"));

        let runtime = env_path("ZYGO_RUNTIME_DIR")
            .or_else(|| env_path("XDG_RUNTIME_DIR").map(|p| p.join("zygo")))
            .unwrap_or_else(|| {
                // macOS has no XDG_RUNTIME_DIR; per-uid /tmp keeps the socket
                // owned by the caller, which `SO_PEERCRED` checks rely on.
                PathBuf::from(format!("/tmp/zygo-{}", current_uid()))
            });

        Self { data, runtime }
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

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn home() -> Option<PathBuf> {
    env_path("HOME")
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
}
