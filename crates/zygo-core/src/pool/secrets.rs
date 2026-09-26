// SPDX-License-Identifier: Apache-2.0
//! Secret values, and the files they become for exactly one request.
//!
//! A secret is delivered as a file, to the child only, and the whole
//! control is *where the write happens*: from outside the sandbox, into
//! its `/run/secrets`, for as long as a request that needs it is in
//! flight. [`Secrets`] counts those requests; [`SecretsLease`] is what
//! takes the files away again on every path out. `write_request_file_at`
//! is shared with `scripts`, which puts a request's code in by the same
//! route.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::{Error, IoContext, Result};

/// Where secret files live inside the sandbox.
pub const SECRETS_DIR_IN_SANDBOX: &str = "/run/secrets";

/// How the supervisor reaches a sandbox's `/run/secrets`.
///
/// Two modes, two routes, for one kernel reason. `/proc/<pid>/root` is
/// traversable only while the process is **dumpable**, and writing the id map
/// clears that for everything in the sandbox. An **agent** is `execve`d
/// afterwards, which resets it, so its own `/proc` entry works. A **held**
/// sandbox's init never execs and is deliberately unreadable, so its init
/// hands a directory descriptor out during the launch instead — the only
/// moment such a thing can be taken.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
#[derive(Clone, Copy)]
pub(super) enum SecretsAt<'a> {
    /// Through a process's `/proc`, for an agent.
    Proc(u32),
    /// Through a descriptor the sandbox handed out, for warm-exec.
    Dir(std::os::fd::BorrowedFd<'a>),
}

/// Secrets for one function, and the in-flight count that governs their files.
#[derive(Debug, Default)]
pub(super) struct Secrets {
    pub(super) values: BTreeMap<String, String>,
    in_flight: u32,
    /// The sandbox's `/run/secrets`, held open from the moment the files are
    /// written until the last request in flight is done with them.
    ///
    /// Shared rather than per-lease because it is the *last* request out that
    /// removes the files, and that is rarely the one that wrote them.
    dir: Option<std::fs::File>,
    /// `values` belong to the request in flight, not to the function.
    ///
    /// A runtime pool's zygote is shared between tenants, so it has no values
    /// of its own: each request brings the calling tenant's, and they are
    /// forgotten when it leaves. See [`Secrets::adopt`].
    per_request: bool,
}

impl Secrets {
    /// A request is starting. `true` when its files have to be written now —
    /// the first request in, with something to deliver.
    fn arrive(&mut self) -> bool {
        self.in_flight += 1;
        self.in_flight == 1 && !self.values.is_empty()
    }

    /// Take one request's own values, for a runtime pool.
    ///
    /// Refused while anything is in flight, and that refusal is a safety
    /// property rather than a convenience: the files are one directory, so
    /// values for a request that shares the zygote with another would be
    /// readable by the other's child. The pool's scheduler gives a request
    /// with secrets the zygote to itself (`supervisor::runtime`); this is
    /// the check that the scheduler kept its word, and it fails the request
    /// rather than trusting it.
    fn adopt(&mut self, values: BTreeMap<String, String>) -> Result<()> {
        if self.in_flight > 0 {
            return Err(Error::BackendUnavailable {
                backend: "pool",
                reason: format!(
                    "a request with its own secrets was given a zygote with {} request{} \
                     already in flight",
                    self.in_flight,
                    if self.in_flight == 1 { "" } else { "s" }
                ),
                remedy: "the request was not run; this is a scheduling bug worth reporting".into(),
            });
        }
        self.values = values;
        self.per_request = true;
        Ok(())
    }

    /// Nothing is in flight: values that were a request's go with it.
    fn forget_if_per_request(&mut self) {
        if self.in_flight == 0 && self.per_request {
            self.values.clear();
            self.per_request = false;
        }
    }

    /// Remove the files, through the descriptor held since they were written.
    ///
    /// `unlinkat` and not a path, because the path went through
    /// `/proc/<pid>/root` of a process that may since have exited — a
    /// directory descriptor keeps the directory reachable regardless. The
    /// (empty, 0700) directory itself is left in place: it carries nothing,
    /// and removing it would need a second descriptor for its parent.
    fn unlink_all(&mut self) {
        let Some(dir) = self.dir.take() else {
            return;
        };
        for name in self.values.keys() {
            let _ = rustix::fs::unlinkat(&dir, name.as_str(), rustix::fs::AtFlags::empty());
        }
    }

    /// A request has finished. `true` when its files have to be removed now —
    /// the last request out, with something to withdraw.
    fn depart(&mut self) -> bool {
        self.in_flight = self.in_flight.saturating_sub(1);
        self.in_flight == 0 && !self.values.is_empty()
    }
}

/// Secrets present in the sandbox for as long as this is alive.
///
/// Dropping it is what withdraws them, so every exit from the request path —
/// the reply, the deadline, a broken connection — takes the files with it.
pub(super) struct SecretsLease<'a> {
    secrets: &'a Mutex<Secrets>,
}

impl Drop for SecretsLease<'_> {
    fn drop(&mut self) {
        withdraw_secrets(self.secrets);
    }
}

/// Make the secret files exist for a request about to run.
///
/// Written from *outside* the sandbox, through `/proc/<pid>/root`, so no
/// process inside ever receives a value: not the agent (it is not in `EXEC`,
/// not in the zygote's memory, not on the connection), and not a warm-exec
/// request's environment. Only the process that runs the handler can read the
/// file, and only while a request is in flight — "delivered as a file, to the
/// child only", enforced by *where* the write happens
/// rather than by asking anything inside to be careful.
///
/// `via_pid` is whichever process in the sandbox the supervisor may look
/// through, and the two warm modes differ. An **agent** is `execve`d, which
/// resets `PR_SET_DUMPABLE`, so its own `/proc` entry is the supervisor's to
/// write. A **warm-exec** sandbox's init is deliberately *not* dumpable — it
/// is a fork of the supervisor and still maps the supervisor's memory — so
/// its `/proc/<pid>/root` is root-owned and an unprivileged supervisor cannot
/// reach through it at all. There the request's own parked process is used
/// instead: it is the supervisor's child, it has not exec'd yet, and it sees
/// the same mount namespace. Any process in the sandbox reaches the same
/// `/run`.
///
/// Ownership needs no `chown`: the supervisor's host uid is exactly what the
/// sandbox's user namespace maps to the handler's uid, so a file this process
/// creates is the handler's file inside.
///
/// `own` is the request's own values — a runtime pool's request, carrying
/// the calling tenant's secrets — and `None` is a function's request, which
/// uses the values the function was served with. See [`Secrets::adopt`] for
/// why the former needs the zygote to itself.
pub(super) fn place_secrets<'a>(
    secrets: &'a Mutex<Secrets>,
    at: SecretsAt<'_>,
    own: Option<BTreeMap<String, String>>,
) -> Result<SecretsLease<'a>> {
    let mut guard = secrets.lock().expect("secrets");
    // Before the count, so a refused adoption leaves nothing to depart from
    // and — more to the point — never touches the values in use.
    if let Some(values) = own {
        let adopted = guard.adopt(values);
        drop(guard);
        adopted?;
        guard = secrets.lock().expect("secrets");
    }
    // Counted before writing, and the lease taken whatever happens next, so a
    // failed write still departs and the count stays honest.
    let write_now = guard.arrive();

    // Every write happens while the guard is held, and the **lease is not
    // created until the guard is gone**. Dropping a lease calls
    // `withdraw_secrets`, which locks this same mutex; a lease that came into
    // existence above an early `?` return was therefore dropped by a thread
    // already holding the lock, and `Mutex` is not reentrant. That is a
    // permanent self-deadlock, one leaked thread per request, and a client
    // that waits for ever.
    //
    // It only fires when a write *fails*, so it never showed in a container
    // running as root.
    let written = if write_now {
        write_secrets(&mut guard, at)
    } else {
        Ok(())
    };
    drop(guard);

    let lease = SecretsLease { secrets };
    // Safe now: this drops the lease with no lock held, so the count departs
    // and anything already written is taken away again.
    written?;
    Ok(lease)
}

/// Write one request's secret files. Called with the lock held; takes none.
fn write_secrets(secrets: &mut Secrets, at: SecretsAt<'_>) -> Result<()> {
    // Either route ends in one directory descriptor, and everything after is
    // the same: the files are created relative to it. Held from here until
    // the last request in flight is finished with them, because a path
    // through `/proc/<pid>/root` stops resolving the moment that process
    // exits — which for warm-exec is *before* the files come out.
    let dir: std::fs::File = match at {
        SecretsAt::Proc(pid) => {
            let path = secrets_dir(pid);
            std::fs::create_dir_all(&path).at(&path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                    .at(&path)?;
            }
            std::fs::File::open(&path).at(&path)?
        }
        // Already created, at mode 0700, by the sandbox's own init.
        SecretsAt::Dir(fd) => std::fs::File::from(
            fd.try_clone_to_owned()
                .map_err(|e| Error::primitive("dup", "the sandbox's /run/secrets", e))?,
        ),
    };

    // The directory is recorded **before** the files are written, so a write
    // that fails half way leaves something `unlink_all` can clean. It was set
    // afterwards until this was found: the second of three
    // writes failing left the first file on the tmpfs at mode 0400, `dir` was
    // `None` so nothing removed it, and every later request on that function
    // failed with EACCES trying to create a file that was already there.
    secrets.dir = Some(dir);
    let dir = secrets.dir.as_ref().expect("just set");

    let mut written = Vec::with_capacity(secrets.values.len());
    for (name, value) in &secrets.values {
        if let Err(e) = write_request_file_at(dir, name, value, "writing a secret into the sandbox")
        {
            // Undo this attempt rather than leaving a partial set: a handler
            // that received two of its three secrets is a worse failure than
            // one that received none and was told why.
            for done in &written {
                let _ = rustix::fs::unlinkat(dir, *done, rustix::fs::AtFlags::empty());
            }
            secrets.dir = None;
            return Err(e);
        }
        written.push(name.as_str());
    }
    Ok(())
}

/// Remove the secret files once nothing in flight needs them.
fn withdraw_secrets(secrets: &Mutex<Secrets>) {
    let mut guard = secrets.lock().expect("secrets");
    if guard.depart() {
        guard.unlink_all();
    }
    // After the files, because `unlink_all` needs the names: a pool zygote
    // keeps nothing of a tenant's once the request that brought it has gone.
    guard.forget_if_per_request();
}

/// The sandbox's `/run/secrets`, as seen from the host.
fn secrets_dir(init_pid: u32) -> PathBuf {
    PathBuf::from(format!("/proc/{init_pid}/root{SECRETS_DIR_IN_SANDBOX}"))
}

/// Create a request-scoped file readable by its owner and nobody else.
///
/// A secret value, or the script one request runs: both are written from
/// outside the sandbox into a directory inside it, both last exactly as long
/// as the requests that need them, and both are `0400`.
///
/// Created with the mode from the start rather than chmodded afterwards, so
/// there is no moment at which it is readable more widely — `/run` is a tmpfs
/// shared by everything in the sandbox.
pub(super) fn write_request_file_at(
    dir: &std::fs::File,
    name: &str,
    value: &str,
    what: &'static str,
) -> Result<()> {
    use rustix::fs::{Mode, OFlags};
    use std::io::Write as _;

    // `openat`, so the directory is named by the descriptor and not by a path
    // that may no longer resolve.
    //
    // `O_EXCL` after an `unlinkat`, not `O_TRUNC`. The old comment said
    // `O_TRUNC` was for "a file from a previous generation", which is not a
    // thing that happens — a rewarm gets a fresh tmpfs. What does happen is a
    // partially written set left by an earlier failure, and opening one of
    // those with `O_TRUNC` fails with EACCES because the file is 0400 and the
    // supervisor is not root. Removing first means the mode of whatever was
    // there cannot decide whether this request works.
    let _ = rustix::fs::unlinkat(dir, name, rustix::fs::AtFlags::empty());
    let fd = rustix::fs::openat(
        dir,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o400),
    )
    .map_err(|e| Error::primitive("openat", what, std::io::Error::from(e)))?;
    let mut file = std::fs::File::from(fd);
    file.write_all(value.as_bytes())
        .map_err(|e| Error::primitive("write", what, e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;

    /// A secret that cannot be written must fail the request, not wedge the
    /// supervisor.
    ///
    /// The bug this pins deadlocked a thread for ever: the lease was created
    /// above the `?`, so a failed write dropped it while its own mutex was
    /// still held. It never fired in a container running as root and fired on
    /// the first request on an ordinary user's machine.
    ///
    /// `init_pid` here is a pid that cannot exist, so `/proc/<pid>/root` is
    /// not there and the write fails the way a permission error would. The
    /// test is written to *finish*: a regression makes it hang rather than
    /// fail, so it runs on its own thread and the assertion is the join.
    #[test]
    fn a_secret_that_cannot_be_written_fails_the_request_instead_of_deadlocking() {
        let secrets = Arc::new(Mutex::new(Secrets {
            values: [("STRIPE_KEY".to_string(), "sk_test".to_string())]
                .into_iter()
                .collect(),
            in_flight: 0,
            dir: None,
            per_request: false,
        }));

        let attempt = {
            let secrets = Arc::clone(&secrets);
            std::thread::spawn(move || {
                // Twice: the first failure must not leave the mutex held, or
                // the second call is what hangs.
                let first = place_secrets(&secrets, SecretsAt::Proc(u32::MAX), None).is_err();
                let second = place_secrets(&secrets, SecretsAt::Proc(u32::MAX), None).is_err();
                (first, second)
            })
        };

        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        while !attempt.is_finished() {
            assert!(
                Instant::now() < deadline,
                "place_secrets deadlocked on the error path"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let (first, second) = attempt.join().expect("the thread did not panic");
        assert!(first, "an unwritable secret must be an error");
        assert!(second, "and the mutex must still be usable afterwards");

        // The count departed both times, so a later request is not told that
        // files it cannot see are already in place.
        assert_eq!(secrets.lock().expect("secrets").in_flight, 0);
    }

    /// The path that must keep working: no secrets to write is not a failure,
    /// and the lease still counts in and out.
    #[test]
    fn a_function_with_no_secrets_places_nothing_and_succeeds() {
        let secrets = Mutex::new(Secrets::default());
        {
            let _lease =
                place_secrets(&secrets, SecretsAt::Proc(u32::MAX), None).expect("nothing to write");
            assert_eq!(secrets.lock().expect("secrets").in_flight, 1);
        }
        assert_eq!(secrets.lock().expect("secrets").in_flight, 0);
    }

    /// A runtime pool's request: its own values go in with it, exist as
    /// files only while its lease lives, and are forgotten when it leaves —
    /// the zygote holds nothing of the tenant's afterwards.
    ///
    /// Through a directory descriptor, the route a held sandbox uses, which
    /// is also the one route that works on a machine with no `/proc`.
    #[test]
    fn a_pool_request_brings_its_own_secrets_and_takes_them_away() {
        let dir = tempfile::tempdir().expect("tempdir");
        let handle = std::fs::File::open(dir.path()).expect("open");
        let secrets = Mutex::new(Secrets::default());
        let own = BTreeMap::from([
            ("STRIPE_KEY".to_string(), "sk_acme".to_string()),
            ("DB_URL".to_string(), "postgres://acme".to_string()),
        ]);
        {
            let _lease = place_secrets(
                &secrets,
                SecretsAt::Dir(std::os::fd::AsFd::as_fd(&handle)),
                Some(own),
            )
            .expect("written");
            assert_eq!(
                std::fs::read_to_string(dir.path().join("STRIPE_KEY")).expect("read"),
                "sk_acme"
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join("DB_URL")).expect("read"),
                "postgres://acme"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(dir.path().join("STRIPE_KEY"))
                    .expect("stat")
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o400, "readable by the owner only");
            }
            let guard = secrets.lock().expect("secrets");
            assert_eq!(guard.in_flight, 1);
            assert!(guard.per_request);
        }
        assert!(
            !dir.path().join("STRIPE_KEY").exists(),
            "gone with the request"
        );
        assert!(!dir.path().join("DB_URL").exists());
        let guard = secrets.lock().expect("secrets");
        assert_eq!(guard.in_flight, 0);
        assert!(guard.values.is_empty(), "nothing of the tenant's is kept");
        assert!(!guard.per_request);

        // The next request — another tenant's — starts from nothing and gets
        // its own, not a mixture.
        drop(guard);
        let other = BTreeMap::from([("STRIPE_KEY".to_string(), "sk_beta".to_string())]);
        let _lease = place_secrets(
            &secrets,
            SecretsAt::Dir(std::os::fd::AsFd::as_fd(&handle)),
            Some(other),
        )
        .expect("written");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("STRIPE_KEY")).expect("read"),
            "sk_beta"
        );
        assert!(
            !dir.path().join("DB_URL").exists(),
            "acme's DB_URL is not beta's"
        );
    }

    /// The scheduler's promise, checked here rather than trusted: a request
    /// with its own secrets that finds the zygote shared is refused, and the
    /// values in use are not touched.
    #[test]
    fn a_pool_request_with_secrets_refuses_a_zygote_that_is_shared() {
        let secrets = Mutex::new(Secrets {
            values: BTreeMap::from([("STRIPE_KEY".to_string(), "sk_acme".to_string())]),
            in_flight: 1,
            dir: None,
            per_request: true,
        });
        let err = match place_secrets(
            &secrets,
            SecretsAt::Proc(u32::MAX),
            Some(BTreeMap::from([(
                "STRIPE_KEY".to_string(),
                "sk_beta".to_string(),
            )])),
        ) {
            Ok(_) => panic!("a shared zygote must be refused"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("already in flight"), "{err}");
        let guard = secrets.lock().expect("still usable");
        assert_eq!(guard.in_flight, 1, "the refused request never counted in");
        assert_eq!(
            guard.values["STRIPE_KEY"], "sk_acme",
            "acme's value is untouched"
        );
    }

    #[test]
    fn the_first_request_in_writes_and_the_last_one_out_removes() {
        let mut s = Secrets {
            values: BTreeMap::from([("KEY".to_string(), "v".to_string())]),
            in_flight: 0,
            dir: None,
            per_request: false,
        };
        assert!(s.arrive(), "first in: write");
        assert!(!s.arrive(), "second in: already there");
        assert!(!s.depart(), "one still needs them");
        assert!(s.depart(), "last out: remove");
    }

    #[test]
    fn a_function_with_no_secrets_never_touches_the_filesystem() {
        // The count still moves, because whether this request is "last out" is
        // only knowable if every arrival was counted — but no file is ever
        // written for nothing.
        let mut s = Secrets::default();
        assert!(!s.arrive());
        assert!(!s.depart());
        assert_eq!(s.in_flight, 0);
    }

    #[test]
    fn departing_more_than_arriving_cannot_underflow() {
        let mut s = Secrets {
            values: BTreeMap::from([("KEY".to_string(), "v".to_string())]),
            in_flight: 0,
            dir: None,
            per_request: false,
        };
        assert!(s.depart(), "at zero with values: remove is the safe answer");
        assert_eq!(s.in_flight, 0);
    }

    /// What these tests were written against, before a script became the
    /// other thing written into a sandbox the same way.
    fn write_secret_file_at(dir: &std::fs::File, name: &str, value: &str) -> Result<()> {
        write_request_file_at(dir, name, value, "writing a secret into the sandbox")
    }

    #[cfg(unix)]
    #[test]
    fn a_secret_file_is_owner_readable_from_the_moment_it_exists() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = std::fs::File::open(tmp.path()).expect("dirfd");
        write_secret_file_at(&dir, "STRIPE_KEY", "sk_live_...").expect("write");
        let path = tmp.path().join("STRIPE_KEY");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode, 0o400, "mode {mode:o}");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "sk_live_...");
    }

    #[cfg(unix)]
    #[test]
    fn rewriting_a_secret_replaces_it_entirely() {
        // A shorter value must not leave the tail of a longer one behind — a
        // rewarm writes over whatever the last generation left.
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = std::fs::File::open(tmp.path()).expect("dirfd");
        let path = tmp.path().join("KEY");
        write_secret_file_at(&dir, "KEY", "a-long-value").expect("write");
        write_secret_file_at(&dir, "KEY", "short").expect("rewrite");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "short");
    }

    /// A secret left behind by an earlier failure does not stop the next
    /// write.
    ///
    /// The file is `0400` and the supervisor is not root, so opening it with
    /// `O_TRUNC` fails with EACCES — and every later request on that function
    /// failed with it, for ever. Attempted with a real `0400` file
    /// rather than asserted about the flags, because the flags are not what
    /// broke: the mode of a file nobody expected to be there was.
    #[cfg(unix)]
    #[test]
    fn a_secret_left_by_a_failed_write_does_not_wedge_the_next_one() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = std::fs::File::open(tmp.path()).expect("dirfd");
        write_secret_file_at(&dir, "KEY", "from-a-failed-attempt").expect("write");

        write_secret_file_at(&dir, "KEY", "the-real-value")
            .expect("a 0400 file from an earlier attempt must not refuse the next write");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("KEY")).expect("read"),
            "the-real-value"
        );
    }

    /// A write that fails part way through leaves nothing behind.
    ///
    /// `write_secrets` records the directory *before* it writes, so the
    /// cleanup has somewhere to aim; it used to record it afterwards, and
    /// `unlink_all` then saw `dir == None` and removed nothing. The failure is
    /// provoked with a name that cannot be created.
    #[cfg(unix)]
    #[test]
    fn a_partly_written_set_of_secrets_is_removed_rather_than_left() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut secrets = Secrets::default();
        secrets.values.insert("AAA_GOOD".into(), "value".into());
        // A name with a slash cannot be created in this directory, and sorts
        // after the good one, so the first write succeeds and the second does
        // not — which is exactly the shape that left a file behind.
        secrets.values.insert("ZZZ/BAD".into(), "value".into());

        let dir = std::fs::File::open(tmp.path()).expect("dirfd");
        let err = {
            use std::os::fd::AsFd;
            write_secrets(&mut secrets, SecretsAt::Dir(dir.as_fd()))
                .expect_err("a name with a slash cannot be created")
        };
        assert!(format!("{err}").contains("secret"), "{err}");

        assert!(
            !tmp.path().join("AAA_GOOD").exists(),
            "the secret written before the failure was left behind"
        );
        assert!(
            secrets.dir.is_none(),
            "a failed placement must not look like a successful one"
        );
    }

    /// A directory named by a descriptor, which is the whole point: the path
    /// it was opened through can disappear and the writes still land.
    #[cfg(unix)]
    #[test]
    fn a_secret_is_written_through_the_descriptor_not_the_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inner = tmp.path().join("run-secrets");
        std::fs::create_dir(&inner).expect("mkdir");
        let dir = std::fs::File::open(&inner).expect("dirfd");

        // Rename the directory out from under the descriptor. A path-based
        // write would now fail or land in the wrong place.
        let moved = tmp.path().join("moved");
        std::fs::rename(&inner, &moved).expect("rename");

        write_secret_file_at(&dir, "KEY", "v").expect("write through the fd");
        assert_eq!(
            std::fs::read_to_string(moved.join("KEY")).expect("read"),
            "v"
        );
        assert!(!inner.exists(), "the old path really is gone");
    }
}
