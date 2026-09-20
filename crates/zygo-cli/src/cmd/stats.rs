//! `zygo stats [name]` — what the supervisor has actually seen.
//!
//! Two different things, printed side by side and labelled as such, because
//! confusing them is the easiest way to read this wrong:
//!
//! * **`requests` and `failures` are counters**, kept since the function was
//!   warmed and surviving nothing else.
//! * **The latencies come from the log**, which is a bounded ring. They
//!   describe the entries still in it and nothing before them.
//!
//! So a function that served a million requests and whose log holds the last
//! five hundred gets a truthful `p50` over five hundred and a `requests`
//! column that says a million. The `samples` column is what joins them, and
//! it is printed rather than assumed.
//!
//! There is no `p99` over nine samples. A percentile needs enough of them to
//! mean anything, and printing `p99` for a handful is a number with a
//! decimal point and no content — the project has a rule about exactly this.
//! Below a hundred samples the column says `max` instead, which is what the
//! figure would have been anyway and does not pretend otherwise.

use serde::Serialize;
use zygo_core::pool::{LogEntry, LogKind, Status};
use zygo_core::sandbox::SandboxState;
use zygo_core::supervisor::client::Client;
use zygo_core::supervisor::{Request, Response};

use crate::cli::Cli;
use crate::output::{self, Style};

/// Below this many samples a `p99` is the slowest one, so it is labelled as
/// the slowest one. A hundred is the first count at which the ninety-ninth
/// percentile is not simply the maximum.
const ENOUGH_FOR_P99: usize = 100;

/// Everything the log can be asked for. The ring is bounded by the supervisor,
/// so this is "all of it" rather than a number anyone has to tune.
const WHOLE_LOG: u32 = u32::MAX;

#[derive(Debug, Serialize)]
struct FunctionStats {
    name: String,
    state: SandboxState,
    runtime: String,
    /// Since the function was warmed.
    requests: u64,
    failures: u64,
    /// How many request entries the log still holds. Every figure below is
    /// over these and no others.
    samples: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    p50_ms: Option<f64>,
    /// Only when there are enough samples for it to differ from `max_ms`.
    #[serde(skip_serializing_if = "Option::is_none")]
    p99_ms: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_ms: Option<f64>,
    /// Killed for overrunning its deadline.
    timeouts: usize,
    /// Killed by something else — the OOM killer, almost always. Counted
    /// apart from timeouts because the two have opposite remedies.
    other_kills: usize,
}

pub fn run(cli: &Cli, name: Option<&str>) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    let mut client = Client::connect(&paths)?;

    let functions = match client.send(&Request::List)? {
        Response::Functions { functions } => functions,
        other => return super::supervisor::report_failure(cli, &other),
    };

    let wanted: Vec<Status> = match name {
        Some(n) => functions.into_iter().filter(|f| f.name == n).collect(),
        None => functions,
    };

    if let Some(n) = name
        && wanted.is_empty()
    {
        anyhow::bail!("no function named `{n}`; `zygo ps` lists the warm ones");
    }

    let mut stats = Vec::with_capacity(wanted.len());
    for status in wanted {
        let entries = match client.send(&Request::Logs {
            name: status.name.clone(),
            after: 0,
            limit: WHOLE_LOG,
            failed: false,
        })? {
            Response::Logs { entries, .. } => entries,
            // A function that went away between the two questions is not an
            // error; it simply has nothing to report.
            _ => Vec::new(),
        };
        stats.push(summarise(status, &entries));
    }

    if cli.json {
        output::json(&stats)?;
        return Ok(0);
    }
    print_table(&stats);
    Ok(0)
}

/// Fold a function's log into the figures above.
fn summarise(status: Status, entries: &[LogEntry]) -> FunctionStats {
    let mut wall: Vec<f64> = Vec::new();
    let mut timeouts = 0;
    let mut other_kills = 0;

    for entry in entries {
        let LogKind::Request {
            exit_code,
            wall_ms,
            timed_out,
            ..
        } = &entry.kind
        else {
            continue;
        };
        wall.push(*wall_ms);
        // 137 is `SIGKILL`, and both the deadline and the OOM killer use it.
        // The log carries which, because nothing else can.
        if *timed_out {
            timeouts += 1;
        } else if *exit_code == 137 {
            other_kills += 1;
        }
    }

    wall.sort_by(f64::total_cmp);
    let samples = wall.len();

    FunctionStats {
        name: status.name,
        state: status.state,
        runtime: status.runtime,
        requests: status.requests,
        failures: status.failures,
        samples,
        p50_ms: percentile(&wall, 0.50),
        p99_ms: (samples >= ENOUGH_FOR_P99)
            .then(|| percentile(&wall, 0.99))
            .flatten(),
        max_ms: wall.last().copied(),
        timeouts,
        other_kills,
    }
}

/// Nearest-rank percentile of a sorted slice.
///
/// Nearest-rank rather than interpolated: these are observed durations, and
/// every figure printed should be one that actually happened.
fn percentile(sorted: &[f64], q: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (q * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted.get(rank - 1).copied()
}

fn print_table(stats: &[FunctionStats]) {
    let style = Style::stdout();
    if stats.is_empty() {
        println!("{}", style.dim("no warm functions"));
        return;
    }

    let ms = |v: Option<f64>| {
        v.map(|v| format!("{v:.1} ms"))
            .unwrap_or_else(|| "—".into())
    };
    let name_w = stats.iter().map(|s| s.name.len()).max().unwrap_or(4).max(4);

    println!(
        "{:name_w$}  {:6}  {:>8}  {:>8}  {:>7}  {:>9}  {:>9}  {:>8}  {:>6}",
        "NAME", "STATE", "REQUESTS", "FAILURES", "SAMPLES", "p50", "p99", "MAX", "KILLED"
    );
    let mut any_short = false;
    for s in stats {
        if s.samples > 0 && s.p99_ms.is_none() {
            any_short = true;
        }
        let killed = s.timeouts + s.other_kills;
        println!(
            "{:name_w$}  {:6}  {:>8}  {:>8}  {:>7}  {:>9}  {:>9}  {:>8}  {:>6}",
            s.name,
            s.state.as_str(),
            s.requests,
            s.failures,
            s.samples,
            ms(s.p50_ms),
            // Not a guess dressed as a percentile: see `ENOUGH_FOR_P99`.
            s.p99_ms
                .map(|v| format!("{v:.1} ms"))
                .unwrap_or_else(|| "—".into()),
            ms(s.max_ms),
            killed,
        );
    }

    let timeouts: usize = stats.iter().map(|s| s.timeouts).sum();
    let others: usize = stats.iter().map(|s| s.other_kills).sum();
    println!();
    println!(
        "{}",
        style.dim(
            "requests and failures are counted since the function was warmed; \
             the latencies are over the entries still in its log"
        )
    );
    if any_short {
        println!(
            "{}",
            style.dim(&format!(
                "p99 is shown from {ENOUGH_FOR_P99} samples up; below that it would be the max"
            ))
        );
    }
    if timeouts + others > 0 {
        println!(
            "{}",
            style.dim(&format!(
                "of the killed: {timeouts} overran a deadline, {others} were killed by \
                 something else — usually the memory limit"
            ))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(wall_ms: f64, exit_code: i32, timed_out: bool) -> LogEntry {
        LogEntry {
            seq: 1,
            at_ms: 0,
            kind: LogKind::Request {
                id: "x".into(),
                exit_code,
                wall_ms,
                timed_out,
                error: None,
                stderr: String::new(),
            },
            text: String::new(),
        }
    }

    fn status() -> Status {
        Status {
            name: "f".into(),
            state: SandboxState::Warm,
            runtime: "python/3.12".into(),
            rss_kb: 1000,
            imports_ms: 1.0,
            requests: 1_000_000,
            failures: 3,
        }
    }

    #[test]
    fn the_percentile_is_one_of_the_observed_values() {
        let sorted = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&sorted, 0.5), Some(2.0));
        assert_eq!(percentile(&sorted, 0.99), Some(4.0));
        assert_eq!(percentile(&[], 0.5), None);
        // Nearest-rank, so never a number that did not happen.
        assert_eq!(percentile(&[7.0], 0.5), Some(7.0));
    }

    /// A handful of samples has no ninety-ninth percentile, and saying so is
    /// the point. The old behaviour of this shape of code is to print the
    /// maximum with a `p99` label on it.
    #[test]
    fn p99_is_withheld_until_there_are_enough_samples() {
        let few: Vec<LogEntry> = (0..10).map(|i| request(i as f64, 0, false)).collect();
        let short = summarise(status(), &few);
        assert_eq!(short.samples, 10);
        assert!(short.p50_ms.is_some());
        assert_eq!(short.p99_ms, None, "ten samples have no p99");
        assert!(short.max_ms.is_some(), "the slowest is still reported");

        let many: Vec<LogEntry> = (0..ENOUGH_FOR_P99)
            .map(|i| request(i as f64, 0, false))
            .collect();
        assert!(summarise(status(), &many).p99_ms.is_some());
    }

    /// Both arrive as exit 137 and they have opposite remedies: one function
    /// is too slow, the other needs more memory.
    #[test]
    fn a_deadline_kill_is_counted_apart_from_every_other_kill() {
        let entries = vec![
            request(1.0, 0, false),
            request(2.0, 137, true),
            request(3.0, 137, false),
            request(4.0, 1, false),
        ];
        let s = summarise(status(), &entries);
        assert_eq!(s.timeouts, 1);
        assert_eq!(s.other_kills, 1);
        assert_eq!(s.samples, 4, "every request is a sample, killed or not");
    }

    /// The counters and the log are different windows, and the summary keeps
    /// them apart rather than quietly reporting the smaller one twice.
    #[test]
    fn the_counters_are_not_the_sample_count() {
        let s = summarise(status(), &[request(1.0, 0, false)]);
        assert_eq!(s.requests, 1_000_000, "the counter is since the warm-up");
        assert_eq!(s.samples, 1, "the log holds one");
    }
}
