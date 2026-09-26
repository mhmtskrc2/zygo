// SPDX-License-Identifier: Apache-2.0
//! Where a request's time went, and whether the quota set it.
//!
//! [`CallTiming`] splits one request into the phases worth telling apart;
//! [`CpuAccounting`] reads what the tenant's CPU quota was doing meanwhile,
//! which is the only way to know whether a tail was Zygo or the kernel
//! enforcing `cpu = 1.0` exactly as asked. Both are read by `zygo bench`
//! and `zygo stats`, and neither touches a sandbox.

use std::time::Instant;

/// Where one request's time went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallTiming {
    /// Registering as a caller and writing `EXEC`.
    ///
    /// This was "waiting for the connection" when a request held the wire for
    /// its whole round trip. It is now only the write, so a number here that is
    /// not near zero means the socket buffer is full.
    pub lock: std::time::Duration,
    /// `EXEC` sent to `FORKED` received — the agent's `fork()`.
    pub fork: std::time::Duration,
    /// Creating the request cgroup and moving the child into it.
    pub admit: std::time::Duration,
    /// `GO` sent to `DONE` received — the handler, and the child's teardown.
    pub run: std::time::Duration,
    /// Removing the request cgroup, after the answer is already in hand.
    pub release: std::time::Duration,
}

impl CallTiming {
    pub fn total(&self) -> std::time::Duration {
        self.lock + self.fork + self.admit + self.run + self.release
    }
}

/// What the tenant's CPU quota is doing, from cgroup v2 `cpu.stat`.
///
/// A warm call forks, so for a moment the tenant has two runnable tasks: the
/// agent and the child it is tearing down. A tenant at `cpu = 1.0` that is
/// asked for back-to-back requests therefore wants slightly more than its
/// quota, and CFS answers by stopping it until the next period. At the default
/// 100 ms period that is a tail of tens of milliseconds which says nothing
/// about how fast Zygo is — it is the quota being enforced, exactly as asked.
///
/// Measured on kernel 5.10 / aarch64: a 1-core tenant driven with no think
/// time was throttled in 27 of 28 periods and saw p99 49 ms; the same tenant at
/// half that offered load was throttled in none and saw p99 1.9 ms. So anything
/// that reports warm latency has to be able to say which of the two it
/// measured, and the supervisor's queue needs it to decide whom to admit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuAccounting {
    /// `usage_usec`: CPU the tenant has consumed.
    pub usage_us: u64,
    /// `nr_periods`: enforcement periods that have elapsed.
    pub periods: u64,
    /// `nr_throttled`: periods in which the quota ran out.
    pub throttled_periods: u64,
    /// `throttled_usec`: total time spent stopped at the quota.
    pub throttled_us: u64,
    /// The quota in cores, `None` when the tenant is uncapped.
    pub quota_cores: Option<f64>,
    /// The enforcement period. A throttled task waits out the rest of one, so
    /// this is the upper bound on the latency a quota can add.
    pub period: std::time::Duration,
}

impl CpuAccounting {
    /// Read `cpu.stat` and `cpu.max` from a cgroup directory.
    ///
    /// The quota is enforced by the nearest ancestor that sets one, so the
    /// counters are read from the tenant rather than from the leaf the
    /// processes happen to live in.
    pub fn read(dir: &std::path::Path) -> Option<CpuAccounting> {
        let stat = std::fs::read_to_string(dir.join("cpu.stat")).ok()?;
        let field = |name: &str| -> u64 {
            stat.lines()
                .find_map(|l| l.strip_prefix(name)?.trim().parse().ok())
                .unwrap_or(0)
        };
        let (quota_cores, period) = read_cpu_max(dir);
        Some(CpuAccounting {
            usage_us: field("usage_usec"),
            periods: field("nr_periods"),
            throttled_periods: field("nr_throttled"),
            throttled_us: field("throttled_usec"),
            quota_cores,
            period,
        })
    }

    /// What happened between two readings.
    pub fn since(&self, earlier: &CpuAccounting) -> CpuAccounting {
        CpuAccounting {
            usage_us: self.usage_us.saturating_sub(earlier.usage_us),
            periods: self.periods.saturating_sub(earlier.periods),
            throttled_periods: self
                .throttled_periods
                .saturating_sub(earlier.throttled_periods),
            throttled_us: self.throttled_us.saturating_sub(earlier.throttled_us),
            ..*self
        }
    }

    /// Whether the quota, rather than the runtime, set the latency.
    ///
    /// One throttled period in a long run is noise; a run that spends a
    /// meaningful share of its periods stopped at the quota is measuring the
    /// quota. The threshold is deliberately low: past 5% the tail is already
    /// dominated by the period, because every throttled request waits out most
    /// of one.
    pub fn saturated(&self) -> bool {
        self.periods > 0 && self.throttled_periods * 20 > self.periods
    }

    /// Share of the quota used, over a window of wall-clock time.
    pub fn demand_cores(&self, elapsed: std::time::Duration) -> f64 {
        if elapsed.is_zero() {
            return 0.0;
        }
        self.usage_us as f64 / elapsed.as_micros() as f64
    }
}

/// Parse `cpu.max`: `"<quota|max> <period>"`, quota in µs per period.
fn read_cpu_max(dir: &std::path::Path) -> (Option<f64>, std::time::Duration) {
    let default_period = std::time::Duration::from_micros(crate::spec::Cpu::PERIOD_US);
    let Ok(text) = std::fs::read_to_string(dir.join("cpu.max")) else {
        return (None, default_period);
    };
    let mut parts = text.split_whitespace();
    let quota = parts.next().unwrap_or("max");
    let period: u64 = parts
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(crate::spec::Cpu::PERIOD_US);
    let cores = quota
        .parse::<f64>()
        .ok()
        .filter(|_| quota != "max")
        .map(|q| q / period as f64);
    (cores, std::time::Duration::from_micros(period))
}

/// How long a `WarmFn` took to become ready, for `zygo ps`.
#[derive(Debug, Clone, Copy)]
pub struct WarmupTiming {
    pub started: Instant,
    pub ready_after: std::time::Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- CpuAccounting ---------------------------------------------------
    //
    // Parsed from files rather than mocked, because the format is the thing
    // being got right: `cpu.stat` gained fields between kernel releases and
    // `cpu.max` has two shapes.

    fn cgroup_with(stat: &str, max: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("cpu.stat"), stat).expect("cpu.stat");
        if let Some(m) = max {
            std::fs::write(dir.path().join("cpu.max"), m).expect("cpu.max");
        }
        dir
    }

    const THROTTLED: &str = "usage_usec 2411882\nuser_usec 1102113\nsystem_usec 1309769\n\
                             nr_periods 28\nnr_throttled 27\nthrottled_usec 2257080\n";

    #[test]
    fn cpu_stat_is_read_with_its_quota_and_period() {
        let dir = cgroup_with(THROTTLED, Some("100000 100000\n"));
        let cpu = CpuAccounting::read(dir.path()).expect("readable");
        assert_eq!(cpu.usage_us, 2_411_882);
        assert_eq!(cpu.periods, 28);
        assert_eq!(cpu.throttled_periods, 27);
        assert_eq!(cpu.throttled_us, 2_257_080);
        assert_eq!(cpu.quota_cores, Some(1.0));
        assert_eq!(cpu.period, std::time::Duration::from_millis(100));
    }

    #[test]
    fn an_uncapped_tenant_reports_no_quota() {
        let dir = cgroup_with(THROTTLED, Some("max 100000\n"));
        let cpu = CpuAccounting::read(dir.path()).expect("readable");
        assert_eq!(cpu.quota_cores, None, "`max` is not a number of cores");
    }

    #[test]
    fn a_fractional_quota_survives_the_round_trip() {
        let dir = cgroup_with(THROTTLED, Some("50000 100000\n"));
        let cpu = CpuAccounting::read(dir.path()).expect("readable");
        assert_eq!(cpu.quota_cores, Some(0.5));
    }

    #[test]
    fn a_shortened_period_is_reported_as_written() {
        // The period is the upper bound on the latency a quota can add, so a
        // reader that assumed 100 ms would misreport a cgroup someone retuned.
        let dir = cgroup_with(THROTTLED, Some("10000 10000\n"));
        let cpu = CpuAccounting::read(dir.path()).expect("readable");
        assert_eq!(cpu.quota_cores, Some(1.0));
        assert_eq!(cpu.period, std::time::Duration::from_millis(10));
    }

    #[test]
    fn a_missing_cpu_max_is_not_a_missing_cgroup() {
        // `cpu.max` is absent until the controller is enabled; the counters are
        // still worth having.
        let dir = cgroup_with(THROTTLED, None);
        let cpu = CpuAccounting::read(dir.path()).expect("readable without cpu.max");
        assert_eq!(cpu.periods, 28);
        assert_eq!(cpu.quota_cores, None);
    }

    #[test]
    fn nothing_to_read_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(CpuAccounting::read(dir.path()).is_none());
    }

    #[test]
    fn unknown_fields_are_zero_not_an_error() {
        // Older kernels omit the burst fields; newer ones may add more.
        let dir = cgroup_with("usage_usec 500\n", Some("100000 100000\n"));
        let cpu = CpuAccounting::read(dir.path()).expect("readable");
        assert_eq!(cpu.usage_us, 500);
        assert_eq!(cpu.throttled_periods, 0);
    }

    #[test]
    fn a_window_is_the_difference_between_two_readings() {
        let before = CpuAccounting {
            usage_us: 1_000,
            periods: 10,
            throttled_periods: 1,
            throttled_us: 50,
            quota_cores: Some(1.0),
            period: std::time::Duration::from_millis(100),
        };
        let after = CpuAccounting {
            usage_us: 3_000,
            periods: 40,
            throttled_periods: 31,
            throttled_us: 2_050,
            ..before
        };
        let window = after.since(&before);
        assert_eq!(window.usage_us, 2_000);
        assert_eq!(window.periods, 30);
        assert_eq!(window.throttled_periods, 30);
        assert_eq!(window.throttled_us, 2_000);
        assert_eq!(window.quota_cores, Some(1.0), "the quota is carried over");
    }

    #[test]
    fn a_counter_that_went_backwards_does_not_underflow() {
        // The tenant cgroup is recreated on a restart, so a later reading can
        // be smaller than an earlier one.
        let later = CpuAccounting {
            usage_us: 5,
            periods: 1,
            throttled_periods: 0,
            throttled_us: 0,
            quota_cores: None,
            period: std::time::Duration::from_millis(100),
        };
        let earlier = CpuAccounting {
            usage_us: 9_000,
            periods: 90,
            throttled_periods: 9,
            throttled_us: 400,
            ..later
        };
        let window = later.since(&earlier);
        assert_eq!(window.usage_us, 0);
        assert_eq!(window.periods, 0);
    }

    #[test]
    fn saturation_is_a_share_of_periods_not_a_single_one() {
        let base = CpuAccounting {
            usage_us: 0,
            periods: 0,
            throttled_periods: 0,
            throttled_us: 0,
            quota_cores: Some(1.0),
            period: std::time::Duration::from_millis(100),
        };
        let idle = CpuAccounting {
            periods: 100,
            ..base
        };
        assert!(!idle.saturated(), "no throttling is not saturation");

        let noise = CpuAccounting {
            periods: 100,
            throttled_periods: 3,
            ..base
        };
        assert!(!noise.saturated(), "3% of periods is noise");

        // The measured run: 27 of 28 periods.
        let measured = CpuAccounting {
            periods: 28,
            throttled_periods: 27,
            ..base
        };
        assert!(measured.saturated());

        assert!(
            !base.saturated(),
            "a window with no periods cannot be judged"
        );
    }

    #[test]
    fn demand_is_cpu_time_over_wall_time() {
        let cpu = CpuAccounting {
            usage_us: 2_000_000,
            periods: 40,
            throttled_periods: 0,
            throttled_us: 0,
            quota_cores: Some(2.0),
            period: std::time::Duration::from_millis(100),
        };
        // Two seconds of CPU in four seconds of wall clock is half a core.
        let demand = cpu.demand_cores(std::time::Duration::from_secs(4));
        assert!((demand - 0.5).abs() < 1e-9, "got {demand}");
        assert_eq!(cpu.demand_cores(std::time::Duration::ZERO), 0.0);
    }
}
