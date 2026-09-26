// SPDX-License-Identifier: Apache-2.0
//! `zygo` — the CLI.
//!
//! A thin client over `zygo-core`. Everything interesting lives in
//! the library; this crate parses arguments, prints tables and maps errors onto
//! exit codes.

mod cli;
mod cmd;
mod output;
mod scope;
// Compiled everywhere and used on one platform: the rules about what
// forwards and where it runs are worth checking on every host, not only on
// the machine that can act on them. `scope` does the same in reverse.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod shim;
mod tty;

use clap::Parser;

use cli::{Cli, Command};

/// Die on a closed pipe the way every other Unix tool does.
///
/// Rust's runtime sets `SIGPIPE` to `SIG_IGN`, so a write to a pipe nobody is
/// reading returns `EPIPE`, `println!` panics on it, and this binary is built
/// with `panic = "abort"` — which is why `zygo doctor | head` printed
/// `Aborted` after perfectly good output. Restoring the default disposition
/// makes the process end quietly at the point the reader went away.
///
/// # Safety
///
/// Called before any thread exists, which is the only requirement.
#[cfg(unix)]
fn die_quietly_on_a_closed_pipe() {
    // SAFETY: `signal` with a valid number and the default disposition, called
    // from `main` before any thread exists.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn die_quietly_on_a_closed_pipe() {}

fn main() -> std::process::ExitCode {
    die_quietly_on_a_closed_pipe();
    let cli = Cli::parse();
    init_tracing(
        cli.verbose,
        cli.json,
        matches!(
            cli.command,
            Command::Supervisor(cli::SupervisorCommand::Run)
        ),
    );
    // May replace this process: see `scope`. Before anything is opened or
    // connected, so nothing has to survive the exec.
    scope::ensure_delegated(&cli);

    // On macOS almost everything belongs to a Linux VM, and the answer comes
    // back as that command's own exit status. Before `run`, because there is
    // no host to run it against.
    match shim::forward(&cli) {
        Ok(Some(code)) => return std::process::ExitCode::from(code),
        Ok(None) => {}
        Err(e) => {
            output::error(&e);
            return std::process::ExitCode::from(125u8);
        }
    }

    match run(&cli) {
        Ok(code) => std::process::ExitCode::from(code),
        Err(e) => {
            output::error(&e);
            // Preserve the library's exit-code convention where there is one:
            // 2 for a spec problem, 125 when the host cannot run sandboxes,
            // 137 for a deadline.
            //
            // Searched through the whole chain, not just the outermost error
            // `downcast_ref` looks only at the top, so any
            // `.context("…")` on the way up — and the CLI adds them freely —
            // turned a spec error into a bare 1. A script checking for 2 saw
            // it only when nobody had added context, which is the worst kind
            // of contract: one that holds until someone improves a message.
            let code = e
                .chain()
                .find_map(|cause| cause.downcast_ref::<zygo_core::Error>())
                .map(|e| e.exit_code())
                .unwrap_or(1);
            std::process::ExitCode::from(code as u8)
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<u8> {
    match &cli.command {
        Command::Doctor { fix, yes } => cmd::doctor::run(cli, *fix, *yes),
        Command::Pull { image, platform } => cmd::image::pull(cli, image, platform.as_deref()),
        Command::Images => cmd::image::list(cli),
        Command::Image(sub) => cmd::image::maintain(cli, sub),
        Command::Spec { file, command } => cmd::spec::run(cli, file.path(), command),
        Command::Run(args) => cmd::run::run(cli, args),
        Command::Backend(sub) => cmd::backend::run(cli, sub),
        Command::Bench(sub) => cmd::bench::run(cli, sub),
        Command::Completion { shell } => {
            use clap::CommandFactory;
            clap_complete::generate(*shell, &mut Cli::command(), "zygo", &mut std::io::stdout());
            Ok(0)
        }

        // The warm-path commands: everything that needs a supervisor.
        Command::Serve(args) => cmd::supervisor::serve(cli, args),
        Command::Exec(args) => cmd::supervisor::exec(cli, args),
        Command::Ps => cmd::supervisor::ps(cli),
        Command::Logs {
            name,
            follow,
            tail,
            failed,
        } => cmd::logs::run(cli, name, *follow, *tail, *failed),
        Command::Stop { name, all } => cmd::supervisor::stop(cli, name.as_deref(), *all),
        Command::Supervisor(command) => cmd::supervisor::supervisor(cli, command),
        Command::Top { interval, once } => cmd::top::run(cli, *interval, *once),
        Command::Stats { name } => cmd::stats::run(cli, name.as_deref()),
        Command::Api(args) => cmd::api::run(cli, args),
        Command::Token(command) => cmd::token::run(cli, command),
        Command::Secrets(command) => cmd::secrets::run(cli, command),
        Command::Mcp(args) => cmd::mcp::run(cli, args),
        Command::Up { file, relock } => cmd::supervisor::up(cli, file.path(), *relock),
        Command::Down { file } => cmd::supervisor::down(cli, file.path()),
        Command::Login {
            registry,
            username,
            password_stdin,
        } => cmd::login::run(cli, registry, username.as_deref(), *password_stdin),
        Command::Agent(crate::cli::AgentCommand::Test {
            binary,
            script,
            script_spawn,
            pool_script,
            args,
        }) => cmd::agent::test(
            cli,
            binary,
            script.as_deref(),
            script_spawn.as_deref(),
            pool_script.as_deref(),
            args,
        ),
        Command::Shell { name, command } => cmd::shell::run(cli, name, command),
    }
}

fn init_tracing(verbose: u8, json: bool, keep_time: bool) {
    use tracing_subscriber::{EnvFilter, fmt};

    let default = match verbose {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_env("ZYGO_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let builder = fmt().with_env_filter(filter).with_writer(std::io::stderr);

    if json {
        builder.json().init();
    } else if keep_time {
        // The supervisor's log is read after the fact, against a client's
        // timings: a line without a time cannot say whether a gate was closed
        // before a request arrived or after it gave up, which is exactly the
        // question the blue/green investigation needed answered.
        builder.with_target(false).init();
    } else {
        builder.without_time().with_target(false).init();
    }
}

#[cfg(test)]
mod tests {
    /// The library's exit code survives a `.context()` on the way up.
    ///
    /// The mapping used `downcast_ref` on the outermost error, so any
    /// context added between the library and `main` replaced 2 or 125 with a
    /// bare 1 — and the CLI adds context on most paths.
    #[test]
    fn a_wrapped_library_error_keeps_its_exit_code() {
        let spec = zygo_core::Error::Spec(zygo_core::spec::SpecError::Invalid {
            field: "fn.x.mem".into(),
            message: "too small".into(),
            remedy: String::new(),
        });
        let bare_code = spec.exit_code();
        assert_eq!(bare_code, 2, "a spec error is exit 2");

        let wrapped: anyhow::Error =
            anyhow::Error::from(spec).context("while resolving the spec file");
        let found = wrapped
            .chain()
            .find_map(|cause| cause.downcast_ref::<zygo_core::Error>())
            .map(|e| e.exit_code())
            .unwrap_or(1);
        assert_eq!(found, 2, "the context hid the spec error's exit code");

        // Two layers, because one is the easy case.
        let deeper = anyhow::Error::from(zygo_core::Error::Spec(
            zygo_core::spec::SpecError::Invalid {
                field: "fn.x.mem".into(),
                message: "too small".into(),
                remedy: String::new(),
            },
        ))
        .context("one")
        .context("two");
        let found = deeper
            .chain()
            .find_map(|cause| cause.downcast_ref::<zygo_core::Error>())
            .map(|e| e.exit_code())
            .unwrap_or(1);
        assert_eq!(found, 2);
    }
}
