// SPDX-License-Identifier: Apache-2.0
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
//! The provider is Lima, driven through `limactl`. A Virtualization.framework
//! helper could replace it; Lima is what a `brew install` can
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

/// Exit status for "the Linux VM could not be reached".
///
/// Distinct from every status a forwarded program can produce and from the
/// ones Zygo already means something by: 125 is "this host cannot run
/// sandboxes", 137 a deadline, 255 what SSH exits with *and* what a program
/// may exit with — which is why the first adoption report (Z-2) could not tell
/// "the transport failed before anything started" from "the program exited
/// 255" except by matching on text. 111 is `ECONNREFUSED`'s number on Linux,
/// which is the sentence this status stands for.
pub const EXIT_VM_UNREACHABLE: u8 = 111;

/// How many times a forwarded command is retried when the transport failed
/// before the guest ran anything.
///
/// Two, because that is the number an adopter measured to zero at width 24
/// (0 → ~7 failures in 144, 1 → 1, 2 → 0), and a third would only lengthen
/// a failure that is going to be reported anyway.
pub const TRANSPORT_RETRIES: u32 = 2;

/// The pause before retry `attempt` (1-based): long enough for a session on
/// the multiplexed connection to close, short enough that a run does not
/// notice.
pub fn transport_backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(100 * u64::from(attempt) * u64::from(attempt) + 100)
}

/// Whether a line of `limactl`'s stderr says the SSH session never opened.
///
/// Every forwarded command is a session on one multiplexed SSH connection
/// (`ControlMaster auto`, one `ControlPath`), and OpenSSH's `MaxSessions`
/// caps how many one connection may carry — ten by default. Past it, the
/// peer refuses the session and `ssh` exits 255 with one of these lines. The
/// guest ran nothing: there is no session for it to have run anything in,
/// which is what makes a retry safe and why this is checked here rather than
/// by a caller, who cannot tell 255-from-SSH from 255-from-the-program.
pub fn is_transport_failure(line: &str) -> bool {
    line.contains("mux_client_request_session")
        || line.contains("Session open refused by peer")
        || line.contains("mux_client_request_session: session request failed")
}

/// Whether a line of `ssh`'s stderr says it never reached the VM's sshd.
///
/// Not a full session cap but no VM to talk to: it is stopping, or booting and
/// its sshd not listening yet. Found by an embedder's test suite on a Mac,
/// where one `zygo stop --all` (which stops the VM) followed by a burst of
/// commands had some of them fail with `ssh: connect to host 127.0.0.1 port N:
/// Connection refused` and exit 255 — and the adopter read the 255 from `zygo
/// images --json` as "the image is not here". The guest ran nothing, so a retry
/// is as safe as for a refused session; what differs is that a short pause does
/// not help, and the retry waits for the VM to be running first.
pub fn is_vm_unreachable(line: &str) -> bool {
    (line.contains("ssh: connect to host")
        && (line.contains("Connection refused") || line.contains("Operation timed out")))
        || line.contains("kex_exchange_identification")
        || line.contains("ssh_exchange_identification")
}

/// What an attempt at a forwarded command came to, when `ssh` exited 255.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// The guest was reached; the status is the command's own.
    Ran,
    /// The session was refused on a live connection ([`is_transport_failure`]).
    SessionRefused,
    /// The VM's sshd was not reached at all ([`is_vm_unreachable`]).
    VmUnreachable,
}

/// How many times a command waits for the VM and tries again after
/// [`Transport::VmUnreachable`]: pauses of 0.5, 1, 2 and 4 s after the wait
/// for the start lock.
pub const UNREACHABLE_RETRIES: u32 = 4;

/// The generation of the VM template this build ships.
///
/// Lima copies the template into the instance when it is created and never
/// reads the source again, so a change to `shim/lima.yaml` reaches a new VM
/// and not an existing one. The number is written into the template as a
/// comment ([`TEMPLATE_GENERATION_MARKER`]) and read back from the
/// instance's copy by `zygo doctor`, which says when the two differ and what
/// to do about it. Bump it whenever the template changes in a way an
/// existing VM should pick up.
///
/// 2: `MaxSessions` raised in the guest's `sshd_config` (Z-2).
pub const TEMPLATE_GENERATION: u32 = 2;

/// The comment the generation is written after, in `shim/lima.yaml`.
pub const TEMPLATE_GENERATION_MARKER: &str = "# zygo-template-generation:";

/// The generation a template (or an instance's copy of one) declares.
///
/// `None` for a template from before the marker existed, which `doctor`
/// reads as older than any generation — because it is.
pub fn template_generation_of(text: &str) -> Option<u32> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix(TEMPLATE_GENERATION_MARKER))
        .and_then(|rest| rest.trim().parse().ok())
}

/// Where a forwarded command runs when its caller's directory is not
/// shared and nothing in the command depends on it.
///
/// The root, not the guest's home: a relative path that slipped past
/// [`cwd_dependency`] would then resolve to a file that does not exist
/// rather than to one of the user's own under `$HOME` that they did not
/// name — and a spec discovered upwards from `/` finds nothing, which is
/// what the same command finds on Linux from `/tmp`.
pub const GUEST_NEUTRAL_DIR: &str = "/";

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
    // `zygo api --openapi` prints a document compiled into *this* binary and
    // exits. Forwarding it answered with the VM's copy of Zygo instead, which
    // is a different build whenever one is mid-upgrade — so a freshly added
    // route was missing from `zygo api --openapi > spec/openapi.json` on a Mac
    // while `--help` on the same binary listed it. Nothing was broken except
    // which binary answered.
    if let Command::Api(args) = command {
        return args.openapi;
    }
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
    /// The argument that made the directory matter — see [`cwd_dependency`].
    pub needed_by: String,
}

impl std::fmt::Display for Unmapped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "on macOS Zygo runs in a Linux VM, and only {} is shared with it — \
             but this command was run from {}, and {}",
            self.home.display(),
            self.cwd.display(),
            self.needed_by
        )
    }
}

/// The directory the forwarded command should run in.
///
/// Identity where it can be: the VM mounts `$HOME` at its own path, so a
/// directory underneath needs no translation at all. Outside `$HOME` the
/// directory has no counterpart, and what happens next depends on whether
/// the command *uses* it. `dependency` is [`cwd_dependency`]'s answer:
///
/// * `None` — nothing in the command resolves against the directory, so it
///   runs in [`GUEST_NEUTRAL_DIR`] and every path it names still means the
///   same file. A server started from `/`, or a systemd unit with its
///   default working directory, can run a sandbox whose mounts are all under
///   `$HOME` — which the first adoption report (Z-5) found it could not.
/// * `Some(reason)` — a relative path, or a spec file found by searching
///   upwards, would mean a different file in the VM. Refused, naming the
///   argument, rather than run against a directory that is not the one the
///   user is looking at.
pub fn workdir(cwd: &Path, home: &Path, dependency: Option<String>) -> Result<PathBuf, Unmapped> {
    if cwd.starts_with(home) {
        return Ok(cwd.to_path_buf());
    }
    match dependency {
        None => Ok(PathBuf::from(GUEST_NEUTRAL_DIR)),
        Some(needed_by) => Err(Unmapped {
            cwd: cwd.to_path_buf(),
            home: home.to_path_buf(),
            needed_by,
        }),
    }
}

/// Every path in the command that names something on *this* side, with what
/// it is, for [`cwd_dependency`].
///
/// A path the guest resolves against its own root — the command after the
/// image in `zygo run`, a `--workdir` inside the sandbox — is not here: it
/// means the same thing wherever the caller was standing. What is here is
/// everything Zygo itself opens on the host before the sandbox exists.
pub fn host_paths(command: &Command) -> Vec<(&'static str, PathBuf)> {
    let mut out: Vec<(&'static str, PathBuf)> = Vec::new();
    let mounts = |out: &mut Vec<(&'static str, PathBuf)>, mounts: &[Mount]| {
        out.extend(mounts.iter().map(|m| ("the mount", m.source.clone())));
    };
    match command {
        Command::Run(args) => {
            mounts(&mut out, &args.sandbox.mounts);
            out.extend(args.spec_file.file.clone().map(|p| ("the spec file", p)));
            out.extend(
                args.requirements
                    .clone()
                    .map(|p| ("the requirements file", p)),
            );
            out.extend(args.outcome.clone().map(|p| ("the outcome file", p)));
        }
        Command::Serve(args) => {
            out.extend(args.handler.clone().map(|p| ("the handler", p)));
            mounts(&mut out, &args.sandbox.mounts);
            out.extend(args.spec_file.file.clone().map(|p| ("the spec file", p)));
            out.extend(
                args.requirements
                    .clone()
                    .map(|p| ("the requirements file", p)),
            );
        }
        Command::Exec(args) => {
            // A digest names something the host already holds, not a file.
            if let Some(script) = &args.script
                && !script.starts_with("sha256:")
            {
                out.push(("the script", PathBuf::from(script)));
            }
        }
        Command::Up { file, .. } | Command::Down { file } | Command::Spec { file, .. } => {
            out.extend(file.file.clone().map(|p| ("the spec file", p)));
        }
        Command::Api(args) => {
            out.extend(args.spec_file.file.clone().map(|p| ("the spec file", p)));
        }
        Command::Mcp(args) => {
            out.extend(args.workspace.clone().map(|p| ("the workspace", p)));
            out.extend(args.spec_file.file.clone().map(|p| ("the spec file", p)));
            mounts(&mut out, &args.sandbox.mounts);
        }
        _ => {}
    }
    out
}

/// Whether the command looks for `sandbox.toml` upwards from the working
/// directory when no `-f` names one.
pub fn discovers_a_spec(command: &Command) -> bool {
    match command {
        Command::Run(args) => args.spec_file.file.is_none(),
        Command::Serve(args) => args.spec_file.file.is_none(),
        Command::Api(args) => args.spec_file.file.is_none(),
        Command::Mcp(args) => args.spec_file.file.is_none(),
        Command::Up { file, .. } | Command::Down { file } | Command::Spec { file, .. } => {
            file.file.is_none()
        }
        _ => false,
    }
}

/// Why this command needs its working directory to exist in the VM, if it
/// does.
///
/// Two ways it can: an argument is a relative path, which means "from
/// here"; or no `-f` was given and a `sandbox.toml` is found by searching
/// upwards from here, which the guest — searching upwards from
/// [`GUEST_NEUTRAL_DIR`] — would not find. `spec_here` is that search,
/// passed in so the rule can be tested without a filesystem.
///
/// `None` means every path the command names is absolute (and so is checked
/// by [`unmapped_paths`] on its own merits) and nothing is discovered, so
/// where the caller stood is of no consequence.
pub fn cwd_dependency(command: &Command, spec_here: Option<&Path>) -> Option<String> {
    for (what, path) in host_paths(command) {
        if path.is_relative() {
            return Some(format!(
                "{what} `{}` is relative to that directory",
                path.display()
            ));
        }
    }
    if discovers_a_spec(command)
        && let Some(found) = spec_here
    {
        return Some(format!(
            "`{}` was found by searching upwards from it (pass -f to name it instead)",
            found.display()
        ));
    }
    None
}

/// The `sandbox.toml` a search upwards from the process's directory finds.
///
/// Only the *path*: parsing it is the command's job, and a spec that does
/// not parse is still a spec the guest would not have found.
#[cfg(target_os = "macos")]
fn spec_discoverable_here() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok();
    while let Some(d) = dir {
        let candidate = d.join("sandbox.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent().map(Path::to_path_buf);
    }
    None
}

/// Mount sources this VM has no counterpart for.
///
/// Every path the command names that the VM cannot see, with what it is for.
///
/// The same rule as [`workdir`], applied to the other half of what a command
/// brings with it: `workdir` has always refused a command run from outside
/// `$HOME`, on the stated grounds that forwarding it would silently run
/// against a directory that is not the one the user is looking at. A mount
/// the guest lacks fails with `applying a bind mount from the spec failed:
/// No such file or directory` about a path that plainly exists on the host
/// (found by an embedder's script driver, whose scratch directory lives under
/// macOS's `/var/folders`, and every one of whose tests failed on it). The
/// other paths fail worse. An `--outcome` file outside
/// `$HOME` is written without complaint — into the *VM's* `/tmp`, where the
/// caller on this side will never find it and concludes the sandbox wrote
/// nothing. Found by running `zygo run --pull never --outcome /tmp/o.json`
/// from a Mac: exit 1 as promised, and no file.
pub fn unmapped_paths(command: &Command, cwd: &Path, home: &Path) -> Vec<(&'static str, PathBuf)> {
    host_paths(command)
        .into_iter()
        .map(|(what, path)| {
            let resolved = if path.is_absolute() {
                path
            } else {
                cwd.join(path)
            };
            (what, resolved)
        })
        .filter(|(_, path)| !path.starts_with(home))
        .collect()
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
/// "zygo: not found". Mixing in something that changes when the
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
/// everything the caller's shell had was dropped at the boundary:
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

/// The `ssh -F` config Lima writes beside the instance, when there is one.
///
/// `~/.lima/<instance>/ssh.config` — or under `LIMA_HOME` — and only once the
/// instance has been created, which is why a missing file means "take the
/// `limactl` path", not an error. Lima rewrites it on every start (the port
/// moves), so it is read per command rather than remembered.
pub fn ssh_config(home: &Path) -> Option<PathBuf> {
    let lima = std::env::var_os("LIMA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".lima"));
    let config = lima.join(INSTANCE).join("ssh.config");
    config.is_file().then_some(config)
}

/// Whether the multiplexed connection's master is alive: `ssh -O check`.
///
/// Sub-millisecond, and the only question it answers is whether a session
/// can be opened without `limactl`. A dead master is not a failure — the VM
/// may be up with nobody connected yet — it just means the slower path.
#[cfg(target_os = "macos")]
fn mux_alive(config: &Path) -> bool {
    std::process::Command::new("ssh")
        .args(["-F"])
        .arg(config)
        .args(["-O", "check", &format!("lima-{INSTANCE}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// The `ssh` invocation that does what `limactl shell --workdir W zygo env …
/// zygo …` does, over the connection Lima already holds.
///
/// One remote command string, because that is what ssh carries: every word
/// is quoted for the guest's shell, the working directory is entered first
/// (the same rule as [`forward_args`] — a relative path means the same file on
/// both sides), and the binary is named by its absolute path so the answer
/// does not depend on what the guest's login shell puts on `PATH`. `-t` only
/// when the caller's stdin is a terminal, which is also when `limactl shell`
/// allocates one; a pipeline must reach the sandbox unbuffered and untouched.
pub fn ssh_args<I, S>(
    config: &Path,
    workdir: &Path,
    args: I,
    env: &[(OsString, OsString)],
    tty: bool,
) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut remote = OsString::from("cd ");
    remote.push(sh_quote(workdir.as_os_str()));
    remote.push(" && ");
    if !env.is_empty() {
        remote.push("env ");
        for (k, v) in env {
            let mut pair = k.clone();
            pair.push("=");
            pair.push(v);
            remote.push(sh_quote(&pair));
            remote.push(" ");
        }
    }
    remote.push(GUEST_BIN);
    for arg in args {
        remote.push(" ");
        remote.push(sh_quote(&arg.into()));
    }

    let mut out: Vec<OsString> = vec!["-F".into(), config.into()];
    if tty {
        out.push("-t".into());
    }
    out.push(format!("lima-{INSTANCE}").into());
    out.push("--".into());
    out.push(remote);
    out
}

/// One word, safe for a POSIX shell: single quotes, with a quote inside
/// written as `'\''`. Every byte goes through, including the ones that are
/// not UTF-8, because a path is bytes.
pub fn sh_quote(word: &std::ffi::OsStr) -> OsString {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let bytes = word.as_bytes();
    if !bytes.is_empty()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(b))
    {
        return word.to_os_string();
    }
    let mut out = Vec::with_capacity(bytes.len() + 2);
    out.push(b'\'');
    for &b in bytes {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    OsString::from_vec(out)
}

/// Whether a name is safe to hand to `env` on the other side of a shell.
pub fn is_forwardable_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// What `zygo doctor` says about the Mac side, as checks a deploy script
/// can read.
///
/// Pure, so the rule is tested on every host: the probing is
/// `describe_vm`'s. The first adoption report (Z-4) read `platform: FAIL`
/// with "macos has no kernel to build a sandbox in" — true, and useless on
/// the one platform Zygo ships a VM for. These are the checks that matter
/// here: whether the VM can be started, whether it *is*, whether it was
/// made from this release's template, and whether the binary inside it is
/// this one.
pub fn host_checks(
    limactl: Option<&Path>,
    template: Option<&Path>,
    linux_binary: Option<&Path>,
    vm: Vm,
    generation: Option<u32>,
    home: Option<&Path>,
) -> Vec<zygo_core::doctor::Check> {
    use zygo_core::doctor::Check;
    let mut checks = Vec::new();

    checks.push(match limactl {
        Some(path) => Check::ok("limactl", format!("at {}", path.display())),
        None => Check::failed(
            "limactl",
            "not installed, so there is no Linux VM to run sandboxes in",
            "brew install lima",
        ),
    });
    checks.push(match template {
        Some(path) => Check::ok("vm template", path.display().to_string()),
        None => Check::failed(
            "vm template",
            "missing; Zygo cannot create the Linux VM without it",
            "set ZYGO_LIMA_TEMPLATE to a Lima template, or reinstall Zygo",
        ),
    });
    checks.push(match linux_binary {
        Some(path) => Check::ok("linux build", path.display().to_string()),
        None => Check::degraded(
            "linux build",
            "none on this Mac; the VM keeps whatever it already has",
            "in a checkout: make guest-build (no Docker) or make tests/linux/bin/zygo-linux-musl; otherwise set ZYGO_LINUX_BIN",
        ),
    });
    checks.push(match vm {
        Vm::Running => Check::ok("vm", format!("instance `{INSTANCE}` is running")),
        Vm::Stopped => Check::degraded(
            "vm",
            format!("instance `{INSTANCE}` is stopped"),
            "it starts by itself on the next command",
        ),
        Vm::Absent => Check::degraded(
            "vm",
            format!("instance `{INSTANCE}` does not exist yet"),
            "it is created by the first command that needs it",
        ),
    });
    if vm != Vm::Absent {
        checks.push(match generation {
            Some(have) if have >= TEMPLATE_GENERATION => {
                Check::ok("vm template generation", format!("{have}"))
            }
            have => Check::degraded(
                "vm template generation",
                format!(
                    "the VM was created from generation {} of the template; this release ships {TEMPLATE_GENERATION}",
                    have.map(|g| g.to_string()).unwrap_or_else(|| "0".into())
                ),
                format!(
                    "`limactl delete {INSTANCE}` — the next command recreates it (about a minute; \
                     nothing under $HOME is lost) — or apply the change by hand: \
                     docs/book/22-troubleshooting.md, \"Session open refused by peer\""
                ),
            ),
        });
    }
    if let Some(home) = home {
        checks.push(Check::ok(
            "shared directory",
            format!("{} is mounted in the VM at the same path", home.display()),
        ));
    }
    if vm != Vm::Running {
        // The checks that matter — kernel, namespaces, cgroups, seccomp — are
        // the guest's to make, and there is no guest to ask. Said as a
        // failure rather than left out, so `ok` stays the *and* of every
        // line and a health check does not read a stopped VM as a healthy
        // one.
        checks.push(Check::failed(
            "sandboxes",
            "nothing can be said about them while the VM is not running",
            "run any zygo command to start it, then `zygo doctor` again",
        ));
    }
    checks
}

/// What `zygo doctor` can say about the VM without starting it.
///
/// Deliberately does **not** start anything: `doctor` is what somebody runs
/// when things are already wrong, and a diagnostic that changes the machine
/// it is diagnosing is worse than one that reports less. A stopped VM is
/// reported as stopped.
#[cfg(target_os = "macos")]
pub struct VmReport {
    /// The Mac side: [`host_checks`].
    pub host: Vec<zygo_core::doctor::Check>,
    /// The guest's own `zygo doctor --json`, when there is a running guest
    /// to ask. `Err` carries what it said instead, for the line that
    /// reports it.
    pub guest: Option<Result<crate::cmd::doctor::JsonReport, String>>,
}

#[cfg(target_os = "macos")]
pub fn describe_vm() -> VmReport {
    let limactl = which("limactl");
    let vm = limactl
        .as_ref()
        .and_then(|l| {
            std::process::Command::new(l)
                .args(status_args())
                .output()
                .ok()
        })
        .map(|out| Vm::parse(&String::from_utf8_lossy(&out.stdout)))
        .unwrap_or(Vm::Absent);
    let generation = instance_template()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| template_generation_of(&text));
    let home = std::env::var_os("HOME").map(PathBuf::from);

    // Canonical for display: the checkout's template is found through
    // `crates/zygo-cli/../../shim/lima.yaml`, which is true and unreadable.
    let tidy = |p: PathBuf| p.canonicalize().unwrap_or(p);
    let host = host_checks(
        limactl.as_deref(),
        template_path().ok().map(tidy).as_deref(),
        linux_binary().map(tidy).as_deref(),
        vm,
        generation,
        home.as_deref(),
    );

    let guest = match (&limactl, vm) {
        (Some(limactl), Vm::Running) => Some(
            std::process::Command::new(limactl)
                .args(["shell", INSTANCE, "zygo", "doctor", "--json"])
                .output()
                .map_err(|e| e.to_string())
                .and_then(|out| {
                    serde_json::from_slice::<crate::cmd::doctor::JsonReport>(&out.stdout).map_err(
                        |_| {
                            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
                            text.push_str(&String::from_utf8_lossy(&out.stderr));
                            text.trim().to_string()
                        },
                    )
                }),
        ),
        _ => None,
    };

    VmReport { host, guest }
}

/// The instance's own copy of the template, which Lima writes at creation.
#[cfg(target_os = "macos")]
fn instance_template() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".lima")
            .join(INSTANCE)
            .join("lima.yaml"),
    )
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
    let dependency = cwd_dependency(&cli.command, spec_discoverable_here().as_deref());
    let workdir = workdir(&cwd, &home, dependency).map_err(|e| {
        anyhow::anyhow!(
            "{e}\n  → run it from somewhere under {}, or name every path absolutely",
            home.display()
        )
    })?;

    // The same rule, for what the command brings with it. Refused here so the
    // reader is told about a path on the host they are looking at, rather than
    // by the guest about a path it has never had — or, for an output file,
    // not told at all.
    let unmapped = unmapped_paths(&cli.command, &cwd, &home);
    if let Some((what, first)) = unmapped.first() {
        anyhow::bail!(
            "on macOS Zygo runs in a Linux VM, and only {} is shared with it — \
             but {what} {} is outside it{}\n  \
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

    // Said before it is done, because it is more than was asked (Z-6). On
    // Linux `stop --all` stops the functions; here it also stops the machine
    // they ran in, and the next command pays the boot.
    if stops_everything(&cli.command) {
        eprintln!(
            "this stops every sandbox and every warm function on this Mac, and then \
             the Linux VM they run in; the next command boots it again (about 16 s)"
        );
    }

    // The fast path: Lima keeps one multiplexed SSH connection to the VM and
    // writes an `ssh -F`-able config beside the instance for exactly this. When
    // that connection's master is alive the VM is running by definition, so the
    // `limactl list` that `ensure_running` would make is skipped, and the
    // command goes over the existing connection rather than through `limactl
    // shell`, which reads the instance, checks it and *then* runs ssh. Measured
    // on the Mac this was written on: `limactl shell zygo true` 40–50 ms, the
    // same over `ssh -F` under 10 ms, `-O check` under a millisecond — against
    // 10 ms for the sandbox itself. A consumer that runs a sandbox per event
    // was paying eight times the sandbox in ceremony.
    //
    // `limactl` stays for what it is for: booting the VM, copying the binary
    // in, and the first command after a boot, before there is a master to
    // check. The retry below reads the same SSH signatures either way.
    let fast = ssh_config(&home).filter(|config| mux_alive(config));
    if fast.is_none() {
        ensure_running(&limactl, &paths)?;
    }
    ensure_guest_binary(&limactl, &paths)?;

    let env = environment_to_forward(cli);
    let (program, args): (PathBuf, Vec<OsString>) = match &fast {
        Some(config) => (
            PathBuf::from("ssh"),
            ssh_args(
                config,
                &workdir,
                std::env::args_os().skip(1),
                &env,
                std::io::IsTerminal::is_terminal(&std::io::stdin()),
            ),
        ),
        None => (
            limactl.clone(),
            forward_args_with_env(&workdir, std::env::args_os().skip(1), &env),
        ),
    };

    // Retried only when the guest demonstrably ran nothing: the SSH session
    // never opened. This is the one layer that can know that, which is why
    // the retry is here and not in every caller.
    let mut attempt = 0;
    let mut unreachable = 0;
    let status = loop {
        let (status, transport) = run_forwarded(&program, &args, cli.verbose > 0)?;
        if transport == Transport::Ran {
            break status;
        }
        if transport == Transport::VmUnreachable {
            if unreachable < UNREACHABLE_RETRIES {
                unreachable += 1;
                tracing::debug!(unreachable, "the VM's sshd was not reached; waiting for it");
                // Wait out whoever is booting it first. Lima reports the VM
                // `Running` while the command that started it is still waiting
                // for its sshd, so `ensure_running` alone returned at once and
                // four tries in three seconds all failed for a command started
                // two to five seconds after a `stop --all`. The booting command
                // holds the start lock until the VM answers; taking it and
                // letting go is the wait.
                drop(paths.lock(VM_START_LOCK)?);
                // Then boot it if it is stopped, and give a sshd that is a
                // moment behind a moment.
                ensure_running(&limactl, &paths)?;
                std::thread::sleep(std::time::Duration::from_millis(500 << (unreachable - 1)));
                continue;
            }
            crate::output::error(&anyhow::anyhow!(
                "could not reach the Linux VM's ssh server\n  \
                 → tried {} times, waiting for the VM between them\n  \
                 → `limactl list` says what state it is in; `zygo doctor` checks the rest",
                UNREACHABLE_RETRIES + 1
            ));
            return Ok(Some(EXIT_VM_UNREACHABLE));
        }
        if attempt < TRANSPORT_RETRIES {
            attempt += 1;
            tracing::debug!(attempt, "the VM refused a session; retrying");
            std::thread::sleep(transport_backoff(attempt));
            continue;
        }
        crate::output::error(&anyhow::anyhow!(
            "could not open a session to the Linux VM (too many at once)\n  \
             → tried {} times; the VM's sshd caps the sessions one connection may carry\n  \
             → `zygo doctor` says whether the VM was made from a template that raises the cap",
            TRANSPORT_RETRIES + 1
        ));
        return Ok(Some(EXIT_VM_UNREACHABLE));
    };

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

/// One attempt at the forwarded command.
///
/// Standard input and output are the child's own — a pipeline on the Mac has
/// to reach the sandbox unbuffered and untouched. Standard error passes
/// through this process, chunk by chunk as it arrives, so the one thing
/// SSH says that Zygo has an answer for ([`is_transport_failure`]) can be
/// recognised, kept back unless `-v` asked for it, and replaced by a
/// sentence a user can act on. The second value says whether that happened
/// *and* `limactl` exited 255, which together mean the guest ran nothing.
#[cfg(target_os = "macos")]
fn run_forwarded(
    program: &Path,
    args: &[OsString],
    verbose: bool,
) -> anyhow::Result<(std::process::ExitStatus, Transport)> {
    use anyhow::Context;
    use std::io::{Read, Write};

    let mut child = std::process::Command::new(program)
        .args(args)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("could not run {}", program.display()))?;

    // Relay the signals the user can send to *this* process.
    //
    // Ctrl-C reaches `limactl` anyway, because it goes to the whole foreground
    // process group. A `kill` by pid does not: it arrives here, this process
    // ends, and `limactl` — and the sandbox behind it — carries on with
    // nobody left to wait for it. So the pid is published and the two
    // signals a person actually sends are forwarded.
    FORWARDED_CHILD.store(child.id() as i32, std::sync::atomic::Ordering::SeqCst);
    install_signal_relay();

    let mut stderr = child.stderr.take().expect("stderr was piped");
    let relay = std::thread::spawn(move || {
        // The transport fails before the guest has said anything, so the
        // signature is in the first few kilobytes if it is anywhere.
        const LOOK_IN: usize = 8 * 1024;
        let mut seen = String::new();
        let mut matched = false;
        let mut unreachable = false;
        let mut out = std::io::stderr();
        let mut buf = [0u8; 4096];
        loop {
            let n = match stderr.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let chunk = &buf[..n];
            if seen.len() < LOOK_IN {
                seen.push_str(&String::from_utf8_lossy(chunk));
            }
            let text = String::from_utf8_lossy(chunk);
            let is_ssh = is_transport_failure(&text);
            let is_down = is_vm_unreachable(&text);
            matched |= is_ssh;
            unreachable |= is_down;
            if (is_ssh || is_down) && !verbose {
                // Zygo's own sentence replaces it, once the retries are spent.
                continue;
            }
            let _ = out.write_all(chunk);
            let _ = out.flush();
        }
        if matched || is_transport_failure(&seen) {
            Transport::SessionRefused
        } else if unreachable || is_vm_unreachable(&seen) {
            Transport::VmUnreachable
        } else {
            Transport::Ran
        }
    });

    let status = child
        .wait()
        .with_context(|| format!("could not wait for {}", program.display()))?;
    FORWARDED_CHILD.store(0, std::sync::atomic::Ordering::SeqCst);
    let transport = relay.join().unwrap_or(Transport::Ran);

    Ok((
        status,
        if status.code() == Some(255) {
            transport
        } else {
            Transport::Ran
        },
    ))
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
             → in a checkout: make guest-build (compiled in the VM, no Docker) \
             or make tests/linux/bin/zygo-linux-musl\n  \
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

    // Whether this is a *replacement*: the stamp says an earlier install
    // happened, so a supervisor started from the old binary may be running.
    let replacing = stamp.is_file();

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

    // A skew the shim caused is a skew the shim fixes (Z-6). The supervisor
    // in the guest, if there is one, was started from the binary that was
    // just replaced, and the next command would be refused with "client
    // speaks control v13, this supervisor speaks v12". `supervisor stop`
    // drains it and exits; the next `serve` starts one of this release.
    // Best effort and quiet on the common case, which is no supervisor.
    if replacing {
        let stopped = std::process::Command::new(limactl)
            .args(["shell", INSTANCE, GUEST_BIN, "supervisor", "stop"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if stopped {
            eprintln!(
                "the supervisor in the Linux VM was running the previous release and has \
                 been stopped; the next `serve` or `up` starts one of this release"
            );
        }
    }
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
        "/../../tests/linux/bin/zygo-linux-musl"
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
            workdir(Path::new("/Users/m/work/fn"), home, None).unwrap(),
            PathBuf::from("/Users/m/work/fn")
        );
        assert_eq!(
            workdir(home, home, None).unwrap(),
            PathBuf::from("/Users/m")
        );
        // Under home, a relative argument is fine: the directory is shared.
        assert_eq!(
            workdir(
                Path::new("/Users/m/work"),
                home,
                Some("the mount `./x`".into())
            )
            .unwrap(),
            PathBuf::from("/Users/m/work")
        );
    }

    /// Z-5 from the first adoption report: `cd /tmp && zygo run --mount
    /// $HOME/x:/data:rw alpine:3 true` was refused although every path in
    /// it is shared. Only a command that *uses* its directory is refused
    /// now, and the message says which argument used it.
    #[test]
    fn a_directory_outside_home_is_refused_only_when_something_depends_on_it() {
        let home = Path::new("/Users/m");
        assert_eq!(
            workdir(Path::new("/tmp/scratch"), home, None).unwrap(),
            PathBuf::from(GUEST_NEUTRAL_DIR),
            "nothing relative, nothing discovered: forwarded, in a neutral directory"
        );

        let err = workdir(
            Path::new("/tmp/scratch"),
            home,
            Some("the mount `./x` is relative to that directory".into()),
        )
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("/tmp/scratch"), "{text}");
        assert!(text.contains("/Users/m"), "{text}");
        assert!(
            text.contains("the mount `./x`"),
            "the argument is named: {text}"
        );
    }

    // A prefix match on strings would accept this; on paths it does not.
    #[test]
    fn a_sibling_directory_that_merely_starts_with_home_is_not_home() {
        let home = Path::new("/Users/m");
        assert!(workdir(Path::new("/Users/martin/work"), home, Some("x".into())).is_err());
    }

    /// The rule, argument by argument.
    #[test]
    fn only_a_relative_path_or_a_discovered_spec_needs_the_working_directory() {
        let none: Option<&Path> = None;

        // The report's exact command: every path absolute, nothing found.
        let absolute = command_of(&[
            "zygo",
            "run",
            "--mount",
            "/Users/m/x:/data:rw",
            "alpine:3",
            "true",
        ]);
        assert_eq!(cwd_dependency(&absolute, none), None);

        // A relative mount means "from here".
        let relative = command_of(&["zygo", "run", "--mount", "./x:/x:rw", "alpine:3", "true"]);
        let why = cwd_dependency(&relative, none).expect("refused");
        assert!(why.contains("the mount `./x`"), "{why}");

        // A handler file is the same kind of thing.
        let served = command_of(&["zygo", "serve", "handler.py", "--name", "f"]);
        let why = cwd_dependency(&served, none).expect("refused");
        assert!(why.contains("the handler `handler.py`"), "{why}");
        let served = command_of(&["zygo", "serve", "/Users/m/handler.py", "--name", "f"]);
        assert_eq!(cwd_dependency(&served, none), None);

        // A spec found by searching upwards would not be found in the VM.
        let found = Path::new("/tmp/project/sandbox.toml");
        let why = cwd_dependency(&absolute, Some(found)).expect("refused");
        assert!(why.contains("/tmp/project/sandbox.toml"), "{why}");
        // Unless `-f` names one, absolutely.
        let named = command_of(&["zygo", "run", "-f", "/Users/m/s.toml", "alpine:3", "true"]);
        assert_eq!(cwd_dependency(&named, Some(found)), None);
        let named = command_of(&["zygo", "run", "-f", "s.toml", "alpine:3", "true"]);
        assert!(
            cwd_dependency(&named, none)
                .unwrap()
                .contains("the spec file `s.toml`")
        );

        // Commands with no host paths at all: the interactive shell, `ps`,
        // `stop`. Where the user stood is of no consequence.
        for args in [
            &["zygo", "shell", "f"][..],
            &["zygo", "ps"][..],
            &["zygo", "stop", "--all"][..],
            &["zygo", "exec", "f", "{}"][..],
            &[
                "zygo",
                "exec",
                "--runtime",
                "py",
                "--script",
                "sha256:abc",
                "{}",
            ][..],
        ] {
            assert_eq!(cwd_dependency(&command_of(args), none), None, "{args:?}");
        }
        let script = command_of(&["zygo", "exec", "--runtime", "py", "--script", "s.py", "{}"]);
        assert!(
            cwd_dependency(&script, none)
                .unwrap()
                .contains("the script `s.py`")
        );

        // `up` discovers, `up -f /abs` does not.
        assert!(cwd_dependency(&command_of(&["zygo", "up"]), Some(found)).is_some());
        assert_eq!(
            cwd_dependency(
                &command_of(&["zygo", "up", "-f", "/Users/m/s.toml"]),
                Some(found)
            ),
            None
        );
    }

    /// SSH's transport failures, and the lines that are not one.
    #[test]
    fn the_transport_signature_is_recognised_and_nothing_else_is() {
        for line in [
            "mux_client_request_session: session request failed: Session open refused by peer\n",
            "Session open refused by peer",
        ] {
            assert!(is_transport_failure(line), "{line:?}");
        }
        for line in [
            "Traceback (most recent call last):\n",
            "error: the sandbox exceeded its 30s deadline and was killed\n",
            "ssh: connect to host 127.0.0.1 port 60022: Connection refused\n",
            "",
        ] {
            assert!(!is_transport_failure(line), "{line:?}");
        }
    }

    /// A VM that is not there yet is its own case: retried, after waiting.
    #[test]
    fn an_unreachable_vm_is_recognised_and_nothing_else_is() {
        for line in [
            "ssh: connect to host 127.0.0.1 port 55246: Connection refused\r\n",
            "kex_exchange_identification: read: Connection reset by peer\n",
        ] {
            assert!(is_vm_unreachable(line), "{line:?}");
        }
        for line in [
            "mux_client_request_session: session request failed: Session open refused by peer\n",
            "Connection to 127.0.0.1 closed by remote host.\n",
            "ConnectionRefusedError: [Errno 111] Connection refused\n",
            "",
        ] {
            assert!(!is_vm_unreachable(line), "{line:?}");
        }
    }

    /// The backoff grows, and the whole retry budget stays well under a
    /// second: a run that needed it should not notice.
    #[test]
    fn the_retry_budget_is_short() {
        let total: std::time::Duration = (1..=TRANSPORT_RETRIES).map(transport_backoff).sum();
        assert!(transport_backoff(1) < transport_backoff(2));
        assert!(total < std::time::Duration::from_secs(1), "{total:?}");
        assert_ne!(
            EXIT_VM_UNREACHABLE, 125,
            "125 is `this host cannot run sandboxes`"
        );
        assert_ne!(
            EXIT_VM_UNREACHABLE, 255,
            "255 is what SSH and a program both exit with"
        );
    }

    /// The retry decision, against a stand-in for `limactl`: SSH's line
    /// and exit 255 together mean the guest ran nothing; either alone does
    /// not. A real refusal needs a VM and a burst wide enough to hit its
    /// session cap, which `tests/linux/verify_shim_concurrency.sh` attempts; this
    /// is the decision itself, on the bytes SSH actually prints.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_refused_session_is_recognised_only_with_exit_255() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = |name: &str, script: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let refused = fake(
            "refused",
            "printf 'mux_client_request_session: session request failed: Session open refused by peer\\n' >&2; exit 255",
        );
        let program_255 = fake("program", "printf 'Traceback\\n' >&2; exit 255");
        let refused_but_ran = fake(
            "odd",
            "printf 'Session open refused by peer\\n' >&2; exit 3",
        );
        let fine = fake("fine", "exit 0");
        let down = fake(
            "down",
            "printf 'ssh: connect to host 127.0.0.1 port 55246: Connection refused\\r\\n' >&2; exit 255",
        );

        let (status, transport) = run_forwarded(&refused, &[], false).unwrap();
        assert_eq!(status.code(), Some(255));
        assert_eq!(
            transport,
            Transport::SessionRefused,
            "SSH's line and 255: the guest ran nothing"
        );

        let (status, transport) = run_forwarded(&down, &[], false).unwrap();
        assert_eq!(status.code(), Some(255));
        assert_eq!(transport, Transport::VmUnreachable, "no sshd to reach");

        let (status, transport) = run_forwarded(&program_255, &[], false).unwrap();
        assert_eq!(status.code(), Some(255));
        assert_eq!(
            transport,
            Transport::Ran,
            "255 from a program is the program's"
        );

        let (_, transport) = run_forwarded(&refused_but_ran, &[], false).unwrap();
        assert_eq!(
            transport,
            Transport::Ran,
            "the line without 255 is not a transport failure"
        );

        let (status, transport) = run_forwarded(&fine, &[], false).unwrap();
        assert!(status.success());
        assert_eq!(transport, Transport::Ran);
    }

    /// The generation marker, as the template writes it and an old
    /// instance's copy lacks it.
    #[test]
    fn the_template_declares_this_generation() {
        let shipped = include_str!("../../../shim/lima.yaml");
        assert_eq!(template_generation_of(shipped), Some(TEMPLATE_GENERATION));
        assert_eq!(template_generation_of("images:\n  - location: x\n"), None);
        assert_eq!(
            template_generation_of("# zygo-template-generation: 1\n"),
            Some(1)
        );
    }

    /// Z-4: the Mac side is its own checks, and `ok` is the *and* of them.
    #[test]
    fn host_checks_name_what_a_deploy_script_needs_to_know() {
        use zygo_core::doctor::Status;
        let names =
            |checks: &[zygo_core::doctor::Check]| checks.iter().map(|c| c.name).collect::<Vec<_>>();
        let worst =
            |checks: &[zygo_core::doctor::Check]| checks.iter().map(|c| c.status).max().unwrap();

        let healthy = host_checks(
            Some(Path::new("/opt/homebrew/bin/limactl")),
            Some(Path::new("/opt/zygo/lima.yaml")),
            Some(Path::new("/opt/zygo/zygo-linux-aarch64")),
            Vm::Running,
            Some(TEMPLATE_GENERATION),
            Some(Path::new("/Users/m")),
        );
        assert_eq!(worst(&healthy), Status::Ok, "{healthy:?}");
        assert!(names(&healthy).contains(&"vm template generation"));

        // No limactl: a failure with the brew line, not "no kernel".
        let bare = host_checks(None, None, None, Vm::Absent, None, None);
        assert_eq!(worst(&bare), Status::Failed);
        assert!(
            bare.iter()
                .any(|c| c.name == "limactl" && c.remedy.as_deref() == Some("brew install lima"))
        );
        assert!(
            !names(&bare).contains(&"vm template generation"),
            "no instance, no generation to compare"
        );

        // A stopped VM is not a broken one, but nothing can vouch for the
        // sandboxes until it is up — so `ok` is false, with the remedy.
        let stopped = host_checks(
            Some(Path::new("/l")),
            Some(Path::new("/t")),
            Some(Path::new("/b")),
            Vm::Stopped,
            Some(TEMPLATE_GENERATION),
            None,
        );
        assert_eq!(worst(&stopped), Status::Failed);
        assert!(
            stopped
                .iter()
                .any(|c| c.name == "vm" && c.status == Status::Degraded)
        );
        assert!(
            stopped
                .iter()
                .any(|c| c.name == "sandboxes" && c.status == Status::Failed)
        );

        // An instance made from an older template says so and names the fix.
        let stale = host_checks(
            Some(Path::new("/l")),
            Some(Path::new("/t")),
            Some(Path::new("/b")),
            Vm::Running,
            None,
            None,
        );
        let generation = stale
            .iter()
            .find(|c| c.name == "vm template generation")
            .unwrap();
        assert_eq!(generation.status, Status::Degraded);
        assert!(
            generation
                .remedy
                .as_deref()
                .unwrap()
                .contains("limactl delete zygo")
        );
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

    /// What goes over the wire is one shell command for the guest, so every
    /// word is quoted and the binary is absolute.
    #[test]
    fn the_ssh_form_quotes_every_word_and_names_the_binary_absolutely() {
        let config = Path::new("/Users/m/.lima/zygo/ssh.config");
        let workdir = Path::new("/Users/m/my project");
        let env = [(OsString::from("ZYGO_LOG"), OsString::from("debug"))];

        let args = ssh_args(
            config,
            workdir,
            [
                "run",
                "--env",
                "GREETING=it's here",
                "alpine:3",
                "echo",
                "hi",
            ],
            &env,
            false,
        );

        assert_eq!(args[0], "-F");
        assert_eq!(args[1], config.as_os_str());
        assert_eq!(args[2], "lima-zygo");
        assert_eq!(args[3], "--");
        assert_eq!(
            args[4],
            "cd '/Users/m/my project' && env ZYGO_LOG=debug /usr/local/bin/zygo run --env \
             'GREETING=it'\\''s here' alpine:3 echo hi"
        );
        assert_eq!(args.len(), 5, "no -t without a terminal");

        let with_tty = ssh_args(config, workdir, ["ps"], &[], true);
        assert_eq!(with_tty[2], "-t");
        assert_eq!(
            with_tty[5],
            "cd '/Users/m/my project' && /usr/local/bin/zygo ps"
        );
    }

    #[test]
    fn a_word_is_left_bare_only_when_a_shell_would_read_it_as_one() {
        let q = |s: &str| sh_quote(std::ffi::OsStr::new(s)).into_string().unwrap();
        assert_eq!(q("alpine:3"), "alpine:3");
        assert_eq!(q("--mem=512M"), "--mem=512M");
        assert_eq!(q("/Users/m/x.py"), "/Users/m/x.py");
        assert_eq!(q(""), "''");
        assert_eq!(q("a b"), "'a b'");
        assert_eq!(q("it's"), "'it'\\''s'");
        assert_eq!(q("$HOME"), "'$HOME'");
        assert_eq!(q("a;b"), "'a;b'");
    }

    /// The config is where Lima puts it, and only counts once the instance
    /// exists — before that there is nothing to connect to.
    #[test]
    fn the_ssh_config_is_lima_s_and_only_when_present() {
        let home = tempfile::tempdir().expect("tempdir");
        assert_eq!(ssh_config(home.path()), None);

        let dir = home.path().join(".lima").join(INSTANCE);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ssh.config"), "Host lima-zygo\n").unwrap();
        assert_eq!(ssh_config(home.path()), Some(dir.join("ssh.config")));
    }

    /// The same rule for the paths that are not mounts. An outcome file
    /// outside `$HOME` used to be forwarded and written inside the VM, where
    /// the caller never saw it.
    #[test]
    fn every_path_the_command_names_is_held_to_the_shared_directory() {
        let home = Path::new("/Users/m");
        let cwd = Path::new("/Users/m/projects/app");
        let parse = |args: &[&str]| Cli::try_parse_from(args).expect("parses").command;

        let run = parse(&[
            "zygo",
            "run",
            "--outcome",
            "/tmp/o.json",
            "--requirements",
            "./requirements.txt",
            "--mount",
            "/Users/m/data:/data",
            "alpine",
        ]);
        assert_eq!(
            unmapped_paths(&run, cwd, home),
            [("the outcome file", PathBuf::from("/tmp/o.json"))],
            "the outcome file is outside; the relative requirements file and the mount are not"
        );

        let ok = parse(&["zygo", "run", "--outcome", "/Users/m/o.json", "alpine"]);
        assert!(unmapped_paths(&ok, cwd, home).is_empty());

        let serve = parse(&["zygo", "serve", "/var/lib/h.py", "--name", "h"]);
        assert_eq!(
            unmapped_paths(&serve, cwd, home),
            [("the handler", PathBuf::from("/var/lib/h.py"))]
        );
    }

    /// The case that started the rule: an embedder's script driver mounts a
    /// scratch directory from the system temporary directory, which on macOS
    /// is outside `$HOME`, and every one of its tests failed with the guest
    /// complaining about a path the host has.
    #[test]
    fn a_mount_the_vm_cannot_see_is_refused_here() {
        let home = Path::new("/Users/m");
        let cwd = Path::new("/Users/m/projects/app");
        let run = |mounts: &[&str]| {
            let mut args = vec!["zygo", "run"];
            for m in mounts {
                args.extend(["--mount", m]);
            }
            args.push("alpine");
            Cli::try_parse_from(args).expect("parses").command
        };

        // Under `$HOME`: the VM has it at the same path.
        assert!(
            unmapped_paths(&run(&["/Users/m/data:/data"]), cwd, home).is_empty(),
            "an absolute path under home is shared"
        );
        // Relative: resolved against the caller's directory, which is itself
        // already known to be under home.
        assert!(
            unmapped_paths(&run(&["./cache:/cache:rw"]), cwd, home).is_empty(),
            "a relative path means a place on this side"
        );

        // The case that failed: macOS puts temporary directories here.
        assert_eq!(
            unmapped_paths(&run(&["/var/folders/ab/T/run:/data:rw"]), cwd, home),
            [("the mount", PathBuf::from("/var/folders/ab/T/run"))]
        );
        // And every one of them is collected, so the message can count them.
        assert_eq!(
            unmapped_paths(
                &run(&["/Users/m/ok:/ok", "/tmp/one:/one", "/etc/two:/two"]),
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
        let mounts = |command: &Command| {
            host_paths(command)
                .into_iter()
                .filter(|(what, _)| *what == "the mount")
                .count()
        };
        let with_mount = command_of(&["zygo", "run", "--mount", "/tmp/x:/x", "alpine:3", "true"]);
        assert_eq!(mounts(&with_mount), 1);

        let served = command_of(&[
            "zygo",
            "serve",
            "h.py",
            "--name",
            "f",
            "--mount",
            "/tmp/x:/x:rw",
        ]);
        assert_eq!(mounts(&served), 1);

        assert_eq!(mounts(&command_of(&["zygo", "ps"])), 0);
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
    /// The stamp lived on the host and described only the file, so
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
    /// `limactl shell` runs with the *guest's* environment, so a secret
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
