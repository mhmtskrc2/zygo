//! `zygo token` — who may talk to the API, and as whom.
//!
//! The operator's side of scoped tokens. `zygo api` reads them; this is what
//! writes them, and it goes through the supervisor rather than touching the
//! file, because the supervisor is the single writer for everything else it
//! owns and a second one here would be a second opinion about what a token is.
//!
//! The secret is printed **once**, by `mint`, and is not stored — the file
//! holds a SHA-256. That is deliberate and it is the reason `ls` can list
//! every token on the host without being a way to steal one.

use zygo_core::supervisor::client::Client;
use zygo_core::supervisor::{Request, Response};
use zygo_core::tokens::Token;

use crate::cli::{Cli, TokenCommand};
use crate::output::{self, Style};

pub fn run(cli: &Cli, command: &TokenCommand) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    let exe = std::env::current_exe()?;
    let mut client = Client::connect_or_start(&paths, &exe)?;

    match command {
        TokenCommand::Mint { tenant } => {
            let response = client.send(&Request::MintToken {
                tenant: tenant.clone(),
            })?;
            let (tokens, secret) = match response {
                Response::Tokens { tokens, secret } => (tokens, secret),
                other => return super::supervisor::report_failure(cli, &other),
            };
            let token = tokens.first();
            let secret = secret.unwrap_or_default();

            if cli.json {
                output::json(&serde_json::json!({ "token": token, "secret": secret }))?;
                return Ok(0);
            }

            let style = Style::stdout();
            let id = token.map(|t| t.id.as_str()).unwrap_or("?");
            println!("{secret}");
            eprintln!(
                "{} {id}  {}",
                style.dim("minted"),
                style.dim(&describe(token)),
            );
            // To stderr, so `ZYGO_API_TOKEN=$(zygo token mint)` is still the
            // obvious thing to type and still captures only the secret.
            eprintln!(
                "{}",
                style.dim("the secret is above and is not stored; it cannot be printed again")
            );
            Ok(0)
        }

        TokenCommand::Ls => {
            let tokens = match client.send(&Request::Tokens)? {
                Response::Tokens { tokens, .. } => tokens,
                other => return super::supervisor::report_failure(cli, &other),
            };
            if cli.json {
                output::json(&tokens)?;
                return Ok(0);
            }
            if tokens.is_empty() {
                eprintln!(
                    "{}",
                    Style::stderr().dim("no tokens; `zygo token mint` makes one")
                );
                return Ok(0);
            }
            print_table(&tokens);
            Ok(0)
        }

        TokenCommand::Revoke { id } => {
            let response = client.send(&Request::RevokeToken { id: id.clone() })?;
            match response {
                Response::Tokens { .. } => {
                    if cli.json {
                        output::json(&serde_json::json!({ "revoked": id }))?;
                    } else {
                        eprintln!(
                            "{} {id}  {}",
                            Style::stderr().dim("revoked"),
                            Style::stderr().dim("it stops working on the next request")
                        );
                    }
                    Ok(0)
                }
                other => super::supervisor::report_failure(cli, &other),
            }
        }
    }
}

/// What a token is, in the words the help uses.
fn describe(token: Option<&Token>) -> String {
    match token.map(|t| t.tenant()) {
        Some(Some(tenant)) => format!("tenant `{tenant}`: its own scripts and calls"),
        Some(None) => "operator: tenants, functions, pools and tokens".to_string(),
        None => String::new(),
    }
}

fn print_table(tokens: &[Token]) {
    let rows: Vec<Vec<String>> = tokens
        .iter()
        .map(|token| {
            let scope = match token.tenant() {
                Some(tenant) => format!("tenant {tenant}"),
                None => "operator".to_string(),
            };
            // Revoked rather than gone: the id in a log line has to resolve
            // to something, and "this token was revoked" is the answer
            // somebody reading that line wants.
            let state = if token.revoked() { "revoked" } else { "active" };
            vec![token.id.clone(), scope, state.to_string()]
        })
        .collect();
    print!("{}", output::table(&["id", "scope", "state"], &rows));
}
