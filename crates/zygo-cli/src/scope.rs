// SPDX-License-Identifier: Apache-2.0
//! Getting into a cgroup Zygo can actually build under.
//!
//! On a systemd machine an ssh login sits in a `session-N.scope`. systemd owns
//! that directory, it is not delegated, and an unprivileged process cannot
//! create a cgroup inside it — so a sandbox started from there has nowhere to
//! put its limits and refuses to run. The controllers *are* delegated to the
//! user manager; it is the session's own scope that is a dead end.
//!
//! The fix a user can apply by hand is to start Zygo inside a transient scope
//! of its own:
//!
//! ```text
//! systemd-run --user --scope -p Delegate=yes -- zygo run …
//! ```
//!
//! Making people type that for every command is not a fix, it is a footnote.
//! So Zygo does it itself: when the cgroup it finds cannot take a child, and
//! `systemd-run` is available, it re-executes itself inside one. `podman` has
//! done the same thing for the same reason for years.
//!
//! Only the commands that build a sandbox are moved. `zygo ps` does not need
//! a cgroup and should not pay a process spawn for one.
//!
//! `zygo bench` is in the list for a reason worth stating: every mode of it
//! warms a sandbox of its own, so leaving it out made the one command that
//! demonstrates the warm path the first one to fail on an ordinary systemd
//! login — with a cgroup error, from a benchmark.
//!
//! # What this costs, and why it is still here
//!
//! It is the largest single cost on the one-shot path. On an idle Ubuntu 24.04
//! VM, kernel 6.8, `zygo run python:3.12-slim python3 -c pass` with the image
//! already in the store, decomposed:
//!
//! | | p50 |
//! |---|---|
//! | `/bin/true` | 0.30 ms |
//! | `systemd-run --user --scope … -- true` | 5.14 ms |
//! | `zygo --version` | 3.23 ms |
//! | `systemd-run --user --scope … -- zygo --version` | 13.72 ms |
//! | `systemd-run --user --scope … -- zygo run …` | 41.21 ms |
//! | **`zygo run …`** (this file makes the scope) | **50.31 ms** |
//!
//! Two things are worth reading off that table. Creating the scope costs about
//! 5 ms; but running the *same* binary inside a brand-new scope costs 13.7 ms
//! against 3.2 ms outside one, so a fresh cgroup is about 10 ms of overhead
//! before Zygo has done anything. And the cgroup work itself is not where it
//! goes: timed step by step inside a scope, every `mkdir` and every
//! `subtree_control` write is under 0.15 ms, while *migrating a process into a
//! freshly created cgroup* is 5.6 ms on its own.
//!
//! ## The obvious fix, and why it is impossible
//!
//! Give `zygo.slice` a home that outlives the command. `user@$UID.service` is
//! the natural one: systemd starts it with `Delegate=`, so the directory, its
//! `cgroup.procs` and its `cgroup.subtree_control` all belong to the user, it
//! holds no processes directly, and its controllers are already enabled. This
//! was built and tried, and the layout works — `zygo.slice/tenants/<name>`
//! gets `memory.max`, `pids.max` and `cpu.max`, and they are writable.
//!
//! What does not work is *getting there*. cgroup v2's delegation containment
//! rule says a process may be migrated only by a writer with write access to
//! the destination's `cgroup.procs` **and to the `cgroup.procs` of the common
//! ancestor of source and destination**. The source is
//! `user-$UID.slice/session-N.scope`, the destination is under
//! `user-$UID.slice/user@$UID.service`, so the common ancestor is
//! `user-$UID.slice` — `root:root 644` on both hosts this was checked on
//! (Ubuntu 24.04 aarch64 / 6.8, and a Raspberry Pi 5 on 6.5). The write
//! returns `EACCES`, and it is meant to: the rule exists so a delegatee cannot
//! move processes out of its own subtree.
//!
//! Asking systemd to do the move instead — `systemd-run --slice=zygo.slice` —
//! puts the scope inside a persistent slice but still pays for `systemd-run`
//! and still creates a fresh scope, which is where the 10 ms is. It buys
//! nothing.
//!
//! Caching the `zygo doctor` host probe was tried too, on the theory that
//! `zygo run` did it twice. An interleaved A/B of 90 runs each: 44.92 ms
//! against 45.33 ms — no difference. The cache was kept for the supervisor,
//! which probes once per function warmed, and not for this.
//!
//! ## What does work: a supervisor
//!
//! A cgroup that is already delegated and already built — which is exactly
//! what the **supervisor** holds. So when one is running, `zygo run` hands
//! the sandbox to it instead of building its own: the client sends the spec
//! and its flags, passes its three streams over `SCM_RIGHTS`, forwards its
//! terminal's signals, and waits (`cmd/run.rs`, `supervisor::run`). None of
//! this file happens, because [`ensure_delegated`] asks first whether a
//! supervisor will take the run.
//!
//! On the same VM, the same command, forty runs a round, two rounds:
//!
//! | | p50 | p90 |
//! |---|---|---|
//! | `zygo run …`, this file makes the scope | 45.9 / 43.0 ms | 50.1 / 49.5 ms |
//! | `zygo run …`, a supervisor takes it | 30.4 / 29.0 ms | 32.9 / 33.8 ms |
//!
//! Fifteen milliseconds, a third of the command, and every one of them was
//! the scope, its second `zygo`, and a cgroup tree built to be thrown away.
//! What is left is the client (~3 ms to start), the sandbox (~8 ms in the
//! supervisor) and `python3 -c pass` itself (~13 ms).
//!
//! An embedder never saw any of this: it calls a warm function, and its
//! supervisor paid for its cgroup once at start-up. It was `zygo run` at a
//! terminal that paid every time, and now only on a machine where nothing
//! else is running. `docs/book/25-performance.md` has the numbers.

use crate::cli::{Cli, Command, SupervisorCommand};

/// Set in the re-executed process, so a scope that is *still* not usable
/// fails with Zygo's own message instead of forking for ever.
#[cfg(target_os = "linux")]
const MARKER: &str = "ZYGO_IN_SCOPE";

/// Whether this command builds a sandbox, and so needs a cgroup of its own.
///
/// Only acted on where cgroups exist; kept everywhere so the list itself is
/// tested on every host rather than only on Linux.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn needs_a_cgroup(command: &Command) -> bool {
    matches!(
        command,
        Command::Run(_)
            // It compiles a Python image's bytecode in a sandbox of its own
            // (`zygo_core::bytecode`), and from an ssh session that failed
            // for want of a cgroup and left the job to the first run.
            | Command::Pull { .. }
            | Command::Serve(_)
            | Command::Up { .. }
            | Command::Bench(_)
            | Command::Supervisor(SupervisorCommand::Run)
    )
}

/// Move into a delegated scope if this process is not in one already.
///
/// Returns normally when nothing needs doing — which is the common case: a
/// container, a systemd service, or a session someone has already delegated.
/// Otherwise it **replaces this process**, so anything after the call runs in
/// the new scope.
#[cfg(target_os = "linux")]
pub fn ensure_delegated(cli: &Cli) {
    use std::os::unix::process::CommandExt;

    if !needs_a_cgroup(&cli.command) || std::env::var_os(MARKER).is_some() {
        return;
    }
    // A running supervisor makes the question moot for a one-shot run:
    // `zygo run` hands the sandbox to it (`cmd/run.rs`) and builds no cgroup
    // here at all. Asked before the probe, because the probe is exactly what
    // a transient scope would then be built to satisfy — and the scope, its
    // second `zygo`, and the cgroup tree built and torn down per command are
    // the 34 ms this exists to avoid.
    if let Command::Run(args) = &cli.command
        && run_will_use_the_supervisor(cli, args)
    {
        return;
    }
    let Some(current) = current_cgroup() else {
        return;
    };
    // Asked of where the sandbox will actually go, which is not always here:
    // from a cgroup that also holds its embedder, `zygo.slice` goes to the top
    // of the delegated tree instead (`Hierarchy::discover`), and a scope would
    // only be paid for — every run, measured at about 10 ms of CPU each.
    if zygo_core::cgroup::Hierarchy::discover().is_ok_and(|h| h.usable_from_here()) {
        return;
    }
    // Attempted, not read: the whole point is that `cgroup.controllers` says
    // yes here and `mkdir` says no.
    if zygo_core::cgroup::probe_delegation(&current).is_ok() {
        return;
    }
    let Some(systemd_run) = which("systemd-run") else {
        // No systemd: let the sandbox fail with the real reason and the
        // two-line fix, rather than inventing one.
        return;
    };
    let Ok(exe) = std::env::current_exe() else {
        return;
    };

    tracing::debug!(
        cgroup = %current.display(),
        "this cgroup cannot hold a sandbox; re-executing in a transient scope"
    );

    let mut command = std::process::Command::new(systemd_run);
    command
        .args(["--user", "--scope", "-p", "Delegate=yes", "-q", "--"])
        .arg(&exe)
        .args(std::env::args_os().skip(1))
        .env(MARKER, "1");
    // `exec` and not `spawn`: stdio, the exit status and any signal the user
    // sends all belong to the command they typed, not to a wrapper.
    let error = command.exec();
    // Only reachable if the exec itself failed. Say so and carry on: the
    // sandbox will fail next with a better message than this one could give.
    tracing::debug!("could not start systemd-run: {error}");
}

#[cfg(not(target_os = "linux"))]
pub fn ensure_delegated(_cli: &Cli) {}

/// Whether this `zygo run` is going to be started by a supervisor rather than
/// here — the same test `cmd/run.rs` makes, asked earlier.
///
/// The isolation is read the way `run` will resolve it: the flag, else the
/// spec's default, else `ns`. A `vm` or `gvisor` run stays local and still
/// needs its own cgroup, and skipping the scope for one of those would trade
/// 34 ms for a run that cannot start.
#[cfg(target_os = "linux")]
fn run_will_use_the_supervisor(cli: &Cli, args: &crate::cli::RunArgs) -> bool {
    use zygo_core::spec::{Isolation, Spec};

    if args.dry_run || args.tty {
        return false;
    }
    let from_spec = Spec::discover(args.spec_file.path())
        .ok()
        .flatten()
        .and_then(|spec| spec.defaults.isolation);
    let isolation = args
        .sandbox
        .isolation
        .or(from_spec)
        .unwrap_or(Isolation::Ns);
    isolation == Isolation::Ns
        && zygo_core::supervisor::client::Client::connect(&crate::cmd::paths(cli)).is_ok()
}

/// This process's cgroup directory, from `/proc/self/cgroup`.
#[cfg(target_os = "linux")]
fn current_cgroup() -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
    Some(std::path::Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/')))
}

#[cfg(target_os = "linux")]
fn which(binary: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn command_of(args: &[&str]) -> Command {
        Cli::try_parse_from(args).expect("parse").command
    }

    /// Only what builds a sandbox is worth a process spawn.
    #[test]
    fn only_the_commands_that_build_a_sandbox_need_a_scope() {
        for args in [
            vec!["zygo", "run", "alpine:3", "true"],
            vec!["zygo", "serve", "h.py", "--name", "x"],
            vec!["zygo", "up"],
            vec!["zygo", "supervisor", "run"],
            vec!["zygo", "pull", "python:3.12-slim"],
            // Every bench mode warms a sandbox, so every one of them needs a
            // cgroup it can build under.
            vec!["zygo", "bench", "warm"],
            vec!["zygo", "bench", "cold"],
            vec!["zygo", "bench", "load"],
        ] {
            assert!(
                needs_a_cgroup(&command_of(&args)),
                "{args:?} builds a sandbox"
            );
        }

        for args in [
            vec!["zygo", "ps"],
            vec!["zygo", "doctor"],
            vec!["zygo", "stop", "--all"],
            vec!["zygo", "logs", "x"],
            vec!["zygo", "exec", "x", "{}"],
            vec!["zygo", "images"],
            vec!["zygo", "supervisor", "status"],
        ] {
            assert!(
                !needs_a_cgroup(&command_of(&args)),
                "{args:?} only talks to a supervisor or reads state"
            );
        }
    }

    /// The guard against re-executing for ever. A scope that is still not
    /// usable has to produce Zygo's own error, not another `systemd-run`.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_already_in_a_scope_is_left_alone() {
        // This test sets the variable the function reads, which nothing else
        // in the suite looks at — but not in its own process: cargo runs the
        // tests of one binary as threads of one process.
        // SAFETY: `set_var` races only with a C `getenv` on another thread;
        // Rust's own `std::env` readers take the same lock. Not verified in
        // this pass: cargo runs tests on several threads, and no audit was made
        // of libc calls in this suite that read the environment.
        unsafe { std::env::set_var(MARKER, "1") };
        let cli = Cli::try_parse_from(["zygo", "run", "alpine:3", "true"]).expect("parse");
        // Returns rather than replacing this process, which is the assertion:
        // reaching the next line at all is the pass.
        ensure_delegated(&cli);
        // SAFETY: as above.
        unsafe { std::env::remove_var(MARKER) };
    }
}
