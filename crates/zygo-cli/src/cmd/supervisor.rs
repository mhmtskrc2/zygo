//! `zygo serve`, `exec`, `ps`, `stop`, and the supervisor process behind them.
//!
//! Every command here is the same shape: connect to the supervisor, send one
//! request, print the answer. That thinness is ADR-008 — the library is the
//! product, and a platform embedding `zygo_core` gets the same behaviour
//! without a CLI in the way.

use std::sync::Arc;

use anyhow::Context;
use zygo_core::lock::LockFile;
use zygo_core::pool::Outcome;
use zygo_core::spec::Spec;
use zygo_core::supervisor::client::Client;
use zygo_core::supervisor::{Change, ControlError, Listener, Request, Response, Supervisor};

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

/// Values for the secrets a function names, from this process's environment.
///
/// This process, because it is the user's shell that has `STRIPE_KEY` set —
/// not the supervisor, which inherited the environment of whichever command
/// happened to start it. Every missing name is reported at once rather than
/// one per attempt, and no value is ever printed.
fn secrets_from_env(
    names: &[String],
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let mut values = std::collections::BTreeMap::new();
    let mut missing = Vec::new();
    for name in names {
        match std::env::var(name) {
            Ok(value) => {
                values.insert(name.clone(), value);
            }
            Err(_) => missing.push(name.as_str()),
        }
    }
    anyhow::ensure!(
        missing.is_empty(),
        "no value for secret{} {} in this environment\n  → export {} before running this command",
        if missing.len() == 1 { "" } else { "s" },
        missing.join(", "),
        missing.join(" ")
    );
    Ok(values)
}

/// `zygo serve <handler> --name <name>` — warm a function, starting the
/// supervisor if there is not one.
///
/// With `--runtime <name>` it warms a **pool** instead: no handler, several
/// zygotes, and the scripts arrive with the requests.
pub fn serve(cli: &Cli, args: &ServeArgs) -> anyhow::Result<u8> {
    let name = match args.target()? {
        crate::cli::ServeTarget::Function(name) => name,
        crate::cli::ServeTarget::Runtime(name) => return serve_runtime(cli, args, &name),
    };
    let paths = super::paths(cli);
    let layer = args.to_layer()?;
    let spec = Spec::discover(args.spec_file.path())?;

    // The supervisor has its own working directory, so a relative `--mount` or
    // handler path has to be anchored here, where the user typed it.
    let base_dir = match &spec {
        Some(spec) => absolute(&spec.base_dir())?,
        None => std::env::current_dir().context("cannot read the working directory")?,
    };

    let options = args.sandbox.resolve_options();

    // Resolve here as well, purely to learn which secrets the function names.
    // Resolution touches no files, so this costs nothing and cannot disagree
    // with what the supervisor will conclude from the same inputs.
    let names = spec
        .clone()
        .unwrap_or_default()
        .resolve_for_serve(
            &name,
            &layer,
            &zygo_core::spec::ResolveOptions {
                base_dir: Some(base_dir.clone()),
                ..options.clone()
            },
        )?
        .secrets;
    let secrets = secrets_from_env(&names)?;

    let exe = std::env::current_exe().context("cannot find this binary to start a supervisor")?;
    let mut client = Client::connect_or_start(&paths, &exe)?;

    let response = client.send(&Request::Serve {
        // The CLI is the operator at a terminal; a tenant's work arrives over
        // the API, where the caller says whose it is.
        tenant: None,
        name: name.clone(),
        spec: spec.map(Box::new),
        layer: Box::new(layer),
        base_dir,
        allow_host_net: options.allow_host_net,
        allow_private_net: options.allow_private_net,
        allow_unlimited: options.allow_unlimited,
        secrets,
        // Always replace: the user just typed what they want this name to be.
        if_changed: false,
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
            change,
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
                    "change": change,
                }))?;
            } else {
                println!(
                    "{} {name} is warm — {runtime}, {} MB resident, ready in {warm_ms:.0} ms",
                    style.green("✓"),
                    rss_kb / 1024
                );
                if change == Change::Replaced {
                    println!(
                        "  {}",
                        style.dim(
                            "replaced the previous one; requests it had accepted finish on it"
                        )
                    );
                }
                println!("  {}", style.dim(&format!("zygo exec {name} '{{}}'")));
            }
            Ok(0)
        }
        other => report_failure(cli, &other),
    }
}

/// `zygo serve --runtime <name>` — warm a pool of anonymous zygotes.
///
/// No handler, no secrets and no sources to hash: a pool is an image, a
/// dependency set and an agent, and everything a request runs arrives with it.
/// That is also why this is a shorter function than `serve` — most of what
/// serving a function does is about the code it is warming.
fn serve_runtime(cli: &Cli, args: &ServeArgs, name: &str) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    let layer = args.to_layer()?;
    let spec = Spec::discover(args.spec_file.path())?;
    let base_dir = match &spec {
        Some(spec) => absolute(&spec.base_dir())?,
        None => std::env::current_dir().context("cannot read the working directory")?,
    };
    let options = args.sandbox.resolve_options();

    let exe = std::env::current_exe().context("cannot find this binary to start a supervisor")?;
    let mut client = Client::connect_or_start(&paths, &exe)?;
    let response = client.send(&Request::ServeRuntime {
        tenant: None,
        name: name.to_string(),
        spec: spec.map(Box::new),
        layer: Box::new(layer),
        base_dir,
        allow_host_net: options.allow_host_net,
        allow_private_net: options.allow_private_net,
        allow_unlimited: options.allow_unlimited,
        // An id from `POST /deps`, and there is nothing to name from here: an
        // operator at a shell has the lockfile on disk, which is what
        // `requirements` is for.
        deps: None,
    })?;

    let style = Style::stdout();
    match response {
        Response::RuntimeServed {
            name,
            runtime,
            warm,
            rss_kb,
            imports_ms,
            warm_ms,
            warnings,
            change,
        } => {
            for w in &warnings {
                output::warn(w);
            }
            if cli.json {
                output::json(&serde_json::json!({
                    "name": name,
                    "runtime": runtime,
                    "warm": warm,
                    "rss_kb": rss_kb,
                    "imports_ms": imports_ms,
                    "warm_ms": warm_ms,
                    "warnings": warnings,
                    "change": change,
                }))?;
            } else {
                println!(
                    "{} runtime {name} is warm — {runtime}, {warm} zygote{}, \
                     {} MB resident, ready in {warm_ms:.0} ms",
                    style.green("✓"),
                    if warm == 1 { "" } else { "s" },
                    rss_kb / 1024
                );
                if change == Change::Replaced {
                    println!(
                        "  {}",
                        style.dim("replaced the previous pool under this name")
                    );
                }
                println!(
                    "  {}",
                    style.dim(&format!(
                        "zygo exec --runtime {name} --script handler.py '{{}}'"
                    ))
                );
            }
            Ok(0)
        }
        other => report_failure(cli, &other),
    }
}

/// `zygo up` — bring every function in the spec file up warm.
///
/// The spec file is the unit of deployment: `zygo serve` is for one handler on
/// the command line, this is for a project. Functions are brought up in the
/// order they are declared and each one reports on its own line, because a spec
/// with ten functions where the sixth cannot start should say which one.
///
/// Running it again is not a second deploy. A function whose spec, secrets and
/// source files are what is already registered is left alone — warm pages,
/// request counters and all — and one that changed is replaced blue/green: the
/// new sandbox is warm before the old one stops taking requests. So editing one
/// handler and running `up` restarts one function.
pub fn up(cli: &Cli, file: Option<&std::path::Path>, relock: bool) -> anyhow::Result<u8> {
    let spec = Spec::discover(file)?.with_context(|| {
        "no sandbox.toml found\n           → create one, or serve a single handler: zygo serve <handler> --name <name>"
    })?;
    let names: Vec<String> = spec.function_names().map(str::to_string).collect();
    anyhow::ensure!(
        !names.is_empty(),
        "{} declares no functions\n  → add a [fn.<name>] section",
        file.map_or_else(|| "the spec file".into(), |p| p.display().to_string())
    );

    let base_dir = absolute(&spec.base_dir())?;
    let paths = super::paths(cli);
    let exe = std::env::current_exe().context("cannot find this binary to start a supervisor")?;
    let mut client = Client::connect_or_start(&paths, &exe)?;

    let store = zygo_core::image::Store::new(paths.clone());
    let lock_path = LockFile::beside(&base_dir);
    let mut lock = LockFile::load(&lock_path)?.unwrap_or_default();
    let lock_before = lock.clone();

    let style = Style::stdout();
    let mut brought_up = Vec::new();
    let mut replaced = Vec::new();
    let mut unchanged = Vec::new();
    // Name *and* reason, and collected rather than printed as they happen.
    //
    // `up --json` used to write one document per failure from inside this
    // loop and then a summary document at the end, so its output was several
    // JSON values concatenated — which `json.load` rejects and `jq` only
    // accepts with `-s` (E-14). Two of the failure branches printed nothing
    // at all in JSON mode, so the reason was simply lost. One document, with
    // every failure and why, is what a caller can actually read.
    let mut failed: Vec<serde_json::Value> = Vec::new();
    let mut failed_names: Vec<String> = Vec::new();
    let mut note_failure = |name: &str, reason: String| {
        failed.push(serde_json::json!({ "name": name, "reason": reason }));
        failed_names.push(name.to_string());
    };

    for name in &names {
        let resolved = spec
            .resolve(
                Some(name),
                &zygo_core::spec::Layer::default(),
                &zygo_core::spec::ResolveOptions {
                    base_dir: Some(base_dir.clone()),
                    ..Default::default()
                },
            )
            .ok();
        let secret_names = resolved
            .as_ref()
            .map(|r| r.secrets.clone())
            .unwrap_or_default();

        // What the spec resolves to on this host, read before anything is
        // served. An image that is not pulled yet says nothing — the serve
        // below reports that as the error it is.
        let observed = match &resolved {
            Some(r) => zygo_core::lock::LockedFn::observe(
                &store,
                &r.image,
                &r.system,
                r.requirements.as_deref(),
                &base_dir,
            )
            .unwrap_or(None),
            None => None,
        };
        // A tag that has moved under a spec nobody edited is the one thing
        // the lock refuses. It changes nothing by itself: it stops.
        if !relock && let (Some(locked), Some(observed)) = (lock.functions.get(name), &observed) {
            let comparison = locked.compare(observed);
            if comparison.changes.is_empty()
                && let Some(drift) = comparison.image_drift()
            {
                if !cli.json {
                    println!(
                        "{} {name} — {drift}\n    {}",
                        style.red("✗"),
                        style.dim(
                            "→ run `zygo up --relock` to accept it, or pull the locked digest"
                        )
                    );
                }
                note_failure(name, drift.to_string());
                continue;
            }
        }

        let secrets = match secrets_from_env(&secret_names) {
            Ok(values) => values,
            Err(e) => {
                if !cli.json {
                    println!("{} {name} — {e:#}", style.red("✗"));
                }
                note_failure(name, format!("{e:#}"));
                continue;
            }
        };
        let response = client.send(&Request::Serve {
            tenant: None,
            name: name.clone(),
            spec: Some(Box::new(spec.clone())),
            layer: Box::new(zygo_core::spec::Layer::default()),
            base_dir: base_dir.clone(),
            // `up` reads a file rather than flags, so the `--allow-*` escapes
            // are not available here: a spec that needs one has to be served
            // deliberately, which is the point of them being flags.
            allow_host_net: false,
            allow_private_net: false,
            allow_unlimited: false,
            secrets,
            if_changed: true,
        })?;
        match response {
            Response::Served {
                runtime,
                rss_kb,
                warm_ms,
                warnings,
                change,
                ..
            } => {
                for w in &warnings {
                    output::warn(w);
                }
                if !cli.json {
                    match change {
                        Change::Started => println!(
                            "{} {name} — {runtime}, {} MB, ready in {warm_ms:.0} ms",
                            style.green("✓"),
                            rss_kb / 1024
                        ),
                        Change::Replaced => println!(
                            "{} {name} — replaced: {runtime}, {} MB, ready in {warm_ms:.0} ms",
                            style.green("✓"),
                            rss_kb / 1024
                        ),
                        Change::Unchanged => println!(
                            "{} {name} — {}",
                            style.dim("·"),
                            style.dim(&format!("unchanged ({runtime}, {} MB)", rss_kb / 1024))
                        ),
                    }
                }
                match change {
                    Change::Started => {}
                    Change::Replaced => replaced.push(name.clone()),
                    Change::Unchanged => unchanged.push(name.clone()),
                }
                brought_up.push(name.clone());

                // Recorded only now: the system layer was built during the
                // serve, so the package versions exist after it and not
                // before. Read again for the same reason.
                if let Some(r) = &resolved
                    && let Ok(Some(now)) = zygo_core::lock::LockedFn::observe(
                        &store,
                        &r.image,
                        &r.system,
                        r.requirements.as_deref(),
                        &base_dir,
                    )
                {
                    if let Some(previous) = lock.functions.get(name) {
                        // Versions that moved under an unchanged package list
                        // are recorded, not refused: `apt` does not keep old
                        // versions, so refusing would strand every fresh host.
                        for drift in previous.compare(&now).drifts {
                            if !matches!(drift, zygo_core::lock::Drift::Image { .. }) {
                                output::warn(&format!("{name}: {drift}"));
                            }
                        }
                    }
                    lock.functions.insert(name.clone(), now);
                }
            }
            other => {
                // Keep going: one function that cannot start is not a reason to
                // leave the rest of the project down.
                //
                // `report_failure` is for a command whose *whole* answer is one
                // failure; here it would be one JSON document per function on
                // top of the summary. In JSON mode the reason is collected
                // instead, and printed once, below.
                if cli.json {
                    note_failure(name, failure_reason(&other));
                } else {
                    print!("{} {name} — ", style.red("✗"));
                    report_failure(cli, &other)?;
                    note_failure(name, failure_reason(&other));
                }
            }
        }
    }

    // Entries for functions the spec no longer declares would otherwise
    // accumulate forever; a lock file is a record of this spec.
    lock.functions.retain(|name, _| names.contains(name));
    let locked_now = lock != lock_before;
    if locked_now {
        lock.save(&lock_path)?;
    }

    if cli.json {
        output::json(&serde_json::json!({
            "up": brought_up,
            "replaced": replaced,
            "unchanged": unchanged,
            "failed": failed,
            "locked": locked_now,
        }))?;
    } else if !failed_names.is_empty() {
        println!();
        println!(
            "{}",
            style.yellow(&format!(
                "  {} of {} functions are up; {} did not start",
                brought_up.len(),
                names.len(),
                failed_names.len()
            ))
        );
    }
    Ok(u8::from(!failed_names.is_empty()))
}

/// `zygo down` — stop every function the spec file declares.
///
/// Only those: a supervisor may be holding functions from another project or
/// from a bare `zygo serve`, and `down` in one directory must not take them
/// with it. `zygo stop --all` is the blunt instrument.
pub fn down(cli: &Cli, file: Option<&std::path::Path>) -> anyhow::Result<u8> {
    let spec = Spec::discover(file)?.with_context(
        || "no sandbox.toml found\n  → to stop everything instead: zygo stop --all",
    )?;
    let names: Vec<String> = spec.function_names().map(str::to_string).collect();

    let paths = super::paths(cli);
    let mut client = match Client::connect(&paths) {
        Ok(client) => client,
        // Nothing running is the state `down` is trying to reach.
        Err(_) => {
            if cli.json {
                output::json(&serde_json::json!({ "stopped": [] }))?;
            } else {
                println!("{}", Style::stdout().dim("no supervisor running"));
            }
            return Ok(0);
        }
    };

    let mut stopped = Vec::new();
    for name in &names {
        if let Response::Stopped { names } = client.send(&Request::Stop {
            name: Some(name.clone()),
        })? {
            stopped.extend(names);
        }
        // A function the spec declares but that was never served is not an
        // error here: `down` is about reaching a state, not about auditing.
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

/// `zygo exec <name> [json]`, or `zygo exec --runtime <name> --script <file>`.
pub fn exec(cli: &Cli, args: &ExecArgs) -> anyhow::Result<u8> {
    anyhow::ensure!(
        !args.batch,
        "`--batch` is not implemented yet: it needs the supervisor's request queue"
    );

    let event = read_event(args)?;
    let paths = super::paths(cli);
    let mut client = Client::connect(&paths)?;

    let timeout_ms = args
        .timeout
        .map_or(DEFAULT_EXEC_TIMEOUT_MS, |t| t.as_millis());
    let request = match &args.runtime {
        Some(runtime) => Request::ExecScript {
            // `zygo exec` is one shot at a terminal: Ctrl-C ends the client,
            // and the supervisor's own deadline ends the request. It prints
            // the request's output when the answer arrives, which is what a
            // command that also prints the result has to do anyway.
            key: None,
            stream: false,
            workspace: None,
            runtime: runtime.clone(),
            script: script_for(args)?,
            event,
            timeout_ms,
            // The CLI is the operator at a terminal. A request for a tenant
            // comes over the API, where the caller says whose it is.
            tenant: None,
        },
        None => Request::Exec {
            key: None,
            stream: false,
            workspace: None,
            name: args
                .name
                .clone()
                .context("which function? `zygo exec <name> '<json>'`")?,
            event,
            timeout_ms,
            tenant: None,
        },
    };
    let response = client.send(&request)?;

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
                return Ok(exit_status(&outcome));
            }
            if cli.json {
                output::json(&outcome.result)?;
            } else {
                println!("{}", serde_json::to_string_pretty(&outcome.result)?);
            }
            Ok(exit_status(&outcome))
        }
        other => report_failure(cli, &other),
    }
}

/// The request's own exit status, as the shell would report it.
///
/// A request killed at its deadline is 137 here exactly as a process killed
/// in a shell is, so a caller can tell "the tool ran out of time" from "the
/// tool raised" (1) without parsing stderr. The request cannot have exited 0
/// and failed unless the supervisor added an error of its own (a result that
/// was not JSON); that is still a failure.
fn exit_status(outcome: &Outcome) -> u8 {
    match outcome.exit_code {
        0 if outcome.error.is_none() => 0,
        1..=255 => outcome.exit_code as u8,
        _ => 1,
    }
}

/// The wall-clock budget when `--timeout` is not given.
///
/// The function's own `timeout` is the real limit and the supervisor enforces
/// it; this bounds the request the client asks for, and — through
/// [`zygo_core::supervisor::client::Client::budget`] — how long the client
/// waits for the answer. Deliberately longer than any sensible handler.
///
/// That second half used to be a comment rather than a fact: nothing set a
/// socket timeout, so a client spoke to a wedged supervisor for ever.
const DEFAULT_EXEC_TIMEOUT_MS: u64 = 60_000;

/// What `--script` names: a file on this host, or a digest the host holds.
///
/// A digest is the embedder's shape — register once with `PUT /scripts`, run
/// it ten thousand times — and a path is the developer's, where the file is
/// right there and registering it first would be a step with no purpose. Told
/// apart by the `sha256:` prefix, which a filename cannot have without a
/// directory in front of it.
fn script_for(args: &ExecArgs) -> anyhow::Result<zygo_core::protocol::Script> {
    let named = args
        .script
        .as_deref()
        .context("`--runtime` needs a `--script`")?;
    let mut script = if named.starts_with("sha256:") {
        zygo_core::protocol::Script {
            path: None,
            source: None,
            digest: Some(named.to_string()),
            entry_point: None,
        }
    } else {
        let source = std::fs::read_to_string(named)
            .with_context(|| format!("cannot read the script `{named}`"))?;
        zygo_core::protocol::Script::inline(source)
    };
    script.entry_point = args.entry_point.clone();
    Ok(script)
}

fn read_event(args: &ExecArgs) -> anyhow::Result<serde_json::Value> {
    // With `--runtime` the first positional is the event: a pool has no
    // function name, so `zygo exec --runtime py --script s.py '{"n":1}'`
    // reads the way it is written rather than binding the JSON to a name.
    if args.runtime.is_some()
        && args.event.is_none()
        && let Some(text) = &args.name
    {
        return parse_event(text);
    }
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
    parse_event(&text)
}

fn parse_event(text: &str) -> anyhow::Result<serde_json::Value> {
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(text).with_context(|| format!("the event is not valid JSON: {text}"))
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
/// A one-line reason from a failure response, for collecting rather than
/// printing. See `up`'s `note_failure`.
fn failure_reason(response: &Response) -> String {
    match response {
        Response::Busy {
            in_flight,
            queued,
            limit,
            ..
        } => format!("busy: {in_flight} of {limit} in flight, {queued} queued"),
        Response::Error { code, message } => format!("{}: {message}", code.as_str()),
        other => format!("unexpected response: {other:?}"),
    }
}

pub(super) fn report_failure(cli: &Cli, response: &Response) -> anyhow::Result<u8> {
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
    fn exec_exits_with_the_request_s_own_status() {
        let outcome = |exit_code: i32, error: Option<&str>| Outcome {
            tenant: "default".into(),
            function: "resize".into(),
            script: None,
            id: "00000001".into(),
            cancelled: false,
            stuck: false,
            workspace: None,
            exit_code,
            result: serde_json::Value::Null,
            stdout: String::new(),
            stderr: String::new(),
            error: error.map(str::to_string),
            metrics: Default::default(),
            timed_out: false,
        };
        assert_eq!(exit_status(&outcome(0, None)), 0);
        // A handler that raised: the agent reports 1.
        assert_eq!(exit_status(&outcome(1, Some("ZeroDivisionError"))), 1);
        // Killed at the deadline: 137, as a shell would say it, so a caller
        // can tell "out of time" from "raised" without reading stderr.
        assert_eq!(exit_status(&outcome(137, Some("killed by SIGKILL"))), 137);
        // A warm-exec program's own status passes through.
        assert_eq!(exit_status(&outcome(3, Some("the program exited 3"))), 3);
        // Exit 0 with a supervisor-side error (stdout was not JSON) is still
        // a failure, and a status the shell cannot hold is a plain failure.
        assert_eq!(exit_status(&outcome(0, Some("stdout is not JSON"))), 1);
        assert_eq!(exit_status(&outcome(-1, Some("?"))), 1);
        assert_eq!(exit_status(&outcome(300, Some("?"))), 1);
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
            name: Some("x".into()),
            event: event.map(str::to_string),
            batch: false,
            timeout: None,
            runtime: None,
            script: None,
            entry_point: None,
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
    fn every_missing_secret_is_named_at_once_and_no_value_is_echoed() {
        // Serialised through an environment variable this test owns; the
        // names are chosen so no real shell would have them.
        unsafe { std::env::set_var("ZYGO_TEST_SECRET_PRESENT", "hunter2") };
        let err = secrets_from_env(&[
            "ZYGO_TEST_SECRET_PRESENT".into(),
            "ZYGO_TEST_SECRET_MISSING_A".into(),
            "ZYGO_TEST_SECRET_MISSING_B".into(),
        ])
        .expect_err("two are missing");
        let text = err.to_string();
        assert!(
            text.contains("MISSING_A") && text.contains("MISSING_B"),
            "{text}"
        );
        assert!(
            !text.contains("PRESENT"),
            "the one that exists is not a problem: {text}"
        );
        assert!(
            !text.contains("hunter2"),
            "a value must never appear in an error: {text}"
        );
        assert!(text.contains("export"), "tell them what to do: {text}");

        let values = secrets_from_env(&["ZYGO_TEST_SECRET_PRESENT".into()]).expect("present");
        assert_eq!(values["ZYGO_TEST_SECRET_PRESENT"], "hunter2");
        unsafe { std::env::remove_var("ZYGO_TEST_SECRET_PRESENT") };
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
            tenant: "default".into(),
            name: "resize".into(),
            image: String::new(),
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
