//! `zygo` — the CLI.
//!
//! A thin client over `zygo-core` (ADR-008). Everything interesting lives in
//! the library; this crate parses arguments, prints tables and maps errors onto
//! exit codes.

mod cli;
mod cmd;
mod output;
mod tty;

use clap::Parser;

use cli::{Cli, Command};

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose, cli.json);

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

        // Phase 2 onwards. Each says which phase it belongs to rather than
        // failing as an unknown command, so `--help` stays honest.
        Command::Serve(args) => cmd::supervisor::serve(cli, args),
        Command::Exec(args) => cmd::supervisor::exec(cli, args),
        Command::Ps => cmd::supervisor::ps(cli),
        Command::Logs { .. } => pending("zygo logs", "2.2"),
        Command::Stop { name, all } => cmd::supervisor::stop(cli, name.as_deref(), *all),
        Command::Supervisor(command) => cmd::supervisor::supervisor(cli, command),
        Command::Top => pending("zygo top", "3"),
        Command::Stats { .. } => pending("zygo stats", "3"),
        Command::Api => pending("zygo api", "2.8"),
        Command::Up { .. } => pending("zygo up", "3"),
        Command::Down { .. } => pending("zygo down", "3"),
        Command::Login { .. } => pending("zygo login", "1.2"),
        Command::Agent(_) => pending("zygo agent test", "2.1"),
        Command::Shell { .. } => pending("zygo shell", "3"),
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
