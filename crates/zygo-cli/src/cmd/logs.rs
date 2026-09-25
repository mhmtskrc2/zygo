// SPDX-License-Identifier: Apache-2.0
//! `zygo logs <name>` — a function's recent log.
//!
//! The supervisor keeps a bounded ring per function name: the zygote's own
//! output (import-time warnings, a crash traceback) and one entry per request
//! with its exit code, timing, stdout and stderr. This command prints it, and
//! with `-f` keeps asking for what came after the last entry it saw — a poll
//! every half second rather than a stream, because the control socket carries
//! one reply per request and a stream would be a second protocol for one
//! command.

use std::time::Duration;

use zygo_core::pool::{LogEntry, LogKind};
use zygo_core::supervisor::client::Client;
use zygo_core::supervisor::{Request, Response};

use crate::cli::Cli;
use crate::output::Style;

/// How often `-f` asks again.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(500);

pub fn run(cli: &Cli, name: &str, follow: bool, tail: u32, failed: bool) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    let mut client = Client::connect(&paths)?;
    let style = Style::stdout();

    let mut after = 0;
    let mut limit = tail;
    loop {
        let response = client.send(&Request::Logs {
            name: name.to_string(),
            after,
            limit,
            failed,
            // The CLI is the operator at a terminal; see `zygo exec`.
            tenant: None,
        })?;
        let (entries, next) = match response {
            Response::Logs { entries, next, .. } => (entries, next),
            other => return super::supervisor::report_failure(cli, &other),
        };
        for entry in &entries {
            if cli.json {
                // One compact object per line — a log is consumed line by
                // line, and a pretty-printed entry spanning ten of them would
                // be neither greppable nor streamable.
                println!("{}", serde_json::to_string(entry)?);
            } else {
                print(&style, entry);
            }
        }
        if !follow {
            return Ok(0);
        }
        // Everything after what was just shown; the first round's tail limit
        // no longer applies, so ask for as much as arrived.
        after = next;
        limit = u32::MAX;
        std::thread::sleep(FOLLOW_INTERVAL);
    }
}

fn print(style: &Style, entry: &LogEntry) {
    let at = timestamp(entry.at_ms);
    match &entry.kind {
        LogKind::Zygote => {
            println!(
                "{} {}  {}",
                style.dim(&at),
                style.dim("zygote "),
                entry.text
            );
        }
        LogKind::Request {
            id,
            exit_code,
            wall_ms,
            timed_out,
            error,
            stderr,
        } => {
            // A killed request says *why* it was killed. Exit 137 alone
            // cannot: the deadline and the OOM killer both produce it, and
            // the two mean opposite things to whoever has to fix it.
            let status = if *exit_code == 0 && error.is_none() {
                style.green(&format!("exit {exit_code}"))
            } else if *timed_out {
                style.red(&format!("exit {exit_code} (deadline)"))
            } else {
                style.red(&format!("exit {exit_code}"))
            };
            println!(
                "{} {} {status}  {}",
                style.dim(&at),
                style.dim(&format!("req {id}")),
                style.dim(&format!("{wall_ms:.1} ms"))
            );
            for line in entry.text.lines() {
                println!("    {line}");
            }
            for line in stderr.lines() {
                println!("    {}", style.yellow(line));
            }
            if let Some(error) = error {
                for line in error.lines() {
                    println!("    {}", style.red(line));
                }
            }
        }
    }
}

/// `HH:MM:SS.mmm` in UTC — enough to line entries up with each other and with
/// the supervisor's own log, without a date library for a debugging aid.
fn timestamp(at_ms: u64) -> String {
    let secs = at_ms / 1000;
    let ms = at_ms % 1000;
    let day_secs = secs % 86_400;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60,
        ms
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_wall_clock_to_the_millisecond() {
        assert_eq!(timestamp(0), "00:00:00.000");
        // 2023-11-14T22:13:20.500Z
        assert_eq!(timestamp(1_700_000_000_500), "22:13:20.500");
    }
}
