// SPDX-License-Identifier: Apache-2.0
//! What each tenant has used, and its delivery to `--usage-webhook`.
//!
//! One fact counted twice on purpose: a dashboard reads the totals and a
//! billing system reads the events, and they come from the same
//! [`Usage`] so the two can never disagree. Delivery is at-least-once
//! from a bounded queue — the serving path is never held up to protect
//! the billing path.

use std::sync::Arc;

use zygo_core::supervisor::Response as Reply;

use super::Api;

/// What each tenant has used, and what has not been delivered yet.
///
/// One place rather than two, because they are one fact counted twice: a
/// dashboard reads the totals and a billing system reads the events, and a
/// deployment that had them disagree would trust neither.
#[derive(Default)]
pub(super) struct Usage {
    totals: std::collections::BTreeMap<String, crate::cmd::otlp::TenantUsage>,
    /// Events waiting for the webhook, oldest first.
    queued: std::collections::VecDeque<zygo_core::pool::Usage>,
    /// Events dropped because the queue was full.
    ///
    /// Counted rather than silently lost: at-least-once delivery that quietly
    /// becomes at-most-once is worse than one that says so.
    dropped: u64,
}

/// How many undelivered usage events to keep.
///
/// A bound rather than a growing queue, because the alternative to dropping is
/// the API process growing without limit while a webhook is down — which takes
/// the *serving* path down with it, to protect the billing path. The wrong way
/// round: requests matter more than their receipts, and the receipts are also
/// in the supervisor's log.
const USAGE_QUEUE: usize = 10_000;

/// How many events go in one webhook delivery.
const USAGE_BATCH: usize = 256;

impl Usage {
    /// Count one finished request, and queue it for delivery.
    fn record(&mut self, usage: zygo_core::pool::Usage) {
        let totals = self.totals.entry(usage.tenant.clone()).or_default();
        totals.tenant = usage.tenant.clone();
        totals.requests += 1;
        if usage.outcome != "ok" {
            totals.failures += 1;
        }
        totals.cpu_ms += usage.cpu_ms;
        totals.wall_ms += usage.wall_ms;
        *totals.by_outcome.entry(usage.outcome.clone()).or_default() += 1;

        if self.queued.len() >= USAGE_QUEUE {
            // The oldest, not the newest: a billing system that has fallen
            // behind wants the most recent state it can get, and the events
            // it lost are the ones furthest from now.
            self.queued.pop_front();
            self.dropped += 1;
        }
        self.queued.push_back(usage);
    }

    pub(super) fn snapshot(&self) -> Vec<crate::cmd::otlp::TenantUsage> {
        self.totals.values().cloned().collect()
    }

    fn take_batch(&mut self) -> Vec<zygo_core::pool::Usage> {
        self.queued
            .drain(..USAGE_BATCH.min(self.queued.len()))
            .collect()
    }

    /// Put a failed batch back at the front, so it is retried in order.
    fn return_batch(&mut self, batch: Vec<zygo_core::pool::Usage>) {
        for usage in batch.into_iter().rev() {
            if self.queued.len() >= USAGE_QUEUE {
                self.dropped += 1;
                continue;
            }
            self.queued.push_front(usage);
        }
    }
}

/// Deliver queued usage events to the webhook, for ever.
///
/// At-least-once: a batch that fails goes back on the front of the queue in
/// order and is tried again. A receiver may therefore see an event twice and
/// should key on `request_id` — which is documented on the flag, because
/// silent duplicates in a billing feed are worse than loud ones.
pub(super) async fn deliver_usage(api: Arc<Api>, url: reqwest::Url, interval: std::time::Duration) {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(client) => client,
        Err(e) => {
            tracing::error!("usage: cannot build an HTTP client: {e}");
            return;
        }
    };

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut failing = false;
    loop {
        ticker.tick().await;
        loop {
            let batch = api.usage.lock().expect("usage").take_batch();
            if batch.is_empty() {
                break;
            }
            let body = serde_json::json!({ "events": batch });
            match client.post(url.clone()).json(&body).send().await {
                Ok(response) if response.status().is_success() => {
                    if failing {
                        tracing::info!("usage: the webhook is answering again");
                        failing = false;
                    }
                }
                outcome => {
                    // Back on the front, in order: a billing feed that
                    // reordered under failure would be one nobody could
                    // reconcile.
                    api.usage.lock().expect("usage").return_batch(batch);
                    if !failing {
                        failing = true;
                        // Once per outage, not once per attempt: a webhook
                        // that is down for an hour must not be an hour of
                        // log lines.
                        match outcome {
                            Ok(r) => tracing::warn!("usage: the webhook answered {}", r.status()),
                            Err(e) => tracing::warn!("usage: the webhook is unreachable: {e}"),
                        }
                    }
                    break;
                }
            }
        }

        let dropped = {
            let mut usage = api.usage.lock().expect("usage");
            std::mem::take(&mut usage.dropped)
        };
        if dropped > 0 {
            tracing::warn!(
                "usage: dropped {dropped} events; the queue holds {USAGE_QUEUE} and the \
                 webhook is behind"
            );
        }
    }
}

/// Map a control reply to an HTTP status and JSON body.
/// Count a finished request, if that is what this reply is.
///
/// Called at the four places something is *run* — a function, a pool, a batch
/// element, a stream — rather than inside `reply_to_response`, which every
/// route uses and most of them do not run anything. An explicit call at four
/// sites is easier to check than an implicit one at thirty.
pub(super) fn count_usage(api: &Api, reply: &Reply) {
    if let Reply::Executed { outcome } = reply {
        api.usage
            .lock()
            .expect("usage")
            .record(zygo_core::pool::Usage::from(outcome.as_ref()));
    }
}
