// SPDX-License-Identifier: Apache-2.0
//! A function's recent log, kept in memory for `zygo logs`.
//!
//! One bounded ring per function *name*: the zygote's own stderr goes in
//! as it is written, and the supervisor adds a line per request. It
//! belongs to the name rather than to a sandbox, so a replacement's
//! import-time output lands where a `zygo logs -f` is already looking.

use std::sync::Mutex;

/// Entries kept per function, most recent last. Bounded, because a function
/// that logs a line per request would otherwise grow the supervisor without
/// limit.
pub const LOG_ENTRIES: usize = 500;

/// Longest text kept per entry. A request that prints a megabyte gets the
/// first four kilobytes and a marker; the full output went to the caller.
pub const LOG_TEXT_BYTES: usize = 4096;

/// What a log entry is about.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LogKind {
    /// A line the zygote itself wrote — the agent's stderr: import-time
    /// output, a crash, a deprecation warning from a dependency.
    Zygote,
    /// One request. `text` is its stdout.
    Request {
        id: String,
        exit_code: i32,
        wall_ms: f64,
        /// The supervisor killed this one for overrunning its deadline.
        ///
        /// Carried into the log because the exit code cannot say it: a
        /// deadline kill and an OOM kill both arrive as 137, and only the
        /// side that enforced the deadline knows which it was. Without this
        /// the log can count kills and not explain any of them, which is the
        /// difference between "your function is too slow" and "your function
        /// needs more memory".
        ///
        /// `default` so a log written by an older supervisor still parses.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        timed_out: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        stderr: String,
    },
}

/// One entry of a function's log.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LogEntry {
    /// Monotonic per function, so `zygo logs -f` can ask for "everything
    /// after the last one I saw".
    pub seq: u64,
    /// Unix time, milliseconds.
    pub at_ms: u64,
    #[serde(flatten)]
    pub kind: LogKind,
    pub text: String,
}

impl LogEntry {
    /// Whether this entry is a request that failed.
    pub fn failed(&self) -> bool {
        matches!(
            &self.kind,
            LogKind::Request {
                exit_code, error, ..
            } if *exit_code != 0 || error.is_some()
        )
    }
}

/// A function's recent log, shared between the pool (which writes the
/// zygote's lines), the supervisor (which writes the requests) and whoever
/// reads it. Survives the function being replaced or going cold: it belongs
/// to the *name*, not to one sandbox.
#[derive(Debug, Default)]
pub struct LogRing {
    inner: Mutex<(std::collections::VecDeque<LogEntry>, u64)>,
}

/// The shared handle.
pub type Logs = std::sync::Arc<LogRing>;

impl LogRing {
    pub fn push(&self, kind: LogKind, text: impl Into<String>) {
        let mut text: String = text.into();
        if text.len() > LOG_TEXT_BYTES {
            let mut cut = LOG_TEXT_BYTES;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("… [truncated]");
        }
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut inner = self.inner.lock().expect("log ring");
        let (entries, next) = &mut *inner;
        let seq = *next;
        *next += 1;
        entries.push_back(LogEntry {
            seq,
            at_ms,
            kind,
            text,
        });
        while entries.len() > LOG_ENTRIES {
            entries.pop_front();
        }
    }

    /// Entries with `seq >= after`, oldest first, at most `limit`, and the
    /// sequence number to ask for next time. `limit` counts from the *end*
    /// when `after` is zero, which is what `--tail` means.
    pub fn since(&self, after: u64, limit: usize, failed_only: bool) -> (Vec<LogEntry>, u64) {
        let inner = self.inner.lock().expect("log ring");
        let (entries, next) = &*inner;
        let matching: Vec<&LogEntry> = entries
            .iter()
            .filter(|e| e.seq >= after)
            .filter(|e| !failed_only || e.failed())
            .collect();
        let start = if after == 0 {
            matching.len().saturating_sub(limit)
        } else {
            0
        };
        let taken: Vec<LogEntry> = matching
            .into_iter()
            .skip(start)
            .take(limit)
            .cloned()
            .collect();
        (taken, *next)
    }
}

/// Read the zygote's output line by line into its log, for as long as the
/// sandbox writes any. Ends at end of file, which is when the sandbox is gone.
pub(super) fn spawn_zygote_log_reader(name: &str, read_end: std::os::fd::OwnedFd, logs: Logs) {
    use std::io::BufRead;
    let name = name.to_string();
    let thread = std::thread::Builder::new()
        .name(format!("zygo-log-{name}"))
        .spawn(move || {
            let reader = std::io::BufReader::new(std::fs::File::from(read_end));
            for line in reader.split(b'\n') {
                let Ok(line) = line else { break };
                let text = String::from_utf8_lossy(&line).into_owned();
                tracing::info!(target: "zygote", function = %name, "{text}");
                logs.push(LogKind::Zygote, text);
            }
        });
    if let Err(e) = thread {
        tracing::warn!("could not start the zygote log reader: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(exit_code: i32, error: Option<&str>) -> LogKind {
        LogKind::Request {
            id: "r".into(),
            exit_code,
            timed_out: false,
            wall_ms: 1.0,
            error: error.map(str::to_string),
            stderr: String::new(),
        }
    }

    #[test]
    fn the_ring_is_bounded_and_keeps_the_newest() {
        let ring = LogRing::default();
        for i in 0..(LOG_ENTRIES + 25) {
            ring.push(LogKind::Zygote, format!("line {i}"));
        }
        let (all, next) = ring.since(0, usize::MAX, false);
        assert_eq!(all.len(), LOG_ENTRIES);
        assert_eq!(all[0].text, "line 25", "the oldest 25 were dropped");
        assert_eq!(
            next,
            (LOG_ENTRIES + 25) as u64,
            "sequence numbers never reset"
        );
    }

    #[test]
    fn tail_is_the_most_recent_and_follow_is_everything_after() {
        let ring = LogRing::default();
        for i in 0..10 {
            ring.push(LogKind::Zygote, format!("{i}"));
        }
        let (tail, next) = ring.since(0, 3, false);
        assert_eq!(
            tail.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["7", "8", "9"]
        );
        assert_eq!(next, 10);

        ring.push(LogKind::Zygote, "10");
        let (more, next) = ring.since(next, usize::MAX, false);
        assert_eq!(more.len(), 1, "only what arrived since");
        assert_eq!(more[0].text, "10");
        assert_eq!(next, 11);
        let (none, _) = ring.since(next, usize::MAX, false);
        assert!(none.is_empty());
    }

    #[test]
    fn failed_only_keeps_requests_that_failed_and_never_zygote_lines() {
        let ring = LogRing::default();
        ring.push(LogKind::Zygote, "warming up");
        ring.push(request(0, None), "fine");
        ring.push(request(1, None), "exit 1");
        ring.push(request(0, Some("ValueError")), "raised");
        let (failed, _) = ring.since(0, usize::MAX, true);
        assert_eq!(
            failed.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            ["exit 1", "raised"]
        );
    }

    #[test]
    fn long_output_is_cut_at_a_character_boundary_and_marked() {
        let ring = LogRing::default();
        // Multi-byte characters straddling the cut must not split.
        let text = "é".repeat(LOG_TEXT_BYTES);
        ring.push(LogKind::Zygote, text);
        let (entries, _) = ring.since(0, 1, false);
        assert!(entries[0].text.ends_with("… [truncated]"));
        assert!(entries[0].text.len() <= LOG_TEXT_BYTES + "… [truncated]".len());
        assert!(
            entries[0]
                .text
                .trim_end_matches("… [truncated]")
                .chars()
                .all(|c| c == 'é')
        );
    }

    #[test]
    fn a_log_entry_round_trips_the_control_socket() {
        let entry = LogEntry {
            seq: 7,
            at_ms: 1_700_000_000_000,
            kind: request(1, Some("boom")),
            text: "out".into(),
        };
        let json = serde_json::to_value(&entry).expect("json");
        assert_eq!(json["kind"], "request", "the kind is a flat tag");
        assert_eq!(json["exit_code"], 1);
        let back: LogEntry = serde_json::from_value(json).expect("back");
        assert_eq!(back, entry);
        assert!(back.failed());
    }
}
