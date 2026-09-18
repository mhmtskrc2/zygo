//! `zygo serve`, `exec`, `ps`, `stop`, and the supervisor process behind them.
//!
//! Every command here is the same shape: connect to the supervisor, send one
//! request, print the answer. That thinness is ADR-008 — the library is the
//! product, and a platform embedding `zygo_core` gets the same behaviour
//! without a CLI in the way.

use std::sync::Arc;

use anyhow::Context;
use zygo_core::spec::Spec;
use zygo_core::supervisor::client::Client;
use zygo_core::supervisor::{ControlError, Listener, Request, Response, Supervisor};

use crate::cli::{Cli, ExecArgs, ServeArgs, SupervisorCommand};
use crate::output::{self, Style};

/// `zygo supervisor <cmd>`.
pub fn supervisor(cli: &Cli, command: &SupervisorCommand) -> anyhow::Result<u8> {
    match command {
        SupervisorCommand::Run => run_supervisor(cli),
        SupervisorCommand::Status => status(cli),
    }
}

/// Run the supervisor in the foreground until something tells it to stop.
fn run_supervisor(cli: &Cli) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    // Bound before the `Supervisor` is built, so "the socket is taken" is
    // reported in milliseconds rather than after warming anything.
    let listener = Listener::bind(&paths).context("could not take the control socket")?;
    let supervisor = Arc::new(Supervisor::new(paths)?);

    tracing::info!(socket = %listener.socket().display(), "supervisor listening");
    listener.serve(supervisor)?;
    Ok(0)
}

fn status(cli: &Cli) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    let style = Style::stdout();
    match Client::connect(&paths) {
        Ok(client) => {
            if cli.json {
                output::json(&serde_json::json!({
                    "running": true,
                    "pid": client.supervisor_pid,
                    "version": client.supervisor_version,
                    "socket": paths.supervisor_sock(),
                }))?;
            } else {
                println!(
                    "supervisor {} running as pid {} on {}",
                    client.supervisor_version,
                    client.supervisor_pid,
                    paths.supervisor_sock().display()
                );
            }
            Ok(0)
        }
        Err(e) => {
            if cli.json {
                output::json(&serde_json::json!({
                    "running": false,
                    "socket": paths.supervisor_sock(),
                    "reason": e.to_string(),
                }))?;
            } else {
                println!("{}", style.dim("no supervisor running"));
            }
            Ok(1)
        }
    }
}

/// `zygo serve <handler> --name <name>` — warm a function, starting the
/// supervisor if there is not one.
pub fn serve(cli: &Cli, args: &ServeArgs) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    let layer = args.to_layer()?;
    let spec = Spec::discover(args.spec_file.path())?;

    // The supervisor has its own working directory, so a relative `--mount` or
    // handler path has to be anchored here, where the user typed it.
    let base_dir = match &spec {
        Some(spec) => absolute(&spec.base_dir())?,
        None => std::env::current_dir().context("cannot read the working directory")?,
    };

    let exe = std::env::current_exe().context("cannot find this binary to start a supervisor")?;
    let mut client = Client::connect_or_start(&paths, &exe)?;

    let options = args.sandbox.resolve_options();
    let response = client.send(&Request::Serve {
        name: args.name.clone(),
        spec: spec.map(Box::new),
        layer: Box::new(layer),
        base_dir,
        allow_host_net: options.allow_host_net,
        allow_private_net: options.allow_private_net,
        allow_unlimited: options.allow_unlimited,
    })?;

    let style = Style::stdout();
    match response {
        Response::Served {
            name,
            runtime,
            rss_kb,
            imports_ms,
            warm_ms,
            warnings,
        } => {
            for w in &warnings {
                output::warn(w);
            }
            if cli.json {
                output::json(&serde_json::json!({
                    "name": name,
                    "runtime": runtime,
                    "rss_kb": rss_kb,
                    "imports_ms": imports_ms,
                    "warm_ms": warm_ms,
                    "warnings": warnings,
                }))?;
            } else {
                println!(
                    "{} {name} is warm — {runtime}, {} MB resident, ready in {warm_ms:.0} ms",
                    style.green("✓"),
                    rss_kb / 1024
                );
                println!("  {}", style.dim(&format!("zygo exec {name} '{{}}'")));
            }
            Ok(0)
        }
        other => report_failure(cli, &other),
    }
}

/// `zygo exec <name> [json]`.
pub fn exec(cli: &Cli, args: &ExecArgs) -> anyhow::Result<u8> {
    anyhow::ensure!(
        !args.batch,
        "`--batch` needs the supervisor's request queue (todo.md, phase 2.2)"
    );

    let event = read_event(args)?;
    let paths = super::paths(cli);
    let mut client = Client::connect(&paths)?;

    let timeout_ms = args
        .timeout
        .map_or(DEFAULT_EXEC_TIMEOUT_MS, |t| t.as_millis());
    let response = client.send(&Request::Exec {
        name: args.name.clone(),
        event,
        timeout_ms,
    })?;

    match response {
        Response::Executed { outcome } => {
            // The handler's own output goes where the caller expects it, so
            // `zygo exec ... | jq` works: the result on stdout, everything
            // else on stderr.
            if !outcome.stdout.is_empty() {
                eprint!("{}", outcome.stdout);
            }
            if !outcome.stderr.is_empty() {
                eprint!("{}", outcome.stderr);
            }
            if let Some(error) = &outcome.error {
                output::error(&anyhow::anyhow!("{error}"));
                return Ok(1);
            }
            if cli.json {
                output::json(&outcome.result)?;
            } else {
                println!("{}", serde_json::to_string_pretty(&outcome.result)?);
            }
            Ok(u8::from(!outcome.succeeded()))
        }
        other => report_failure(cli, &other),
    }
}

/// The wall-clock budget when `--timeout` is not given.
///
/// The function's own `timeout` is the real limit and the supervisor enforces
/// it; this only bounds how long the *client* waits, so it is deliberately
/// longer than any sensible handler.
const DEFAULT_EXEC_TIMEOUT_MS: u64 = 60_000;

fn read_event(args: &ExecArgs) -> anyhow::Result<serde_json::Value> {
    let text = match &args.event {
        Some(text) => text.clone(),
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("cannot read the event from stdin")?;
            buf
        }
    };
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text).with_context(|| format!("the event is not valid JSON: {text}"))
}

/// `zygo ps`.
pub fn ps(cli: &Cli) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    let mut client = match Client::connect(&paths) {
        Ok(client) => client,
        // Nothing warm is a legitimate answer to "what is warm", not an error.
        Err(_) => {
            if cli.json {
                output::json(&serde_json::json!({ "functions": [] }))?;
            } else {
                println!("{}", Style::stdout().dim("no supervisor running"));
            }
            return Ok(0);
        }
    };

    match client.send(&Request::List)? {
        Response::Functions { functions } => {
            if cli.json {
                output::json(&serde_json::json!({ "functions": functions }))?;
                return Ok(0);
            }
            if functions.is_empty() {
                println!("{}", Style::stdout().dim("no warm functions"));
                return Ok(0);
            }
            let rows: Vec<Vec<String>> = functions
                .iter()
                .map(|f| {
                    vec![
                        f.name.clone(),
                        f.state.as_str().to_string(),
                        f.runtime.clone(),
                        output::human_bytes(f.rss_kb * 1024),
                        f.requests.to_string(),
                        f.failures.to_string(),
                    ]
                })
                .collect();
            print!(
                "{}",
                output::table(
                    &["NAME", "STATE", "RUNTIME", "RSS", "REQUESTS", "FAILURES"],
                    &rows
                )
            );
            Ok(0)
        }
        other => report_failure(cli, &other),
    }
}

/// `zygo stop <name>` / `zygo stop --all`.
pub fn stop(cli: &Cli, name: Option<&str>, all: bool) -> anyhow::Result<u8> {
    anyhow::ensure!(
        all || name.is_some(),
        "name a function to stop, or pass `--all`"
    );
    anyhow::ensure!(
        !(all && name.is_some()),
        "`--all` stops everything; do not also name a function"
    );

    let paths = super::paths(cli);
    let mut client = match Client::connect(&paths) {
        Ok(client) => client,
        Err(e) if all => {
            // `stop --all` is what you run to be sure nothing is left; no
            // supervisor means that is already true.
            tracing::debug!("nothing to stop: {e}");
            if cli.json {
                output::json(&serde_json::json!({ "stopped": [] }))?;
            }
            return Ok(0);
        }
        Err(e) => return Err(e.into()),
    };

    let stopped = match client.send(&Request::Stop {
        name: name.map(str::to_string),
    })? {
        Response::Stopped { names } => names,
        other => return report_failure(cli, &other),
    };

    // Stopping the last function leaves a supervisor with nothing to hold open.
    if all {
        let _ = client.send(&Request::Shutdown);
    }

    if cli.json {
        output::json(&serde_json::json!({ "stopped": stopped }))?;
    } else if stopped.is_empty() {
        println!("{}", Style::stdout().dim("nothing to stop"));
    } else {
        for name in &stopped {
            println!("stopped {name}");
        }
    }
    Ok(0)
}

/// Print a non-success response and turn it into an exit code.
///
/// `BUSY` is not an error: it is the answer to "can you take this right now",
/// and it gets its own exit code so a shell loop can tell it from a failure and
/// retry rather than give up.
fn report_failure(cli: &Cli, response: &Response) -> anyhow::Result<u8> {
    match response {
        Response::Busy {
            name,
            in_flight,
            queued,
            limit,
        } => {
            if cli.json {
                output::json(&serde_json::json!({
                    "busy": true,
                    "name": name,
                    "in_flight": in_flight,
                    "queued": queued,
                    "limit": limit,
                }))?;
            } else {
                output::warn(&format!(
                    "{name} is busy: {in_flight} of {limit} in flight, {queued} queued — retry"
                ));
            }
            Ok(EXIT_BUSY)
        }
        Response::Error { code, message } => {
            if cli.json {
                output::json(&serde_json::json!({
                    "error": code.as_str(),
                    "message": message,
                }))?;
            } else {
                output::error(&anyhow::anyhow!("{message}"));
            }
            Ok(exit_code_for(*code))
        }
        other => {
            output::error(&anyhow::anyhow!(
                "the supervisor sent an unexpected answer: {other:?}"
            ));
            Ok(1)
        }
    }
}

/// `429`, in the only currency a shell understands.
const EXIT_BUSY: u8 = 75;

fn exit_code_for(code: ControlError) -> u8 {
    match code {
        // "there is no such thing", which a script may reasonably branch on.
        ControlError::NotFound => 4,
        _ => 1,
    }
}

/// Make a path absolute without requiring it to exist.
///
/// `canonicalize` would resolve symlinks and fail on a directory that is about
/// to be created; all this needs is something the supervisor can interpret.
fn absolute(path: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().context("cannot read the working directory")?;
    Ok(cwd.join(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zygo_core::pool::Status;
    use zygo_core::sandbox::SandboxState;

    fn cli() -> Cli {
        use clap::Parser;
        Cli::try_parse_from(["zygo", "ps"]).expect("parse")
    }

    #[test]
    fn busy_gets_its_own_exit_code_so_a_script_can_retry() {
        let code = report_failure(
            &cli(),
            &Response::Busy {
                name: "resize".into(),
                in_flight: 4,
                queued: 16,
                limit: 4,
            },
        )
        .expect("report");
        assert_eq!(code, EXIT_BUSY);
        assert_ne!(code, 1, "backpressure must be distinguishable from failure");
        assert_ne!(code, 0, "and from success");
    }

    #[test]
    fn a_missing_function_is_a_different_exit_code_from_a_broken_one() {
        let missing = report_failure(
            &cli(),
            &Response::error(ControlError::NotFound, "no function named `x`"),
        )
        .expect("report");
        let broken = report_failure(
            &cli(),
            &Response::error(ControlError::WarmFailed, "the image is not pulled"),
        )
        .expect("report");
        assert_eq!(missing, 4);
        assert_eq!(broken, 1);
        assert_ne!(missing, broken);
    }

    #[test]
    fn an_event_may_be_given_or_omitted() {
        let args = |event: Option<&str>| ExecArgs {
            name: "x".into(),
            event: event.map(str::to_string),
            batch: false,
            timeout: None,
        };
        assert_eq!(
            read_event(&args(Some(r#"{"n":1}"#))).expect("json"),
            serde_json::json!({ "n": 1 })
        );
        // An empty argument is `null` rather than a parse error: `zygo exec x ''`
        // is how you call a handler that takes nothing.
        assert_eq!(
            read_event(&args(Some("   "))).expect("blank"),
            serde_json::Value::Null
        );
        let err = read_event(&args(Some("{not json}"))).expect_err("malformed");
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }

    #[test]
    fn relative_paths_are_anchored_where_the_user_typed_them() {
        // The supervisor's working directory is its own; a `base_dir` of "."
        // would resolve a handler somewhere nobody meant.
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(
            absolute(std::path::Path::new(".")).expect("relative"),
            cwd.join(".")
        );
        assert_eq!(
            absolute(std::path::Path::new("/etc")).expect("absolute"),
            std::path::PathBuf::from("/etc")
        );
        assert!(
            absolute(std::path::Path::new("does/not/exist"))
                .expect("a path need not exist")
                .is_absolute()
        );
    }

    #[test]
    fn stop_needs_a_name_or_all_but_not_both() {
        let err = stop(&cli(), None, false).expect_err("neither");
        assert!(err.to_string().contains("--all"), "{err}");

        let err = stop(&cli(), Some("x"), true).expect_err("both");
        assert!(err.to_string().contains("do not also name"), "{err}");
    }

    #[test]
    fn ps_renders_a_status_table() {
        // Guards the column order, which is the part a script would parse.
        let f = Status {
            name: "resize".into(),
            state: SandboxState::Warm,
            runtime: "python3.12".into(),
            rss_kb: 15_360,
            imports_ms: 120.0,
            requests: 42,
            failures: 1,
        };
        let rows = vec![vec![
            f.name.clone(),
            f.state.as_str().to_string(),
            f.runtime.clone(),
            output::human_bytes(f.rss_kb * 1024),
            f.requests.to_string(),
            f.failures.to_string(),
        ]];
        let table = output::table(
            &["NAME", "STATE", "RUNTIME", "RSS", "REQUESTS", "FAILURES"],
            &rows,
        );
        assert!(table.contains("resize"));
        assert!(table.contains("warm"));
        assert!(table.contains("15.0 MB"), "{table}");
    }
}
