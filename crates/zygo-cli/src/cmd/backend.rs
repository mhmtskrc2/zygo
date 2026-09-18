//! `zygo backend list` and `zygo backend install`.

use zygo_core::backend;
use zygo_core::spec::Isolation;

use crate::cli::{BackendCommand, Cli};
use crate::output::{self, Style};

pub fn run(cli: &Cli, command: &BackendCommand) -> anyhow::Result<u8> {
    match command {
        BackendCommand::List => list(cli),
        BackendCommand::Install { name } => install(name),
    }
}

fn list(cli: &Cli) -> anyhow::Result<u8> {
    let style = Style::stdout();
    let mut rows = Vec::new();
    let mut json_rows = Vec::new();

    for isolation in Isolation::ALL {
        let (status, detail) = match backend::for_isolation(*isolation) {
            Ok(_) => ("available".to_string(), String::new()),
            Err(zygo_core::Error::BackendUnavailable { reason, .. }) => {
                ("unavailable".to_string(), reason)
            }
            Err(e) => ("unavailable".to_string(), e.to_string()),
        };

        json_rows.push(serde_json::json!({
            "backend": isolation.as_str(),
            "status": status,
            "detail": detail,
        }));
        rows.push(vec![
            isolation.as_str().to_string(),
            if status == "available" {
                style.green(&status)
            } else {
                style.dim(&status)
            },
            detail,
        ]);
    }

    if cli.json {
        output::json(&json_rows)?;
    } else {
        print!("{}", output::table(&["backend", "status", "detail"], &rows));
    }
    Ok(0)
}

fn install(name: &str) -> anyhow::Result<u8> {
    match name {
        "gvisor" => anyhow::bail!(
            "the gvisor backend is not wired up yet (todo.md, phase 4)\n  \
             → use --isolation ns meanwhile"
        ),
        "vm" => anyhow::bail!(
            "the vm backend is linked into the binary, not downloaded (todo.md, phase 2.5)"
        ),
        other => anyhow::bail!("unknown backend `{other}`\n  → known backends: gvisor, vm"),
    }
}
