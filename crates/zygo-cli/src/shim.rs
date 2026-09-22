//! Shim mode: on macOS, the sandboxes live in a Linux VM.
//!
//! Every boundary Zygo builds — user namespaces, cgroups, seccomp, Landlock —
//! is a Linux kernel feature. macOS has none of them and never will, so there
//! is no native port to write. Docker faced the same wall and answered it the
//! same way: the `docker` command on a Mac is a client, and the daemon it
//! talks to runs in a Linux VM the user is not meant to think about.
//!
//! So `zygo` on macOS forwards. The command the user typed is run by a Linux
//! `zygo` inside a VM, with the same arguments, the same working directory and
//! the same standard streams, and its exit status comes back out. What makes
//! that workable rather than a nuisance is the path rule: the VM mounts the
//! user's home directory at *the same path* it has on the host, so
//! `./handler.py` means the same file on both sides and nothing has to be
//! rewritten.
//!
//! That rule is also the limit, and it is enforced rather than hoped for: a
//! command run from outside `$HOME` is refused with the reason, because
//! forwarding it would silently run against a directory that is not the one
//! the user is looking at.
//!
//! The provider is [`Lima`], driven through `limactl`. The design allows for a
//! Virtualization.framework helper instead; Lima is what a `brew install` can
//! rely on today, it gives virtiofs mounts
//! and a working init without a signed helper binary, and everything above
//! this line is provider-independent.
//!
//! Only [`forward`] is macOS-only. The decisions — what stays here, what a
//! path maps to, what `limactl` is asked — are plain functions compiled and
//! tested everywhere, so the rules are checked on every host rather than on
//! the one machine that can run them.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use zygo_core::spec::Mount;

use crate::cli::{AgentCommand, Cli, Command};

/// The Lima instance Zygo keeps for itself.
///
/// Named, not shared: a user's other Lima VMs are theirs, and a `zygo`
/// command must never be the reason one of them restarts.
pub const INSTANCE: &str = "zygo";

/// Whether this command can be answered without a Linux kernel.
///
/// The list is short and everything else forwards, which is the safe way
/// round: a command wrongly forwarded still works, and a command wrongly kept
/// here fails on macOS in whatever way its first Linux-only call happens to
/// fail — which is how `zygo run` used to end, several layers down, in "warm
/// exec enters Linux namespaces".
///
/// * `completion` prints a script and reads nothing.
/// * `agent test` checks an agent *you are writing, here*, against the
///   protocol. Forwarding it would test the VM's interpreter instead of the
///   one on your `PATH`, which is the opposite of what was asked.
/// * `doctor` answers about the host it runs on. On macOS the useful answer
///   is about both, so it runs here and reports the VM as one of its checks.
pub fn runs_on_the_host(command: &Command) -> bool {
    matches!(
        command,
        Command::Completion { .. }
            | Command::Doctor { .. }
            | Command::Agent(AgentCommand::Test { .. })
    )
}

/// Why a directory cannot be forwarded.
#[derive(Debug, PartialEq, Eq)]
pub struct Unmapped {
    pub cwd: PathBuf,
    pub home: PathBuf,
}

impl std::fmt::Display for Unmapped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "on macOS Zygo runs in a Linux VM, and only {} is shared with it — \
             but this command was run from {}",
            self.home.display(),
            self.cwd.display()
        )
    }
}

/// The directory the forwarded command should run in.
///
/// Identity or refusal, never a guess. The VM mounts `$HOME` at its own path,
/// so anything underneath needs no translation at all; anything outside it
/// has no counterpart, and inventing one — running in the VM's `/tmp`, say —
/// would mean the command silently read different files from the ones in
/// front of the user.
pub fn workdir(cwd: &Path, home: &Path) -> Result<PathBuf, Unmapped> {
    if cwd.starts_with(home) {
        Ok(cwd.to_path_buf())
    } else {
        Err(Unmapped {
            cwd: cwd.to_path_buf(),
            home: home.to_path_buf(),
        })
    }
}

/// Mount sources this VM has no counterpart for.
///
/// The same rule as [`workdir`], applied to the other half of what a command
/// brings with it. `workdir` has always refused a command run from outside
/// `$HOME`, on the stated grounds that forwarding it would silently run
/// against a directory that is not the one the user is looking at — and a
/// `--mount` is exactly that, with the silence replaced by a worse noise:
/// the guest reports `applying a bind mount from the spec failed: No such
/// file or directory` about a path that plainly exists on the host, and
/// points at `zygo doctor`, which has nothing to say about it.
///
/// Found by running an embedder's script driver against Zygo: it puts its
/// scratch directory in the system temporary directory, which on macOS is
/// `/var/folders/…`, and every one of its tests failed on this line.
///
/// Relative paths are resolved against `cwd` first, because they mean a
/// place on this side and the VM never sees them as written.
pub fn unmapped_mounts<'a>(
    mounts: impl IntoIterator<Item = &'a Mount>,
    cwd: &Path,
    home: &Path,
) -> Vec<PathBuf> {
    mounts
        .into_iter()
        .map(|m| {
            if m.source.is_absolute() {
                m.source.clone()
            } else {
                cwd.join(&m.source)
            }
        })
        .filter(|source| !source.starts_with(home))
        .collect()
}

/// Every mount the command carries, whichever command it is.
pub fn mounts_of(command: &Command) -> &[Mount] {
    match command {
        Command::Run(args) => &args.sandbox.mounts,
        Command::Serve(args) => &args.sandbox.mounts,
        _ => &[],
    }
}

/// What `limactl list` said about the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vm {
    Running,
    Stopped,
    /// `limactl` has never heard of it.
    Absent,
}

impl Vm {
    /// Read `limactl list --format '{{.Status}}' zygo`.
    ///
    /// An unknown instance is an error on stderr and an empty list on stdout,
    /// depending on the version, so both are read as [`Vm::Absent`] rather
    /// than trusting one of them.
    pub fn parse(stdout: &str) -> Vm {
        match stdout.trim().lines().next().map(str::trim) {
            Some("Running") => Vm::Running,
            Some("Stopped") | Some("Broken") => Vm::Stopped,
            _ => Vm::Absent,
        }
    }
}

/// `limactl list --format {{.Status}} zygo`.
pub fn status_args() -> Vec<OsString> {
    ["list", "--format", "{{.Status}}", INSTANCE]
        .iter()
        .map(OsString::from)
        .collect()
}

/// Where the guest keeps the Linux binary every forwarded command runs.
pub const GUEST_BIN: &str = "/usr/local/bin/zygo";

/// Where it is staged before `install` puts it in place.
///
/// Staged rather than written straight to [`GUEST_BIN`], because that file is
/// what the running supervisor was started from: overwriting it in place
/// would change an executable with processes still mapped to it.
const GUEST_STAGE: &str = "/tmp/zygo.incoming";

/// `limactl copy <host file> zygo:/tmp/zygo.incoming`.
pub fn copy_args(source: &Path) -> Vec<OsString> {
    vec![
        "copy".into(),
        source.into(),
        format!("{INSTANCE}:{GUEST_STAGE}").into(),
    ]
}

/// `limactl shell zygo sudo install -m 0755 /tmp/zygo.incoming /usr/local/bin/zygo`.
pub fn install_args() -> Vec<OsString> {
    [
        "shell",
        INSTANCE,
        "sudo",
        "install",
        "-m",
        "0755",
        GUEST_STAGE,
        GUEST_BIN,
    ]
    .iter()
    .map(OsString::from)
    .collect()
}

/// What identifies a build, for deciding whether the guest's copy is current.
///
/// Length and modification time, not a hash: this runs before *every*
/// forwarded command, and reading six megabytes to discover that nothing
/// changed would cost more than the command. The two together move whenever
/// the binary is rebuilt or reinstalled, which is the only case that matters.
///
/// `instance` is what makes the stamp about *this* VM. The stamp lives on the
/// host, so `limactl delete zygo` used to leave it behind: the VM was gone,
/// its copy of the binary with it, and the next command read the stale stamp,
/// concluded the binary was installed and failed inside the new VM with
/// "zygo: not found" (B-27). Mixing in something that changes when the
/// instance is recreated makes a deleted VM look exactly like a changed
/// binary, which is a case this already handles.
pub fn build_stamp(len: u64, modified_secs: u64, instance: u64) -> String {
    format!("{len}-{modified_secs}-{instance}")
}

/// A number that changes when the Lima instance is recreated.
///
/// Its configuration file's modification time: Lima writes one when the
/// instance is created, so a `delete` and a fresh `start` give a different
/// value. Zero when it cannot be read — which is the safe direction, because
/// it will not match a stamp written when it could, and the binary is
/// reinstalled.
#[cfg(target_os = "macos")]
fn instance_generation() -> u64 {
    let Some(home) = std::env::var_os("HOME") else {
        return 0;
    };
    let config = PathBuf::from(home)
        .join(".lima")
        .join(INSTANCE)
        .join("lima.yaml");
    std::fs::metadata(config)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `limactl start --tty=false zygo`, or the same with a template the first
/// time, when there is no instance to start yet.
///
/// `--tty=false` because the first run is otherwise interactive: `limactl`
/// offers to edit the template, and a prompt nobody is watching is a hang.
pub fn start_args(vm: Vm, template: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["start".into(), "--tty=false".into()];
    if vm == Vm::Absent {
        args.push("--name".into());
        args.push(INSTANCE.into());
        args.push(template.into());
    } else {
        args.push(INSTANCE.into());
    }
    args
}

/// `limactl shell --workdir <dir> zygo zygo <args…>`.
///
/// The inner `zygo` is the Linux build inside the VM. `--workdir` is what
/// makes a relative path in `args` mean the same file on both sides; without
/// it `limactl` lands in the user's home directory and `serve handler.py`
/// resolves somewhere else entirely.
#[cfg_attr(not(test), allow(dead_code))]
pub fn forward_args<I, S>(workdir: &Path, args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    forward_args_with_env(workdir, args, &[])
}

/// The same, with variables to set inside the VM.
///
/// `limactl shell` runs the command with the *guest's* environment, so
/// everything the caller's shell had was dropped at the boundary (B-28):
/// `ZYGO_API_TOKEN` never reached `zygo api`, `ZYGO_LOG=debug` turned nothing
/// on, and a secret named by `secrets = [...]` — which is read from the shell
/// that runs `up` — arrived empty, so the command failed on a Mac and worked
/// on Linux.
///
/// They are passed to `env` inside the VM rather than set on `limactl`, which
/// would only change `limactl`'s own environment. A name is refused rather
/// than quoted if it is not a plain identifier, because it is going through a
/// shell on the other side.
pub fn forward_args_with_env<I, S>(
    workdir: &Path,
    args: I,
    env: &[(OsString, OsString)],
) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut out: Vec<OsString> = vec![
        "shell".into(),
        "--workdir".into(),
        workdir.into(),
        INSTANCE.into(),
    ];
    if !env.is_empty() {
        out.push("env".into());
        for (k, v) in env {
            let mut pair = k.clone();
            pair.push("=");
            pair.push(v);
            out.push(pair);
        }
    }
    out.push("zygo".into());
    out.extend(args.into_iter().map(Into::into));
    out
}

/// Whether a name is safe to hand to `env` on the other side of a shell.
pub fn is_forwardable_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What `zygo doctor` can say about the VM without starting it.
///
/// Deliberately does **not** start anything: `doctor` is what somebody runs
/// when things are already wrong, and a diagnostic that changes the machine
/// it is diagnosing is worse than one that reports less. A stopped VM is
/// reported as stopped.
#[cfg(target_os = "macos")]
pub struct VmReport {
    pub limactl: Option<PathBuf>,
    pub vm: Vm,
    /// The guest's own `zygo doctor`, when there is a guest to ask.
    pub guest: Option<(String, u8)>,
}

#[cfg(target_os = "macos")]
pub fn describe_vm() -> VmReport {
    let Some(limactl) = which("limactl") else {
        return VmReport {
            limactl: None,
            vm: Vm::Absent,
            guest: None,
        };
    };
    let vm = std::process::Command::new(&limactl)
        .args(status_args())
        .output()
        .map(|out| Vm::parse(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or(Vm::Absent);

    let guest = (vm == Vm::Running)
        .then(|| {
            std::process::Command::new(&limactl)
                .args(["shell", INSTANCE, "zygo", "doctor"])
                .output()
                .ok()
                .map(|out| {
                    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                    (text, out.status.code().unwrap_or(1) as u8)
                })
        })
        .flatten();

    VmReport {
        limactl: Some(limactl),
        vm,
        guest,
    }
}

/// Run the command in the VM and return its exit status.
///
/// Returns `Ok(None)` when nothing needed forwarding, so the caller carries
/// on and handles the command itself.
#[cfg(target_os = "macos")]
pub fn forward(cli: &Cli) -> anyhow::Result<Option<u8>> {
    use anyhow::Context;

    if runs_on_the_host(&cli.command) {
        return Ok(None);
    }

    let limactl = which("limactl").ok_or_else(|| {
        anyhow::anyhow!(
            "macOS has no user namespaces, cgroups or seccomp, so Zygo runs its \
             sandboxes in a Linux VM — and `limactl`, which starts it, is not \
             installed\n  → brew install lima"
        )
    })?;

    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set, so there is no shared directory to run in")?;
    let cwd = std::env::current_dir().context("this process has no working directory")?;
    let workdir = workdir(&cwd, &home)
        .map_err(|e| anyhow::anyhow!("{e}\n  → run it from somewhere under {}", home.display()))?;

    // The same rule, for what the command brings with it. Refused here so the
    // reader is told about a path on the host they are looking at, rather than
    // by the guest about a path it has never had.
    let unmapped = unmapped_mounts(mounts_of(&cli.command), &cwd, &home);
    if let Some(first) = unmapped.first() {
        anyhow::bail!(
            "on macOS Zygo runs in a Linux VM, and only {} is shared with it — \
             but this command mounts {}{}\n  \
             → move it under {}, or set TMPDIR there if it is a temporary directory",
            home.display(),
            first.display(),
            match unmapped.len() {
                1 => String::new(),
                n => format!(" (and {} other{})", n - 1, if n == 2 { "" } else { "s" }),
            },
            home.display()
        );
    }

    let paths = crate::cmd::paths(cli);
    paths.ensure()?;
    // Before anything is started: a stop has nothing to do here.
    if needs_nothing_when_the_vm_is_down(&cli.command) && !is_running(&limactl)? {
        if cli.json {
            crate::output::json(&serde_json::json!({ "stopped": [] }))?;
        } else {
            println!("nothing is running; the Linux VM is stopped");
        }
        return Ok(Some(0));
    }

    ensure_running(&limactl, &paths)?;
    ensure_guest_binary(&limactl, &paths)?;

    let env = environment_to_forward(cli);
    let mut child = std::process::Command::new(&limactl)
        .args(forward_args_with_env(
            &workdir,
            std::env::args_os().skip(1),
            &env,
        ))
        .spawn()
        .with_context(|| format!("could not run {}", limactl.display()))?;

    // Relay the signals the user can send to *this* process.
    //
    // Ctrl-C reaches `limactl` anyway, because it goes to the whole foreground
    // process group. A `kill` by pid does not: it arrives here, this process
    // ends, and `limactl` — and the sandbox behind it — carries on with
    // nobody left to wait for it (B-28). So the pid is published and the two
    // signals a person actually sends are forwarded.
    FORWARDED_CHILD.store(child.id() as i32, std::sync::atomic::Ordering::SeqCst);
    install_signal_relay();

    let status = child
        .wait()
        .with_context(|| format!("could not wait for {}", limactl.display()))?;
    FORWARDED_CHILD.store(0, std::sync::atomic::Ordering::SeqCst);

    // "Stop everything" includes the machine Zygo started to do it in. A VM
    // holding no warm functions is four gigabytes of a laptop doing nothing,
    // and the user did not ask for a VM in the first place — they asked for a
    // sandbox, and they have just said they are finished with it.
    //
    // Only on success, and only for `--all`: `stop <name>` leaves the others
    // running and so leaves the VM, and stopping it after a failed teardown
    // would hide whatever did not stop.
    if status.success() && stops_everything(&cli.command) {
        stop_vm(&limactl);
    }

    // A signal is not an exit code. `limactl` relays the inner status where
    // it can; where the child died on a signal there is none to relay, and
    // 128 + n is what every shell reports.
    Ok(Some(exit_status(&status)))
}

/// The `limactl` child's pid while one is running, for the signal relay.
///
/// An `AtomicI32` because a signal handler may only touch things that are
/// async-signal-safe, and a lock is not one of them.
#[cfg(target_os = "macos")]
static FORWARDED_CHILD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(target_os = "macos")]
extern "C" fn relay_signal(signum: libc::c_int) {
    let pid = FORWARDED_CHILD.load(std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: `kill` is async-signal-safe; `pid` is a child of ours or
        // already gone, in which case this fails harmlessly.
        unsafe { libc::kill(pid, signum) };
    }
}

#[cfg(target_os = "macos")]
fn install_signal_relay() {
    for signum in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        let handler: extern "C" fn(libc::c_int) = relay_signal;
        // SAFETY: installing a handler that only calls `kill` and reads an
        // atomic, both of which are safe from a signal context.
        unsafe { libc::signal(signum, handler as *const () as libc::sighandler_t) };
    }
}

/// Which of this shell's variables the command inside the VM should see.
///
/// Everything would be wrong — `PATH`, `HOME` and `SHELL` describe the Mac and
/// would mislead the guest. What is forwarded is what Zygo itself reads, plus
/// the secrets the command is about to look for:
///
/// * every `ZYGO_*` variable: the API token, the log level, the data root;
/// * for `serve`, the names given with `--secret`;
/// * for `up` and `serve`, the names a spec's `secrets = [...]` lists, because
///   those are read from the shell that runs the command and that shell is on
///   this side of the boundary.
///
/// A secret's *value* crosses as an argument to `env` inside the VM, which is
/// visible in the guest's process list for the length of the command. That is
/// the same exposure `zygo up` has on Linux, where the value is in this
/// process's environment, and it is why secrets reach the sandbox as files
/// rather than as environment variables once they are across.
#[cfg(target_os = "macos")]
fn environment_to_forward(cli: &Cli) -> Vec<(OsString, OsString)> {
    let mut wanted: Vec<String> = Vec::new();

    for (key, _) in std::env::vars_os() {
        if let Some(name) = key.to_str()
            && name.starts_with("ZYGO_")
        {
            wanted.push(name.to_string());
        }
    }

    match &cli.command {
        crate::cli::Command::Serve(args) => wanted.extend(args.secrets.iter().cloned()),
        crate::cli::Command::Up { .. } => {}
        _ => {}
    }
    if matches!(
        cli.command,
        crate::cli::Command::Up { .. } | crate::cli::Command::Serve(_)
    ) && let Ok(Some(spec)) = zygo_core::spec::Spec::discover(None)
    {
        for f in spec.functions.values() {
            if let Some(names) = &f.secrets {
                wanted.extend(names.iter().cloned());
            }
        }
        if let Some(names) = &spec.defaults.secrets {
            wanted.extend(names.iter().cloned());
        }
    }

    wanted.sort();
    wanted.dedup();
    wanted
        .into_iter()
        .filter(|name| is_forwardable_env_name(name))
        .filter_map(|name| std::env::var_os(&name).map(|value| (OsString::from(name), value)))
        .collect()
}

/// Whether this command has nothing left to do once the VM is already down.
///
/// Stopping is the one job a stopped machine has finished. Without this,
/// `zygo stop --all` on a stopped VM took the ordinary forwarding path:
/// `ensure_running` booted the machine — sixteen seconds, or fifty-five from
/// nothing — so that a supervisor which does not exist could be told to stop
/// functions that do not exist, and then [`stops_everything`] stopped it
/// again. The narration gave it away, announcing a start from a command whose
/// entire purpose is the opposite.
///
/// `down` and a named `stop` are here for the same reason: they cannot find
/// anything to stop in a machine that is not running.
pub fn needs_nothing_when_the_vm_is_down(command: &Command) -> bool {
    matches!(command, Command::Stop { .. } | Command::Down { .. })
}

/// Whether this command means the user is finished with the VM.
///
/// Not an idle timer: that needs something running to notice the idleness, and
/// the only way to have one on macOS is a launchd agent, which Zygo does not
/// install. `stop --all` is the moment the user says so out loud, and acting
/// on it costs them nothing they did not ask for — the next command starts the
/// VM again, as the first one did.
pub fn stops_everything(command: &Command) -> bool {
    matches!(command, Command::Stop { all: true, .. })
}

/// `limactl stop zygo`, best effort and quietly.
///
/// Best effort on purpose: the command the user typed has already succeeded
/// and its exit status is theirs. A VM that will not stop is worth a line in
/// the log, not a failure on a teardown that worked.
///
/// Quiet for the same reason. `limactl stop` writes about thirty lines of its
/// own progress to stderr, one of them at `level=error` on a shutdown that
/// worked, and all of it arrives after the user's command has printed its
/// answer — so the last thing on screen is an error from a VM manager they
/// never asked about. The output is captured and kept for `-v`, where someone
/// debugging the shim wants precisely those lines.
#[cfg(target_os = "macos")]
fn stop_vm(limactl: &Path) {
    match std::process::Command::new(limactl)
        .args(["stop", INSTANCE])
        .output()
    {
        Ok(out) if out.status.success() => {
            tracing::debug!(
                lima = %String::from_utf8_lossy(&out.stderr).trim(),
                "the Linux VM is stopped; the next command starts it again"
            );
            eprintln!("the Linux VM is stopped; the next command starts it again");
        }
        // A failure is the one case where Lima's own words are worth showing:
        // they say what is still holding the VM open, which nothing here can.
        Ok(out) => tracing::warn!(
            lima = %String::from_utf8_lossy(&out.stderr).trim(),
            "the Linux VM did not stop ({})",
            out.status
        ),
        Err(e) => tracing::warn!("could not stop the Linux VM: {e}"),
    }
}

#[cfg(not(target_os = "macos"))]
pub fn forward(_cli: &Cli) -> anyhow::Result<Option<u8>> {
    Ok(None)
}

/// Put this release's Linux binary in the VM, if it is not there already.
///
/// A `brew upgrade` replaces the binary on the host; without this the VM
/// keeps answering with the old one, and the two disagree in whatever way
/// that release changed — a protocol version, a spec field, an exit code.
/// The check has to be nearly free, because it runs before every forwarded
/// command, so it compares a stamp kept on the host rather than asking the
/// guest.
#[cfg(target_os = "macos")]
fn ensure_guest_binary(limactl: &Path, paths: &zygo_core::paths::Paths) -> anyhow::Result<()> {
    let Some(source) = linux_binary() else {
        // Nothing to install. The guest may already have one — from an
        // earlier release, or put there by hand — so ask before refusing.
        // The round trip is affordable because this path is rare: an install
        // ships the Linux build beside the binary, and a checkout has it
        // after one `make`.
        if guest_has_zygo(limactl) {
            return Ok(());
        }
        anyhow::bail!(
            "the VM has no Linux build of Zygo to run, and there is none on \
             this Mac to put there\n  \
             → in a checkout: make poc/zygo-linux-musl\n  \
             → otherwise: set ZYGO_LINUX_BIN to a Linux `zygo` for this \
             machine's architecture"
        );
    };
    let Ok(meta) = std::fs::metadata(&source) else {
        return Ok(());
    };
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let want = build_stamp(meta.len(), modified, instance_generation());

    let stamp = stamp_path();
    if std::fs::read_to_string(&stamp).is_ok_and(|have| have.trim() == want) {
        return Ok(());
    }

    // Two commands starting together would otherwise both copy to the same
    // staging path and both `install` from it — one writing the file while
    // the other reads it. Serialised, and the stamp is read again inside, so
    // the second one finds the work already done.
    let _guard = paths.lock(GUEST_BIN_LOCK)?;
    if std::fs::read_to_string(&stamp).is_ok_and(|have| have.trim() == want) {
        return Ok(());
    }

    tracing::info!(binary = %source.display(), "installing this release's Linux binary in the VM");
    for args in [copy_args(&source), install_args()] {
        let status = std::process::Command::new(limactl)
            .args(args)
            .status()
            .map_err(|e| anyhow::anyhow!("could not run {}: {e}", limactl.display()))?;
        if !status.success() {
            anyhow::bail!(
                "could not put the Linux build of Zygo into the VM ({status})\n                   → check it by hand: limactl shell {INSTANCE} -- zygo --version"
            );
        }
    }

    if let Some(dir) = stamp.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&stamp, &want);
    Ok(())
}

/// Whether the VM already has something to run.
///
/// Asked only when there is nothing on this side to install, so the answer
/// decides between carrying on with whatever is in there and refusing with a
/// sentence that names what to build. Without it the forwarded command comes
/// back as exit 127 and the words `zygo: not found`, which reads like the
/// user's `PATH` is wrong.
#[cfg(target_os = "macos")]
fn guest_has_zygo(limactl: &Path) -> bool {
    std::process::Command::new(limactl)
        .args(["shell", INSTANCE, "test", "-x", GUEST_BIN])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The Linux build that belongs in the VM.
///
/// Three places, in the order that makes a developer's checkout win over an
/// installed copy: an explicit override, the layout a package manager
/// installs, and this repository's own cross-build.
#[cfg(target_os = "macos")]
fn linux_binary() -> Option<PathBuf> {
    let named = std::env::var_os("ZYGO_LINUX_BIN").map(PathBuf::from);
    let installed = std::env::current_exe().ok().and_then(|exe| {
        exe.parent().map(|dir| {
            dir.join(format!(
                "../share/zygo/zygo-linux-{}",
                std::env::consts::ARCH
            ))
        })
    });
    let checkout = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../poc/zygo-linux-musl"
    ));
    [named, installed, Some(checkout)]
        .into_iter()
        .flatten()
        .find(|c| c.is_file())
}

#[cfg(target_os = "macos")]
fn stamp_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("zygo").join(format!("vm-{INSTANCE}.stamp"))
}

/// Lock key for "somebody is starting the VM".
///
/// Creating the instance takes about a minute, and for that whole minute the
/// directory exists while the hostagent does not. A second `limactl start`
/// arriving in that window does not queue behind the first: it finds the
/// half-made instance, fails to connect to a `ha.sock` that has not been
/// created yet, and exits `fatal` — leaving the user with an error about a
/// socket for a command that was `zygo run python -c print`. Typing two
/// commands in the first minute was enough to see it.
pub const VM_START_LOCK: &str = "vm-start";

/// Lock key for "somebody is putting the Linux binary into the VM".
///
/// Separate from [`VM_START_LOCK`] because it is a different job with a
/// different fast path: the stamp check costs one `stat` and is the answer
/// almost every time, so the lock is only reached on the command after a
/// rebuild.
pub const GUEST_BIN_LOCK: &str = "vm-guest-binary";

/// Start the instance if it is not already up.
///
/// Serialised across processes, because `limactl start` on one instance is
/// not safe to run twice at once. The status is read again after the lock is
/// held: the common case for the second command is that the first one has by
/// then finished starting the VM, and there is nothing left to do.
#[cfg(target_os = "macos")]
fn ensure_running(limactl: &Path, paths: &zygo_core::paths::Paths) -> anyhow::Result<()> {
    if is_running(limactl)? {
        return Ok(());
    }

    // Only to decide whether to say something: the wait below is up to a
    // minute, and a command that appears to have hung is worse than a slow
    // one that says what it is waiting for.
    if paths.is_locked(VM_START_LOCK) {
        eprintln!("waiting for the Linux VM another zygo command is starting");
    }
    let _guard = paths.lock(VM_START_LOCK)?;

    // Held now. Whoever we waited for has finished, and usually finished the
    // job — so ask again rather than starting a VM that is already up.
    let out = std::process::Command::new(limactl)
        .args(status_args())
        .output()
        .map_err(|e| anyhow::anyhow!("could not ask limactl about the VM: {e}"))?;
    let vm = Vm::parse(&String::from_utf8_lossy(&out.stdout));
    if vm == Vm::Running {
        return Ok(());
    }

    let template = template_path()?;

    // One line of ours, and `limactl`'s thirty kept back unless they are
    // needed. Booting a virtual machine has a lot to say — a hostagent
    // socket, an ssh port, six forwarding decisions, three requirements
    // satisfied — and none of it was asked for by somebody who typed `zygo
    // run python -c print`. The whole point of this mode is that the VM is
    // not the user's business.
    //
    // Which does not mean silence: a machine that takes a minute to appear
    // should say it is doing something, and a failure has to arrive with
    // everything `limactl` said about it.
    let first_time = vm == Vm::Absent;
    eprintln!(
        "starting the Linux VM Zygo runs its sandboxes in{}",
        if first_time {
            " (the first time takes about a minute)"
        } else {
            ""
        }
    );
    let out = std::process::Command::new(limactl)
        .args(start_args(vm, &template))
        .output()
        .map_err(|e| anyhow::anyhow!("could not start the VM: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "the Linux VM would not start (limactl exited {})\n{}\n  \
             → see it for yourself with `limactl start {INSTANCE}`\n  \
             → an instance left half-created reports a missing `ha.sock`; \
             `limactl delete {INSTANCE}` discards it and the next command \
             builds a fresh one",
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .map(|l| format!("  {l}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    Ok(())
}

/// Whether `limactl` says the instance is up right now.
#[cfg(target_os = "macos")]
fn is_running(limactl: &Path) -> anyhow::Result<bool> {
    let out = std::process::Command::new(limactl)
        .args(status_args())
        .output()
        .map_err(|e| anyhow::anyhow!("could not ask limactl about the VM: {e}"))?;
    Ok(Vm::parse(&String::from_utf8_lossy(&out.stdout)) == Vm::Running)
}

/// Where the VM's template lives.
///
/// Beside the binary when installed, and in the checkout when developing, so
/// `cargo run` on a Mac behaves the same way a `brew install` does.
#[cfg(target_os = "macos")]
fn template_path() -> anyhow::Result<PathBuf> {
    let named = std::env::var_os("ZYGO_LIMA_TEMPLATE").map(PathBuf::from);
    let candidates = named
        .into_iter()
        .chain(
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.join("../share/zygo/lima.yaml"))),
        )
        .chain(std::iter::once(PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../shim/lima.yaml"
        ))));
    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    anyhow::bail!(
        "the VM template is missing; Zygo cannot create the Linux VM without it\n  \
         → set ZYGO_LIMA_TEMPLATE to a Lima template, or reinstall Zygo"
    )
}

#[cfg(target_os = "macos")]
fn exit_status(status: &std::process::ExitStatus) -> u8 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => code as u8,
        (None, Some(signal)) => 128u8.saturating_add(signal as u8),
        (None, None) => 1,
    }
}

#[cfg(target_os = "macos")]
fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn command_of(args: &[&str]) -> Command {
        Cli::try_parse_from(args).expect("parse").command
    }

    #[test]
    fn the_commands_that_need_no_kernel_stay_here() {
        for args in [
            &["zygo", "completion", "bash"][..],
            &["zygo", "doctor"][..],
            &["zygo", "agent", "test", "python3", "--", "agent.py"][..],
        ] {
            assert!(
                runs_on_the_host(&command_of(args)),
                "{args:?} should not need the VM"
            );
        }
    }

    #[test]
    fn everything_that_builds_a_sandbox_is_forwarded() {
        for args in [
            &["zygo", "run", "python:3.12", "true"][..],
            &["zygo", "serve", "handler.py", "--name", "f"][..],
            &["zygo", "exec", "f", "{}"][..],
            &["zygo", "ps"][..],
            &["zygo", "up"][..],
            &["zygo", "pull", "python:3.12"][..],
            &["zygo", "images"][..],
            &["zygo", "shell", "f"][..],
        ] {
            assert!(
                !runs_on_the_host(&command_of(args)),
                "{args:?} needs a Linux kernel and must be forwarded"
            );
        }
    }

    #[test]
    fn a_directory_under_home_keeps_its_path() {
        let home = Path::new("/Users/m");
        assert_eq!(
            workdir(Path::new("/Users/m/work/fn"), home).unwrap(),
            PathBuf::from("/Users/m/work/fn")
        );
        assert_eq!(workdir(home, home).unwrap(), PathBuf::from("/Users/m"));
    }

    #[test]
    fn a_directory_outside_home_is_refused_and_says_both_paths() {
        let home = Path::new("/Users/m");
        let err = workdir(Path::new("/tmp/scratch"), home).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("/tmp/scratch"), "{text}");
        assert!(text.contains("/Users/m"), "{text}");
    }

    // A prefix match on strings would accept this; on paths it does not.
    #[test]
    fn a_sibling_directory_that_merely_starts_with_home_is_not_home() {
        let home = Path::new("/Users/m");
        assert!(workdir(Path::new("/Users/martin/work"), home).is_err());
    }

    #[test]
    fn the_first_start_names_the_instance_and_the_later_ones_do_not() {
        let template = Path::new("/opt/zygo/lima.yaml");
        let first = start_args(Vm::Absent, template);
        assert!(first.iter().any(|a| a == "--name"));
        assert!(first.iter().any(|a| a == template));

        let later = start_args(Vm::Stopped, template);
        assert!(!later.iter().any(|a| a == "--name"));
        assert!(!later.iter().any(|a| a == template));
        assert!(later.iter().any(|a| a == INSTANCE));
    }

    #[test]
    fn a_start_never_waits_for_an_answer_nobody_is_there_to_give() {
        for vm in [Vm::Absent, Vm::Stopped] {
            assert!(
                start_args(vm, Path::new("t.yaml"))
                    .iter()
                    .any(|a| a == "--tty=false"),
                "{vm:?} would prompt"
            );
        }
    }

    #[test]
    fn the_forwarded_command_carries_the_working_directory() {
        let args = forward_args(Path::new("/Users/m/fn"), ["serve", "handler.py"]);
        let words: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            words,
            [
                "shell",
                "--workdir",
                "/Users/m/fn",
                "zygo",
                "zygo",
                "serve",
                "handler.py"
            ]
        );
    }

    #[test]
    fn the_binary_is_staged_before_it_replaces_the_one_in_use() {
        let copy: Vec<String> = copy_args(Path::new("/opt/zygo/zygo-linux-aarch64"))
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(copy[0], "copy");
        assert_eq!(copy[1], "/opt/zygo/zygo-linux-aarch64");
        assert_eq!(copy[2], format!("{INSTANCE}:{GUEST_STAGE}"));

        let install: Vec<String> = install_args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        // The staged copy is the source and the in-use binary is the target,
        // never the other way round: `install` replaces the file rather than
        // writing through it, so a running supervisor keeps the image it
        // started from.
        let stage = install.iter().position(|a| a == GUEST_STAGE).unwrap();
        let target = install.iter().position(|a| a == GUEST_BIN).unwrap();
        assert!(stage < target, "{install:?}");
        assert!(install.contains(&"0755".to_string()));
    }

    /// A stop against a stopped machine must not start one to do it.
    #[test]
    fn stopping_needs_nothing_from_a_stopped_machine() {
        for args in [
            vec!["zygo", "stop", "--all"],
            vec!["zygo", "stop", "f"],
            vec!["zygo", "down"],
        ] {
            assert!(
                needs_nothing_when_the_vm_is_down(&command_of(&args)),
                "{args:?} has nothing to stop in a machine that is not running"
            );
        }

        // Everything else needs the VM, including the ones that only read:
        // the answer lives in there.
        for args in [
            vec!["zygo", "ps"],
            vec!["zygo", "run", "alpine:3", "true"],
            vec!["zygo", "logs", "f"],
            vec!["zygo", "images"],
            vec!["zygo", "up"],
        ] {
            assert!(
                !needs_nothing_when_the_vm_is_down(&command_of(&args)),
                "{args:?} has to reach the VM"
            );
        }
    }

    /// The other half of the path rule. Found by a real consumer: an embedder
    /// mounts a scratch directory from the system temporary directory, which
    /// on macOS is outside `$HOME`, and every one of its tests failed with
    /// the guest complaining about a path the host has.
    #[test]
    fn a_mount_the_vm_cannot_see_is_refused_here() {
        let home = Path::new("/Users/m");
        let cwd = Path::new("/Users/m/projects/app");
        let mount = |s: &str| s.parse::<Mount>().expect("mount");

        // Under `$HOME`: the VM has it at the same path.
        assert!(
            unmapped_mounts(&[mount("/Users/m/data:/data")], cwd, home).is_empty(),
            "an absolute path under home is shared"
        );
        // Relative: resolved against the caller's directory, which is itself
        // already known to be under home.
        assert!(
            unmapped_mounts(&[mount("./cache:/cache:rw")], cwd, home).is_empty(),
            "a relative path means a place on this side"
        );

        // The case that failed: macOS puts temporary directories here.
        assert_eq!(
            unmapped_mounts(&[mount("/var/folders/ab/T/run:/data:rw")], cwd, home),
            [PathBuf::from("/var/folders/ab/T/run")]
        );
        // And every one of them is collected, so the message can count them.
        assert_eq!(
            unmapped_mounts(
                &[
                    mount("/Users/m/ok:/ok"),
                    mount("/tmp/one:/one"),
                    mount("/etc/two:/two"),
                ],
                cwd,
                home
            )
            .len(),
            2
        );
    }

    /// Only the commands that can carry one are asked.
    #[test]
    fn mounts_are_read_from_whichever_command_has_them() {
        let with_mount = command_of(&["zygo", "run", "--mount", "/tmp/x:/x", "alpine:3", "true"]);
        assert_eq!(mounts_of(&with_mount).len(), 1);

        let served = command_of(&[
            "zygo",
            "serve",
            "h.py",
            "--name",
            "f",
            "--mount",
            "/tmp/x:/x:rw",
        ]);
        assert_eq!(mounts_of(&served).len(), 1);

        assert!(mounts_of(&command_of(&["zygo", "ps"])).is_empty());
    }

    #[test]
    fn only_stopping_everything_stops_the_machine_too() {
        assert!(stops_everything(&command_of(&["zygo", "stop", "--all"])));
        // One function stopping leaves the others running, and them the VM.
        assert!(!stops_everything(&command_of(&["zygo", "stop", "f"])));
        assert!(!stops_everything(&command_of(&["zygo", "ps"])));
        assert!(!stops_everything(&command_of(&["zygo", "down"])));
    }

    #[test]
    fn a_rebuild_changes_the_stamp_and_an_unchanged_file_does_not() {
        const VM: u64 = 1_700_000_500;
        assert_eq!(
            build_stamp(6_364_736, 1_700_000_000, VM),
            build_stamp(6_364_736, 1_700_000_000, VM)
        );
        assert_ne!(
            build_stamp(6_364_736, 1_700_000_000, VM),
            build_stamp(6_364_800, 1_700_000_000, VM)
        );
        assert_ne!(
            build_stamp(6_364_736, 1_700_000_000, VM),
            build_stamp(6_364_736, 1_700_000_001, VM)
        );
    }

    /// A VM that was deleted and recreated needs the binary put back, even
    /// though the binary on the host has not moved.
    ///
    /// B-27: the stamp lived on the host and described only the file, so
    /// `limactl delete zygo` left it behind claiming an installation that had
    /// gone with the VM. The next command forwarded into a VM with no `zygo`
    /// in it.
    #[test]
    fn recreating_the_vm_changes_the_stamp_even_when_the_binary_has_not() {
        let same_binary = (6_364_736, 1_700_000_000);
        assert_ne!(
            build_stamp(same_binary.0, same_binary.1, 1_700_000_500),
            build_stamp(same_binary.0, same_binary.1, 1_700_009_999),
            "a recreated VM must not match the stamp written for the old one"
        );
    }

    /// The environment the caller had is carried into the VM.
    ///
    /// B-28: `limactl shell` runs with the *guest's* environment, so a secret
    /// or a token from the user's shell arrived empty and the command failed
    /// on a Mac while working on Linux.
    #[test]
    fn variables_are_carried_into_the_vm_as_an_env_prefix() {
        let env = vec![
            (OsString::from("ZYGO_API_TOKEN"), OsString::from("t0ken")),
            (OsString::from("STRIPE_KEY"), OsString::from("sk_live_x")),
        ];
        let args = forward_args_with_env(Path::new("/Users/m/p"), ["exec", "fetch"], &env);
        let words: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();

        // The *last* `zygo` is the binary inside the VM; the first is the
        // Lima instance, which is also called zygo.
        let env_at = words
            .iter()
            .position(|w| w == "env")
            .expect("an env prefix");
        let binary_at = words
            .iter()
            .rposition(|w| w == "zygo")
            .expect("the inner binary");
        assert!(
            env_at < binary_at,
            "env must come before the command it sets up: {words:?}"
        );
        assert!(words.contains(&"ZYGO_API_TOKEN=t0ken".to_string()));
        assert!(words.contains(&"STRIPE_KEY=sk_live_x".to_string()));
        assert_eq!(words.last().map(String::as_str), Some("fetch"));

        // And with nothing to carry there is no `env` in the way.
        let plain = forward_args_with_env(Path::new("/Users/m/p"), ["ps"], &[]);
        assert!(!plain.iter().any(|a| a == "env"));
    }

    /// A name that is not a plain identifier does not go through a shell.
    #[test]
    fn only_ordinary_variable_names_are_forwarded() {
        for good in ["ZYGO_API_TOKEN", "STRIPE_KEY", "_x", "A1"] {
            assert!(
                is_forwardable_env_name(good),
                "{good} should be forwardable"
            );
        }
        for bad in ["", "1ABC", "A B", "A;rm -rf /", "A=B", "A$X"] {
            assert!(
                !is_forwardable_env_name(bad),
                "{bad:?} should not be forwarded"
            );
        }
    }

    #[test]
    fn limactl_status_words_map_onto_what_to_do_about_them() {
        assert_eq!(Vm::parse("Running\n"), Vm::Running);
        assert_eq!(Vm::parse("Stopped\n"), Vm::Stopped);
        assert_eq!(Vm::parse("Broken\n"), Vm::Stopped);
        // Both shapes an unknown instance takes.
        assert_eq!(Vm::parse(""), Vm::Absent);
        assert_eq!(Vm::parse("\n"), Vm::Absent);
    }
}
