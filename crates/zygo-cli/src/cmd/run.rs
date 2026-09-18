//! `zygo run` — a one-shot sandbox.
//!
//! The launcher itself is phase 1.3; what works today is everything up to it:
//! resolve the spec and flags, make sure the image is in the store, build the
//! rootfs view and the mount plan. `--dry-run` prints exactly that, which is
//! also how the plan gets reviewed without a Linux host.

use std::os::fd::AsRawFd;

use anyhow::Context;
use zygo_core::backend;
use zygo_core::image::{PullProgress, Reference, RegistryClient, Store};
use zygo_core::sandbox::{RootfsView, SandboxConfig};
use zygo_core::spec::Spec;

use crate::cli::{Cli, RunArgs};
use crate::output::{self, Style};
use crate::tty;

pub fn run(cli: &Cli, args: &RunArgs) -> anyhow::Result<u8> {
    let overrides = args.to_layer()?;
    let options = zygo_core::spec::ResolveOptions {
        one_shot: true,
        ..args.sandbox.resolve_options()
    };

    // A spec file is optional for `run`: the flags alone are enough.
    let spec = Spec::discover(args.spec_file.path())?.unwrap_or_default();
    let resolved = spec.resolve(None, &overrides, &options)?;

    for w in &resolved.warnings {
        output::warn(w);
    }

    let paths = super::paths(cli);
    paths.ensure()?;
    let store = Store::new(paths);

    let reference: Reference = resolved
        .image
        .parse()
        .with_context(|| format!("cannot use image `{}`", resolved.image))?;

    let entry = match store.get(&reference) {
        Some(e) => e,
        None => {
            // Same behaviour as `docker run`: pull on first use.
            let client = RegistryClient::new(store.clone())?;
            let style = Style::stdout();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            runtime.block_on(client.pull(&reference, |event| {
                if let PullProgress::Resolving { reference } = event {
                    eprintln!("{} {reference}", style.dim("pulling"));
                }
            }))?
        }
    };

    // Overlay needs a kernel that permits it inside a user namespace; the store
    // falls back to a flattened rootfs when it does not.
    let overlay_supported = zygo_core::doctor::run()
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
        .join(format!("root-{}", std::process::id()));
    std::fs::create_dir_all(&newroot)?;

    // The image's own config supplies the default command and, just as
    // importantly, `PATH` — without which a bare `python3` cannot be resolved.
    let image_config = {
        let client = RegistryClient::new(store.clone())?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(client.image_config(&entry))?
    };

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

    let mut config =
        SandboxConfig::from_resolved(&resolved, &view, &newroot, argv, &image_config.env_pairs());

    if args.dry_run {
        return print_plan(cli, &resolved, &view, &config);
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

    let backend = backend::for_isolation(resolved.isolation)?;
    let mut sandbox = backend.start(&config)?;

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

    let code = sandbox.wait()?;

    if let Some(handle) = relay {
        let _ = handle.join();
    }
    drop(raw_mode);

    let _ = std::fs::remove_dir(&newroot);
    Ok(code.clamp(0, 255) as u8)
}

/// Relay `SIGINT` and `SIGTERM` to the sandbox's init process.
///
/// Signalling pid 1 of a pid namespace is how the whole tree is reached: the
/// kernel tears the namespace down when its init exits.
#[cfg(unix)]
fn forward_signals(pid: u32) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static TARGET: AtomicU32 = AtomicU32::new(0);
    TARGET.store(pid, Ordering::SeqCst);

    extern "C" fn relay(signal: i32) {
        let pid = TARGET.load(Ordering::SeqCst);
        if pid != 0 {
            // SAFETY: `kill` is async-signal-safe, which is the whole
            // constraint on a signal handler.
            unsafe { libc::kill(pid as libc::pid_t, signal) };
        }
    }

    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        // SAFETY: the handler only reads an atomic and calls `kill`.
        unsafe { libc::signal(signal, relay as *const () as libc::sighandler_t) };
    }
}

#[cfg(not(unix))]
fn forward_signals(_pid: u32) {}

fn print_plan(
    cli: &Cli,
    resolved: &zygo_core::spec::ResolvedFn,
    view: &RootfsView,
    config: &SandboxConfig,
) -> anyhow::Result<u8> {
    if cli.json {
        let mut value = super::spec::to_json(resolved);
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
