//! Per-tenant admission control: `concurrency`, a bounded queue, and the `429`.
//!
//! Requirement N4 makes limits mandatory, and a limit nobody is told about is a
//! hang. So a tenant at its ceiling gets one of two answers and never a third:
//! wait briefly in a bounded queue, or be turned away immediately with enough
//! numbers to decide what to do next (design doc §7.5: the platform queues or
//! routes to another machine).
//!
//! Unbounded queueing is the failure this exists to prevent. A queue with no
//! limit converts "too much load" into "every request times out and the
//! supervisor's memory grows", which is strictly worse than a fast rejection
//! for both the caller and the host.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Why a request was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    /// The queue was already full. Nothing was waited for.
    Busy {
        in_flight: u32,
        queued: u32,
        limit: u32,
    },
    /// Waited in the queue and the deadline passed first.
    TimedOut {
        in_flight: u32,
        queued: u32,
        limit: u32,
    },
    /// The function is shutting down.
    Closed,
}

impl Rejected {
    /// The numbers to report, for the two variants that have them.
    pub fn load(&self) -> Option<(u32, u32, u32)> {
        match *self {
            Rejected::Busy {
                in_flight,
                queued,
                limit,
            }
            | Rejected::TimedOut {
                in_flight,
                queued,
                limit,
            } => Some((in_flight, queued, limit)),
            Rejected::Closed => None,
        }
    }
}

#[derive(Debug, Default)]
struct State {
    in_flight: u32,
    queued: u32,
    closed: bool,
}

/// Admission control for one function.
#[derive(Debug)]
pub struct Gate {
    /// How many requests may be in flight at once — the spec's `concurrency`.
    limit: u32,
    /// How many may wait for a slot before the rest are turned away.
    queue_limit: u32,
    state: Mutex<State>,
    slot_freed: Condvar,
}

impl Gate {
    /// `limit` is clamped to at least one: a function that can serve nothing is
    /// a configuration that has no useful meaning, and silently accepting it
    /// would deadlock every caller instead of failing at resolve time.
    pub fn new(limit: u32, queue_limit: u32) -> Gate {
        Gate {
            limit: limit.max(1),
            queue_limit,
            state: Mutex::new(State::default()),
            slot_freed: Condvar::new(),
        }
    }

    /// The queue depth a `concurrency` implies when nobody chose one.
    ///
    /// Four deep per slot: enough that a burst rides over a slow request,
    /// short enough that a caller waiting at the back is told to go away well
    /// before any sensible client timeout.
    pub fn default_queue_limit(concurrency: u32) -> u32 {
        concurrency.max(1).saturating_mul(4)
    }

    pub fn limit(&self) -> u32 {
        self.limit
    }

    /// Take a slot, waiting up to `wait` for one.
    pub fn enter(&self, wait: Duration) -> Result<Permit<'_>, Rejected> {
        let mut state = self.state.lock().expect("gate");
        if state.closed {
            return Err(Rejected::Closed);
        }
        if state.in_flight < self.limit {
            state.in_flight += 1;
            return Ok(Permit { gate: self });
        }
        if state.queued >= self.queue_limit {
            return Err(Rejected::Busy {
                in_flight: state.in_flight,
                queued: state.queued,
                limit: self.limit,
            });
        }

        state.queued += 1;
        let (mut state, timeout) = self
            .slot_freed
            .wait_timeout_while(state, wait, |s| !s.closed && s.in_flight >= self.limit)
            .expect("gate");
        // Decremented here rather than on each exit path below, so no early
        // return can leak a queue slot and shrink the queue for good.
        state.queued -= 1;

        if state.closed {
            return Err(Rejected::Closed);
        }
        if timeout.timed_out() {
            return Err(Rejected::TimedOut {
                in_flight: state.in_flight,
                queued: state.queued,
                limit: self.limit,
            });
        }
        state.in_flight += 1;
        Ok(Permit { gate: self })
    }

    /// In-flight and queued right now, for `zygo ps`.
    pub fn load(&self) -> (u32, u32) {
        let state = self.state.lock().expect("gate");
        (state.in_flight, state.queued)
    }

    /// Turn away everything waiting and everything that arrives later.
    ///
    /// Callers already holding a permit keep it: a function being stopped should
    /// finish what it accepted rather than abandon a half-served request.
    pub fn close(&self) {
        let mut state = self.state.lock().expect("gate");
        state.closed = true;
        self.slot_freed.notify_all();
    }

    fn release(&self) {
        let mut state = self.state.lock().expect("gate");
        state.in_flight = state.in_flight.saturating_sub(1);
        // One waiter, because one slot came free.
        self.slot_freed.notify_one();
    }
}

/// A held slot. Releasing it on drop is what makes a panicking request handler
/// give its slot back instead of shrinking the function's capacity by one.
#[derive(Debug)]
pub struct Permit<'a> {
    gate: &'a Gate,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.gate.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    const NOW: Duration = Duration::ZERO;

    #[test]
    fn requests_up_to_the_limit_are_admitted_immediately() {
        let gate = Gate::new(3, 0);
        let a = gate.enter(NOW).expect("first");
        let b = gate.enter(NOW).expect("second");
        let c = gate.enter(NOW).expect("third");
        assert_eq!(gate.load(), (3, 0));

        assert!(matches!(gate.enter(NOW), Err(Rejected::Busy { .. })));
        drop(c);
        assert_eq!(gate.load(), (2, 0));
        let _d = gate.enter(NOW).expect("a slot came free");
        drop((a, b));
    }

    #[test]
    fn a_full_queue_is_told_so_with_the_numbers_to_act_on() {
        let gate = Gate::new(1, 2);
        let _held = gate.enter(NOW).expect("the one slot");
        // Nobody is waiting yet, so a zero-length wait fills and drains the
        // queue slot in one go: it times out rather than being turned away.
        assert!(matches!(gate.enter(NOW), Err(Rejected::TimedOut { .. })));

        let err = Gate::new(1, 0);
        let _one = err.enter(NOW).expect("the one slot");
        match err.enter(NOW) {
            Err(Rejected::Busy {
                in_flight,
                queued,
                limit,
            }) => {
                assert_eq!((in_flight, queued, limit), (1, 0, 1));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_permit_is_returned_even_when_the_holder_panics() {
        let gate = Arc::new(Gate::new(1, 0));
        let g = Arc::clone(&gate);
        let joined = std::thread::spawn(move || {
            let _permit = g.enter(NOW).expect("slot");
            panic!("the handler blew up");
        })
        .join();
        assert!(joined.is_err(), "the thread should have panicked");
        assert_eq!(gate.load(), (0, 0), "the slot came back");
        gate.enter(NOW).expect("the function still serves requests");
    }

    #[test]
    fn a_waiter_is_woken_when_a_slot_frees() {
        let gate = Arc::new(Gate::new(1, 4));
        let held = gate.enter(NOW).expect("slot");

        let g = Arc::clone(&gate);
        let waiter = std::thread::spawn(move || g.enter(Duration::from_secs(5)).is_ok());

        // Wait for the other thread to actually be in the queue before
        // releasing, so the test proves a wake-up rather than a lucky race.
        while gate.load().1 == 0 {
            std::thread::yield_now();
        }
        drop(held);
        assert!(waiter.join().expect("join"), "the waiter got the slot");
        assert_eq!(gate.load(), (0, 0));
    }

    #[test]
    fn the_queue_slot_is_given_back_after_a_timeout() {
        // A timeout that leaked its queue slot would shrink the queue on every
        // slow request until the function rejected everything.
        let gate = Gate::new(1, 1);
        let _held = gate.enter(NOW).expect("slot");
        for _ in 0..5 {
            assert!(matches!(
                gate.enter(Duration::from_millis(1)),
                Err(Rejected::TimedOut { .. })
            ));
            assert_eq!(gate.load().1, 0, "the queue drained");
        }
    }

    #[test]
    fn closing_turns_away_waiters_and_newcomers() {
        let gate = Arc::new(Gate::new(1, 4));
        let held = gate.enter(NOW).expect("slot");

        // The permit borrows the gate, so the thread reports why it failed
        // rather than handing a permit back across the join.
        let g = Arc::clone(&gate);
        let waiter = std::thread::spawn(move || g.enter(Duration::from_secs(5)).err());
        while gate.load().1 == 0 {
            std::thread::yield_now();
        }

        gate.close();
        assert_eq!(waiter.join().expect("join"), Some(Rejected::Closed));
        assert!(matches!(gate.enter(NOW), Err(Rejected::Closed)));

        // The holder keeps its permit: a stop must not abandon a request that
        // was already accepted.
        assert_eq!(gate.load().0, 1);
        drop(held);
        assert_eq!(gate.load(), (0, 0));
    }

    #[test]
    fn concurrency_is_never_exceeded_under_contention() {
        // The property that matters: whatever the interleaving, the number of
        // simultaneous holders never passes the limit.
        const LIMIT: u32 = 4;
        let gate = Arc::new(Gate::new(LIMIT, 64));
        let now = Arc::new(AtomicU32::new(0));
        let peak = Arc::new(AtomicU32::new(0));

        let threads: Vec<_> = (0..32)
            .map(|_| {
                let (gate, now, peak) = (Arc::clone(&gate), Arc::clone(&now), Arc::clone(&peak));
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let Ok(permit) = gate.enter(Duration::from_secs(5)) else {
                            continue;
                        };
                        let held = now.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(held, Ordering::SeqCst);
                        std::thread::yield_now();
                        now.fetch_sub(1, Ordering::SeqCst);
                        drop(permit);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().expect("join");
        }

        let peak = peak.load(Ordering::SeqCst);
        assert!(peak <= LIMIT, "{peak} requests were in flight at once");
        assert!(peak > 1, "the test never actually contended (peak {peak})");
        assert_eq!(gate.load(), (0, 0), "every permit was returned");
    }

    #[test]
    fn a_concurrency_of_zero_still_serves_requests() {
        // Resolution should never produce this, but a gate that admits nobody
        // would hang every caller rather than fail loudly.
        let gate = Gate::new(0, 0);
        assert_eq!(gate.limit(), 1);
        let _permit = gate.enter(NOW).expect("one request at a time");
    }

    #[test]
    fn the_default_queue_is_four_deep_per_slot() {
        assert_eq!(Gate::default_queue_limit(1), 4);
        assert_eq!(Gate::default_queue_limit(4), 16);
        assert_eq!(Gate::default_queue_limit(0), 4, "clamped like the limit");
        assert_eq!(
            Gate::default_queue_limit(u32::MAX),
            u32::MAX,
            "no overflow panic on an absurd concurrency"
        );
    }

    #[test]
    fn rejection_reports_the_load_except_when_closed() {
        assert_eq!(
            Rejected::Busy {
                in_flight: 4,
                queued: 16,
                limit: 4
            }
            .load(),
            Some((4, 16, 4))
        );
        assert_eq!(Rejected::Closed.load(), None);
    }
}
