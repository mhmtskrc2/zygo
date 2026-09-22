//! `zygo doctor` — can this host run sandboxes, and if not, what fixes it.

use serde::Serialize;
use zygo_core::doctor::{self, Status};
use zygo_core::spec::Isolation;

use crate::cli::Cli;
use crate::output::Style;

#[derive(Serialize)]
struct JsonCheck {
    name: &'static str,
    status: &'static str,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    remedy: Option<String>,
}

#[derive(Serialize)]
struct JsonReport {
    checks: Vec<JsonCheck>,
    backends: Vec<&'static str>,
    ok: bool,
}

pub fn run(cli: &Cli, fix: bool, yes: bool) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    let report = doctor::run(&paths);

    if fix {
        return apply_fixes(cli, &report, yes);
    }

    // Two questions, and the answer is the *and* of them: does the host have
    // what the backend needs, and does this binary implement it? Reporting
    // only the first printed `backends available: ns, vm` on a machine with
    // `/dev/kvm` where `zygo backend list` said — correctly — that `vm` is not
    // built. The library cannot join them, because a backend's own
    // availability check consults this report and the pair would recurse.
    let usable: Vec<&'static str> = Isolation::ALL
        .iter()
        .filter(|i| report.supports(**i) && zygo_core::backend::for_isolation(**i, &paths).is_ok())
        .map(|i| i.as_str())
        .collect();

    if cli.json {
        crate::output::json(&JsonReport {
            checks: report
                .checks
                .iter()
                .map(|c| JsonCheck {
                    name: c.name,
                    status: c.status.as_str(),
                    detail: c.detail.clone(),
                    remedy: c.remedy.clone(),
                })
                .collect(),
            backends: usable.clone(),
            ok: report.exit_code() == 0,
        })?;
        return Ok(report.exit_code() as u8);
    }

    let style = Style::stdout();
    let name_width = report
        .checks
        .iter()
        .map(|c| c.name.len())
        .max()
        .unwrap_or(0);
    let detail_width = report
        .checks
        .iter()
        .map(|c| c.detail.len())
        .max()
        .unwrap_or(0);

    for check in &report.checks {
        let status = match check.status {
            Status::Ok => style.green("ok"),
            Status::Degraded => style.yellow("degraded"),
            Status::Absent => style.dim("-"),
            Status::Failed => style.red("FAIL"),
        };
        println!(
            "{:name_width$}  {:detail_width$}  {status}",
            check.name, check.detail
        );
        // Only show a remedy where it changes what the user should do: an
        // absent optional backend is not a problem to be fixed.
        if let Some(remedy) = &check.remedy
            && check.status != Status::Ok
        {
            println!("{:name_width$}  {}", "", style.dim(&format!("→ {remedy}")));
        }
    }

    println!();
    if usable.is_empty() {
        println!("{} no isolation backend is usable here", style.red("✗"));
    } else {
        println!("backends available: {}", style.bold(&usable.join(", ")));
    }

    // On macOS the host's own answer is always the same and always no. The
    // one worth printing is the VM's, so it goes below and its exit code is
    // the one that leaves.
    if let Some(code) = vm_section(&style) {
        return Ok(code);
    }

    Ok(report.exit_code() as u8)
}

/// `zygo doctor --fix`: print the plan, ask, then run it.
///
/// The plan comes from [`doctor::fix::plan`] and nothing else. What is printed
/// and what is run are the same list, walked twice — a `--fix` that described
/// one thing and did another would be worse than no `--fix` at all, and the
/// commands here are ones that weaken the host.
fn apply_fixes(cli: &Cli, report: &doctor::Report, yes: bool) -> anyhow::Result<u8> {
    let style = Style::stdout();
    let plan = doctor::fix::plan(report);

    if plan.is_empty() {
        if cli.json {
            crate::output::json(&serde_json::json!({ "fixes": [], "applied": false }))?;
        } else if report.exit_code() == 0 {
            println!(
                "{} nothing to fix: this host can run sandboxes",
                style.green("✓")
            );
        } else {
            println!(
                "{} nothing here can be fixed in one command — run `zygo doctor` \
                 for what is wrong",
                style.yellow("!")
            );
        }
        return Ok(0);
    }

    if cli.json {
        // Machine-readable, and deliberately inert: a provisioning tool reads
        // the plan here and decides for itself. `--fix --json --yes` still
        // applies, so this only refuses to act when nobody said `--yes`.
        crate::output::json(&serde_json::json!({
            "fixes": plan.iter().map(|f| serde_json::json!({
                "check": f.check,
                "what": f.what,
                "why": f.why,
                "cost": f.cost,
                "needs_root": f.needs_root(),
                "commands": f.commands.iter().map(|c| serde_json::json!({
                    "command": c.display(),
                    "stdin": c.stdin,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "applied": yes,
        }))?;
        if !yes {
            return Ok(0);
        }
    } else {
        println!("{}", style.bold("zygo doctor --fix would run:"));
        println!();
        for (n, fix) in plan.iter().enumerate() {
            println!(
                "{}. {} {}",
                n + 1,
                style.bold(&fix.what),
                style.dim(&format!("({})", fix.check))
            );
            for line in wrap(&fix.why, 74) {
                println!("     {}", style.dim(&line));
            }
            if let Some(cost) = &fix.cost {
                println!("     {}", style.yellow("what this costs:"));
                for line in wrap(cost, 74) {
                    println!("     {}", style.yellow(&line));
                }
            }
            for command in &fix.commands {
                println!("     {}", command.display());
                // A `tee` writes a file, and "what would be done" has to
                // include what goes in it — a plan that says `tee
                // /etc/sysctl.d/…` and nothing else is a plan nobody can
                // consent to.
                if let Some(body) = &command.stdin {
                    for line in body.lines() {
                        println!("       {}", style.dim(&format!("| {line}")));
                    }
                }
            }
            println!();
        }

        if !yes && !confirm(&style)? {
            println!("nothing was changed");
            return Ok(1);
        }
    }

    let mut failures = 0;
    for fix in &plan {
        for command in &fix.commands {
            if !cli.json {
                println!("{} {}", style.dim("+"), command.display());
            }
            match run_command(command) {
                Ok(()) => {}
                Err(e) => {
                    failures += 1;
                    eprintln!("  {} {e:#}", style.red("failed:"));
                    // The commands in one fix are a sequence — the sysctl and
                    // the file that makes it survive a reboot — so a failure
                    // part way through means the rest of *this* fix would be
                    // applied to a state it was not written for.
                    break;
                }
            }
        }
    }

    println!();
    if failures == 0 {
        println!(
            "{} applied {}. Run `zygo doctor` to see what this host says now.",
            style.green("✓"),
            match plan.len() {
                1 => "1 fix".to_string(),
                n => format!("{n} fixes"),
            }
        );
        Ok(0)
    } else {
        println!(
            "{} {failures} of {} could not be applied",
            style.red("✗"),
            plan.len()
        );
        Ok(1)
    }
}

/// Run one command, feeding it the file it is meant to write.
///
/// The `tee` commands are how a file lands somewhere this user cannot redirect
/// into: the path is an argument and the content arrives on stdin, so no shell
/// is started and nothing in either is quoted, expanded or word-split. The
/// content is the one the plan printed, because it comes from the same field.
fn run_command(command: &doctor::fix::Command) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::process::{Command as Process, Stdio};

    let (program, args) = if command.root && !is_root() {
        // Named here rather than discovered as "No such file or directory",
        // which is what `Command::new("sudo")` reports when there is no
        // `sudo` and reads like the *fix* was missing rather than the way to
        // run it.
        anyhow::ensure!(
            which("sudo").is_some(),
            "`{}` has to run as root, and there is no `sudo` here — run \
             `zygo doctor --fix` as root, or run the command yourself",
            command.display()
        );
        ("sudo", command.argv.as_slice())
    } else {
        (command.argv[0].as_str(), &command.argv[1..])
    };

    let mut process = Process::new(program);
    process.args(args);
    if command.stdin.is_some() {
        process.stdin(Stdio::piped());
        // `tee` echoes what it writes, which here is a file the user has
        // already been shown.
        process.stdout(Stdio::null());
    }

    let mut child = process
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not run `{}`: {e}", command.display()))?;
    if let Some(body) = &command.stdin {
        child
            .stdin
            .as_mut()
            .expect("stdin was piped")
            .write_all(body.as_bytes())?;
        drop(child.stdin.take());
    }
    let status = child.wait()?;
    anyhow::ensure!(
        status.success(),
        "`{}` exited with {status}",
        command.display()
    );
    Ok(())
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(program))
        .find(|p| p.is_file())
}

fn is_root() -> bool {
    // SAFETY: `geteuid` takes nothing and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

fn confirm(style: &Style) -> anyhow::Result<bool> {
    use std::io::{BufRead as _, IsTerminal as _, Write as _};

    if !std::io::stdin().is_terminal() {
        println!(
            "{}",
            style.dim("stdin is not a terminal, so there is nobody to ask — rerun with --yes")
        );
        return Ok(false);
    }
    print!("apply these? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// Break `text` into lines of at most `width`, on spaces.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.len() + 1 + word.len() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

/// Print what the Linux VM says about itself, and return its exit code.
///
/// `None` anywhere but macOS, where there is no VM to ask.
#[cfg(target_os = "macos")]
fn vm_section(style: &Style) -> Option<u8> {
    use crate::shim::{self, Vm};

    let report = shim::describe_vm();
    println!();
    match &report.limactl {
        Some(path) => println!("the Linux VM   limactl at {}", path.display()),
        None => {
            println!(
                "{} `limactl` is not installed, so there is no Linux VM to run sandboxes in",
                style.red("✗")
            );
            println!("{}", style.dim("  → brew install lima"));
            return Some(1);
        }
    }

    match report.vm {
        Vm::Running => println!(
            "               instance `zygo` is {}",
            style.green("running")
        ),
        Vm::Stopped => {
            println!(
                "               instance `zygo` is {}",
                style.yellow("stopped")
            );
            println!(
                "{}",
                style.dim("  → it starts by itself on the next command")
            );
            return Some(1);
        }
        Vm::Absent => {
            println!(
                "               instance `zygo` {}",
                style.yellow("does not exist yet")
            );
            println!(
                "{}",
                style.dim("  → it is created by the first command that needs it")
            );
            return Some(1);
        }
    }

    let Some((text, code)) = report.guest else {
        println!(
            "{}",
            style.dim("  → the VM is running but did not answer `zygo doctor`")
        );
        return Some(1);
    };
    println!();
    println!("what that VM says about itself:");
    for line in text.lines() {
        println!("  {line}");
    }
    Some(code)
}

#[cfg(not(target_os = "macos"))]
fn vm_section(_style: &Style) -> Option<u8> {
    None
}
