//! `zygo` — the CLI.
//!
//! A thin client over `zygo-core` (ADR-008). Everything interesting lives in
//! the library; this crate parses arguments, prints tables and maps errors onto
//! exit codes.

mod cli;
mod cmd;
mod output;
mod scope;
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
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn die_quietly_on_a_closed_pipe() {}

fn main() -> std::process::ExitCode {
    die_quietly_on_a_closed_pipe();
    let cli = Cli::parse();
    init_tracing(cli.verbose, cli.json);
    // May replace this process: see `scope`. Before anything is opened or
    // connected, so nothing has to survive the exec.
    scope::ensure_delegated(&cli);

    match run(&cli) {
        Ok(code) => std::process::ExitCode::from(code),
        Err(e) => {
            output::error(&e);
            // Preserve the library's exit-code convention where there is one:
            // 2 for a spec problem, 125 when the host cannot run sandboxes.
            let code = e
                .downcast_ref::<zygo_core::Error>()
                .map(|e| e.exit_code())
                .unwrap_or(1);
            std::process::ExitCode::from(code as u8)
        }
    }
}

fn run(cli: &Cli) -> anyhow::Result<u8> {
    match &cli.command {
        Command::Doctor => cmd::doctor::run(cli),
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

        // Phase 2 onwards. Each says which phase it belongs to rather than
        // failing as an unknown command, so `--help` stays honest.
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
        Command::Top => pending("zygo top", "3"),
        Command::Stats { .. } => pending("zygo stats", "3"),
        Command::Api(args) => cmd::api::run(cli, args),
        Command::Up { file, relock } => cmd::supervisor::up(cli, file.path(), *relock),
        Command::Down { file } => cmd::supervisor::down(cli, file.path()),
        Command::Login { .. } => pending("zygo login", "1.2"),
        Command::Agent(crate::cli::AgentCommand::Test { binary, args }) => {
            cmd::agent::test(cli, binary, args)
        }
        Command::Shell { name, command } => cmd::shell::run(cli, name, command),
    }
}

/// A command that is designed but not built yet. Points at the plan rather than
/// pretending the feature does not exist.
fn pending(what: &str, phase: &str) -> anyhow::Result<u8> {
    anyhow::bail!(
        "{what} is not implemented yet (todo.md, phase {phase})\n  \
         → available today: zygo doctor, pull, images, run --dry-run, spec"
    )
}

fn init_tracing(verbose: u8, json: bool) {
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
    } else {
        builder.without_time().with_target(false).init();
    }
}
