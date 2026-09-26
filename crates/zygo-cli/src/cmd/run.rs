// SPDX-License-Identifier: Apache-2.0
//! `zygo run` — a one-shot sandbox.
//!
//! What this command owns is everything up to the launcher:
//! resolve the spec and flags, make sure the image is in the store, build the
//! rootfs view and the mount plan. `--dry-run` prints exactly that, which is
//! also how the plan gets reviewed without a Linux host.

use std::os::fd::AsRawFd;

use anyhow::Context;
use zygo_core::backend;
use zygo_core::image::{PullProgress, Reference, RegistryClient, Store};
use zygo_core::sandbox::{RootfsView, SandboxConfig};
use zygo_core::spec::Spec;

use crate::cli::{Cli, PullPolicy, RunArgs};
use crate::output::{self, Style};
use crate::tty;

pub fn run(cli: &Cli, args: &RunArgs) -> anyhow::Result<u8> {
    let phase = std::cell::Cell::new(Phase::Plan);
    let result = run_in_phases(cli, args, &phase);

    // A sandbox that never started is not a program that failed, and the
    // exit status cannot say which (the first real consumer had to classify
    // a start failure by matching on the error's text, and reported it as
    // the tenant's fault meanwhile). The outcome file can:
    // written here too, with `started: false` and the phase that failed,
    // so a caller reports "unavailable" rather than "harness" without
    // reading stderr. Best effort — the error is the answer, and a file
    // that could not be written must not replace it.
    if let (Err(e), Some(path)) = (&result, &args.outcome)
        && phase.get() != Phase::Run
    {
        let code = e
            .chain()
            .find_map(|cause| cause.downcast_ref::<zygo_core::Error>())
            .map(|e| e.exit_code())
            .unwrap_or(1);
        let _ = Outcome::never_started(phase.get(), code).write(path);
    }
    result
}

fn run_in_phases(cli: &Cli, args: &RunArgs, phase: &std::cell::Cell<Phase>) -> anyhow::Result<u8> {
    // Where a one-shot run spends its time, in three phases, because "the
    // p99 is twenty times the p50 on this host" cannot be acted on without
    // knowing *which* twenty milliseconds became twenty seconds. `zygo bench
    // warm` has had this breakdown from the start; the one-shot path did not,
    // and a Raspberry Pi with a saturated SD card is where that showed.
    let planning = std::time::Instant::now();
    let overrides = args.to_layer()?;
    let options = zygo_core::spec::ResolveOptions {
        one_shot: true,
        ..args.sandbox.resolve_options()
    };

    // A spec file is optional for `run`: the flags alone are enough.
    let spec = Spec::discover(args.spec_file.path())?.unwrap_or_default();
    let resolved = spec.resolve(None, &overrides, &options)?;

    // Zygo's own output shares a file descriptor with the sandbox's, so a
    // caller capturing the streams would otherwise read "pulling python:3.12-slim"
    // as something the program wrote.
    let say = |message: &str| {
        if !args.quiet {
            eprintln!("{message}");
        }
    };
    let warn = |message: &str| {
        if !args.quiet {
            output::warn(message);
        }
    };

    for w in &resolved.warnings {
        warn(w);
    }

    let paths = super::paths(cli);
    paths.ensure()?;
    let store = Store::new(paths);

    let reference: Reference = match resolved.image.parse() {
        Ok(r) => r,
        // `-v` is Zygo's verbose flag, and Docker's volume flag. Someone
        // carrying a command over types `zygo run -v $PWD:/src image`, clap
        // takes the pair as the image argument, and the reference parser
        // reports an empty path component — accurate, and about the wrong
        // thing entirely. Recognise the shape and name the flag that does it.
        Err(e) => match mount_pair(&resolved.image) {
            Some((host, guest)) => anyhow::bail!(
                "`{}` is a mount, not an image\n  \
                 → `-v` means verbose in Zygo, not volume; \
                 use: zygo run --mount {host}:{guest} <image> …",
                resolved.image
            ),
            None => {
                return Err(anyhow::Error::new(e))
                    .with_context(|| format!("cannot use image `{}`", resolved.image));
            }
        },
    };

    let entry = match (store.get(&reference), args.pull) {
        (Some(e), PullPolicy::Missing | PullPolicy::Never) => e,
        (None, PullPolicy::Never) => {
            // The caller said it would rather fail than wait. Refused here, in
            // the plan phase, so `--outcome` says `started: false` and the
            // caller can report "not set up" rather than "too slow".
            anyhow::bail!(
                "`{}` is not in the store, and --pull never was given\n  \
                 → pull it first: zygo pull {}",
                resolved.image,
                resolved.image
            );
        }
        (_, _) => {
            // Same behaviour as `docker run`: pull on first use — or again,
            // under `--pull always`, for a tag that may have moved.
            let client = RegistryClient::new(store.clone())?;
            let style = Style::stdout();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(client.pull(&reference, |event| {
                if let PullProgress::Resolving { reference } = event {
                    say(&format!("{} {reference}", style.dim("pulling")));
                }
            }))?
        }
    };

    // A running supervisor already sits in a delegated, built cgroup — the
    // one thing this process, on an ordinary systemd session, cannot get
    // into: it would need a transient scope, a second `zygo`, and a cgroup
    // tree built and torn down per command, and cgroup delegation
    // containment forbids the cheaper move (`scope.rs` has the numbers:
    // 45 ms against 11). So when a supervisor is up the sandbox is started
    // *there*, with this process's own three streams handed over, and this
    // process does what it would have done anyway: forward its terminal's
    // signals and wait.
    //
    // Everything up to here — the spec, the flags, the pull on first use —
    // stays local, because the supervisor does not pull and the store is
    // shared. Everything after is the supervisor's, resolved from the same
    // spec and layer so the run is the run it would have been.
    //
    // `--tty` stays local: it builds a pty pair and relays it, and handing a
    // terminal that already belongs to a shell's session to a sandbox that
    // wants it as its own is a `TIOCSCTTY` the kernel refuses. `--dry-run`
    // prints a plan and starts nothing.
    #[cfg(target_os = "linux")]
    if !args.dry_run
        && !args.tty
        && resolved.isolation == zygo_core::spec::Isolation::Ns
        && let Ok(client) = zygo_core::supervisor::client::Client::connect(store.paths())
    {
        return run_through_supervisor(
            cli,
            args,
            client,
            &Plan {
                spec: &spec,
                layer: &overrides,
                options: &options,
                resolved: &resolved,
                started: planning,
            },
            phase,
        );
    }

    // `system = [...]` from the spec: the packages installed once, as a layer.
    let entry = if resolved.system.is_empty() {
        entry
    } else {
        let derived = zygo_core::derive::ensure(&store, &entry, &resolved.system)?;
        if derived.built {
            say(&format!(
                "{} {}",
                Style::stdout().dim("installed"),
                derived.versions.join(" ")
            ));
        }
        derived.image
    };

    // `requirements`: the venv the warm path builds, for a one-shot.
    //
    // The same cache, keyed the same way, so a `zygo run --requirements` and a
    // `zygo serve --requirements` on the same image and the same file share
    // one venv. `examples/ci-job` documented this flag before it existed
    // (the use-case pass), and the workaround it forced — installing
    // packages into a directory and bind-mounting it — rebuilt them per job.
    let mut resolved = resolved;
    let venv = match &resolved.requirements {
        Some(requirements) => {
            anyhow::ensure!(
                requirements.is_file(),
                "`{}` does not exist\n  → --requirements takes a path to a \
                 requirements file, resolved against the working directory",
                requirements.display()
            );
            let venv = zygo_core::venv::ensure(&store, &entry, requirements)?;
            if venv.built {
                say(&format!(
                    "{} {}",
                    Style::stdout().dim("built venv"),
                    venv.dir.display()
                ));
            }
            resolved.mounts.push(venv.mount());
            Some(venv)
        }
        None => None,
    };

    // Python's standard library compiled, once, as a layer: the slim images
    // ship none, and a read-only root cannot cache it. After the venv, whose
    // cache is keyed on the image it was built for. `--dry-run` builds nothing.
    let entry = if args.dry_run {
        entry
    } else {
        let compiled = zygo_core::bytecode::ensure(&store, &entry)?;
        if compiled.built {
            say(&format!(
                "{} {}",
                Style::stdout().dim("compiled bytecode for"),
                entry.reference
            ));
        }
        compiled.image
    };

    // The network, when this run has one: the allowlist is resolved here on
    // the host, and `/etc/resolv.conf` is bound in rather than written.
    // `--dry-run` wants the mount in the plan it prints, so this comes first.
    let net = zygo_core::net::setup(store.paths(), &resolved.name, &resolved)?;
    if let Some(mount) = net.mount.clone() {
        resolved.mounts.push(mount);
    }
    for w in &net.warnings {
        warn(w);
    }
    let resolved = resolved;

    // Overlay needs a kernel that permits it inside a user namespace; the store
    // falls back to a flattened rootfs when it does not.
    //
    // `gvisor` and `vm` always take the flattened one, for the same reason
    // written two ways: an OCI bundle's `root.path` is a single directory, and
    // `krun_set_root` takes a single directory. There is nowhere for a stack
    // of lowerdirs to go. gVisor's Sentry keeps an overlay of its own above it,
    // and a guest kernel can build one inside itself if the layers are ever
    // needed back.
    let host = zygo_core::doctor::cached(store.paths());
    let overlay_supported = !matches!(
        resolved.isolation,
        zygo_core::spec::Isolation::Gvisor | zygo_core::spec::Isolation::Vm
    ) && host
        .checks
        .iter()
        .any(|c| c.name == "overlayfs (userns)" && c.status == zygo_core::doctor::Status::Ok);

    // The sandbox root is read-only, so every path the launcher mounts over has
    // to exist before it is assembled; the store fills in what the image lacks.
    let mount_points = zygo_core::sandbox::mount::required_mount_points(&resolved.mounts);
    let view = store.rootfs_view(&entry.layers, overlay_supported, &mount_points)?;
    let newroot = store
        .paths()
        .tmp()
        .join(format!("root-{}", zygo_core::process_token()));
    std::fs::create_dir_all(&newroot)?;
    // Removed however this function leaves, not only when it succeeds. The
    // cleanup used to be one line before the `Ok`, so a run that failed after
    // the directory was made — a bad mount, a missing image, a kernel that
    // refused — left it behind, and only a failing run ever did. Found by the
    // embedder's script driver: its tests fail on purpose, and the tmp
    // directory filled up with empty `root-<pid>` directories while successful
    // runs left none.
    let newroot = Scratch(newroot);
    let newroot = &newroot.0;

    // The image's own config supplies the default command and, just as
    // importantly, `PATH` — without which a bare `python3` cannot be resolved.
    //
    // Read from the store directly, as the pool does. It used to go through a
    // registry client — an HTTP client with its TLS roots, and a
    // multi-threaded async runtime with a worker per core — built on every
    // run to read one small file that is always already local.
    let image_config: zygo_core::image::ImageConfig =
        serde_json::from_slice(&store.read_blob(&entry.config)?)
            .context("the image's config in the store is not valid JSON")?;

    let argv = if resolved.cmd.is_empty() {
        let argv = image_config.default_argv();
        anyhow::ensure!(
            !argv.is_empty(),
            "`{}` declares no entrypoint or cmd\n  → give a command: zygo run {} <command>",
            resolved.image,
            resolved.image
        );
        argv
    } else {
        resolved.cmd.clone()
    };

    // The image's own environment, with the venv's `PATH` in front of it when
    // there is one. In front rather than instead: the image ships other
    // programs, and a venv is not a reason to lose them.
    let mut env = image_config.env_pairs();
    if venv.is_some() {
        let image_path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        env.retain(|(k, _)| k != "PATH" && k != "VIRTUAL_ENV");
        env.extend(zygo_core::venv::Venv::env_over(&image_path));
    }

    let mut config = SandboxConfig::from_resolved(&resolved, &view, newroot, argv, &env);
    config.allow_resolved = net.allowed;
    config.pasta_pid_file = net.pid_file;

    if args.dry_run {
        let seccomp_source = spec.seccomp_source(None, &overrides);
        return print_plan(cli, &resolved, seccomp_source, &host, &view, &config);
    }

    // A terminal of the sandbox's own, when asked for. The master stays here;
    // the slave becomes the sandbox's stdio, and the caller's real terminal is
    // never exposed to tenant code.
    let mut terminal = None;
    let mut raw_mode = None;
    if args.tty {
        let pty = tty::open()?;
        tty::copy_window_size(std::io::stdin().as_raw_fd(), pty.slave.as_raw_fd());
        // Raw mode is restored when `raw_mode` drops, including on the error
        // paths below — a terminal left raw makes the user's shell look broken.
        raw_mode = tty::RawMode::enable(std::io::stdin().as_raw_fd())?;
        config.stdio = Some(pty.slave.as_raw_fd());
        terminal = Some(pty);
    }

    let backend = backend::for_isolation(resolved.isolation, store.paths())?;
    let plan_ms = planning.elapsed().as_secs_f64() * 1000.0;

    // Everything between here and `wait`: `clone3`, the mount plan,
    // `pivot_root`, the cgroup, seccomp, Landlock, and `execve`. The program
    // has not run one instruction of its own when this phase ends.
    let starting = std::time::Instant::now();
    phase.set(Phase::Start);
    let mut sandbox = backend.start(&config)?;
    phase.set(Phase::Run);
    let start_ms = starting.elapsed().as_secs_f64() * 1000.0;

    // The slave belongs to the sandbox now; holding it open here would keep the
    // pty alive after the sandbox exits and the relay would never see EOF.
    let relay = terminal.map(|pty| {
        drop(pty.slave);
        tty::relay(pty.master)
    });

    // Forward the terminal's signals to the sandbox rather than dying and
    // leaving it orphaned. `PDEATHSIG` would catch that case anyway, but the
    // program deserves the chance to shut down on its own terms.
    forward_signals(sandbox.pid());

    let started = std::time::Instant::now();
    let waited = sandbox.wait();
    // The backend sampled this during teardown, which is the last moment the
    // cgroup exists. Reading the directory from here found it already gone and
    // reported zero — which reads exactly like "it was not killed".
    let kernel = sandbox.outcome();

    if let Some(handle) = relay {
        let _ = handle.join();
    }
    drop(raw_mode);

    // Why it ended, for a caller that cannot tell from the exit status.
    //
    // A deadline kill and an out-of-memory kill are both exit 137, and a judge
    // — or any caller deciding between "too slow" and "too big" — cannot tell
    // them apart from that alone (the use-case pass). The launcher
    // knows the first; the kernel's `memory.events` records the second. Both
    // are written to the file `--outcome` names, out of band, because standard
    // output belongs to the program.
    let outcome = Outcome {
        // `sandbox.wait()` returns the library's own error type, so the
        // conventions it carries are read directly rather than downcast.
        exit_code: match &waited {
            Ok(code) => (*code).clamp(0, 255),
            Err(e) => e.exit_code(),
        },
        timed_out: waited
            .as_ref()
            .err()
            .is_some_and(zygo_core::Error::timed_out),
        oom_killed: kernel.oom_kills > 0,
        peak_rss_kb: kernel.peak_rss_kb,
        wall_ms: started.elapsed().as_secs_f64() * 1000.0,
        plan_ms,
        start_ms,
        started: true,
        phase: Phase::Run,
    };
    if let Some(path) = &args.outcome {
        outcome.write(path)?;
    }
    // On stderr, and only when asked: standard output belongs to the program,
    // and a line per run is noise everywhere except the run being looked at.
    if cli.verbose > 0 && !args.quiet {
        eprintln!(
            "timing: plan {plan_ms:.1} ms, start {start_ms:.1} ms, run {:.1} ms",
            outcome.wall_ms
        );
    }

    let code = waited?;
    Ok(code.clamp(0, 255) as u8)
}

/// What the plan phase produced, handed to the supervisor path whole.
///
/// The supervisor resolves the run again from the same spec and layer, so
/// it needs the inputs rather than only the result; `resolved` is kept for
/// the deadline this process waits on. One struct rather than five
/// arguments beside the client and the flags, so the call site says what
/// the plan is.
#[cfg(target_os = "linux")]
struct Plan<'a> {
    spec: &'a Spec,
    layer: &'a zygo_core::spec::Layer,
    options: &'a zygo_core::spec::ResolveOptions,
    resolved: &'a zygo_core::spec::ResolvedFn,
    /// When planning began, for the `plan` figure in `--outcome`.
    started: std::time::Instant,
}

/// `zygo run`, with the sandbox started by the supervisor on this process's
/// behalf. See the branch in [`run`] for why.
///
/// The conversation is `RUN` → three descriptors over `SCM_RIGHTS` →
/// `STARTED{pid}` → … → `RAN{…}`. The pid arrives first so the terminal's
/// signals can be forwarded while the program runs; the wait has the
/// sandbox's own deadline plus grace as a liveness bound, and the supervisor
/// enforces the real deadline through the cgroup either way.
#[cfg(target_os = "linux")]
fn run_through_supervisor(
    cli: &Cli,
    args: &RunArgs,
    mut client: zygo_core::supervisor::client::Client,
    plan: &Plan<'_>,
    phase: &std::cell::Cell<Phase>,
) -> anyhow::Result<u8> {
    use zygo_core::supervisor::protocol::{Request, Response};

    let Plan {
        spec,
        layer,
        options,
        resolved,
        started: planning,
    } = *plan;
    let request = Request::Run {
        spec: Some(Box::new(spec.clone())),
        layer: Box::new(layer.clone()),
        // Absolute, because `.` means this process's directory and the
        // supervisor is in another one.
        base_dir: std::path::absolute(spec.base_dir())?,
        allow_host_net: options.allow_host_net,
        allow_private_net: options.allow_private_net,
        allow_unlimited: options.allow_unlimited,
        tty: false,
        ignored_signals: ignored_signals(),
    };
    let plan_ms = planning.elapsed().as_secs_f64() * 1000.0;

    let starting = std::time::Instant::now();
    phase.set(Phase::Start);
    let pid = client.run_start(&request, [0, 1, 2])?;
    phase.set(Phase::Run);
    let start_ms = starting.elapsed().as_secs_f64() * 1000.0;

    // The sandbox is the supervisor's child, not this process's, but it is
    // this process's terminal: a Ctrl-C typed here reaches here.
    forward_signals(pid);

    // Zero is `--timeout 0`, as long as it takes: no liveness bound either.
    let deadline = resolved.limits.timeout.get();
    let budget = (!deadline.is_zero()).then(|| deadline + std::time::Duration::from_secs(30));
    let ran = client.run_wait(budget)?;
    let Response::Ran {
        exit_code,
        timed_out,
        oom_killed,
        peak_rss_kb,
        wall_ms,
    } = ran
    else {
        anyhow::bail!("the supervisor answered {ran:?} rather than RAN");
    };

    let outcome = Outcome {
        exit_code,
        timed_out,
        oom_killed,
        peak_rss_kb,
        wall_ms,
        plan_ms,
        start_ms,
        started: true,
        phase: Phase::Run,
    };
    if let Some(path) = &args.outcome {
        outcome.write(path)?;
    }
    if cli.verbose > 0 && !args.quiet {
        eprintln!(
            "timing: plan {plan_ms:.1} ms, start {start_ms:.1} ms, run {wall_ms:.1} ms \
             (through the supervisor)"
        );
    }
    if timed_out && !args.quiet {
        eprintln!(
            "error: the sandbox exceeded its {:?} deadline and was killed",
            resolved.limits.timeout.get()
        );
    }
    Ok(exit_code.clamp(0, 255) as u8)
}

/// The signals this process ignores, as the mask a supervisor applies to a
/// program it starts for us — bit `n - 1` for signal `n`.
///
/// A sandbox this process starts itself inherits them; one the supervisor
/// starts would inherit the supervisor's instead, and `nohup zygo run …`
/// has to mean the same thing either way.
#[cfg(target_os = "linux")]
fn ignored_signals() -> u64 {
    let mut mask = 0u64;
    for signal in 1..=64 {
        // SAFETY: a query, with a null new action; `action` is a live local.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: `sigaction` with a valid signal number, a null new action and
        // a live local to fill in; it changes nothing.
        let queried = unsafe { libc::sigaction(signal, std::ptr::null(), &mut action) } == 0;
        if queried && action.sa_sigaction == libc::SIG_IGN {
            mask |= 1u64 << (signal - 1);
        }
    }
    mask
}

/// How far a one-shot run got.
///
/// `plan` is everything before a sandbox exists — the spec, the image, the
/// venv, the network; `start` is building it, up to `execve`; `run` is the
/// program. An outcome that stopped short of `run` is Zygo's failure (or
/// the host's), not the program's, and a caller can say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum Phase {
    Plan,
    Start,
    Run,
}

/// Why a one-shot sandbox ended, beyond its exit status.
#[derive(serde::Serialize)]
struct Outcome {
    exit_code: i32,
    /// The launcher's own deadline killed it.
    timed_out: bool,
    /// The kernel killed something here for running out of memory.
    oom_killed: bool,
    peak_rss_kb: u64,
    /// The program's own time: from `execve` to exit, including the
    /// backend's teardown of the sandbox afterwards.
    wall_ms: f64,
    /// Before the sandbox: the spec, the image store, the host probe, the
    /// rootfs view. Reads, mostly; on a slow disk this is where they show.
    plan_ms: f64,
    /// Building the sandbox, up to and including `execve`.
    start_ms: f64,
    /// Whether the program ran at all. `false` is Zygo failing, not the
    /// program: a caller reports it as *unavailable*, not as the code's.
    started: bool,
    /// The phase the run reached: `run` when the program ran, otherwise
    /// the one that failed.
    phase: Phase,
}

impl Outcome {
    /// The outcome of a run that never reached the program.
    fn never_started(phase: Phase, exit_code: i32) -> Outcome {
        Outcome {
            exit_code,
            timed_out: false,
            oom_killed: false,
            peak_rss_kb: 0,
            wall_ms: 0.0,
            plan_ms: 0.0,
            start_ms: 0.0,
            started: false,
            phase,
        }
    }

    /// Written whole, then renamed: a reader that finds the file finds all of
    /// it, never half a JSON document.
    fn write(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let temporary = path.with_extension("outcome-tmp");
        std::fs::write(&temporary, serde_json::to_vec(self)?)
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        std::fs::rename(&temporary, path)
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }
}

/// A directory that belongs to one run, removed when that run is over.
///
/// `remove_dir` rather than `remove_dir_all`: this is a `pivot_root` target
/// and it is empty once the sandbox is gone. If it is not — a mount that
/// outlived its namespace — the removal fails and the directory stays, which
/// is the right way round. Deleting a tree through a live mount point would
/// reach whatever is mounted there.
struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir(&self.0);
    }
}

/// Relay this process's terminal signals to the sandbox.
///
/// The program in the sandbox is pid 1 of its pid namespace, and the kernel
/// delivers a signal from outside a pid namespace to its init only if init
/// has a handler for it: a Python program gets its `KeyboardInterrupt`, a
/// `sleep` gets nothing. So each signal goes to init *and* to init's process
/// group — the sandbox started with `setsid`, so that is everything it
/// forked, and a child dying of `SIGINT` ends a shell the way a terminal's
/// Ctrl-C does — and the **second** signal of any kind is a `SIGKILL` to
/// init, which is the one signal a namespace init cannot ignore and which
/// takes the whole namespace with it. One Ctrl-C asks; two insist.
///
/// The same relay serves a sandbox this process started and one a supervisor
/// started for it: in both the pid is init's, seen from here.
#[cfg(unix)]
fn forward_signals(pid: u32) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static TARGET: AtomicU32 = AtomicU32::new(0);
    static DELIVERED: AtomicU32 = AtomicU32::new(0);
    TARGET.store(pid, Ordering::SeqCst);

    extern "C" fn relay(signal: i32) {
        let pid = TARGET.load(Ordering::SeqCst) as libc::pid_t;
        if pid == 0 {
            return;
        }
        // SAFETY: `kill` is async-signal-safe, which is the whole
        // constraint on a signal handler; the atomics are lock-free.
        unsafe {
            if DELIVERED.fetch_add(1, Ordering::SeqCst) > 0 {
                libc::kill(pid, libc::SIGKILL);
                return;
            }
            libc::kill(pid, signal);
            // `ESRCH` when init leads no group of its own — a sandbox this
            // process started shares its group, and the terminal has already
            // signalled that group itself.
            libc::kill(-pid, signal);
        }
    }

    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only reads atomics and calls `kill`.
        let previous = unsafe { libc::signal(signal, relay as *const () as libc::sighandler_t) };
        // A signal this process was told to ignore — `nohup zygo run …`, or
        // a job a script put in the background — is not one to pass on.
        if previous == libc::SIG_IGN {
            // SAFETY: restoring the disposition just read.
            unsafe { libc::signal(signal, libc::SIG_IGN) };
        }
    }
}

#[cfg(not(unix))]
fn forward_signals(_pid: u32) {}

/// `host:guest` read as an image reference, if that is what this looks like.
///
/// Deliberately narrow: the guest side has to be absolute, which is what a
/// bind mount requires anyway, and that is what keeps `alpine:3` and
/// `localhost:5000/team/app:v2` out of it — in both the text after the first
/// colon is a tag or a port, and neither starts with `/`.
fn mount_pair(s: &str) -> Option<(&str, &str)> {
    let (host, guest) = s.split_once(':')?;
    let host_is_a_path = host.starts_with('/')
        || host.starts_with('.')
        || host.starts_with('~')
        || host.contains('/');
    (guest.starts_with('/') && host_is_a_path).then_some((host, guest))
}

/// How many syscalls a profile names on this architecture.
///
/// `None` where there is no `ns` backend to ask — which is macOS, where
/// `--dry-run` is forwarded into the VM and never printed here.
fn allowed_syscalls(profile: zygo_core::spec::SeccompProfile) -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        Some(zygo_core::backend::ns::seccomp::allowed_names(profile).len())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = profile;
        None
    }
}

/// The host's Landlock line from `doctor`, for the plan.
///
/// Landlock is applied by the launcher from what the kernel offers, not
/// from anything in the spec, so the plan says what *this* host will do.
fn landlock_line(host: &zygo_core::doctor::Report) -> (bool, String) {
    match host.checks.iter().find(|c| c.name == "landlock") {
        Some(c) if c.status == zygo_core::doctor::Status::Ok => (true, c.detail.clone()),
        Some(c) => (false, format!("{}; seccomp still applies", c.detail)),
        None => (false, "not probed on this host".to_string()),
    }
}

fn print_plan(
    cli: &Cli,
    resolved: &zygo_core::spec::ResolvedFn,
    seccomp_source: zygo_core::spec::SeccompSource,
    host: &zygo_core::doctor::Report,
    view: &RootfsView,
    config: &SandboxConfig,
) -> anyhow::Result<u8> {
    let (landlock_on, landlock) = landlock_line(host);
    if cli.json {
        let mut value = super::spec::to_json(resolved);
        // The profile, where it came from, and its size — so two plans can
        // be diffed and a reviewer can see that `--seccomp` took. The first
        // adoption report (Z-3) read the flag as dead because nothing here
        // said otherwise.
        value["seccomp"]["source"] = serde_json::json!(seccomp_source);
        value["seccomp"]["allowed_syscalls"] =
            serde_json::json!(allowed_syscalls(resolved.seccomp));
        value["landlock"] = serde_json::json!({
            "enabled": landlock_on,
            "detail": landlock,
        });
        value["argv"] = serde_json::json!(config.argv);
        value["rootfs"] = match view {
            RootfsView::Overlay { lower } => serde_json::json!({
                "kind": "overlay",
                "lower": lower.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
            }),
            RootfsView::Flat { dir } => serde_json::json!({
                "kind": "flat",
                "dir": dir.display().to_string(),
            }),
        };
        value["mounts_plan"] = serde_json::json!(config.mounts.describe());
        value["cgroup"] = serde_json::json!(
            resolved
                .limits
                .cgroup_writes()
                .iter()
                .map(|w| format!("{} = {}", w.file, w.value))
                .collect::<Vec<_>>()
        );
        output::json(&value)?;
        return Ok(0);
    }

    let style = Style::stdout();
    println!("{}", style.bold("argv"));
    println!("  {}", config.argv.join(" "));

    println!("{}", style.bold("rootfs"));
    match view {
        RootfsView::Overlay { lower } => {
            println!("  overlay, read-only, {} layers", lower.len());
            for dir in lower {
                println!("    {}", style.dim(&dir.display().to_string()));
            }
        }
        RootfsView::Flat { dir } => println!("  flattened: {}", dir.display()),
    }

    println!("{}", style.bold("mounts"));
    for line in config.mounts.describe() {
        println!("  {line}");
    }

    println!("{}", style.bold("network"));
    println!("  {}", resolved.network);
    for rule in &resolved.allow {
        println!("  {} {rule}", style.dim("allow"));
    }

    println!("{}", style.bold("seccomp"));
    println!(
        "  {} {}{}",
        resolved.seccomp,
        style.dim(&format!("(from {seccomp_source})")),
        match allowed_syscalls(resolved.seccomp) {
            Some(n) => format!(", {n} syscalls allowed"),
            None => String::new(),
        }
    );

    println!("{}", style.bold("landlock"));
    println!("  {landlock}");

    println!("{}", style.bold("cgroup"));
    for w in resolved.limits.cgroup_writes() {
        println!("  {} = {}", w.file, w.value);
    }
    println!(
        "  {} {} ({})",
        style.dim("timeout"),
        resolved.limits.timeout,
        style.dim("supervisor timer + cgroup.kill")
    );

    println!("{}", style.bold("writable paths"));
    for p in config.mounts.writable_targets() {
        println!("  {}", p.display());
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::{Outcome, Phase, mount_pair};

    /// A caller reading the file can tell "never started" from "ran and
    /// failed" by one boolean, and which phase failed by one word.
    #[test]
    fn an_outcome_that_never_started_says_so_and_where() {
        let never = serde_json::to_value(Outcome::never_started(Phase::Start, 125)).unwrap();
        assert_eq!(never["started"], false);
        assert_eq!(never["phase"], "start");
        assert_eq!(never["exit_code"], 125);
        assert_eq!(never["timed_out"], false);
        assert_eq!(serde_json::to_value(Phase::Plan).unwrap(), "plan");
        assert_eq!(serde_json::to_value(Phase::Run).unwrap(), "run");
    }

    /// The shapes a Docker user actually types, and the references they must
    /// not be confused with.
    #[test]
    fn a_volume_pair_is_told_apart_from_an_image_reference() {
        for s in [
            "/Users/m/proj:/src",
            "/Users/m/proj:/src:ro",
            ".:/app",
            "./data:/data",
            "~/proj:/src",
            "sub/dir:/mnt",
        ] {
            assert!(mount_pair(s).is_some(), "`{s}` is a mount");
        }

        for s in [
            "alpine:3",
            "python:3.12-slim",
            "localhost:5000/team/app",
            "localhost:5000/team/app:v2",
            "ghcr.io/o/r@sha256:abc",
            "alpine",
        ] {
            assert!(mount_pair(s).is_none(), "`{s}` is an image");
        }
    }

    #[test]
    fn the_pair_comes_back_split_for_the_suggestion() {
        assert_eq!(mount_pair("/p:/src"), Some(("/p", "/src")));
        // Split on the *first* colon, so a mode suffix stays with the guest
        // and the suggestion round-trips through `--mount`.
        assert_eq!(mount_pair("/p:/src:ro"), Some(("/p", "/src:ro")));
    }
}
