// SPDX-License-Identifier: Apache-2.0
//! The supervisor's own tests: the registry, the idle policy, the
//! rewarm backoff, ownership, and the control connection itself.

use std::collections::BTreeMap;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::dispatch::dispatch;
use super::lifecycle::{Backoff, Launcher};
use super::listener::{current_uid, peer_uid, peer_verdict, reject_foreign_peer};
use super::registry::{Cold, Sources};
use super::*;
use crate::paths::Paths;
use crate::pool::Status;
use crate::spec::{Layer, ResolveOptions, ResolvedFn};

fn paths(dir: &tempfile::TempDir) -> Paths {
    Paths::rooted(dir.path())
}

// --- idle tiering ------------------------------------------------------
//
// These exercise the policy without a sandbox, which is what `tier_idle`
// being a callable pass rather than a thread is for: a test that had to
// sleep out a ten-minute `idle_timeout` would not be written.

#[test]
fn nothing_to_tier_is_not_an_event() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    assert!(supervisor.tier_idle().is_empty());
}

#[test]
fn a_cold_function_is_still_listed_and_still_registered() {
    // The distinction that matters to someone reading `ps`: a function that
    // went quiet is not a function that was never served, and `exec` on it
    // is a cold start rather than an error.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");

    let status = Status {
        name: "resize".into(),
        tenant: crate::spec::resolve::DEFAULT_TENANT.into(),
        image: String::new(),
        state: crate::sandbox::SandboxState::Cold,
        runtime: "python/3.12".into(),
        rss_kb: 0,
        imports_ms: 0.0,
        requests: 17,
        failures: 1,
    };
    supervisor.cold.lock().expect("cold").insert(
        "resize".into(),
        Cold {
            resolved: resolved_fn("resize"),
            secrets: BTreeMap::new(),
            sources: Sources::default(),
            last_status: status.clone(),
            since: Instant::now(),
        },
    );

    let listed = supervisor.list();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].state, crate::sandbox::SandboxState::Cold);
    assert_eq!(
        listed[0].requests, 17,
        "going cold must not reset someone's counters"
    );
}

#[test]
fn stopping_a_cold_function_deregisters_it() {
    // Without this it would come back on the next request, having been
    // explicitly stopped.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    supervisor.cold.lock().expect("cold").insert(
        "resize".into(),
        Cold {
            resolved: resolved_fn("resize"),
            secrets: BTreeMap::new(),
            sources: Sources::default(),
            last_status: cold_status("resize"),
            since: Instant::now(),
        },
    );

    assert_eq!(
        supervisor.stop(Some("resize"), true),
        Ok(Response::Stopped {
            names: vec!["resize".into()]
        })
    );
    assert!(supervisor.list().is_empty());
    // And it is gone for good, not merely asleep.
    assert!(matches!(
        supervisor.stop(Some("resize"), true),
        Err(Response::Error {
            code: ControlError::NotFound,
            ..
        })
    ));
}

#[test]
fn stop_all_takes_cold_functions_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    for name in ["a", "b"] {
        supervisor.cold.lock().expect("cold").insert(
            name.into(),
            Cold {
                resolved: resolved_fn(name),
                secrets: BTreeMap::new(),
                sources: Sources::default(),
                last_status: cold_status(name),
                since: Instant::now(),
            },
        );
    }
    assert_eq!(
        supervisor.stop(None, true),
        Ok(Response::Stopped {
            names: vec!["a".into(), "b".into()]
        })
    );
    assert!(supervisor.list().is_empty());
}

#[test]
fn a_cold_function_that_cannot_be_rewarmed_reports_why() {
    // There is no image in this scratch store, so waking it fails — the
    // point is that the caller is told, rather than getting "not found" for
    // a function that is registered.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    supervisor.cold.lock().expect("cold").insert(
        "resize".into(),
        Cold {
            resolved: resolved_fn("resize"),
            secrets: BTreeMap::new(),
            sources: Sources::default(),
            last_status: cold_status("resize"),
            since: Instant::now(),
        },
    );
    let response = supervisor
        .exec(
            "resize",
            serde_json::Value::Null,
            Duration::from_secs(1),
            None,
        )
        .expect_err("no image to wake it from");
    assert!(
        matches!(
            response,
            Response::Error {
                code: ControlError::WarmFailed,
                ..
            }
        ),
        "{response:?}"
    );
}

#[test]
fn the_tiering_thresholds_are_the_resolved_specs() {
    // The policy has no thresholds of its own; a function with an
    // `idle_timeout` of an hour must not be paused because the supervisor
    // felt like it.
    let f = resolved_fn("resize");
    assert_eq!(f.idle_timeout.get(), Duration::from_secs(600));
    assert_eq!(f.cold_after.get(), Duration::from_secs(3600));
    assert!(
        f.cold_after.get() >= f.idle_timeout.get(),
        "resolution should already have rejected this"
    );
}

fn resolved_fn(name: &str) -> ResolvedFn {
    crate::spec::resolve_standalone(
        name,
        &Layer {
            image: Some("python:3.12-slim".into()),
            entry: Some(std::path::PathBuf::from("/tmp/handler.py")),
            ..Default::default()
        },
        &ResolveOptions::default(),
    )
    .expect("resolve")
}

fn cold_status(name: &str) -> Status {
    Status {
        name: name.into(),
        tenant: crate::spec::resolve::DEFAULT_TENANT.into(),
        image: String::new(),
        state: crate::sandbox::SandboxState::Cold,
        runtime: "python/3.12".into(),
        rss_kb: 0,
        imports_ms: 0.0,
        requests: 0,
        failures: 0,
    }
}

// --- rewarm backoff ---------------------------------------------------

#[test]
fn the_first_rewarm_after_a_crash_is_immediate() {
    // The target is "back within 500 ms",
    // and a warm-up is ~300 ms. Any delay before the first attempt spends
    // budget that is not there.
    assert_eq!(Backoff::delay(0), Duration::ZERO);
    assert_eq!(Backoff::default().ready(Instant::now()), Ok(()));
}

#[test]
fn repeated_failures_back_off_exponentially_up_to_a_ceiling() {
    assert_eq!(Backoff::delay(1), REWARM_BACKOFF_BASE);
    assert_eq!(Backoff::delay(2), REWARM_BACKOFF_BASE * 2);
    assert_eq!(Backoff::delay(3), REWARM_BACKOFF_BASE * 4);
    assert_eq!(Backoff::delay(10), REWARM_BACKOFF_MAX, "capped");
    // A function that has been failing all day must not overflow its way
    // back to retrying instantly.
    assert_eq!(Backoff::delay(u32::MAX), REWARM_BACKOFF_MAX);
    assert_eq!(Backoff::delay(64), REWARM_BACKOFF_MAX);
}

#[test]
fn a_failing_function_is_made_to_wait_and_told_how_long() {
    let now = Instant::now();
    let backoff = Backoff {
        failures: 3,
        last_attempt: Some(now),
    };
    let left = backoff.ready(now).expect_err("too soon");
    assert!(left <= Backoff::delay(3) && !left.is_zero(), "{left:?}");

    // Once the delay has passed, another attempt is allowed.
    let later = now + Backoff::delay(3);
    assert_eq!(backoff.ready(later), Ok(()));
}

#[test]
fn a_first_attempt_is_never_held_back_however_long_ago_it_was() {
    // `last_attempt: None` is the "never tried" case, which must not be
    // confused with "tried a long time ago".
    let backoff = Backoff {
        failures: 9,
        last_attempt: None,
    };
    assert_eq!(backoff.ready(Instant::now()), Ok(()));
}

#[test]
fn a_deliberate_serve_clears_the_crash_history() {
    // Someone who has just edited their handler and run `zygo serve` should
    // not be made to wait out a backoff earned by the previous version.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    supervisor.rewarms.lock().expect("rewarms").insert(
        "resize".into(),
        Backoff {
            failures: 5,
            last_attempt: Some(Instant::now()),
        },
    );

    // The serve itself fails (no image in this scratch store), but the
    // history is cleared first, which is the behaviour under test.
    let layer = Layer {
        image: Some("python:3.12-slim".into()),
        ..Default::default()
    };
    let _ = supervisor.serve(
        "resize",
        None,
        &layer,
        &ResolveOptions::default(),
        BTreeMap::new(),
        false,
    );
    assert!(
        !supervisor
            .rewarms
            .lock()
            .expect("rewarms")
            .contains_key("resize"),
        "the backoff survived an explicit serve"
    );
}

// --- the launcher thread ---------------------------------------------

#[test]
fn every_job_runs_on_one_thread_that_is_not_the_callers() {
    // The property the whole design rests on: `PDEATHSIG` is delivered when
    // the creating *thread* exits, so sandbox creation must never happen on
    // a thread that comes and goes with a CLI command.
    let launcher = Launcher::new().expect("launcher");
    let mut seen = std::collections::HashSet::new();
    let mut callers = std::collections::HashSet::new();

    for _ in 0..8 {
        let where_it_ran = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    callers.insert(std::thread::current().id());
                    launcher
                        .run(|| std::thread::current().id())
                        .expect("job ran")
                })
                .join()
                .expect("join")
        });
        seen.insert(where_it_ran);
    }

    assert_eq!(seen.len(), 1, "work was spread over {} threads", seen.len());
    let launcher_thread = seen.into_iter().next().expect("one");
    assert!(
        !callers.contains(&launcher_thread),
        "work ran on the calling thread, which is exactly what must not happen"
    );
    assert_ne!(launcher_thread, std::thread::current().id());
}

#[test]
fn the_launcher_thread_outlives_the_caller() {
    let launcher = Launcher::new().expect("launcher");
    // Deliberately a thread that ends straight after asking.
    let first = std::thread::scope(|scope| {
        scope
            .spawn(|| launcher.run(|| std::thread::current().id()).expect("job"))
            .join()
            .expect("join")
    });
    let second = launcher.run(|| std::thread::current().id()).expect("job");
    assert_eq!(
        first, second,
        "the thread that ran the first job must still be there for the second"
    );
}

#[test]
fn a_job_returns_its_value_to_the_caller() {
    let launcher = Launcher::new().expect("launcher");
    assert_eq!(launcher.run(|| 6 * 7).expect("job"), 42);
    assert_eq!(
        launcher
            .run(|| "from the launcher".to_string())
            .expect("job"),
        "from the launcher"
    );
    // Results carry errors through unchanged, which is how a failed warm-up
    // reaches the client.
    let failed: std::result::Result<(), String> = launcher
        .run(|| Err("no such image".to_string()))
        .expect("job");
    assert_eq!(failed, Err("no such image".into()));
}

/// The mechanism itself, with no Zygo in the way: does a child outlive the
/// thread that created it when that thread is the launcher's?
#[cfg(target_os = "linux")]
#[test]
fn a_child_created_through_the_launcher_survives_the_caller() {
    use std::time::Duration;

    /// Fork a child that sets `PDEATHSIG` and then sleeps. Returns its pid.
    fn spawn_child() -> i32 {
        let mut fds = [0i32; 2];
        // SAFETY: `fds` is a live array of two ints, which is what pipe wants.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0, "pipe");
        // SAFETY: fork has no preconditions.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork");
        if pid == 0 {
            // SAFETY: the child is single-threaded and does nothing that
            // allocates before it exits.
            unsafe {
                libc::close(fds[0]);
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                libc::write(fds[1], b"1".as_ptr().cast(), 1);
                libc::close(fds[1]);
                libc::sleep(30);
                libc::_exit(0);
            }
        }
        // SAFETY: the parent owns both descriptors and the buffer is live.
        unsafe {
            libc::close(fds[1]);
            let mut byte = 0u8;
            libc::read(fds[0], (&raw mut byte).cast(), 1);
            libc::close(fds[0]);
        }
        pid
    }

    /// Still running, as opposed to killed and waiting to be reaped.
    fn running(pid: i32) -> bool {
        let mut status = 0;
        // SAFETY: `status` is live; WNOHANG makes this non-blocking.
        let got = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
        got == 0
    }

    let launcher = Launcher::new().expect("launcher");
    let through_launcher = std::thread::scope(|scope| {
        scope
            .spawn(|| launcher.run(spawn_child).expect("job"))
            .join()
            .expect("join")
    });

    // And the same thing done the wrong way, as the control.
    let on_a_short_lived_thread = std::thread::spawn(spawn_child).join().expect("join");

    std::thread::sleep(Duration::from_millis(300));
    assert!(
        running(through_launcher),
        "a sandbox created through the launcher must survive the command that asked for it"
    );
    assert!(
        !running(on_a_short_lived_thread),
        "the control did not reproduce the failure, so this test proves nothing"
    );

    // SAFETY: killing a process this test created.
    unsafe {
        libc::kill(through_launcher, libc::SIGKILL);
        let mut status = 0;
        libc::waitpid(through_launcher, &raw mut status, 0);
    }
}

#[test]
fn binding_creates_an_owner_only_socket_and_a_pid_file() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let paths = paths(&dir);
    let listener = Listener::bind(&paths).expect("bind");

    let socket = paths.supervisor_sock();
    assert!(socket.exists());
    let mode = std::fs::metadata(&socket)
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "the control socket is owner-only");

    let runtime = std::fs::metadata(paths.runtime()).expect("stat");
    assert_eq!(
        runtime.permissions().mode() & 0o777,
        0o700,
        "and so is the directory holding it"
    );

    let pid = std::fs::read_to_string(paths.supervisor_pid()).expect("pid file");
    assert_eq!(pid.trim(), std::process::id().to_string());
    drop(listener);
}

#[test]
fn dropping_the_listener_takes_the_socket_with_it() {
    // A leftover socket makes the next supervisor think it has a conflict.
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = paths(&dir);
    let listener = Listener::bind(&paths).expect("bind");
    assert!(paths.supervisor_sock().exists());
    drop(listener);
    assert!(!paths.supervisor_sock().exists());
    assert!(!paths.supervisor_pid().exists());
}

#[test]
fn a_second_supervisor_is_refused_while_the_first_is_listening() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = paths(&dir);
    let _first = Listener::bind(&paths).expect("bind");

    let err = Listener::bind(&paths).expect_err("the socket is taken");
    assert!(
        err.to_string().contains("already listening"),
        "unhelpful message: {err}"
    );
}

#[test]
fn a_socket_left_by_a_dead_supervisor_is_reclaimed() {
    // The case that would otherwise need a manual `rm`: the process died
    // without running its `Drop`, so the file is there but nothing answers.
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = paths(&dir);
    paths.ensure().expect("ensure");

    let socket = paths.supervisor_sock();
    let orphan = UnixListener::bind(&socket).expect("bind");
    drop(orphan); // closes the listening fd, leaves the path behind
    assert!(socket.exists(), "the stale socket file is still there");

    let listener = Listener::bind(&paths).expect("a stale socket is not a conflict");
    assert!(UnixStream::connect(&socket).is_ok(), "and this one answers");
    drop(listener);
}

#[test]
fn hello_is_required_before_anything_that_changes_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let mut greeted = false;

    for request in [
        Request::List,
        Request::Stop {
            name: None,
            runtimes: true,
        },
        Request::Exec {
            name: "x".into(),
            event: serde_json::Value::Null,
            timeout_ms: 1,
            tenant: None,
            key: None,
            stream: false,
            workspace: None,
        },
    ] {
        let response = dispatch(&supervisor, request, &mut greeted);
        assert!(
            matches!(
                response,
                Response::Error {
                    code: ControlError::BadMessage,
                    ..
                }
            ),
            "{response:?}"
        );
    }
    assert!(!greeted);
}

#[test]
fn a_client_speaking_another_control_version_is_turned_away() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let mut greeted = false;

    let response = dispatch(
        &supervisor,
        Request::Hello {
            control: CONTROL_VERSION + 1,
            client: "zygo from the future".into(),
        },
        &mut greeted,
    );
    match response {
        Response::Error {
            code: ControlError::VersionMismatch,
            message,
        } => {
            assert!(message.contains(&CONTROL_VERSION.to_string()), "{message}");
        }
        other => panic!("{other:?}"),
    }
    assert!(!greeted, "a rejected client is not greeted");
}

#[test]
fn a_greeting_unlocks_the_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let mut greeted = false;

    let response = dispatch(
        &supervisor,
        Request::Hello {
            control: CONTROL_VERSION,
            client: "zygo test".into(),
        },
        &mut greeted,
    );
    assert!(matches!(response, Response::Welcome { .. }));
    assert!(greeted);

    assert!(matches!(
        dispatch(&supervisor, Request::Ping, &mut greeted),
        Response::Pong
    ));
    assert!(matches!(
        dispatch(&supervisor, Request::List, &mut greeted),
        Response::Functions { functions } if functions.is_empty()
    ));
}

/// Two tenants, byte-identical scripts, one file.
///
/// Deduplication is not a saving here, it is the property that makes a
/// content-addressed store safe to share: a name derived from the bytes
/// cannot be claimed, so the second tenant to register a script gets the
/// first one's file *because it is the same file*, and neither can put
/// different bytes under a digest the other is running. The tenants cannot
/// reach each other through it either, and that is proved where it can be
/// — in the agent, by `test_two_scripts_in_one_pool_cannot_see_each_other`.
#[test]
fn two_tenants_registering_the_same_script_get_one_file() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let mut greeted = true;
    let source = "def handler(event):\n    return {'ok': True}\n";

    let first = dispatch(
        &supervisor,
        Request::PutScript {
            source: source.into(),
            tenant: None,
        },
        &mut greeted,
    );
    let second = dispatch(
        &supervisor,
        Request::PutScript {
            source: source.into(),
            tenant: None,
        },
        &mut greeted,
    );

    let (digest, size) = match (&first, &second) {
        (
            Response::Script {
                digest: a,
                size,
                existed: false,
            },
            Response::Script {
                digest: b,
                existed: true,
                ..
            },
        ) => {
            assert_eq!(a, b, "the same bytes must have the same name");
            (a.clone(), *size)
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(size as usize, source.len());
    assert_eq!(
        crate::scripts::ScriptStore::new(supervisor.paths())
            .list()
            .expect("list")
            .len(),
        1,
        "two registrations left two files"
    );

    // And it is there to be found, by name, by either of them.
    match dispatch(
        &supervisor,
        Request::GetScript {
            digest: digest.clone(),
        },
        &mut greeted,
    ) {
        Response::Script { digest: back, .. } => assert_eq!(back, digest),
        other => panic!("{other:?}"),
    }

    assert!(matches!(
        dispatch(
            &supervisor,
            Request::DeleteScript {
                digest: digest.clone()
            },
            &mut greeted
        ),
        Response::Ok
    ));
    assert!(matches!(
        dispatch(&supervisor, Request::GetScript { digest }, &mut greeted),
        Response::Error {
            code: ControlError::NotFound,
            ..
        }
    ));
}

/// A tenant owns the scripts it registered, and deleting it takes them.
#[test]
fn deleting_a_tenant_takes_its_scripts_and_leaves_the_shared_ones() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let mut greeted = true;
    let put = |source: &str, tenant: &str, greeted: &mut bool| -> String {
        match dispatch(
            &supervisor,
            Request::PutScript {
                source: source.into(),
                tenant: Some(tenant.into()),
            },
            greeted,
        ) {
            Response::Script { digest, .. } => digest,
            other => panic!("{other:?}"),
        }
    };

    dispatch(
        &supervisor,
        Request::CreateTenant { id: "a".into() },
        &mut greeted,
    );
    dispatch(
        &supervisor,
        Request::CreateTenant { id: "b".into() },
        &mut greeted,
    );
    let shared = put("shared = 1\n", "a", &mut greeted);
    assert_eq!(shared, put("shared = 1\n", "b", &mut greeted));
    let only_a = put("only_a = 1\n", "a", &mut greeted);

    match dispatch(
        &supervisor,
        Request::DeleteTenant { id: "a".into() },
        &mut greeted,
    ) {
        Response::Tenants {
            removed_scripts, ..
        } => assert_eq!(removed_scripts, vec![only_a.clone()]),
        other => panic!("{other:?}"),
    }

    let store = crate::scripts::ScriptStore::new(supervisor.paths());
    let parse = crate::scripts::ScriptDigest::parse;
    assert!(
        !store.contains(&parse(&only_a).unwrap()),
        "the script only the deleted tenant had is still on disk"
    );
    assert!(
        store.contains(&parse(&shared).unwrap()),
        "a script another tenant still refers to was deleted"
    );
}

/// A tenant may only reach the functions that are theirs.
///
/// The other half of `a_tenant_cannot_run_another_tenants_script_by_digest`:
/// a pool call is refused by digest, and a warm function is refused by
/// name. Both answer `not_found`, because the difference between "no such
/// function" and "not yours" is a fact about another customer.
///
/// Registered cold, which is the part of the registry a test can build
/// without a kernel: `owned_by` reads the same two maps a warm one is in.
#[test]
fn a_tenant_cannot_reach_another_tenants_function_by_name() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");

    let mut resolved = resolved_fn("resize");
    resolved.tenant = "acme".into();
    supervisor.cold.lock().expect("cold").insert(
        "resize".into(),
        Cold {
            resolved,
            secrets: BTreeMap::new(),
            sources: Sources::default(),
            last_status: cold_status("resize"),
            since: Instant::now(),
        },
    );

    // The operator reaches everything on their own host.
    assert!(supervisor.owned_by("resize", None).is_ok());
    assert!(supervisor.owned_by("resize", Some("acme")).is_ok());

    for stranger in ["globex", crate::spec::resolve::DEFAULT_TENANT] {
        let refused = supervisor
            .owned_by("resize", Some(stranger))
            .expect_err("another tenant's function");
        match refused {
            Response::Error { code, message } => {
                assert_eq!(code, ControlError::NotFound);
                assert!(
                    !message.contains("acme"),
                    "the refusal named the owner: {message}"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    // And a name nobody served is refused identically, which is what
    // makes the two indistinguishable from outside.
    let message = |response| match response {
        Response::Error { message, .. } => message,
        other => panic!("{other:?}"),
    };
    let missing = message(
        supervisor
            .owned_by("resize-2", Some("acme"))
            .expect_err("no such function"),
    );
    let stranger = message(
        supervisor
            .owned_by("resize", Some("globex"))
            .expect_err("somebody else's"),
    );
    assert_eq!(
        missing.replace("resize-2", "resize"),
        stranger,
        "the two refusals differ, so one can be told from the other"
    );
}

/// A token is minted once, resolves, and stops resolving when revoked.
#[test]
fn a_token_round_trips_through_the_control_protocol() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let mut greeted = true;

    let (id, secret) = match dispatch(
        &supervisor,
        Request::MintToken {
            tenant: Some("acme".into()),
        },
        &mut greeted,
    ) {
        Response::Tokens { tokens, secret } => (
            tokens.first().expect("a token").id.clone(),
            secret.expect("the secret is on the mint"),
        ),
        other => panic!("{other:?}"),
    };

    // Minting for a tenant registered it, so an embedder onboarding a
    // customer makes one call rather than two in the right order.
    match dispatch(
        &supervisor,
        Request::Tenants {
            id: Some("acme".into()),
        },
        &mut greeted,
    ) {
        Response::Tenants { tenants, .. } => assert_eq!(tenants.len(), 1),
        other => panic!("{other:?}"),
    }

    let store = crate::tokens::Tokens::new(supervisor.paths());
    assert_eq!(
        store
            .resolve(&secret)
            .expect("resolve")
            .and_then(|t| t.tenant().map(str::to_string)),
        Some("acme".into())
    );

    // A listing never carries a secret, whatever it carries.
    match dispatch(&supervisor, Request::Tokens, &mut greeted) {
        Response::Tokens { tokens, secret } => {
            assert_eq!(tokens.len(), 1);
            assert!(secret.is_none(), "a listing answered with a secret");
        }
        other => panic!("{other:?}"),
    }

    assert!(matches!(
        dispatch(
            &supervisor,
            Request::RevokeToken { id: id.clone() },
            &mut greeted
        ),
        Response::Tokens { .. }
    ));
    assert!(
        store.resolve(&secret).expect("resolve").is_none(),
        "a revoked token still resolves"
    );
    assert!(matches!(
        dispatch(&supervisor, Request::RevokeToken { id }, &mut greeted),
        Response::Tokens { .. }
    ));
}

/// Deleting a tenant takes their keys with their code.
#[test]
fn deleting_a_tenant_revokes_their_tokens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let mut greeted = true;
    let mint = |tenant: &str, greeted: &mut bool| match dispatch(
        &supervisor,
        Request::MintToken {
            tenant: Some(tenant.into()),
        },
        greeted,
    ) {
        Response::Tokens { secret, .. } => secret.expect("a secret"),
        other => panic!("{other:?}"),
    };
    let acme = mint("acme", &mut greeted);
    let globex = mint("globex", &mut greeted);

    dispatch(
        &supervisor,
        Request::DeleteTenant { id: "acme".into() },
        &mut greeted,
    );

    let store = crate::tokens::Tokens::new(supervisor.paths());
    assert!(
        store.resolve(&acme).expect("resolve").is_none(),
        "the deleted tenant's key still opens the door"
    );
    assert!(
        store.resolve(&globex).expect("resolve").is_some(),
        "another tenant's token went with it"
    );
}

/// A tenant may only name a script it registered.
///
/// A digest is not a capability — anybody holding the bytes can compute
/// one — so a tenant that learns another's digest must not be able to run
/// it. The answer is the same one a digest nobody registered gets, which
/// is deliberate: "exists but not yours" is a fact about another tenant.
#[test]
fn a_tenant_cannot_run_another_tenants_script_by_digest() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let mut greeted = true;
    dispatch(
        &supervisor,
        Request::CreateTenant { id: "a".into() },
        &mut greeted,
    );
    dispatch(
        &supervisor,
        Request::CreateTenant { id: "b".into() },
        &mut greeted,
    );
    let digest = match dispatch(
        &supervisor,
        Request::PutScript {
            source: "secret = 1\n".into(),
            tenant: Some("a".into()),
        },
        &mut greeted,
    ) {
        Response::Script { digest, .. } => digest,
        other => panic!("{other:?}"),
    };

    // No pool is registered, so a request that got past the ownership
    // check would fail with `no runtime named` instead — which is exactly
    // how this test tells the two refusals apart.
    let by_stranger = dispatch(
        &supervisor,
        Request::ExecScript {
            runtime: "nowhere".into(),
            script: crate::protocol::Script {
                path: None,
                source: None,
                digest: Some(digest.clone()),
                entry_point: None,
            },
            event: serde_json::Value::Null,
            timeout_ms: 1_000,
            tenant: Some("b".into()),
            key: None,
            stream: false,
            workspace: None,
        },
        &mut greeted,
    );
    match by_stranger {
        Response::Error { code, message } => {
            assert_eq!(code, ControlError::NotFound);
            assert!(message.contains("no script"), "{message}");
        }
        other => panic!("{other:?}"),
    }

    // And the owner gets past the ownership check, to the missing pool.
    let by_owner = dispatch(
        &supervisor,
        Request::ExecScript {
            runtime: "nowhere".into(),
            script: crate::protocol::Script {
                path: None,
                source: None,
                digest: Some(digest),
                entry_point: None,
            },
            event: serde_json::Value::Null,
            timeout_ms: 1_000,
            tenant: Some("a".into()),
            key: None,
            stream: false,
            workspace: None,
        },
        &mut greeted,
    );
    match by_owner {
        Response::Error { message, .. } => {
            assert!(message.contains("no runtime"), "{message}");
        }
        other => panic!("{other:?}"),
    }
}

/// A digest becomes a path, so it is parsed before it is used as one.
#[test]
fn a_digest_that_is_not_one_is_refused_rather_than_looked_up() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let mut greeted = true;

    for bad in ["../../etc/passwd", "sha256:nope", ""] {
        let response = dispatch(
            &supervisor,
            Request::GetScript { digest: bad.into() },
            &mut greeted,
        );
        assert!(
            matches!(
                response,
                Response::Error {
                    code: ControlError::BadSpec,
                    ..
                }
            ),
            "{bad:?} → {response:?}"
        );
    }
}

#[test]
fn calling_a_function_that_was_never_served_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");

    let response = supervisor
        .exec(
            "nope",
            serde_json::Value::Null,
            Duration::from_secs(1),
            None,
        )
        .expect_err("no such function");
    match response {
        Response::Error {
            code: ControlError::NotFound,
            message,
        } => assert!(message.contains("zygo serve"), "no next step: {message}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn stopping_a_function_that_was_never_served_says_so() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    match supervisor.stop(Some("nope"), true) {
        Err(Response::Error {
            code: ControlError::NotFound,
            message,
        }) => assert!(
            message.contains("no function or runtime pool named `nope`"),
            "the CLI's stop looked in both registries, and says so: {message}"
        ),
        other => panic!("expected not_found, got {other:?}"),
    }
    match supervisor.stop(Some("nope"), false) {
        Err(Response::Error {
            code: ControlError::NotFound,
            message,
        }) => assert_eq!(
            message, "no function named `nope`",
            "`DELETE /fn/<name>` asks about functions only"
        ),
        other => panic!("expected not_found, got {other:?}"),
    }
    // Stopping everything when there is nothing is not an error: `zygo stop
    // --all` is something you run to be sure, not to be told off.
    assert_eq!(
        supervisor.stop(None, true),
        Ok(Response::Stopped { names: vec![] })
    );
}

/// A registered pool with no zygote in it: enough for the registry to
/// hold, and nothing for a Mac to have to fork.
fn empty_pool(name: &str, secrets: &[&str]) -> Arc<runtime::RuntimePool> {
    let resolved = crate::spec::Spec::default()
        .resolve_runtime(
            name,
            &Layer {
                image: Some("python:3.12-slim".into()),
                runtime: Some(crate::spec::Runtime::Builtin(
                    crate::spec::BuiltinRuntime::Python,
                )),
                secrets: Some(secrets.iter().map(|s| s.to_string()).collect()),
                ..Default::default()
            },
            &ResolveOptions::default(),
        )
        .expect("a pool resolves");
    Arc::new(runtime::RuntimePool {
        deps: None,
        gate: Gate::named(&format!("runtime.{name}"), 4, 16),
        zygotes: Mutex::new(Vec::new()),
        resolved,
        registered: Instant::now(),
    })
}

#[test]
fn stop_by_name_reaches_a_pool_and_says_which_kind_went() {
    // The gap this closes: `zygo stop py312` answered "no function named
    // py312" while the pool ran, and the only way to end it was `--all`
    // or the HTTP route.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    supervisor
        .runtimes
        .lock()
        .expect("runtimes")
        .insert("py312".into(), empty_pool("py312", &[]));

    assert_eq!(
        supervisor.stop(Some("py312"), true),
        Ok(Response::Stopped {
            names: vec!["runtime.py312".into()]
        })
    );
    assert!(supervisor.runtimes().is_empty(), "the pool is deregistered");
    assert!(matches!(
        supervisor.stop(Some("py312"), true),
        Err(Response::Error {
            code: ControlError::NotFound,
            ..
        })
    ));
}

#[test]
fn delete_fn_does_not_reach_a_pool_of_the_same_name() {
    // `DELETE /fn/<name>` is a route about one function; a pool is shared
    // by every tenant on the host and is not something that route may
    // take down by accident.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    supervisor
        .runtimes
        .lock()
        .expect("runtimes")
        .insert("py312".into(), empty_pool("py312", &[]));

    assert!(matches!(
        supervisor.stop(Some("py312"), false),
        Err(Response::Error {
            code: ControlError::NotFound,
            ..
        })
    ));
    assert_eq!(supervisor.runtimes().len(), 1, "the pool is untouched");
}

#[test]
fn a_function_and_a_pool_under_one_name_both_stop() {
    // `stop` means "forget this name". The clash is the operator's own —
    // only an operator serves — and stopping half of it would leave them
    // guessing which half.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    supervisor.cold.lock().expect("cold").insert(
        "shared".into(),
        Cold {
            resolved: resolved_fn("shared"),
            secrets: BTreeMap::new(),
            sources: Sources::default(),
            last_status: cold_status("shared"),
            since: Instant::now(),
        },
    );
    supervisor
        .runtimes
        .lock()
        .expect("runtimes")
        .insert("shared".into(), empty_pool("shared", &[]));

    assert_eq!(
        supervisor.stop(Some("shared"), true),
        Ok(Response::Stopped {
            names: vec!["shared".into(), "runtime.shared".into()]
        })
    );
    assert!(supervisor.list().is_empty());
    assert!(supervisor.runtimes().is_empty());
}

#[test]
fn stop_all_takes_the_pools_with_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    supervisor.cold.lock().expect("cold").insert(
        "a".into(),
        Cold {
            resolved: resolved_fn("a"),
            secrets: BTreeMap::new(),
            sources: Sources::default(),
            last_status: cold_status("a"),
            since: Instant::now(),
        },
    );
    for name in ["py312", "node22"] {
        supervisor
            .runtimes
            .lock()
            .expect("runtimes")
            .insert(name.into(), empty_pool(name, &[]));
    }

    assert_eq!(
        supervisor.stop(None, true),
        Ok(Response::Stopped {
            names: vec!["a".into(), "runtime.node22".into(), "runtime.py312".into()]
        })
    );
    assert!(supervisor.runtimes().is_empty());
    // Functions only, when asked for functions only.
    supervisor
        .runtimes
        .lock()
        .expect("runtimes")
        .insert("py312".into(), empty_pool("py312", &[]));
    assert_eq!(
        supervisor.stop(None, false),
        Ok(Response::Stopped { names: vec![] })
    );
    assert_eq!(supervisor.runtimes().len(), 1);
}

// --- secrets for a runtime pool ---------------------------------------

fn with_secret_key(dir: &tempfile::TempDir) -> Supervisor {
    let mut supervisor = Supervisor::new(paths(dir)).expect("supervisor");
    let key =
        crate::secrets::SecretKey::parse(&crate::secrets::SecretKey::generate().expect("keygen"))
            .expect("parse");
    supervisor.secret_key = Some(std::sync::Arc::new(key));
    supervisor
}

#[test]
fn a_pool_that_names_secrets_needs_a_key_on_this_host() {
    // Refused when the pool is registered, not on its first request: a
    // pool whose every request would fail is a pool nobody meant to
    // start. Before any zygote is warmed, which is also what lets this
    // run on a machine with no sandbox.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let layer = Layer {
        image: Some("python:3.12-slim".into()),
        runtime: Some(crate::spec::Runtime::Builtin(
            crate::spec::BuiltinRuntime::Python,
        )),
        secrets: Some(vec!["STRIPE_KEY".into()]),
        ..Default::default()
    };
    match supervisor.serve_runtime("py312", None, &layer, &ResolveOptions::default(), None) {
        Err(Response::Error {
            code: ControlError::BadSpec,
            message,
        }) => {
            assert!(message.contains("runtime.py312.secrets"), "{message}");
            assert!(message.contains(crate::secrets::KEY_ENV), "{message}");
        }
        other => panic!("expected bad_spec, got {other:?}"),
    }
    assert!(supervisor.runtimes().is_empty(), "nothing was registered");
}

#[test]
fn a_pool_request_whose_tenant_lacks_a_named_secret_is_refused_before_any_fork() {
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = with_secret_key(&dir);
    supervisor.runtimes.lock().expect("runtimes").insert(
        "py312".into(),
        empty_pool("py312", &["STRIPE_KEY", "DB_URL"]),
    );
    let store = supervisor.secret_store().expect("a key was set");
    store
        .put("acme", "STRIPE_KEY", "sk_live_acme")
        .expect("put");

    let script = crate::protocol::Script::inline("def handler(e): return e");
    let refused = supervisor
        .exec_script(
            "py312",
            script.clone(),
            serde_json::Value::Null,
            Duration::from_secs(1),
            Some("acme"),
            None,
        )
        .expect_err("DB_URL is not in acme's store");
    match refused {
        Response::Error {
            code: ControlError::BadSpec,
            message,
        } => {
            assert!(message.contains("DB_URL"), "{message}");
            assert!(message.contains("acme"), "{message}");
            assert!(
                !message.contains("STRIPE_KEY"),
                "only the missing one is named: {message}"
            );
            assert!(!message.contains("sk_live"), "never a value: {message}");
            assert!(
                message.contains("PUT /tenants/acme/secrets/DB_URL"),
                "and the remedy: {message}"
            );
        }
        other => panic!("expected bad_spec, got {other:?}"),
    }

    // The operator's own requests read the operator's own store — the
    // `default` tenant — and are held to the same rule.
    let refused = supervisor
        .exec_script(
            "py312",
            script.clone(),
            serde_json::Value::Null,
            Duration::from_secs(1),
            None,
            None,
        )
        .expect_err("the default tenant has nothing stored");
    match refused {
        Response::Error {
            code: ControlError::BadSpec,
            message,
        } => assert!(
            message.contains("`default`") && message.contains("STRIPE_KEY, DB_URL"),
            "{message}"
        ),
        other => panic!("expected bad_spec, got {other:?}"),
    }

    // With both stored, the request gets past the secrets and on to a
    // zygote — which this machine cannot warm, so the failure moves to
    // where a fork would be. That is the boundary under test.
    store.put("acme", "DB_URL", "postgres://x").expect("put");
    let past = supervisor
        .exec_script(
            "py312",
            script,
            serde_json::Value::Null,
            Duration::from_secs(1),
            Some("acme"),
            None,
        )
        .expect_err("no zygote can be warmed here");
    match past {
        Response::Error { code, message } => {
            assert_ne!(code, ControlError::BadSpec, "{message}");
            assert!(!message.contains("sk_live"), "never a value: {message}");
            assert!(!message.contains("postgres://"), "never a value: {message}");
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

#[test]
fn a_pool_without_secrets_asks_no_store_for_anything() {
    // A host with no key serves a pool that names no secrets, and its
    // requests never look for the store — the same bargain a function
    // with no secrets gets.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let pool = empty_pool("py312", &[]);
    assert_eq!(
        supervisor.secrets_for_pool_request(&pool, Some("acme")),
        Ok(None)
    );
}

#[test]
fn a_secret_the_spec_names_but_nobody_supplied_is_a_spec_problem() {
    // Reported before any sandbox exists, and naming the variable: the
    // alternative is a handler that fails opening a file that is not there.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let layer = Layer {
        image: Some("python:3.12-slim".into()),
        entry: Some(std::path::PathBuf::from("/tmp/handler.py")),
        secrets: Some(vec!["STRIPE_KEY".into(), "DB_URL".into()]),
        ..Default::default()
    };
    let response = supervisor
        .serve(
            "pay",
            None,
            &layer,
            &ResolveOptions::default(),
            BTreeMap::from([("STRIPE_KEY".to_string(), "sk".to_string())]),
            false,
        )
        .expect_err("DB_URL has no value");
    match response {
        Response::Error {
            code: ControlError::BadSpec,
            message,
        } => {
            assert!(message.contains("DB_URL"), "{message}");
            assert!(
                !message.contains("STRIPE_KEY"),
                "only the missing one: {message}"
            );
            assert!(
                !message.contains("sk"),
                "a value must never be echoed: {message}"
            );
        }
        other => panic!("{other:?}"),
    }
}

// --- blue/green ------------------------------------------------------

fn handler_in(dir: &tempfile::TempDir, body: &str) -> (Layer, ResolvedFn) {
    let path = dir.path().join("handler.py");
    std::fs::write(&path, body).expect("write handler");
    let layer = Layer {
        image: Some("python:3.12-slim".into()),
        entry: Some(path),
        ..Default::default()
    };
    let resolved =
        crate::spec::resolve_standalone("f", &layer, &ResolveOptions::default()).expect("resolve");
    (layer, resolved)
}

#[test]
fn sources_follow_the_bytes_on_disk_not_the_path() {
    // The spec cannot see an edit to the handler; this is what can.
    let dir = tempfile::tempdir().expect("tempdir");
    let (_, before) = handler_in(&dir, "def handler(e): return 1\n");
    let at_start = Sources::of(&before);
    assert_eq!(at_start, Sources::of(&before), "hashing is deterministic");

    std::fs::write(dir.path().join("handler.py"), "def handler(e): return 2\n").expect("edit");
    assert_ne!(
        at_start,
        Sources::of(&before),
        "an edited handler is a change"
    );

    std::fs::remove_file(dir.path().join("handler.py")).expect("remove");
    let gone = Sources::of(&before);
    assert_ne!(at_start, gone, "a deleted handler is a change");
    assert_eq!(
        gone.0[0].1, None,
        "and is recorded as unreadable, not as empty"
    );
}

#[test]
fn a_cold_function_is_unchanged_only_while_its_inputs_are() {
    // Registration is compared, not just the name: `up` after editing the
    // handler, changing a secret, or changing the spec must replace, and
    // `up` after none of those must not.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let (_, resolved) = handler_in(&dir, "def handler(e): return 1\n");
    let secrets = BTreeMap::from([("KEY".to_string(), "v1".to_string())]);
    supervisor.cold.lock().expect("cold").insert(
        "f".into(),
        Cold {
            resolved: resolved.clone(),
            secrets: secrets.clone(),
            sources: Sources::of(&resolved),
            last_status: cold_status("f"),
            since: Instant::now(),
        },
    );

    assert!(supervisor.is_registered_as("f", &resolved, &secrets));

    let other_secret = BTreeMap::from([("KEY".to_string(), "v2".to_string())]);
    assert!(!supervisor.is_registered_as("f", &resolved, &other_secret));

    let mut other_spec = resolved.clone();
    other_spec.concurrency += 1;
    assert!(!supervisor.is_registered_as("f", &other_spec, &secrets));

    assert!(
        !supervisor.is_registered_as("g", &resolved, &secrets),
        "a different name"
    );

    std::fs::write(dir.path().join("handler.py"), "def handler(e): return 2\n").expect("edit");
    assert!(
        !supervisor.is_registered_as("f", &resolved, &secrets),
        "the handler changed on disk"
    );
}

#[test]
fn a_serve_that_would_change_nothing_does_not_touch_the_registry() {
    // No image in this store, so an actual warm-up would fail loudly; a
    // request for the identical function does not get that far. It does,
    // however, try to *wake* the function, which is a warm-up — so the
    // failure it reports is the wake-up's, not a "bad spec".
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let (layer, resolved) = handler_in(&dir, "def handler(e): return 1\n");
    supervisor.cold.lock().expect("cold").insert(
        "f".into(),
        Cold {
            resolved: resolved.clone(),
            secrets: BTreeMap::new(),
            sources: Sources::of(&resolved),
            last_status: cold_status("f"),
            since: Instant::now(),
        },
    );

    let response = supervisor
        .serve(
            "f",
            None,
            &layer,
            &ResolveOptions::default(),
            BTreeMap::new(),
            true,
        )
        .expect_err("waking needs an image this store does not have");
    assert!(
        matches!(
            response,
            Response::Error {
                code: ControlError::WarmFailed,
                ..
            }
        ),
        "{response:?}"
    );
    assert!(
        supervisor.cold.lock().expect("cold").contains_key("f"),
        "a failed wake-up leaves the registration in place"
    );
}

#[test]
fn a_bad_spec_is_reported_as_a_spec_problem_not_a_warm_failure() {
    // The distinction matters to the person reading: one is their file,
    // the other is the machine.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");

    let layer = Layer {
        image: Some("python:3.12-slim".into()),
        // `network = "host"` without `--allow-host-net` is refused at
        // resolve time.
        network: Some(crate::spec::Network::Host),
        ..Default::default()
    };
    let response = supervisor
        .serve(
            "x",
            None,
            &layer,
            &ResolveOptions::default(),
            BTreeMap::new(),
            false,
        )
        .expect_err("resolution should fail");
    assert!(matches!(
        response,
        Response::Error {
            code: ControlError::BadSpec,
            ..
        }
    ));
}

#[test]
fn shutdown_is_acknowledged_before_the_loop_stops() {
    // The client needs its answer: a supervisor that closed the socket
    // first would look like a crash.
    let dir = tempfile::tempdir().expect("tempdir");
    let supervisor = Supervisor::new(paths(&dir)).expect("supervisor");
    let mut greeted = true;
    assert_eq!(
        dispatch(&supervisor, Request::Shutdown, &mut greeted),
        Response::Ok
    );
    assert!(!supervisor.is_stopping(), "`dispatch` only answers");
    supervisor.shutdown();
    assert!(supervisor.is_stopping());
}

#[test]
fn the_peer_check_accepts_our_own_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let paths = paths(&dir);
    let listener = Listener::bind(&paths).expect("bind");
    let client = UnixStream::connect(paths.supervisor_sock()).expect("connect");
    let (server, _) = listener.listener.accept().expect("accept");

    assert_eq!(peer_uid(&server), Some(current_uid()));
    assert!(
        reject_foreign_peer(&server).is_none(),
        "our own uid must not be refused"
    );
    drop(client);
}

/// A peer whose credentials cannot be read is refused, not admitted.
///
/// The syscall is the input to this decision, so the decision is what is
/// tested; a socket on which `SO_PEERCRED` genuinely fails cannot be
/// conjured from inside a process that owns both ends. The bug this pins
/// was that the syscall and the policy shared one `?`, so "could
/// not read the credentials" and "the credentials are ours" returned the
/// same answer: allowed.
#[test]
fn a_peer_that_cannot_be_identified_is_refused() {
    let ours = 1000;

    let unknown = peer_verdict(ours, None).expect("an unidentifiable peer is refused");
    match unknown {
        Response::Error { code, message } => {
            assert_eq!(code, ControlError::Unauthorised);
            assert!(message.contains("cannot read the credentials"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }

    let stranger = peer_verdict(ours, Some(1001)).expect("another user is refused");
    assert!(matches!(
        stranger,
        Response::Error {
            code: ControlError::Unauthorised,
            ..
        }
    ));

    // The positive case, so neither refusal above can be satisfied by a
    // check that refuses everything.
    assert!(peer_verdict(ours, Some(ours)).is_none());
}
