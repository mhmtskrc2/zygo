// SPDX-License-Identifier: Apache-2.0
//! `zygo secrets` — the operator's side of per-tenant secrets.
//!
//! The store is in `zygo-core`; this is the terminal in front of it. The one
//! thing worth saying here is what `set` does *not* do: there is no `--value`
//! flag. An argument is visible in `ps` to every process on the machine and
//! lands in the shell's history, so a value comes from the terminal with echo
//! off or from standard input, exactly as `zygo login` takes a password.
//!
//! There is no `get`. The store seals values and can list names; a command
//! that read one back would be a way to turn "can run zygo" into "has every
//! customer's API key", which is what sealing them is for.

use zygo_core::supervisor::client::Client;
use zygo_core::supervisor::{Request, Response};

use crate::cli::{Cli, SecretsCommand};
use crate::output::{self, Style};

pub fn run(cli: &Cli, command: &SecretsCommand) -> anyhow::Result<u8> {
    // Keygen touches nothing: it is what an operator runs *before* there is a
    // supervisor to talk to.
    if let SecretsCommand::Keygen = command {
        let key = zygo_core::secrets::SecretKey::generate()?;
        println!("{key}");
        let style = Style::stderr();
        eprintln!(
            "{}",
            style.dim(&format!(
                "put this in {} before starting the supervisor; Zygo does not store it, \
                 and a secret sealed with it cannot be read without it",
                zygo_core::secrets::KEY_ENV
            ))
        );
        return Ok(0);
    }

    let paths = super::paths(cli);
    let exe = std::env::current_exe()?;
    let mut client = Client::connect_or_start(&paths, &exe)?;

    match command {
        SecretsCommand::Keygen => unreachable!("handled above"),

        SecretsCommand::Set {
            tenant,
            name,
            stdin,
        } => {
            let value = if *stdin {
                super::login::read_all_stdin()?
            } else {
                super::login::prompt_password(tenant, &format!("Value for {name}: "))?
            };
            anyhow::ensure!(!value.is_empty(), "the value is empty");
            let response = client.send(&Request::PutSecret {
                tenant: tenant.clone(),
                name: name.clone(),
                value,
            })?;
            match response {
                Response::Secrets { names } => {
                    if cli.json {
                        output::json(&serde_json::json!({ "secrets": names }))?;
                    } else {
                        eprintln!("{} {name} for {tenant}", Style::stderr().dim("stored"));
                    }
                    Ok(0)
                }
                other => super::supervisor::report_failure(cli, &other),
            }
        }

        SecretsCommand::Ls { tenant } => {
            let names = match client.send(&Request::Secrets {
                tenant: tenant.clone(),
            })? {
                Response::Secrets { names } => names,
                other => return super::supervisor::report_failure(cli, &other),
            };
            if cli.json {
                output::json(&names)?;
                return Ok(0);
            }
            if names.is_empty() {
                eprintln!(
                    "{}",
                    Style::stderr().dim(&format!("{tenant} has no secrets"))
                );
                return Ok(0);
            }
            for name in &names {
                println!("{name}");
            }
            Ok(0)
        }

        SecretsCommand::Rm { tenant, name } => {
            let response = client.send(&Request::DeleteSecret {
                tenant: tenant.clone(),
                name: name.clone(),
            })?;
            match response {
                Response::Secrets { .. } => {
                    if cli.json {
                        output::json(&serde_json::json!({ "removed": name }))?;
                    } else {
                        eprintln!("{} {name} from {tenant}", Style::stderr().dim("removed"));
                    }
                    Ok(0)
                }
                other => super::supervisor::report_failure(cli, &other),
            }
        }
    }
}
