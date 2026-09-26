// SPDX-License-Identifier: Apache-2.0
//! What a finished request produced, and what it cost.
//!
//! [`Outcome`] is the answer a caller gets back — result, streams, metrics,
//! and which of the four kills exit 137 can mean, if any. [`Usage`] is the
//! same request as somebody counting it sees it. Both are plain data that
//! cross the control socket, which is why they live apart from the
//! sandboxes that produce them.

use crate::protocol::Metrics;

/// What a completed request produced.
///
/// Serialisable because it crosses the supervisor's control socket on its way
/// back to `zygo exec`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Outcome {
    pub exit_code: i32,
    pub result: serde_json::Value,
    pub stdout: String,
    pub stderr: String,
    /// Set when the handler raised; `result` is then meaningless.
    pub error: Option<String>,
    pub metrics: Metrics,
    /// The supervisor killed this request for overrunning its deadline.
    ///
    /// Recorded here because nothing else can tell: a deadline kill and an OOM
    /// kill both arrive as exit 137, and only the side that enforced the
    /// deadline knows which it was. The HTTP API's 408 depends on it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
    /// The agent stopped saying this request was alive, and it was killed.
    ///
    /// The fourth reading of exit 137, and the one that is not about the
    /// caller's limits at all. A deadline says the work is too slow; an
    /// out-of-memory kill says it is too big; a cancel says somebody changed
    /// their mind. This says the *sandbox* went quiet — an agent that stopped
    /// scheduling, a child wedged where no signal it can send will reach it —
    /// and the advice that follows is about the function, not about the
    /// number in its spec.
    ///
    /// Only reachable for a request long enough to miss a heartbeat. A short
    /// one that wedges is killed by its own deadline, which is the older
    /// backstop and is still the right one when the budget is seconds.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stuck: bool,
    /// Somebody asked for this request to stop, and it did.
    ///
    /// The third reading of exit 137, and the same argument as `timed_out`:
    /// a cancel kill, a deadline kill and an out-of-memory kill are one signal
    /// and three different things to tell a caller. Only the side that sent
    /// the signal knows which.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    /// The request's workspace, packed as a tar, when `?out=1` asked for it.
    ///
    /// Base64, because this crosses a JSON control socket and then a JSON HTTP
    /// answer. A tar is bytes and JSON has no way to carry bytes; the
    /// alternative is a second transport for the one route that needs one,
    /// which is more moving parts than the encoding costs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Who this request was for, and what it ran.
    ///
    /// Here rather than reconstructed by whoever wants to bill for it: the
    /// supervisor is the only side that knows all three at once, and a
    /// caller's own tenant and function are not facts it needs protecting
    /// from. See [`Usage`].
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tenant: String,
    /// The function or runtime pool the request ran in.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub function: String,
    /// The script's digest, for a pool request that named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// The request's own id, which is what `DELETE /requests/<id>` names.
    ///
    /// Returned with the answer as well as in a header, so a caller that kept
    /// the body has what it needs to cancel a *later* identical call — and so
    /// a log line about a slow request can be joined to the request itself.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
}

impl Outcome {
    pub fn succeeded(&self) -> bool {
        self.exit_code == 0 && self.error.is_none()
    }

    /// One word for how this request ended.
    ///
    /// The four kills exit 137 can mean are already separate fields; this is
    /// the single value something counting requests groups by, so that a
    /// dashboard does not have to re-derive the precedence every time.
    ///
    /// Order matters and is the caller's: `cancelled` first, because a caller
    /// who stopped their own request does not want to read "timeout".
    pub fn outcome(&self) -> &'static str {
        if self.cancelled {
            "cancelled"
        } else if self.stuck {
            "stuck"
        } else if self.timed_out {
            "timeout"
        } else if self.succeeded() {
            "ok"
        } else {
            "error"
        }
    }
}

/// What one finished request cost, and for whom.
///
/// Built from an [`Outcome`] by whoever is counting. It exists as a type
/// rather than a JSON literal in three places because an embedder bills from
/// it, and a field that means one thing in the OTLP export and another in the
/// webhook is a support question nobody can answer.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    pub tenant: String,
    pub function: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    pub request_id: String,
    pub wall_ms: f64,
    pub cpu_ms: f64,
    pub peak_rss_kb: u64,
    /// `ok`, `error`, `timeout`, `cancelled` or `stuck`. See
    /// [`Outcome::outcome`].
    pub outcome: String,
    /// When the request finished, as milliseconds since the epoch.
    ///
    /// Wall-clock rather than a monotonic reading, because it leaves this
    /// process and has to mean something to whoever receives it.
    pub finished_ms: u64,
}

impl From<&Outcome> for Usage {
    fn from(o: &Outcome) -> Usage {
        Usage {
            tenant: o.tenant.clone(),
            function: o.function.clone(),
            script: o.script.clone(),
            request_id: o.id.clone(),
            wall_ms: o.metrics.wall_ms,
            cpu_ms: o.metrics.cpu_ms,
            peak_rss_kb: o.metrics.peak_rss_kb,
            outcome: o.outcome().to_string(),
            finished_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One word for how a request ended, and the order it is decided in.
    ///
    /// The precedence is the caller's: somebody who cancelled their own
    /// request does not want to read "timeout" because the deadline also
    /// happened to pass while the kill landed.
    #[test]
    fn the_four_kills_are_told_apart_and_the_caller_comes_first() {
        let base = Outcome {
            tenant: "acme".into(),
            function: "render".into(),
            script: None,
            id: "00000001".into(),
            exit_code: 0,
            result: serde_json::Value::Null,
            stdout: String::new(),
            stderr: String::new(),
            error: None,
            metrics: Metrics::default(),
            timed_out: false,
            cancelled: false,
            stuck: false,
            workspace: None,
        };

        assert_eq!(base.outcome(), "ok");
        assert_eq!(
            Outcome {
                exit_code: 1,
                error: Some("boom".into()),
                ..base.clone()
            }
            .outcome(),
            "error"
        );
        assert_eq!(
            Outcome {
                exit_code: 137,
                timed_out: true,
                ..base.clone()
            }
            .outcome(),
            "timeout"
        );
        assert_eq!(
            Outcome {
                exit_code: 137,
                stuck: true,
                ..base.clone()
            }
            .outcome(),
            "stuck"
        );
        assert_eq!(
            Outcome {
                exit_code: 137,
                cancelled: true,
                ..base.clone()
            }
            .outcome(),
            "cancelled"
        );

        // A cancel that raced the deadline is still a cancel.
        assert_eq!(
            Outcome {
                exit_code: 137,
                cancelled: true,
                timed_out: true,
                stuck: true,
                ..base.clone()
            }
            .outcome(),
            "cancelled"
        );

        // And the event carries what somebody bills on.
        let usage = Usage::from(&base);
        assert_eq!(usage.tenant, "acme");
        assert_eq!(usage.function, "render");
        assert_eq!(usage.request_id, "00000001");
        assert_eq!(usage.outcome, "ok");
        assert!(usage.finished_ms > 1_700_000_000_000, "{usage:?}");
    }

    #[test]
    fn an_outcome_is_only_a_success_if_nothing_went_wrong() {
        let base = Outcome {
            id: "00000001".into(),
            cancelled: false,
            stuck: false,
            workspace: None,
            tenant: "default".into(),
            function: "resize".into(),
            script: None,
            exit_code: 0,
            result: serde_json::Value::Null,
            stdout: String::new(),
            stderr: String::new(),
            error: None,
            metrics: Metrics::default(),
            timed_out: false,
        };
        assert!(base.succeeded());
        assert!(
            !Outcome {
                exit_code: 1,
                ..base.clone()
            }
            .succeeded()
        );
        assert!(
            !Outcome {
                error: Some("ValueError".into()),
                ..base
            }
            .succeeded(),
            "a handler that raised did not succeed, whatever its exit code"
        );
    }
}
