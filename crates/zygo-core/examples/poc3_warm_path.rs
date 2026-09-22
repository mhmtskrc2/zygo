//! PoC 3 — warm request overhead.
//!
//! **The gate for the whole architecture.** If a request cannot be served in
//! under 2 ms at p50, the zygote model does not buy enough over `docker exec`
//! to justify building the rest.
//!
//! Measures the full supervisor-side round trip against the *real* reference
//! agent, not a throwaway, so the number is the one that will ship:
//!
//! ```text
//! EXEC ──► agent ──fork()──► child
//!      ◄── FORKED                      ├─ fork phase
//!   [create request cgroup, move pid]  ├─ cgroup phase
//! GO   ──►                             │
//!      ◄── DONE                        └─ run phase
//!   [remove cgroup]
//! ```
//!
//! The handler is empty, so what is measured is overhead and nothing else.
//!
//! ```bash
//! cargo run --release --example poc3_warm_path -- --n 100000
//! cargo run --release --example poc3_warm_path -- --n 100000 --no-cgroup
//! ```

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::Instant;

use zygo_core::protocol::{FrameReader, FrameWriter, Message};

fn main() {
    let args = Args::parse();
    println!("PoC 3 — warm request overhead");
    println!("  requests   {}", args.n);
    println!("  warmup     {}", args.warmup);
    println!(
        "  cgroup     {}",
        if args.cgroup {
            "per request (design doc §3.2)"
        } else {
            "disabled (--no-cgroup)"
        }
    );
    println!(
        "  host       {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    println!();

    let mut harness = match Harness::start(&args) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("could not start the agent: {e}");
            std::process::exit(1);
        }
    };

    let ready = harness.ready();
    println!("agent ready: {ready}");
    println!();

    for i in 0..args.warmup {
        harness.request(&format!("w{i}"));
    }

    let mut total = Vec::with_capacity(args.n);
    let mut fork_phase = Vec::with_capacity(args.n);
    let mut cgroup_phase = Vec::with_capacity(args.n);
    let mut run_phase = Vec::with_capacity(args.n);

    let started = Instant::now();
    for i in 0..args.n {
        let sample = harness.request(&format!("r{i}"));
        total.push(sample.total);
        fork_phase.push(sample.fork);
        cgroup_phase.push(sample.cgroup);
        run_phase.push(sample.run);

        if args.n >= 10_000 && i > 0 && i % (args.n / 10) == 0 {
            eprint!("\r  {}%", i * 100 / args.n);
        }
    }
    let elapsed = started.elapsed();
    if args.n >= 10_000 {
        eprintln!("\r      ");
    }

    report("total (EXEC → DONE)", &mut total);
    report("  fork  (EXEC → FORKED)", &mut fork_phase);
    if args.cgroup {
        report("  cgroup (FORKED → GO)", &mut cgroup_phase);
    }
    report("  run   (GO → DONE)", &mut run_phase);

    println!();
    println!(
        "throughput   {:.0} req/s sequential, {:.1} s wall",
        args.n as f64 / elapsed.as_secs_f64(),
        elapsed.as_secs_f64()
    );

    let p50 = percentile(&total, 50.0);
    let p99 = percentile(&total, 99.0);
    let pass = p50 < 2_000.0 && p99 < 10_000.0;
    println!();
    println!(
        "acceptance   p50 < 2 ms and p99 < 10 ms  →  {}",
        if pass { "PASS" } else { "FAIL" }
    );

    harness.shutdown();
    if !pass {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------

struct Args {
    n: usize,
    warmup: usize,
    cgroup: bool,
    agent: PathBuf,
}

impl Args {
    fn parse() -> Args {
        let argv: Vec<String> = std::env::args().collect();
        let value = |flag: &str| -> Option<String> {
            argv.iter()
                .position(|a| a == flag)
                .and_then(|i| argv.get(i + 1))
                .cloned()
        };
        Args {
            n: value("--n").and_then(|v| v.parse().ok()).unwrap_or(20_000),
            warmup: value("--warmup")
                .and_then(|v| v.parse().ok())
                .unwrap_or(500),
            // A per-request cgroup is the design's default (open question A2);
            // `--no-cgroup` isolates how much of the budget it costs.
            cgroup: !argv.iter().any(|a| a == "--no-cgroup"),
            agent: value("--agent")
                .map(PathBuf::from)
                .unwrap_or_else(default_agent_path),
        }
    }
}

fn default_agent_path() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is crates/zygo-core.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agents/python/zygo_agent.py")
}

struct Sample {
    total: f64,
    fork: f64,
    cgroup: f64,
    run: f64,
}

struct Harness {
    reader: FrameReader<UnixStream>,
    writer: FrameWriter<UnixStream>,
    child: Child,
    cgroup: Option<CgroupHarness>,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn start(args: &Args) -> std::io::Result<Harness> {
        let dir = tempfile::tempdir()?;

        let handler = dir.path().join("handler.py");
        std::fs::write(&handler, "def handler(event):\n    return None\n")?;

        let sock_path = dir.path().join("agent.sock");
        let listener = UnixListener::bind(&sock_path)?;

        let child = Command::new(python())
            .arg(&args.agent)
            .arg(&sock_path)
            .arg(&handler)
            .spawn()?;

        let (stream, _) = listener.accept()?;
        let reader = FrameReader::new(stream.try_clone()?);
        let writer = FrameWriter::new(stream);

        Ok(Harness {
            reader,
            writer,
            child,
            cgroup: if args.cgroup {
                CgroupHarness::new()
            } else {
                None
            },
            _dir: dir,
        })
    }

    fn ready(&mut self) -> String {
        match self.reader.read() {
            Ok(Some(Message::Ready {
                proto,
                imports_ms,
                rss_kb,
                runtime,
                ..
            })) => format!(
                "{runtime}, proto {proto}, imports {imports_ms:.1} ms, rss {} MB",
                rss_kb / 1024
            ),
            other => {
                eprintln!("expected READY, got {other:?}");
                std::process::exit(1);
            }
        }
    }

    fn request(&mut self, id: &str) -> Sample {
        let t0 = Instant::now();

        self.writer
            .write(&Message::Exec {
                script: None,
                id: id.to_string(),
                event: serde_json::Value::Null,
                timeout_ms: 30_000,
                env_overrides: Default::default(),
                stream: false,
                workspace: None,
            })
            .expect("EXEC");

        let pid = match self.reader.read() {
            Ok(Some(Message::Forked { pid, .. })) => pid,
            other => panic!("expected FORKED, got {other:?}"),
        };
        let t_forked = Instant::now();

        // The real supervisor's work in the FORKED → GO window: give the child
        // its own cgroup before it runs a single instruction of user code.
        if let Some(cg) = &self.cgroup {
            cg.admit(id, pid);
        }
        let t_admitted = Instant::now();

        self.writer
            .write(&Message::Go { id: id.to_string() })
            .expect("GO");

        match self.reader.read() {
            Ok(Some(Message::Done { exit_code: 0, .. })) => {}
            other => panic!("expected a successful DONE, got {other:?}"),
        }
        let t_done = Instant::now();

        if let Some(cg) = &self.cgroup {
            cg.release(id);
        }

        Sample {
            total: micros(t0, t_done),
            fork: micros(t0, t_forked),
            cgroup: micros(t_forked, t_admitted),
            run: micros(t_admitted, t_done),
        }
    }

    fn shutdown(&mut self) {
        let _ = self.writer.write(&Message::Shutdown { grace_ms: 0 });
        let _ = self.child.wait();
    }
}

fn micros(from: Instant, to: Instant) -> f64 {
    to.duration_since(from).as_secs_f64() * 1e6
}

fn python() -> String {
    std::env::var("PYTHON").unwrap_or_else(|_| "python3".to_string())
}

// ---------------------------------------------------------------------------
// cgroup
// ---------------------------------------------------------------------------

/// Per-request cgroup creation, exactly as the supervisor will do it.
struct CgroupHarness {
    root: PathBuf,
}

impl CgroupHarness {
    fn new() -> Option<CgroupHarness> {
        if !cfg!(target_os = "linux") {
            eprintln!(
                "note: not Linux — the cgroup phase is skipped, so `total` is \
                 optimistic by roughly the cost of a mkdir plus four writes"
            );
            return None;
        }

        let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?.trim();
        let root = Path::new("/sys/fs/cgroup")
            .join(rel.trim_start_matches('/'))
            .join("zygo-poc3");

        if let Err(e) = std::fs::create_dir_all(&root) {
            eprintln!("note: cannot create a cgroup at {}: {e}", root.display());
            eprintln!("      the cgroup phase is skipped; run privileged for the full number");
            return None;
        }
        Some(CgroupHarness { root })
    }

    fn admit(&self, id: &str, pid: u32) {
        let dir = self.root.join(format!("req-{id}"));
        let _ = std::fs::create_dir(&dir);
        // Order matters: limits before the pid, so the process is never
        // resident in an unlimited cgroup even briefly.
        let _ = write(&dir.join("memory.max"), "268435456");
        let _ = write(&dir.join("pids.max"), "64");
        let _ = write(&dir.join("cgroup.procs"), &pid.to_string());
    }

    fn release(&self, id: &str) {
        let _ = std::fs::remove_dir(self.root.join(format!("req-{id}")));
    }
}

fn write(path: &Path, value: &str) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new().write(true).open(path)?;
    f.write_all(value.as_bytes())
}

// ---------------------------------------------------------------------------
// statistics
// ---------------------------------------------------------------------------

fn percentile(sorted_or_not: &[f64], p: f64) -> f64 {
    let mut v = sorted_or_not.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    percentile_sorted(&v, p)
}

fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
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

fn report(label: &str, samples: &mut [f64]) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    println!(
        "{label:<24} p50 {:>7.0} µs   p90 {:>7.0}   p99 {:>7.0}   p99.9 {:>8.0}   max {:>8.0}   mean {:>7.0}",
        percentile_sorted(samples, 50.0),
        percentile_sorted(samples, 90.0),
        percentile_sorted(samples, 99.0),
        percentile_sorted(samples, 99.9),
        samples.last().copied().unwrap_or(0.0),
        mean,
    );
}
