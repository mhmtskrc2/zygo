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

    // `system = [...]` from the spec: the packages installed once, as a layer.
    let entry = if resolved.system.is_empty() {
        entry
    } else {
        let derived = zygo_core::derive::ensure(&store, &entry, &resolved.system)?;
        if derived.built {
            eprintln!(
                "{} {}",
                Style::stdout().dim("installed"),
                derived.versions.join(" ")
            );
        }
        derived.image
    };

    // The network, when this run has one: the allowlist is resolved here on
    // the host, and `/etc/resolv.conf` is bound in rather than written.
    // `--dry-run` wants the mount in the plan it prints, so this comes first.
    let mut resolved = resolved;
    let net = zygo_core::net::setup(store.paths(), &resolved.name, &resolved)?;
    if let Some(mount) = net.mount.clone() {
        resolved.mounts.push(mount);
    }
    for w in &net.warnings {
        output::warn(w);
    }
    let resolved = resolved;

    // Overlay needs a kernel that permits it inside a user namespace; the store
    // falls back to a flattened rootfs when it does not.
    //
    // `gvisor` always takes the flattened one: an OCI bundle's `root.path` is a
    // single directory, so there is nowhere for a stack of lowerdirs to go.
    // gVisor's Sentry keeps an overlay of its own above it in any case.
    let overlay_supported = resolved.isolation != zygo_core::spec::Isolation::Gvisor
        && zygo_core::doctor::run()
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
        SandboxConfig::from_resolved(&resolved, &view, newroot, argv, &image_config.env_pairs());
    config.allow_resolved = net.allowed;
    config.pasta_pid_file = net.pid_file;

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

    Ok(code.clamp(0, 255) as u8)
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

#[cfg(test)]
mod tests {
    use super::mount_pair;

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
