// SPDX-License-Identifier: Apache-2.0
//! `zygo doctor` — can this host run sandboxes, and if not, what fixes it.

use serde::{Deserialize, Serialize};
use zygo_core::doctor::{self, Status};
use zygo_core::paths::which;
use zygo_core::spec::Isolation;

use crate::cli::Cli;
use crate::output::Style;

/// One line of `zygo doctor --json`.
///
/// The shape a health check parses, so it is documented
/// (`docs/book/22-troubleshooting.md`, "What `doctor --json` says") and read back
/// here: on macOS the guest's report arrives as this document and is merged
/// with the Mac's own checks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonCheck {
    pub name: String,
    /// `ok`, `degraded`, `absent` or `FAIL`, as the text output prints it.
    pub status: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
    /// On macOS, which side made the check: `host` (the Mac) or `vm`.
    /// Absent on Linux, where there is one side.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side: Option<String>,
}

impl JsonCheck {
    fn from_check(c: &doctor::Check, side: Option<&str>) -> JsonCheck {
        JsonCheck {
            name: c.name.to_string(),
            status: c.status.as_str().to_string(),
            detail: c.detail.clone(),
            remedy: c.remedy.clone(),
            side: side.map(str::to_string),
        }
    }

    fn failed(&self) -> bool {
        self.status == Status::Failed.as_str()
    }
}

/// The whole of `zygo doctor --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonReport {
    pub checks: Vec<JsonCheck>,
    /// The isolation backends this host can use *right now*: the host has
    /// what each needs and this binary implements it.
    pub backends: Vec<String>,
    /// Whether sandboxes can run here. The exit status is 0 exactly when
    /// this is true — [`verdict`] derives both from the same checks.
    pub ok: bool,
}

/// `ok` and the exit status, from one place, so they cannot disagree.
///
/// The first adoption report (Z-4) saw `ok: false` beside exit 0 and
/// concluded neither could be trusted alone. Both come from here now: `ok`
/// is "no check failed", and the exit status is `!ok`.
pub fn verdict(checks: &[JsonCheck]) -> (bool, u8) {
    let ok = !checks.iter().any(JsonCheck::failed);
    (ok, u8::from(!ok))
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

    // On macOS the host's checks are about reaching the VM, and the kernel's
    // are the VM's own. One report, from both sides, with one verdict.
    #[cfg(target_os = "macos")]
    {
        return on_a_mac(cli, &report, &usable);
    }

    #[allow(unreachable_code)]
    {
        let checks: Vec<JsonCheck> = report
            .checks
            .iter()
            .map(|c| JsonCheck::from_check(c, None))
            .collect();
        let (ok, code) = verdict(&checks);
        if cli.json {
            crate::output::json(&JsonReport {
                checks,
                backends: usable.iter().map(|s| s.to_string()).collect(),
                ok,
            })?;
            return Ok(code);
        }

        let style = Style::stdout();
        print_checks(&style, &checks);
        println!();
        print_backends(&style, &usable);
        Ok(code)
    }
}

/// The checks as a table, one line each, with the remedy under any that
/// is not `ok`.
fn print_checks(style: &Style, checks: &[JsonCheck]) {
    let name_width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let detail_width = checks.iter().map(|c| c.detail.len()).max().unwrap_or(0);

    for check in checks {
        let status = match check.status.as_str() {
            "ok" => style.green("ok"),
            "degraded" => style.yellow("degraded"),
            "absent" => style.dim("-"),
            _ => style.red("FAIL"),
        };
        println!(
            "{:name_width$}  {:detail_width$}  {status}",
            check.name, check.detail
        );
        // Only show a remedy where it changes what the user should do: an
        // absent optional backend is not a problem to be fixed.
        if let Some(remedy) = &check.remedy
            && check.status != Status::Ok.as_str()
        {
            println!("{:name_width$}  {}", "", style.dim(&format!("→ {remedy}")));
        }
    }
}

fn print_backends(style: &Style, usable: &[impl AsRef<str>]) {
    if usable.is_empty() {
        println!("{} no isolation backend is usable here", style.red("✗"));
    } else {
        let names: Vec<&str> = usable.iter().map(AsRef::as_ref).collect();
        println!("backends available: {}", style.bold(&names.join(", ")));
    }
}

/// `zygo doctor` on macOS: the Mac's checks, then the VM's, one verdict.
///
/// The text and the JSON are built from the same merged list, so the two
/// cannot say different things — which they did (Z-4): the text went on to
/// ask the VM and the JSON stopped at the platform line.
#[cfg(target_os = "macos")]
fn on_a_mac(cli: &Cli, report: &doctor::Report, _usable: &[&'static str]) -> anyhow::Result<u8> {
    let vm = crate::shim::describe_vm();

    let mut checks: Vec<JsonCheck> = report
        .checks
        .iter()
        .chain(vm.host.iter())
        .map(|c| JsonCheck::from_check(c, Some("host")))
        .collect();
    let mut backends: Vec<String> = Vec::new();
    let mut guest_checks: Vec<JsonCheck> = Vec::new();
    match &vm.guest {
        Some(Ok(guest)) => {
            guest_checks = guest
                .checks
                .iter()
                .cloned()
                .map(|mut c| {
                    c.side = Some("vm".into());
                    c
                })
                .collect();
            backends = guest.backends.clone();
        }
        Some(Err(said)) => guest_checks.push(JsonCheck {
            name: "vm doctor".into(),
            status: Status::Failed.as_str().into(),
            detail: "the VM is running but did not answer `zygo doctor --json`".into(),
            remedy: Some(format!(
                "limactl shell {} -- zygo doctor{}",
                crate::shim::INSTANCE,
                if said.is_empty() {
                    String::new()
                } else {
                    format!(" (it said: {said})")
                }
            )),
            side: Some("vm".into()),
        }),
        None => {}
    }
    checks.extend(guest_checks.iter().cloned());
    let (ok, code) = verdict(&checks);

    if cli.json {
        crate::output::json(&JsonReport {
            checks,
            backends,
            ok,
        })?;
        return Ok(code);
    }

    let style = Style::stdout();
    let host: Vec<JsonCheck> = checks
        .iter()
        .filter(|c| c.side.as_deref() == Some("host"))
        .cloned()
        .collect();
    print_checks(&style, &host);
    if !guest_checks.is_empty() {
        println!();
        println!("what that VM says about itself:");
        print_checks(&style, &guest_checks);
    }
    println!();
    print_backends(&style, &backends);
    Ok(code)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn check(status: Status) -> JsonCheck {
        JsonCheck {
            name: "x".into(),
            status: status.as_str().into(),
            detail: String::new(),
            remedy: None,
            side: None,
        }
    }

    /// Z-4: `ok: false` beside exit 0 meant neither could be trusted alone.
    /// Both come from one function now, and this is the pair.
    #[test]
    fn ok_and_the_exit_status_cannot_disagree() {
        assert_eq!(verdict(&[]), (true, 0));
        assert_eq!(verdict(&[check(Status::Ok)]), (true, 0));
        assert_eq!(
            verdict(&[check(Status::Degraded)]),
            (true, 0),
            "degraded is usable"
        );
        assert_eq!(verdict(&[check(Status::Absent)]), (true, 0));
        assert_eq!(
            verdict(&[check(Status::Ok), check(Status::Failed)]),
            (false, 1)
        );
    }

    /// The document round-trips, because on macOS the guest's is read back.
    #[test]
    fn the_json_shape_reads_back() {
        let text = r#"{"checks":[{"name":"kernel","status":"ok","detail":"6.8.0"},
            {"name":"pasta","status":"FAIL","detail":"missing","remedy":"apt install passt"}],
            "backends":["ns"],"ok":false}"#;
        let report: JsonReport = serde_json::from_str(text).unwrap();
        assert_eq!(report.checks.len(), 2);
        assert_eq!(report.backends, ["ns"]);
        assert!(!report.ok);
        assert_eq!(verdict(&report.checks), (false, 1));
        assert!(report.checks[0].side.is_none());
    }
}
