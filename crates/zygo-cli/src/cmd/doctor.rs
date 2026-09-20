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

pub fn run(cli: &Cli) -> anyhow::Result<u8> {
    let report = doctor::run();

    // Two questions, and the answer is the *and* of them: does the host have
    // what the backend needs, and does this binary implement it? Reporting
    // only the first printed `backends available: ns, vm` on a machine with
    // `/dev/kvm` where `zygo backend list` said — correctly — that `vm` is not
    // built. The library cannot join them, because a backend's own
    // availability check consults this report and the pair would recurse.
    let usable: Vec<&'static str> = Isolation::ALL
        .iter()
        .filter(|i| report.supports(**i) && zygo_core::backend::for_isolation(**i).is_ok())
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

    Ok(report.exit_code() as u8)
}
