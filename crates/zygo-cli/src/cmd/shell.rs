//! `zygo shell <name>` — a shell inside a warm sandbox, for debugging.
//!
//! The design calls this "a debug fork; it does not touch the zygote"
//! (§4.3), and that is exactly what it is: the sandbox's *namespaces* are
//! entered by a fresh process, so the warm agent keeps its memory, its request
//! counters and its place in the idle policy. Nothing about the function
//! changes because you looked at it.
//!
//! **The client does the entering, not the supervisor.** The supervisor answers
//! `SHELL` with a pid and stops there. Running the shell on its side would mean
//! proxying a terminal over the control socket — a byte stream through a frame
//! protocol that carries nothing else like it — and it would buy nothing: this
//! process runs as the same user, so entering the user namespace Zygo created
//! grants it the same capability set inside that namespace as the supervisor
//! would have had. The terminal stays where it already is.
//!
//! **What still applies, and what does not.** The shell sees the sandbox's
//! filesystem, its pid table, its network and its hostname, because those are
//! properties of the namespaces it just entered — an egress allowlist is
//! nftables rules *inside* that network namespace and is as real for this shell
//! as for a request. It drops every capability and sets `no_new_privs`, so it
//! cannot do more than the sandbox's own code. It deliberately does **not**
//! install the seccomp filter or the Landlock ruleset, and it does **not** join
//! the tenant's cgroup: a debug shell that is killed by the tenant's memory
//! limit, or that cannot run the tool you came to run, is not a debug shell.
//! `zygo shell` says so on the way in.

use std::path::Path;

use zygo_core::supervisor::client::Client;
use zygo_core::supervisor::{Request, Response};

use crate::cli::Cli;
use crate::output::Style;

/// Tried in order. The first that exists in the sandbox wins; an image with
/// none of them gets a clear error rather than `ENOENT` from `execve`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const SHELLS: &[&str] = &["/bin/bash", "/bin/sh", "/busybox/sh"];

/// `zygo shell <name> [-- cmd…]`.
pub fn run(cli: &Cli, name: &str, command: &[String]) -> anyhow::Result<u8> {
    let paths = super::paths(cli);
    let mut client = Client::connect(&paths)?;

    let (pid, workdir) = match client.send(&Request::Shell {
        name: name.to_string(),
    })? {
        Response::Sandbox { pid, workdir, .. } => (pid, workdir),
        other => return super::supervisor::report_failure(cli, &other),
    };

    let style = Style::stdout();
    if command.is_empty() {
        eprintln!(
            "{} {}",
            style.dim(&format!("entering {name} (pid {pid});")),
            style.dim("no seccomp, no Landlock, not in the tenant's cgroup")
        );
    }

    enter(pid, &workdir, command)
}

#[cfg(target_os = "linux")]
fn enter(pid: u32, workdir: &Path, command: &[String]) -> anyhow::Result<u8> {
    use std::os::fd::{AsRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    use anyhow::Context;

    // Opened here rather than in the child: a failure is then an ordinary
    // error with a path in it, instead of something reported from between
    // `fork` and `execve` where nothing may allocate.
    //
    // User namespace first, because it is what grants the capability to enter
    // the rest; the mount namespace last, so everything above is resolved
    // against the host's filesystem while it still is the host's.
    let order = ["user", "pid", "net", "ipc", "uts", "cgroup", "mnt"];
    let mut namespaces: Vec<OwnedFd> = Vec::with_capacity(order.len());
    for kind in order {
        let path = format!("/proc/{pid}/ns/{kind}");
        let file = std::fs::File::open(&path).with_context(|| {
            format!("cannot open {path}; the sandbox may have gone since the supervisor answered")
        })?;
        namespaces.push(OwnedFd::from(file));
    }
    let raw: Vec<i32> = namespaces.iter().map(|fd| fd.as_raw_fd()).collect();

    let argv = if command.is_empty() {
        None
    } else {
        Some(command.to_vec())
    };
    let workdir = workdir.to_path_buf();

    let mut attempts = Vec::new();
    for shell in SHELLS {
        let (program, args): (String, Vec<String>) = match &argv {
            Some(cmd) => (cmd[0].clone(), cmd[1..].to_vec()),
            None => ((*shell).to_string(), vec!["-i".to_string()]),
        };

        let raw = raw.clone();
        let workdir = workdir.clone();
        let mut process = std::process::Command::new(&program);
        process.args(&args);
        // A shell needs to know it has a terminal and which one; everything
        // else the sandbox sets for a request is the function's, not ours.
        process.env(
            "TERM",
            std::env::var("TERM").unwrap_or_else(|_| "xterm".into()),
        );
        process.env("PS1", "zygo:\\w$ ");

        // SAFETY: between `fork` and `execve`, so this process is
        // single-threaded — which `setns` into a user namespace requires.
        // Every call here is async-signal-safe and nothing allocates.
        unsafe {
            process.pre_exec(move || {
                for fd in &raw {
                    if libc::setns(*fd, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                // Inside the sandbox now. A missing workdir is not worth
                // refusing over — `/` always exists.
                let dir = std::ffi::CString::new(workdir.as_os_str().as_encoded_bytes())
                    .unwrap_or_else(|_| c"/".to_owned());
                if libc::chdir(dir.as_ptr()) != 0 {
                    libc::chdir(c"/".as_ptr());
                }
                // No more than the sandbox's own code can do. `no_new_privs`
                // first, so a setuid binary in the image cannot undo it.
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                drop_capabilities();
                Ok(())
            });
        }

        match process.status() {
            Ok(status) => {
                return Ok(status.code().unwrap_or(130).clamp(0, 255) as u8);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && argv.is_none() => {
                attempts.push(*shell);
                continue;
            }
            Err(e) => {
                return Err(e).with_context(|| format!("could not run `{program}` in the sandbox"));
            }
        }
    }

    anyhow::bail!(
        "this image has none of {}\n  → give a command: zygo shell <name> -- <program>",
        attempts.join(", ")
    )
}

/// Empty the bounding, ambient, effective, permitted and inheritable sets.
///
/// The same thing the launcher does to a sandbox's own init. Entering a user
/// namespace hands this process a full capability set inside it; a debug shell
/// has no use for one, and dropping it means the shell can do exactly what the
/// function's own code can and nothing more.
///
/// # Safety
/// Called between `fork` and `execve`. `prctl` and `capset` are
/// async-signal-safe and nothing here allocates.
#[cfg(target_os = "linux")]
unsafe fn drop_capabilities() {
    const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
    const CAP_LAST_CAP_GUESS: libc::c_int = 63;

    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: libc::c_int,
    }
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        );
        for cap in 0..=CAP_LAST_CAP_GUESS {
            // EINVAL simply means this kernel has no such capability.
            libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0);
        }
        let header = CapHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };
        let data = [CapData::default(); 2];
        libc::syscall(libc::SYS_capset, &header, data.as_ptr());
    }
}

#[cfg(not(target_os = "linux"))]
fn enter(_pid: u32, _workdir: &Path, _command: &[String]) -> anyhow::Result<u8> {
    anyhow::bail!(
        "entering a sandbox needs Linux namespaces; this host runs {}",
        std::env::consts::OS
    )
}
