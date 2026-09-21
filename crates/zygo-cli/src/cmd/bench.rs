//! `zygo bench` — measure the warm path against the real pool.
//!
//! PoC 3 measured p50 1887 µs against a 2000 µs budget: **6% of headroom**. The
//! supervisor's queue, timers and metrics all land on this path, so
//! a number that is only ever measured by hand will be spent without anyone
//! noticing. This runs the same measurement against `zygo_core::pool`, which is
//! the code that ships.

use std::time::{Duration, Instant};

use anyhow::Context;
use zygo_core::pool::{CpuAccounting, Pool, PoolConfig};
use zygo_core::spec::{Layer, ResolveOptions, Spec};

use crate::cli::{BenchCommand, Cli};
use crate::output::{self, Style};

/// The budgets from the design document, §5.
const WARM_P50_BUDGET_US: f64 = 2_000.0;
const WARM_P99_BUDGET_US: f64 = 10_000.0;

/// The acceptance criterion for warm-exec: "a Go binary under 3 ms".
///
/// A different budget because it is a different thing: an agent request is a
/// `fork()` of a warm interpreter, a warm-exec request is a fresh process
/// entered into the sandbox and `execve`d — six `setns` calls, the full
/// hardening sequence and the program's own start-up, every time. Measured at
/// p50 2.2 ms for `sh -c cat`; with `python3` per request it is 54 ms, which is
/// exactly why interpreters get an agent instead.
const EXEC_P50_BUDGET_US: f64 = 3_000.0;

pub fn run(cli: &Cli, command: &BenchCommand) -> anyhow::Result<u8> {
    match command {
        BenchCommand::Warm {
            n,
            no_cgroup,
            rate,
            cpu,
            cmd,
        } => warm(
            cli,
            *n,
            !no_cgroup,
            *rate,
            *cpu,
            (!cmd.is_empty()).then_some(cmd.as_slice()),
        ),
        BenchCommand::Cold { n, image, command } => cold(cli, *n, image, command.as_deref()),
        BenchCommand::Load {
            seconds,
            concurrency,
            cpu,
        } => load(cli, *seconds, *concurrency, *cpu),
    }
}

fn warm(
    cli: &Cli,
    n: u32,
    per_request_cgroup: bool,
    rate: Option<f64>,
    cpu: Option<f64>,
    cmd: Option<&[String]>,
) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    let pool = Pool::new(PoolConfig {
        per_request_cgroup,
        ..PoolConfig::new(paths.clone())
    })?;

    // An empty handler, so what is measured is overhead and nothing else —
    // or, for warm-exec, the smallest program that completes the contract.
    let dir = tempfile::tempdir()?;
    let handler = dir.path().join("handler.py");
    std::fs::write(&handler, "def handler(event):\n    return None\n")?;

    let spec = Spec::default();
    let resolved = spec.resolve(
        None,
        &Layer {
            entry: cmd.is_none().then(|| handler.clone()),
            cmd: cmd.map(<[String]>::to_vec),
            image: cmd.is_some().then(|| "python:3.12-slim".to_string()),
            cpu: cpu.map(zygo_core::spec::Cpu),
            ..Default::default()
        },
        &ResolveOptions {
            one_shot: true,
            ..Default::default()
        },
    )?;

    let style = Style::stdout();
    eprintln!(
        "{} {}  {}{}",
        style.dim("image"),
        resolved.image,
        style.dim(if per_request_cgroup {
            "per-request cgroup"
        } else {
            "no per-request cgroup"
        }),
        match cmd {
            Some(c) => format!("  {}", style.dim(&format!("warm-exec: {}", c.join(" ")))),
            None => String::new(),
        }
    );

    let warmup_started = Instant::now();
    let function = pool
        .serve(&resolved)
        .with_context(|| format!("could not warm `{}`", resolved.image))?;
    let warmup = warmup_started.elapsed();

    let status = function.status();
    eprintln!(
        "{} {} in {:.0} ms (imports {:.1} ms, rss {} MB)",
        style.dim("warm"),
        status.runtime,
        warmup.as_secs_f64() * 1000.0,
        status.imports_ms,
        status.rss_kb / 1024
    );

    // A short warm-up run first: the first few requests pay for page faults in
    // the interpreter that every later one inherits.
    let settle = (n / 20).clamp(50, 500);
    for _ in 0..settle {
        function.call(serde_json::Value::Null)?;
    }

    let mut samples = Vec::with_capacity(n as usize);
    let mut phases: Vec<zygo_core::pool::CallTiming> = Vec::with_capacity(n as usize);
    let mut handler_us: Vec<f64> = Vec::with_capacity(n as usize);

    // The tenant's CPU quota, before and after. A run with no think time asks
    // for slightly more than one core — the agent and the child it is tearing
    // down overlap — so a `cpu = 1.0` tenant meets its own quota and CFS stops
    // it until the next period. Without these counters that tail reads as a
    // defect in the runtime; with them it reads as the limit working.
    let cpu_before = function.cpu_accounting();

    let interval = request_interval(rate);
    let started = Instant::now();
    for i in 0..n {
        // Paced from the start of the run rather than from the last request, so
        // a slow request is absorbed instead of shifting every one after it.
        if let Some(interval) = interval {
            let due = interval.mul_f64(f64::from(i));
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        let t0 = Instant::now();
        let (outcome, timing) =
            function.call_timed(serde_json::Value::Null, Duration::from_secs(30))?;
        samples.push(t0.elapsed().as_secs_f64() * 1e6);
        phases.push(timing);
        // What the child says it spent inside the handler. The difference
        // between this and the `run` phase is plumbing: the pipe, the child's
        // start-up and its exit.
        handler_us.push(outcome.metrics.wall_ms * 1000.0);

        anyhow::ensure!(
            outcome.succeeded(),
            "request {i} failed: {}",
            outcome.error.unwrap_or_else(|| "no error given".into())
        );
        if n >= 1000 && i > 0 && i % (n / 10) == 0 {
            eprint!("\r  {}%", i * 100 / n);
        }
    }
    let elapsed = started.elapsed();
    let quota = match (cpu_before, function.cpu_accounting()) {
        (Some(before), Some(after)) => Some(after.since(&before)),
        _ => None,
    };
    if n >= 1000 {
        eprintln!("\r      ");
    }

    // What the machine itself costs for a fork and a teardown, with no sandbox
    // and no Zygo in the way. A p99 that merely tracks this floor is a property
    // of the host, not of the code — and on a busy or nested-virtualised
    // machine the floor can be most of the budget.
    let floor = measure_fork_floor(500);

    let mut report = Report::of(&samples, elapsed, n, quota);
    if cmd.is_some() {
        report.p50_budget = EXEC_P50_BUDGET_US;
        report.label = "warm-exec request overhead";
    }
    if cli.json {
        output::json(&report.to_json())?;
    } else {
        report.print(&style);
        print_phases(&phases);
        if cmd.is_none() {
            print_handler_share(&phases, &handler_us);
        }
        print_floor(&style, &floor, &report);
        // Last: it is a conclusion about the breakdown, so it reads after it
        // rather than in the middle of it.
        print_cgroup_note(&phases, &report, &style);
    }

    let _ = function.shutdown();
    Ok(u8::from(!report.within_budget()))
}

/// Requirement N2: a cold `run` with the image already in the store.
const COLD_BUDGET_MS: f64 = 50.0;

/// The acceptance criterion for throughput.
const LOAD_TARGET_PER_SECOND: f64 = 600.0;

/// `zygo bench cold` — build a sandbox, run a program, tear it down.
///
/// This is requirement N2, and the default command is the one N2 is written
/// about: starting a Python interpreter and exiting. The image must already be
/// in the store, because N2 is explicitly about the cached case — pulling is a
/// network measurement and belongs nowhere near this number.
fn cold(cli: &Cli, n: u32, image: &str, command: Option<&[String]>) -> anyhow::Result<u8> {
    use zygo_core::image::{Reference, Store};
    use zygo_core::sandbox::SandboxConfig;

    let paths = super::paths(cli);
    paths.ensure()?;
    let store = Store::new(paths.clone());
    let reference: Reference = image.parse()?;
    let entry = store.get(&reference).with_context(|| {
        format!("`{image}` is not in the store\n  → pull it first: zygo pull {image}")
    })?;

    let argv: Vec<String> = match command {
        Some(cmd) => cmd.to_vec(),
        None => ["python3", "-c", "pass"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
    };

    let spec = Spec::default();
    let resolved = spec.resolve(
        None,
        &Layer {
            image: Some(image.to_string()),
            cmd: Some(argv.clone()),
            ..Default::default()
        },
        &ResolveOptions {
            one_shot: true,
            ..Default::default()
        },
    )?;

    let overlay = zygo_core::doctor::run(&paths)
        .checks
        .iter()
        .any(|c| c.name == "overlayfs (userns)" && c.status == zygo_core::doctor::Status::Ok);
    let mount_points = zygo_core::sandbox::mount::required_mount_points(&resolved.mounts);
    let view = store.rootfs_view(&entry.layers, overlay, &mount_points)?;
    let backend = zygo_core::backend::for_isolation(resolved.isolation, &paths)?;

    let style = Style::stdout();
    eprintln!(
        "{} {image} {}  {}",
        style.dim("cold"),
        argv.join(" "),
        style.dim(if overlay { "overlayfs" } else { "flattened" })
    );

    let mut samples = Vec::with_capacity(n as usize);
    for i in 0..n {
        // A fresh root per iteration: reusing one would measure a warm page
        // cache for the mount points rather than what a real `run` does.
        let newroot = paths
            .tmp()
            .join(format!("bench-cold-{}-{i}", std::process::id()));
        std::fs::create_dir_all(&newroot)?;
        let config = SandboxConfig::from_resolved(&resolved, &view, &newroot, argv.clone(), &[]);

        let t0 = Instant::now();
        let mut sandbox = backend
            .start(&config)
            .with_context(|| format!("iteration {i} could not start"))?;
        let code = sandbox.wait()?;
        samples.push(t0.elapsed().as_secs_f64() * 1000.0);

        anyhow::ensure!(code == 0, "iteration {i} exited {code}");
        let _ = std::fs::remove_dir_all(&newroot);
    }

    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    let q = |p: f64| percentile(&samples, p);
    let p50 = q(50.0);

    if cli.json {
        output::json(&serde_json::json!({
            "image": image,
            "command": argv,
            "runs": n,
            "p50_ms": p50,
            "p90_ms": q(90.0),
            "p99_ms": q(99.0),
            "max_ms": samples.last().copied().unwrap_or(0.0),
            "budget_ms": COLD_BUDGET_MS,
            "pass": p50 < COLD_BUDGET_MS,
            "rootfs": if overlay { "overlayfs" } else { "flattened" },
        }))?;
    } else {
        println!("cold start over {n} runs, image already in the store");
        println!(
            "  p50 {:>7.1} ms   p90 {:>7.1}   p99 {:>7.1}   max {:>7.1}",
            p50,
            q(90.0),
            q(99.0),
            samples.last().copied().unwrap_or(0.0)
        );
        println!();
        let ok = p50 < COLD_BUDGET_MS;
        println!(
            "  p50 < {COLD_BUDGET_MS:.0} ms   {}",
            if ok {
                style.green("PASS")
            } else {
                style.red("FAIL")
            }
        );
        if !overlay {
            // Worth saying: a flattened rootfs is a different measurement, and
            // on this host it is the only one available.
            println!(
                "{}",
                style.dim(
                    "  note: this host has no unprivileged overlayfs, so the rootfs is\n  \
                     flattened — the same bind mount every run, which is the cheap case"
                )
            );
        }
    }
    let within_budget = p50 < COLD_BUDGET_MS;
    Ok(u8::from(!within_budget))
}

/// `zygo bench load` — sustained throughput through one warm function.
///
/// The acceptance criterion is ≥ 600 requests/s at a
/// concurrency of 4. Measuring it needs concurrent callers, and the interesting
/// number is not just the total: [`CallTiming::lock`] says how much of each
/// request was spent waiting for the connection, which is the difference
/// between "the machine is busy" and "the callers are queueing behind each
/// other".
fn load(cli: &Cli, seconds: u32, concurrency: u32, cpu: Option<f64>) -> anyhow::Result<u8> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    anyhow::ensure!(concurrency >= 1, "concurrency must be at least 1");

    let paths = super::paths(cli);
    let pool = Pool::new(PoolConfig::new(paths.clone()))?;

    let dir = tempfile::tempdir()?;
    let handler = dir.path().join("handler.py");
    std::fs::write(&handler, "def handler(event):\n    return None\n")?;

    let spec = Spec::default();
    let resolved = spec.resolve(
        None,
        &Layer {
            entry: Some(handler.clone()),
            cpu: cpu.map(zygo_core::spec::Cpu),
            concurrency: Some(concurrency),
            ..Default::default()
        },
        &ResolveOptions {
            one_shot: true,
            ..Default::default()
        },
    )?;

    let style = Style::stdout();
    let function = Arc::new(
        pool.serve(&resolved)
            .with_context(|| format!("could not warm `{}`", resolved.image))?,
    );
    eprintln!(
        "{} {} clients for {seconds}s",
        style.dim("load"),
        concurrency
    );

    for _ in 0..50 {
        function.call(serde_json::Value::Null)?;
    }

    let stop = Arc::new(AtomicBool::new(false));
    let cpu_before = function.cpu_accounting();
    let started = Instant::now();

    let workers: Vec<_> = (0..concurrency)
        .map(|_| {
            let (function, stop) = (Arc::clone(&function), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut samples = Vec::new();
                let mut lock_us = Vec::new();
                let mut failures = 0u64;
                let mut served = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    served += 1;
                    let t0 = Instant::now();
                    match function.call_timed(serde_json::Value::Null, Duration::from_secs(30)) {
                        Ok((outcome, timing)) => {
                            samples.push(t0.elapsed().as_secs_f64() * 1e6);
                            lock_us.push(timing.lock.as_secs_f64() * 1e6);
                            if !outcome.succeeded() {
                                failures += 1;
                            }
                        }
                        Err(_) => failures += 1,
                    }
                }
                (samples, lock_us, failures, served)
            })
        })
        .collect();

    std::thread::sleep(Duration::from_secs(u64::from(seconds)));
    stop.store(true, Ordering::Relaxed);

    let mut samples = Vec::new();
    let mut lock_us = Vec::new();
    let mut failures = 0u64;
    // Per worker, because a total hides starvation completely: one client
    // taking every slot and three taking none looks exactly like four clients
    // sharing fairly.
    let mut per_worker = Vec::new();
    for w in workers {
        let (s, l, f, served) = w.join().map_err(|_| anyhow::anyhow!("a worker panicked"))?;
        samples.extend(s);
        lock_us.extend(l);
        failures += f;
        per_worker.push(served);
    }
    per_worker.sort_unstable();
    let elapsed = started.elapsed();
    let quota = match (cpu_before, function.cpu_accounting()) {
        (Some(before), Some(after)) => Some(after.since(&before)),
        _ => None,
    };

    let count = samples.len() as u32;
    let report = Report::of(&samples, elapsed, count, quota);
    lock_us.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));

    let met = report.per_second >= LOAD_TARGET_PER_SECOND;
    if cli.json {
        let mut json = report.to_json();
        json["concurrency"] = concurrency.into();
        json["failures"] = failures.into();
        json["target_per_second"] = LOAD_TARGET_PER_SECOND.into();
        json["meets_target"] = met.into();
        json["lock_p50_us"] = percentile(&lock_us, 50.0).into();
        json["lock_max_us"] = lock_us.last().copied().unwrap_or(0.0).into();
        json["requests_per_client"] = per_worker.clone().into();
        output::json(&json)?;
    } else {
        println!(
            "sustained load: {concurrency} clients, {:.1}s, empty handler",
            elapsed.as_secs_f64()
        );
        println!(
            "  {:.0} requests/s   {count} requests   {failures} failed",
            report.per_second
        );
        println!(
            "  p50 {:>7.0} µs   p90 {:>7.0}   p99 {:>7.0}   max {:>8.0}",
            report.p50, report.p90, report.p99, report.max
        );
        println!();
        println!(
            "  >= {LOAD_TARGET_PER_SECOND:.0} requests/s   {}",
            if met {
                style.green("PASS")
            } else {
                style.red("FAIL")
            }
        );
        print_contention(&style, &lock_us, &report, concurrency);
        print_fairness(&style, &per_worker);
        report.print_quota(&style);
    }

    let _ = function.shutdown();
    Ok(u8::from(!met))
}

/// Show how the work was split between clients.
///
/// A throughput total says nothing about fairness, and fairness is where an
/// unfair lock shows up: one client taking nearly every slot while the others
/// wait reads as healthy throughput right up until you look at a percentile of
/// *their* latency.
fn print_fairness(style: &Style, per_worker: &[u64]) {
    if per_worker.len() < 2 {
        return;
    }
    let (min, max) = (per_worker[0], per_worker[per_worker.len() - 1]);
    let total: u64 = per_worker.iter().sum();
    let fair = total / per_worker.len() as u64;
    println!();
    println!("  requests per client: min {min}, max {max}, even would be {fair}");
    if min * 4 < max {
        println!(
            "{}",
            style.yellow(
                "  The work is not being shared. A warm function serialises on one
                   connection, and the lock guarding it is not fair, so a client that
                   releases and immediately re-acquires can starve the others for seconds
                   at a time. Until the agent can hold several forks at once, concurrency
                   above 1 buys nothing and costs fairness."
            )
        );
    }
}

/// Say how much of each request was spent waiting for the connection.
///
/// This is the number that says *why* a concurrency target is or is not met. A
/// warm function serialises on its own connection, because the wire protocol is
/// one request in flight per agent; adding callers therefore adds queueing
/// rather than throughput until the agent itself can hold several forks at once.
fn print_contention(style: &Style, lock_us: &[f64], report: &Report, concurrency: u32) {
    if lock_us.is_empty() {
        return;
    }
    let lock_p50 = percentile(lock_us, 50.0);
    let share = lock_p50 / report.p50.max(1.0) * 100.0;
    println!();
    println!(
        "  waiting for the connection: p50 {lock_p50:>6.0} µs   p99 {:>7.0}            max {:>9.0}   ({share:.0}% of p50)",
        percentile(lock_us, 99.0),
        lock_us.last().copied().unwrap_or(0.0)
    );

    // The tail, not the median, is where this hurts. Measured at concurrency 4:
    // the requests-per-client split was a reasonable 684 to 1166, while one
    // client still waited 2.17 s for the connection — so a percentile of the
    // pooled samples reports zero contention right up to the maximum.
    let lock_max = lock_us.last().copied().unwrap_or(0.0);
    if concurrency > 1 && lock_max > report.p50 * 20.0 {
        println!(
            "{}",
            style.yellow(
                "  One client waited far longer for the connection than a request takes.\n  \
                 A warm function serves one request at a time — the agent handles one\n  \
                 `EXEC` to completion before reading the next — and the lock guarding\n  \
                 that connection is not fair, so waiting is unbounded and badly skewed.\n  \
                 Until the agent can hold several forks at once, concurrency above 1\n  \
                 buys no throughput and costs a long latency tail."
            )
        );
    } else if concurrency > 1 && share > 30.0 {
        println!(
            "{}",
            style.yellow(
                "  Most of each request is spent queueing behind another one: the agent\n  \
                 handles one `EXEC` to completion before reading the next, so\n  \
                 `concurrency` bounds what the supervisor admits, not what the agent\n  \
                 can overlap."
            )
        );
    }
}

/// The gap between request starts for an offered load in requests/s.
///
/// `None` means unpaced, which drives the tenant to its own CPU quota — see
/// [`Report::saturated`].
fn request_interval(rate: Option<f64>) -> Option<Duration> {
    let rate = rate?;
    if !rate.is_finite() || rate <= 0.0 {
        return None;
    }
    Some(Duration::from_secs_f64(1.0 / rate))
}

struct Report {
    p50: f64,
    p90: f64,
    p99: f64,
    p999: f64,
    max: f64,
    mean: f64,
    per_second: f64,
    count: u32,
    elapsed: Duration,
    /// What the tenant's CPU quota did during the run, when it could be read.
    quota: Option<CpuAccounting>,
    /// The p50 budget this run is judged against: the agent's or warm-exec's.
    p50_budget: f64,
    label: &'static str,
}

impl Report {
    fn of(samples: &[f64], elapsed: Duration, count: u32, quota: Option<CpuAccounting>) -> Report {
        let mut sorted = samples.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in a duration"));

        Report {
            p50: percentile(&sorted, 50.0),
            p90: percentile(&sorted, 90.0),
            p99: percentile(&sorted, 99.0),
            p999: percentile(&sorted, 99.9),
            max: sorted.last().copied().unwrap_or(0.0),
            mean: sorted.iter().sum::<f64>() / sorted.len().max(1) as f64,
            per_second: count as f64 / elapsed.as_secs_f64(),
            count,
            elapsed,
            quota,
            p50_budget: WARM_P50_BUDGET_US,
            label: "warm request overhead",
        }
    }

    /// Whether the tenant spent the run stopped at its own CPU quota.
    ///
    /// When it did, the tail is the quota's enforcement period, not Zygo's
    /// cost, and the p99 measured here is not a number about this code.
    fn saturated(&self) -> bool {
        self.quota.is_some_and(|q| q.saturated())
    }

    /// Whether the p99 budget can be judged at all.
    fn p99_is_meaningful(&self) -> bool {
        !self.saturated()
    }

    /// `false` only for a budget that was both judged and missed.
    ///
    /// A saturated run is not a failure of this code, and reporting it as one
    /// would send anyone reading CI after the wrong thing — the same mistake
    /// the phase-0 report records twice under test methodology. It is not a
    /// pass either: the p99 simply was not measured, which the output says.
    fn within_budget(&self) -> bool {
        self.p50 < self.p50_budget && (!self.p99_is_meaningful() || self.p99 < WARM_P99_BUDGET_US)
    }

    /// How much of the budget is left. The number worth watching: the first
    /// measured 6%, and the supervisor's own work still has to fit in it.
    fn headroom_percent(&self) -> f64 {
        (self.p50_budget - self.p50) / self.p50_budget * 100.0
    }

    fn print(&self, style: &Style) {
        println!("{} over {} requests, empty handler", self.label, self.count);
        println!(
            "  p50 {:>7.0} µs   p90 {:>7.0}   p99 {:>7.0}   p99.9 {:>8.0}   max {:>8.0}   mean {:>7.0}",
            self.p50, self.p90, self.p99, self.p999, self.max, self.mean
        );
        println!("  {:.0} requests/s, sequential", self.per_second);
        println!();

        let verdict = |ok: bool, text: &str| {
            if ok {
                style.green(text)
            } else {
                style.red(text)
            }
        };
        println!(
            "  p50 < {:.0} µs   {}",
            self.p50_budget,
            verdict(
                self.p50 < self.p50_budget,
                if self.p50 < self.p50_budget {
                    "PASS"
                } else {
                    "FAIL"
                }
            )
        );
        if self.p99_is_meaningful() {
            println!(
                "  p99 < {:.0} µs  {}",
                WARM_P99_BUDGET_US,
                verdict(
                    self.p99 < WARM_P99_BUDGET_US,
                    if self.p99 < WARM_P99_BUDGET_US {
                        "PASS"
                    } else {
                        "FAIL"
                    }
                )
            );
        } else {
            println!(
                "  p99 < {:.0} µs  {}",
                WARM_P99_BUDGET_US,
                style.yellow("NOT MEASURED — the tenant was at its CPU quota")
            );
        }

        let headroom = self.headroom_percent();
        let note = format!("  headroom at p50: {headroom:.0}%");
        // The supervisor's queue, timers and metrics have yet to be added to
        // this path; a thin margin now is a regression waiting to happen.
        println!(
            "{}",
            if headroom < 15.0 {
                style.yellow(&note)
            } else {
                note
            }
        );
        self.print_quota(style);
    }

    /// What the tenant's CPU quota did, and what it means for the numbers above.
    fn print_quota(&self, style: &Style) {
        let Some(q) = self.quota else {
            return;
        };
        println!();
        let demand = q.demand_cores(self.elapsed);
        let per_request = if self.count > 0 {
            q.usage_us as f64 / f64::from(self.count) / 1000.0
        } else {
            0.0
        };
        match q.quota_cores {
            Some(cores) => println!(
                "  cpu   {:.2} cores used of {:.2} quota ({:.0}%), {:.2} ms per request",
                demand,
                cores,
                demand / cores * 100.0,
                per_request
            ),
            None => println!(
                "  cpu   {demand:.2} cores used, no quota, {per_request:.2} ms per request"
            ),
        }
        if q.periods > 0 {
            println!(
                "        throttled in {} of {} periods, {:.0} ms stopped in total",
                q.throttled_periods,
                q.periods,
                q.throttled_us as f64 / 1000.0
            );
        }

        if !self.saturated() {
            return;
        }
        // The one thing a reader must not conclude is that Zygo is slow.
        let bound = q.period.as_secs_f64() * 1e6 / 2.0;
        println!();
        println!(
            "{}",
            style.yellow(
                "  This run saturated the tenant's own CPU quota, so the tail above is\n  \
                 the quota being enforced, not Zygo's cost. A throttled request waits out\n  \
                 the rest of the enforcement period:"
            )
        );
        println!(
            "        period {:.0} ms → an expected {:.0} µs of added latency per throttled request",
            q.period.as_secs_f64() * 1000.0,
            bound
        );
        let capacity = q.quota_cores.unwrap_or(1.0) * 1000.0 / per_request.max(0.01);
        println!(
            "        this tenant sustains about {capacity:.0} requests/s; \
             measure latency below that with --rate, or raise --cpu"
        );
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "requests": self.count,
            "p50_us": self.p50,
            "p90_us": self.p90,
            "p99_us": self.p99,
            "p999_us": self.p999,
            "max_us": self.max,
            "mean_us": self.mean,
            "requests_per_second": self.per_second,
            "budget": { "p50_us": self.p50_budget, "p99_us": WARM_P99_BUDGET_US },
            "headroom_p50_percent": self.headroom_percent(),
            "pass": self.within_budget(),
            "p99_measured": self.p99_is_meaningful(),
            "cpu": self.quota.map(|q| serde_json::json!({
                "quota_cores": q.quota_cores,
                "period_us": q.period.as_micros() as u64,
                "used_cores": q.demand_cores(self.elapsed),
                "ms_per_request": q.usage_us as f64 / f64::from(self.count.max(1)) / 1000.0,
                "periods": q.periods,
                "throttled_periods": q.throttled_periods,
                "throttled_us": q.throttled_us,
                "saturated": q.saturated(),
            })),
        })
    }
}

/// Split the `run` phase into the handler and everything around it.
///
/// The child reports its own wall time, so the remainder is plumbing: the
/// child's start-up after `GO`, the result pipe, and its exit. A tail that
/// lives there is Zygo's; a tail in the handler is the handler's.
fn print_handler_share(phases: &[zygo_core::pool::CallTiming], handler_us: &[f64]) {
    if handler_us.len() != phases.len() {
        return;
    }
    let mut handler = handler_us.to_vec();
    let mut plumbing: Vec<f64> = phases
        .iter()
        .zip(handler_us)
        .map(|(t, h)| (t.run.as_secs_f64() * 1e6 - h).max(0.0))
        .collect();
    handler.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
    plumbing.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));

    println!(
        "{:<24} p50 {:>7.0} µs   p99 {:>7.0}   max {:>8.0}",
        "    of which handler",
        percentile(&handler, 50.0),
        percentile(&handler, 99.0),
        handler.last().copied().unwrap_or(0.0)
    );
    println!(
        "{:<24} p50 {:>7.0} µs   p99 {:>7.0}   max {:>8.0}",
        "    of which plumbing",
        percentile(&plumbing, 50.0),
        percentile(&plumbing, 99.0),
        plumbing.last().copied().unwrap_or(0.0)
    );
}

/// Cost of a bare `fork()` + `waitpid()` on this machine, in microseconds.
///
/// Deliberately the smallest possible child: it exits immediately. Anything the
/// warm path does is on top of this.
#[cfg(unix)]
fn measure_fork_floor(iterations: usize) -> Vec<f64> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let t0 = Instant::now();
        // SAFETY: the child does nothing but `_exit`, which is
        // async-signal-safe; the parent only waits for it.
        unsafe {
            match libc::fork() {
                0 => libc::_exit(0),
                -1 => return samples,
                pid => {
                    let mut status = 0;
                    libc::waitpid(pid, &mut status, 0);
                }
            }
        }
        samples.push(t0.elapsed().as_secs_f64() * 1e6);
    }
    samples
}

#[cfg(not(unix))]
fn measure_fork_floor(_iterations: usize) -> Vec<f64> {
    Vec::new()
}

/// Put the measurement next to what the machine can do at all.
fn print_floor(style: &Style, floor: &[f64], report: &Report) {
    if floor.is_empty() {
        return;
    }
    let mut sorted = floor.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));

    let floor_p50 = percentile(&sorted, 50.0);
    let floor_p99 = percentile(&sorted, 99.0);
    println!();
    println!(
        "  {} bare fork+wait on this host: p50 {:.0} µs, p99 {:.0} µs",
        style.dim("floor"),
        floor_p50,
        floor_p99
    );

    // If the machine's own p99 is already most of the budget, the run says more
    // about the machine than about Zygo, and the number should say so.
    if floor_p99 > WARM_P99_BUDGET_US * 0.2 {
        println!(
            "{}",
            style.yellow(&format!(
                "  the host's own fork p99 is {:.0}% of the p99 budget — this measurement is \n                   dominated by the machine, not by the warm path. Re-run on an idle host.",
                floor_p99 / WARM_P99_BUDGET_US * 100.0
            ))
        );
    }
    let _ = report;
}

/// Where the time went, phase by phase.
///
/// A total percentile says a tail exists; only the breakdown says which phase
/// owns it. Three plausible explanations for one were measured and refuted
/// before this existed.
/// The share of the p99 spent creating and joining the request's own cgroup.
///
/// `admit` and `release` are the per-request cgroup and nothing else, so when
/// they own the tail the number above is about open question A2 rather than
/// about the warm path. Returned as a fraction of the total p99.
fn cgroup_share_of_p99(phases: &[zygo_core::pool::CallTiming]) -> Option<f64> {
    if phases.is_empty() {
        return None;
    }
    let p99_of = |get: fn(&zygo_core::pool::CallTiming) -> Duration| {
        let mut v: Vec<f64> = phases.iter().map(|t| get(t).as_secs_f64() * 1e6).collect();
        v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        percentile(&v, 99.0)
    };
    let total = p99_of(|t| t.lock + t.fork + t.admit + t.run + t.release);
    if total <= 0.0 {
        return None;
    }
    Some((p99_of(|t| t.admit) + p99_of(|t| t.release)) / total)
}

/// Say when the tail belongs to the per-request cgroup rather than to Zygo.
///
/// The same duty as the CPU-quota note: a reader must not conclude from a
/// failed budget that the runtime is slow, when what they measured is a
/// decision that is still open. `bench warm` on a default configuration is
/// the command the README points newcomers at, and until A2 is settled it
/// prints `p99 FAIL` — so it has to say what the number is about, and how to
/// take the measurement without it.
fn print_cgroup_note(phases: &[zygo_core::pool::CallTiming], report: &Report, style: &Style) {
    let Some(share) = cgroup_share_of_p99(phases) else {
        return;
    };
    // Only when it owns the tail *and* the tail is what failed. A run inside
    // budget needs no excuse, and one that failed on p50 has another cause.
    if share < 0.5 || !report.p99_is_meaningful() || report.p99 < WARM_P99_BUDGET_US {
        return;
    }
    println!();
    println!(
        "{}",
        style.yellow(&format!(
            "  {:.0}% of the p99 above is `admit` and `release` — creating this request's\n  \
             own cgroup and removing it, not running the handler. That is the cost of\n  \
             per-request containment, not a property of the warm path.",
            share * 100.0
        ))
    );
    println!(
        "        measure without it: zygo bench warm --no-cgroup{}",
        match report.count {
            0 => String::new(),
            n => format!(" --n {n}"),
        }
    );
}

fn print_phases(phases: &[zygo_core::pool::CallTiming]) {
    let micros = |d: Duration| d.as_secs_f64() * 1e6;
    /// One row of the breakdown: a label and the phase it reads.
    type Column = (&'static str, fn(&zygo_core::pool::CallTiming) -> Duration);

    let columns: [Column; 5] = [
        ("  lock    (contention)", |t| t.lock),
        ("  fork    (EXEC→FORKED)", |t| t.fork),
        ("  admit   (cgroup)", |t| t.admit),
        ("  run     (GO→DONE)", |t| t.run),
        ("  release (cgroup rm)", |t| t.release),
    ];

    println!();
    for (label, get) in columns {
        let mut values: Vec<f64> = phases.iter().map(|t| micros(get(t))).collect();
        values.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        println!(
            "{label:<24} p50 {:>7.0} µs   p99 {:>7.0}   max {:>8.0}",
            percentile(&values, 50.0),
            percentile(&values, 99.0),
            values.last().copied().unwrap_or(0.0)
        );
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p / 100.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        sorted[lo] + (sorted[hi] - sorted[lo]) * (rank - lo as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run on a host with no readable cgroup: the latency is all there is.
    fn report(samples: &[f64]) -> Report {
        Report::of(samples, Duration::from_secs(1), samples.len() as u32, None)
    }

    /// A run whose tenant cgroup was read, throttled in `throttled` of
    /// `periods` enforcement periods.
    fn report_with_quota(samples: &[f64], periods: u64, throttled: u64) -> Report {
        Report::of(
            samples,
            Duration::from_secs(1),
            samples.len() as u32,
            Some(CpuAccounting {
                usage_us: 1_000_000,
                periods,
                throttled_periods: throttled,
                throttled_us: throttled * 50_000,
                quota_cores: Some(1.0),
                period: Duration::from_millis(100),
            }),
        )
    }

    /// `--rate` is a frequency; the loop needs a gap. Getting this the wrong
    /// way round does not fail, it just paces at 300 *seconds* per request —
    /// which is exactly what it did the first time.
    #[test]
    fn a_rate_becomes_the_gap_between_requests() {
        let gap = request_interval(Some(300.0)).expect("paced");
        assert!(
            (gap.as_secs_f64() - 1.0 / 300.0).abs() < 1e-9,
            "300 requests/s is 3.3 ms apart, not 300 s; got {gap:?}"
        );
        assert_eq!(request_interval(Some(1.0)), Some(Duration::from_secs(1)));
        assert_eq!(
            request_interval(Some(1000.0)),
            Some(Duration::from_millis(1))
        );
    }

    #[test]
    fn a_rate_that_cannot_be_paced_runs_unpaced() {
        assert_eq!(request_interval(None), None, "no --rate at all");
        assert_eq!(request_interval(Some(0.0)), None, "0/s would never finish");
        assert_eq!(request_interval(Some(-5.0)), None);
        assert_eq!(request_interval(Some(f64::NAN)), None);
        assert_eq!(
            request_interval(Some(f64::INFINITY)),
            None,
            "infinitely fast is just unpaced"
        );
    }

    #[test]
    fn percentiles_interpolate() {
        let sorted: Vec<f64> = (1..=100).map(|n| n as f64).collect();
        assert_eq!(percentile(&sorted, 0.0), 1.0);
        assert_eq!(percentile(&sorted, 100.0), 100.0);
        assert!((percentile(&sorted, 50.0) - 50.5).abs() < 0.01);
        assert_eq!(percentile(&[], 50.0), 0.0);
    }

    #[test]
    fn the_budget_is_the_design_documents() {
        // §5: p50 < 2 ms, p99 < 10 ms.
        assert_eq!(WARM_P50_BUDGET_US, 2_000.0);
        assert_eq!(WARM_P99_BUDGET_US, 10_000.0);
    }

    #[test]
    fn a_run_inside_the_budget_passes_and_one_outside_does_not() {
        let fast: Vec<f64> = (0..1000).map(|i| 1000.0 + (i % 100) as f64).collect();
        assert!(report(&fast).within_budget());

        let slow: Vec<f64> = (0..1000).map(|_| 2500.0).collect();
        assert!(!report(&slow).within_budget());

        // p50 inside the budget but a long tail outside it still fails.
        let mut tailed: Vec<f64> = (0..1000).map(|_| 1000.0).collect();
        tailed.extend((0..50).map(|_| 50_000.0));
        assert!(!report(&tailed).within_budget(), "p99 was ignored");
    }

    /// Phase 0 measured 6%. If a change eats into that, the number should say
    /// so rather than the run merely still passing.
    #[test]
    fn headroom_is_reported_as_a_percentage_of_the_budget() {
        let r = report(&[1_880.0; 100]);
        assert!(
            (r.headroom_percent() - 6.0).abs() < 0.5,
            "{}",
            r.headroom_percent()
        );

        let r = report(&[1_000.0; 100]);
        assert!((r.headroom_percent() - 50.0).abs() < 0.5);
    }

    #[test]
    fn the_json_report_carries_the_budget_it_was_judged_against() {
        let v = report(&[1_500.0; 100]).to_json();
        assert_eq!(v["budget"]["p50_us"], 2_000.0);
        assert_eq!(v["pass"], true);
        assert!(v["headroom_p50_percent"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn exit_status_follows_the_verdict() {
        assert_eq!(u8::from(!report(&[1_000.0; 100]).within_budget()), 0);
        assert_eq!(u8::from(!report(&[9_000.0; 100]).within_budget()), 1);
    }

    // --- saturation ------------------------------------------------------
    //
    // The tail this guards was chased for a whole session as if it were a
    // defect in the runtime. It was the tenant's own CPU quota: a run with no
    // think time asks for slightly more than one core, so CFS stops it until
    // the next period. These tests hold the tool to reporting which of the two
    // it measured.

    /// The measured shape: p50 well inside the budget, a tail of half the CFS
    /// period, and the cgroup saying it was throttled in nearly every period.
    fn a_throttled_run() -> Report {
        let mut samples: Vec<f64> = (0..980).map(|_| 1_400.0).collect();
        samples.extend((0..20).map(|_| 48_000.0));
        report_with_quota(&samples, 28, 27)
    }

    #[test]
    fn a_tail_made_by_the_cpu_quota_is_not_reported_as_a_failure() {
        let r = a_throttled_run();
        assert!(r.saturated());
        assert!(
            r.p99 > WARM_P99_BUDGET_US,
            "the fixture should be over budget at p99"
        );
        assert!(
            !r.p99_is_meaningful(),
            "a saturated run cannot judge the p99 budget"
        );
        assert!(
            r.within_budget(),
            "the quota doing its job is not a failure of this code"
        );
        assert_eq!(u8::from(!r.within_budget()), 0, "and CI should not go red");
    }

    #[test]
    fn a_saturated_run_still_fails_on_p50() {
        // Saturation excuses the tail, not the median: a p50 over budget is a
        // real regression whatever the quota was doing.
        let r = report_with_quota(&[2_500.0; 1000], 28, 27);
        assert!(r.saturated());
        assert!(!r.within_budget());
    }

    #[test]
    fn an_unsaturated_run_is_judged_on_both_budgets() {
        let mut samples: Vec<f64> = (0..980).map(|_| 1_400.0).collect();
        samples.extend((0..20).map(|_| 48_000.0));
        let r = report_with_quota(&samples, 200, 1);
        assert!(!r.saturated(), "1 period in 200 is noise, not saturation");
        assert!(r.p99_is_meaningful());
        assert!(!r.within_budget(), "a real tail must still fail");
    }

    #[test]
    fn a_host_without_a_readable_cgroup_judges_both_budgets() {
        // macOS, or cgroup v1, or a run outside the hierarchy: no counters
        // means no excuse.
        let mut samples: Vec<f64> = (0..980).map(|_| 1_400.0).collect();
        samples.extend((0..20).map(|_| 48_000.0));
        let r = report(&samples);
        assert!(!r.saturated());
        assert!(!r.within_budget());
    }

    #[test]
    fn the_json_report_says_whether_the_p99_was_measured() {
        let v = a_throttled_run().to_json();
        assert_eq!(v["p99_measured"], false);
        assert_eq!(v["cpu"]["saturated"], true);
        assert_eq!(v["cpu"]["throttled_periods"], 27);
        assert_eq!(v["cpu"]["quota_cores"], 1.0);
        assert_eq!(v["cpu"]["period_us"], 100_000);
        // A consumer that only reads `pass` must not be told the p99 passed.
        assert!(v["p99_us"].as_f64().unwrap() > WARM_P99_BUDGET_US);

        let v = report(&[1_500.0; 100]).to_json();
        assert_eq!(v["p99_measured"], true);
        assert!(v["cpu"].is_null(), "no cgroup, no cpu section");
    }
}
