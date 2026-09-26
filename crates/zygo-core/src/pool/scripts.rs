// SPDX-License-Identifier: Apache-2.0
//! A request's own script, written into the sandbox it will run in.
//!
//! Protocol 1.1 lets a request bring its code with it. The code arrives
//! as a file under a read-only bind mount that belongs to one sandbox
//! alone: the supervisor writes it, nothing inside can change it, and it
//! is gone when the last request that named it is done. [`Scripts`]
//! counts those requests per digest; `ScriptLease` removes the file. The
//! store the digests come from is [`crate::scripts`]; this module is only
//! the last hop, from the host into `/run/script`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use super::secrets::write_request_file_at;
#[cfg(target_os = "linux")]
use crate::error::Error;
use crate::error::{IoContext, Result};
use crate::paths::Paths;

/// Where a request's own script lands inside the sandbox (protocol 1.1).
///
/// A **read-only bind mount** of a directory on the host that belongs to this
/// sandbox alone. The supervisor writes the scripts into the host side; the
/// sandbox sees them through a mount it cannot write. That asymmetry is the
/// whole control, and it is the mount namespace enforcing it rather than
/// anything the tenant is asked to respect.
///
/// It has to be a mount, and the two obvious alternatives are both worth
/// naming because both were tried:
///
/// * **Mode bits cannot do it.** Everything in a sandbox runs as the uid the
///   supervisor maps to, so the *owner* bits are what a tenant gets: a
///   directory the supervisor can write is a directory the tenant can write,
///   whatever the mode says.
/// * **Landlock cannot do it either.** A rule grants; within one ruleset a
///   read-only rule on `/run/script` is unioned with the read-write rule on
///   the `/run` tmpfs above it rather than overriding it. Measured on 6.8,
///   ABI 4. Subtracting needs a second *layer*, applied in the request's own
///   child, which needs a protocol contract to carry it.
///
/// The host directory is `0311` — write and traverse, no read — so a script
/// can open a digest it already knows and cannot list what else is in flight
/// beside it. In a runtime pool that "what else" is other tenants' code.
///
/// The digest check in the child (`spec/protocol.md` §3.8) stays, and is not
/// redundant: it covers the kernels and backends where this mount is not what
/// it should be, and it is what makes a swapped file a refused request rather
/// than a served one.
pub const SCRIPT_DIR_IN_SANDBOX: &str = "/run/script";

/// The mode the host-side script directory is created with. See above.
#[cfg(target_os = "linux")]
pub const SCRIPT_DIR_MODE: u32 = 0o311;

/// Everywhere else, the same directory without the unreadable part.
///
/// There is no sandbox off Linux and no `O_PATH` to open a directory that
/// denies read to its owner, so a `0311` here is not a boundary — it is only a
/// directory this process cannot open. The tests that exercise the placement
/// logic run on a developer's Mac, and this is what lets them.
#[cfg(not(target_os = "linux"))]
pub const SCRIPT_DIR_MODE: u32 = 0o711;

/// Scripts present in the sandbox for the requests that named them.
///
/// Counted, not merely present: a script is shared, two requests may run the
/// same one at once, and the second must not find the file the first has just
/// removed. The same reason [`Secrets`] counts, for the same boundary — except
/// that the count is per script rather than per function, because two requests
/// in flight on a pool zygote are generally two *different* scripts.
#[derive(Debug, Default)]
pub(super) struct Scripts {
    /// File name — the digest's hex — to the number of requests that need it.
    resident: BTreeMap<String, u32>,
    /// The host side of the bind mount, held open from the first file written
    /// until the last is gone.
    dir: Option<std::fs::File>,
    /// Whether this function has already said that it cannot deliver a script
    /// as a file. Said once: it is a property of the sandbox, so a request
    /// that logs it logs it for every request after.
    pub(super) warned: bool,
}

/// A script file that exists in the sandbox for as long as this is alive.
///
/// Dropping it is what takes the file away, so every exit from the request
/// path — the reply, the deadline, a broken connection — removes it, exactly
/// as [`SecretsLease`] does.
pub(super) struct ScriptLease<'a> {
    scripts: &'a Mutex<Scripts>,
    name: String,
}

impl Drop for ScriptLease<'_> {
    fn drop(&mut self) {
        let mut guard = self.scripts.lock().expect("scripts");
        let last = match guard.resident.get_mut(&self.name) {
            Some(count) => {
                *count -= 1;
                *count == 0
            }
            None => false,
        };
        if !last {
            return;
        }
        guard.resident.remove(&self.name);
        if let Some(dir) = guard.dir.as_ref() {
            let _ = rustix::fs::unlinkat(dir, self.name.as_str(), rustix::fs::AtFlags::empty());
        }
        // The last script out closes the directory too: holding a descriptor
        // into a sandbox that may be replaced under us buys nothing once there
        // is nothing in there to remove.
        if guard.resident.is_empty() {
            guard.dir = None;
        }
    }
}

/// Put one request's script where its child will find it.
///
/// `dir` is the sandbox's `/run/script` as seen from the host — through
/// `/proc/<agent pid>/root`, the same route secrets take and for the same
/// reason: nothing inside the sandbox is asked to cooperate, and the zygote
/// never holds the bytes.
pub(super) fn place_script<'a>(
    scripts: &'a Mutex<Scripts>,
    dir: &std::path::Path,
    name: &str,
    source: &str,
) -> Result<ScriptLease<'a>> {
    let mut guard = scripts.lock().expect("scripts");
    let count = guard.resident.entry(name.to_string()).or_insert(0);
    *count += 1;
    let write_now = *count == 1;

    let written = if write_now {
        write_script(&mut guard, dir, name, source)
    } else {
        Ok(())
    };
    drop(guard);

    // The lease is built with no lock held, for the reason `place_secrets`
    // spells out: dropping one takes this same mutex, and a lease created
    // above an early `?` return would be dropped by the thread holding it.
    let lease = ScriptLease {
        scripts,
        name: name.to_string(),
    };
    written?;
    Ok(lease)
}

/// Write one script file. Called with the lock held; takes none.
fn write_script(
    scripts: &mut Scripts,
    dir: &std::path::Path,
    name: &str,
    source: &str,
) -> Result<()> {
    if scripts.dir.is_none() {
        // The host side of the bind mount. It was made before the sandbox
        // started — it has to be, because the mount names it — so this
        // ordinarily only opens it.
        ensure_script_dir(dir)?;
        // `O_PATH`, because the mode denies read to this process too: it is
        // the same uid as everything in the sandbox, which is the point. An
        // `O_PATH` descriptor names the directory without opening it for
        // anything, and is exactly what `openat` and `unlinkat` need.
        #[cfg(target_os = "linux")]
        let opened = std::fs::File::from(
            rustix::fs::open(
                dir,
                rustix::fs::OFlags::PATH
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(|e| Error::io(dir, std::io::Error::from(e)))?,
        );
        #[cfg(not(target_os = "linux"))]
        let opened = std::fs::File::open(dir).at(dir)?;
        scripts.dir = Some(opened);
    }
    let dir = scripts.dir.as_ref().expect("just set");
    write_request_file_at(dir, name, source, "writing a script into the sandbox")
}

/// Make the host side of the script mount, at the mode the sandbox needs.
///
/// `0311`: the supervisor writes and traverses, and *nothing* reads the
/// directory itself — not the sandbox, which would otherwise have an
/// inventory of every script in flight, and not this process either, which
/// has no need to list what it put there.
pub(super) fn ensure_script_dir(dir: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dir).at(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(SCRIPT_DIR_MODE)).at(dir)?;
    }
    Ok(())
}

/// A directory for one sandbox's scripts, on the host.
///
/// One per sandbox and never reused: a pool that is replaced must not inherit
/// the scripts of the one before it, and two pools must not be able to see
/// each other's. The counter is what makes a replacement under the same name
/// a different directory.
pub(super) fn new_script_dir(paths: &Paths, name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static GENERATION: AtomicU64 = AtomicU64::new(1);
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
    paths.tmp().join(format!(
        "scripts-{}-{}-{generation}",
        crate::cgroup::sanitise(name),
        crate::process_token()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Where a script goes, and what it looks like when it gets there.
    ///
    /// `0400` on the file and `0711` on the directory: a child can open a
    /// script whose digest it knows, and `readdir` tells it nothing about what
    /// else is in flight on a pool zygote it shares with other tenants.
    #[cfg(unix)]
    #[test]
    fn a_script_arrives_at_0400_in_a_directory_that_cannot_be_listed() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("run-script");
        let scripts = Mutex::new(Scripts::default());

        let lease = place_script(&scripts, &dir, "abc123", "x = 1\n").expect("place");
        let file = dir.join("abc123");
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "x = 1\n");
        assert_eq!(
            std::fs::metadata(&file).expect("stat").permissions().mode() & 0o777,
            0o400
        );
        assert_eq!(
            std::fs::metadata(&dir).expect("stat").permissions().mode() & 0o777,
            SCRIPT_DIR_MODE,
            "the directory a tenant could list is the one thing this must not be"
        );

        drop(lease);
        assert!(
            !file.exists(),
            "the script outlived the request that named it"
        );
    }

    /// Two requests, one script: the first to finish must not take the file
    /// away from the second. The per-script count is the whole reason
    /// [`Scripts`] holds one.
    #[cfg(unix)]
    #[test]
    fn a_script_two_requests_share_leaves_when_the_second_one_does() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("run-script");
        let scripts = Mutex::new(Scripts::default());

        let first = place_script(&scripts, &dir, "shared", "handler = 1\n").expect("first");
        let second = place_script(&scripts, &dir, "shared", "handler = 1\n").expect("second");
        drop(first);
        assert!(
            dir.join("shared").exists(),
            "the request still running lost its script"
        );
        drop(second);
        assert!(!dir.join("shared").exists());
    }

    /// Two tenants' scripts in flight at once are two files, and neither
    /// departure disturbs the other.
    #[cfg(unix)]
    #[test]
    fn two_scripts_in_flight_are_two_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("run-script");
        let scripts = Mutex::new(Scripts::default());

        let a = place_script(&scripts, &dir, "aaa", "a = 1\n").expect("a");
        let b = place_script(&scripts, &dir, "bbb", "b = 2\n").expect("b");
        // Named rather than listed: the directory is `0311`, so nothing may
        // read it — this test included, unless it runs as root, which is the
        // only place the `read_dir` this used to be ever passed.
        assert!(
            dir.join("aaa").exists() && dir.join("bbb").exists(),
            "one file per script in flight"
        );
        drop(a);
        assert!(!dir.join("aaa").exists());
        assert!(dir.join("bbb").exists());
        drop(b);
        assert!(!dir.join("bbb").exists());
        assert!(
            scripts.lock().expect("scripts").dir.is_none(),
            "the last script out closes the directory it was written through"
        );
    }

    /// A placement that cannot happen leaves the count honest.
    ///
    /// The lease is taken either way — `place_script` builds it after the lock
    /// is released and before it reports the failure — so a sandbox that
    /// refuses one write does not leave a phantom reference behind that stops
    /// the next request's file ever being removed.
    #[cfg(unix)]
    #[test]
    fn a_failed_placement_does_not_leak_a_reference() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A file where the directory should be: `create_dir_all` cannot win.
        let dir = tmp.path().join("run-script");
        std::fs::write(&dir, "not a directory").expect("write");
        let scripts = Mutex::new(Scripts::default());

        assert!(
            place_script(&scripts, &dir, "abc", "x = 1\n").is_err(),
            "a file is not a directory"
        );
        assert!(
            scripts.lock().expect("scripts").resident.is_empty(),
            "the failed request is still counted as holding its script"
        );
    }
}
