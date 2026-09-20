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
    let Some(current) = current_cgroup() else {
        return;
    };
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
        // Safe: this test runs in its own process and sets the variable the
        // function reads, which nothing else in the suite looks at.
        unsafe { std::env::set_var(MARKER, "1") };
        let cli = Cli::try_parse_from(["zygo", "run", "alpine:3", "true"]).expect("parse");
        // Returns rather than replacing this process, which is the assertion:
        // reaching the next line at all is the pass.
        ensure_delegated(&cli);
        unsafe { std::env::remove_var(MARKER) };
    }
}
