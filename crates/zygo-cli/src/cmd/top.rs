// SPDX-License-Identifier: Apache-2.0
//! `zygo top` — the live table.
//!
//! What justifies this existing beside `zygo ps` is the pair of columns `ps`
//! cannot have: a rate needs two samples, and `ps` takes one. Everything else
//! here is `ps` on a timer.
//!
//! So the first frame has no rates, and says `—` rather than `0`. A zero
//! would be a measurement; a dash is the absence of one, and the difference
//! matters on the frame somebody screenshots.
//!
//! The rate is over the interval that actually elapsed, not the one that was
//! asked for. On a loaded machine those differ, and dividing by the requested
//! interval would quietly overstate every figure.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use zygo_core::pool::Status;
use zygo_core::supervisor::client::Client;
use zygo_core::supervisor::{Request, Response};

use crate::cli::Cli;
use crate::output::{self, Style};

/// Counters from the previous frame, to subtract.
struct Sample {
    at: Instant,
    counters: HashMap<String, (u64, u64)>,
}

pub fn run(cli: &Cli, interval: f64, once: bool) -> anyhow::Result<u8> {
    anyhow::ensure!(interval > 0.0, "the interval must be greater than zero");
    let interval = Duration::from_secs_f64(interval);

    let paths = super::paths(cli);
    let mut client = Client::connect(&paths)?;
    let style = Style::stdout();
    // Clearing the screen is for a person watching. Piped into a file it
    // would write escape sequences into it, and `--once` is the scriptable
    // form, so neither clears.
    let animate = !once && !cli.json && std::io::IsTerminal::is_terminal(&std::io::stdout());

    let mut previous: Option<Sample> = None;
    loop {
        let functions = match client.send(&Request::List)? {
            Response::Functions { functions } => functions,
            other => return super::supervisor::report_failure(cli, &other),
        };
        // Two registries, two tables. A pool has no single state and no one
        // handler, so its row is warm/paused/cold counts rather than the
        // function columns with three of them left blank.
        let pools = match client.send(&Request::Runtimes)? {
            Response::Runtimes { runtimes } => runtimes,
            other => return super::supervisor::report_failure(cli, &other),
        };
        let now = Instant::now();
        let rows: Vec<Row> = functions
            .iter()
            .map(|f| Row::of(f, previous.as_ref(), now))
            .collect();

        if cli.json {
            output::json(&serde_json::json!({
                "functions": rows,
                "runtimes": pools,
            }))?;
            return Ok(0);
        }

        if animate {
            // Home, then clear to the end: the frame is redrawn over the last
            // one rather than scrolling, and a terminal that does not
            // understand it is left with a readable log instead of garbage.
            print!("\x1b[H\x1b[J");
        }
        print(&rows, &style, previous.is_none() && pools.is_empty());
        print_pools(&pools, &style);

        previous = Some(Sample {
            at: now,
            counters: functions
                .iter()
                .map(|f| (f.name.clone(), (f.requests, f.failures)))
                .collect(),
        });

        if once {
            return Ok(0);
        }
        std::thread::sleep(interval);
    }
}

#[derive(serde::Serialize)]
struct Row {
    name: String,
    state: String,
    rss_kb: u64,
    requests: u64,
    failures: u64,
    /// `None` on the first frame, and for a function that was not in the
    /// previous one: there is nothing to subtract from.
    #[serde(skip_serializing_if = "Option::is_none")]
    requests_per_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    failures_per_s: Option<f64>,
}

impl Row {
    fn of(f: &Status, previous: Option<&Sample>, now: Instant) -> Row {
        let rates = previous.and_then(|p| {
            let (req, fail) = p.counters.get(&f.name)?;
            // Over the interval that happened, not the one that was asked
            // for.
            let seconds = now.duration_since(p.at).as_secs_f64();
            if seconds <= 0.0 {
                return None;
            }
            Some((
                f.requests.saturating_sub(*req) as f64 / seconds,
                f.failures.saturating_sub(*fail) as f64 / seconds,
            ))
        });
        Row {
            name: f.name.clone(),
            state: f.state.as_str().to_string(),
            rss_kb: f.rss_kb,
            requests: f.requests,
            failures: f.failures,
            requests_per_s: rates.map(|(r, _)| r),
            failures_per_s: rates.map(|(_, f)| f),
        }
    }
}

/// One row per runtime pool: how many zygotes it has, in what state, and how
/// much room is left between them and `max_warm`.
///
/// Counts rather than a state, because a pool does not have one: four zygotes
/// of which two are frozen is a pool that is working and idle at once, and
/// collapsing that into a word would lose the thing an operator is looking
/// for.
fn print_pools(pools: &[zygo_core::supervisor::RuntimeStatus], style: &Style) {
    if pools.is_empty() {
        return;
    }
    let name_w = pools.iter().map(|p| p.name.len()).max().unwrap_or(7).max(7);
    println!();
    println!(
        "{:name_w$}  {:>5} {:>6} {:>5}  {:>7}  {:>9}  {:>8}  {:>8}",
        "RUNTIME", "WARM", "PAUSED", "ROOM", "IN/QUEUE", "RSS", "REQUESTS", "FAILURES"
    );
    for p in pools {
        println!(
            "{:name_w$}  {:>5} {:>6} {:>5}  {:>7}  {:>9}  {:>8}  {:>8}",
            p.name,
            p.warm,
            p.paused,
            p.cold,
            format!("{}/{}", p.in_flight, p.queued),
            format!("{:.1} MB", p.rss_kb as f64 / 1024.0),
            p.requests,
            p.failures,
        );
    }
    println!(
        "{}",
        style.dim("  ROOM is what is left between the zygotes that exist and max_warm")
    );
}

fn print(rows: &[Row], style: &Style, first_frame: bool) {
    if rows.is_empty() {
        println!("{}", style.dim("no warm functions"));
        return;
    }
    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(4).max(4);
    println!(
        "{:name_w$}  {:6}  {:>9}  {:>8}  {:>8}  {:>8}",
        "NAME", "STATE", "RSS", "REQ/S", "REQUESTS", "FAILURES"
    );
    for r in rows {
        let rate = match r.requests_per_s {
            Some(v) => format!("{v:.1}"),
            None => "—".into(),
        };
        println!(
            "{:name_w$}  {:6}  {:>9}  {:>8}  {:>8}  {:>8}",
            r.name,
            r.state,
            format!("{:.1} MB", r.rss_kb as f64 / 1024.0),
            rate,
            r.requests,
            r.failures,
        );
    }
    if first_frame {
        println!();
        println!(
            "{}",
            style.dim("a rate needs two samples; the next frame has one")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zygo_core::sandbox::SandboxState;

    fn status(name: &str, requests: u64, failures: u64) -> Status {
        Status {
            tenant: "default".into(),
            name: name.into(),
            image: String::new(),
            state: SandboxState::Warm,
            runtime: "python/3.12".into(),
            rss_kb: 2048,
            imports_ms: 1.0,
            requests,
            failures,
        }
    }

    /// The first frame has nothing to subtract from, and says so instead of
    /// reporting a rate of zero — which is a measurement, and would be wrong.
    #[test]
    fn the_first_frame_has_no_rate() {
        let row = Row::of(&status("f", 10, 1), None, Instant::now());
        assert_eq!(row.requests_per_s, None);
        assert_eq!(row.requests, 10, "the counter is still shown");
    }

    #[test]
    fn a_rate_is_the_difference_over_the_time_that_passed() {
        let then = Instant::now();
        let previous = Sample {
            at: then,
            counters: [("f".to_string(), (10, 1))].into_iter().collect(),
        };
        // Two seconds later, twenty more requests and one more failure.
        let now = then + Duration::from_secs(2);
        let row = Row::of(&status("f", 30, 2), Some(&previous), now);
        assert_eq!(row.requests_per_s, Some(10.0));
        assert_eq!(row.failures_per_s, Some(0.5));
    }

    /// A function that appears between frames has no previous counter, and a
    /// rate computed from its absolute count would be its whole lifetime's
    /// traffic reported as if it had happened in one interval.
    #[test]
    fn a_function_that_was_not_there_before_gets_no_rate() {
        let then = Instant::now();
        let previous = Sample {
            at: then,
            counters: [("other".to_string(), (5, 0))].into_iter().collect(),
        };
        let row = Row::of(
            &status("new", 900, 0),
            Some(&previous),
            then + Duration::from_secs(1),
        );
        assert_eq!(row.requests_per_s, None);
    }

    /// A supervisor restart resets the counters, and subtracting a larger
    /// previous value would underflow. Saturating makes that a zero rate
    /// rather than an enormous one.
    #[test]
    fn counters_going_backwards_do_not_become_an_enormous_rate() {
        let then = Instant::now();
        let previous = Sample {
            at: then,
            counters: [("f".to_string(), (1000, 10))].into_iter().collect(),
        };
        let row = Row::of(
            &status("f", 3, 0),
            Some(&previous),
            then + Duration::from_secs(1),
        );
        assert_eq!(row.requests_per_s, Some(0.0));
    }
}
