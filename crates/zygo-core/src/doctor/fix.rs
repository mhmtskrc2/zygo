//! What `zygo doctor --fix` would do, as data.
//!
//! `doctor` already prints a remedy beside every check that is not `ok`. A
//! remedy is a sentence, though, and the two that matter most on Ubuntu 24.04
//! — the AppArmor restriction on unprivileged user namespaces, and the
//! AppArmor profile that confines `pasta` — are a sentence people paste wrong,
//! or skip, or apply without reading what it turns off.
//!
//! So each one is a [`Fix`]: a name, the reason it is needed, what it costs,
//! and the exact commands. The commands are the *only* thing that runs, they
//! are printed before anything happens, and the caller confirms. Nothing here
//! executes anything — that is the CLI's job, so that "what would be done" and
//! "do it" cannot drift apart.
//!
//! Deliberately not a fix for everything `doctor` reports. A kernel that is
//! too old, a cgroup hierarchy that is not v2, a missing `/dev/kvm`: those are
//! not one command, and pretending otherwise is how a `--fix` flag becomes a
//! flag people stop trusting.

use super::{Report, Status};

/// One repair `zygo doctor --fix` can carry out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fix {
    /// The `doctor` check this repairs, so the two can be read together.
    pub check: &'static str,
    /// One line: what this does.
    pub what: String,
    /// Why the host needs it, in the words of the failure it causes.
    pub why: String,
    /// What is given up. `None` when nothing is — a file in the user's own
    /// systemd configuration costs nothing.
    ///
    /// Printed in full before the confirmation, because two of these turn off
    /// a kernel protection for **every** process on the machine, not only for
    /// Zygo's.
    pub cost: Option<String>,
    /// The commands, in order, exactly as they will run.
    pub commands: Vec<Command>,
}

/// One command in a fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Program and arguments. No shell: nothing here is quoted, expanded or
    /// word-split, so nothing here can be turned into a different command by
    /// a path with a space in it.
    pub argv: Vec<String>,
    /// Whether it has to run as root.
    pub root: bool,
    /// What to write on the command's standard input.
    ///
    /// How a file lands somewhere this user cannot redirect into: `tee` takes
    /// the path as an argument and the content arrives on a pipe, so no shell
    /// is started. Carried here rather than worked out by the caller from the
    /// path, so that the plan that is *printed* contains the bytes that are
    /// *written*.
    pub stdin: Option<String>,
}

impl Command {
    fn user(argv: &[&str]) -> Command {
        Command {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            root: false,
            stdin: None,
        }
    }

    fn root(argv: &[&str]) -> Command {
        Command {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            root: true,
            stdin: None,
        }
    }

    /// The same command, fed `body` on its standard input.
    fn writing(mut self, body: String) -> Command {
        self.stdin = Some(body);
        self
    }

    /// How it reads in a shell, for the plan `--fix` prints.
    pub fn display(&self) -> String {
        let body = self
            .argv
            .iter()
            .map(|a| {
                if a.is_empty() || a.contains([' ', '\n', '\t', '"', '\'', '$']) {
                    format!("{a:?}")
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        if self.root {
            format!("sudo {body}")
        } else {
            body
        }
    }
}

impl Fix {
    pub fn needs_root(&self) -> bool {
        self.commands.iter().any(|c| c.root)
    }
}

/// Where the persistent form of the userns sysctl goes.
///
/// A `sysctl -w` lasts until the next boot, which makes for a machine that
/// works today and fails on Monday with no record of why. The file is named
/// after Zygo so that whoever finds it in a year knows who to blame.
const SYSCTL_FILE: &str = "/etc/sysctl.d/60-zygo-userns.conf";

/// The fixes this host needs, in the order they should be applied.
///
/// Empty when there is nothing to do, which is the common case and the one
/// worth saying plainly.
pub fn plan(report: &Report) -> Vec<Fix> {
    let mut fixes = Vec::new();
    if !cfg!(target_os = "linux") {
        return fixes;
    }

    for check in &report.checks {
        match check.name {
            "user namespaces" if check.status == Status::Failed && apparmor_restricts_userns() => {
                fixes.push(apparmor_userns_fix());
            }
            "cgroup v2" if check.status == Status::Failed => {
                fixes.push(delegation_fix());
            }
            name if name.starts_with("egress") && check.status != Status::Ok => {
                if let Some(fix) = egress_fix(&check.detail) {
                    fixes.push(fix);
                }
            }
            _ => {}
        }
    }

    // Not tied to a check: `pasta` being *present* is what `doctor` reports,
    // and its being confined is only discovered when a networked sandbox
    // fails to start. Offered whenever the profile is loaded and enforcing.
    if let Some(fix) = pasta_profile_fix() {
        fixes.push(fix);
    }

    fixes
}

fn apparmor_userns_fix() -> Fix {
    Fix {
        check: "user namespaces",
        what: "let unprivileged user namespaces mount".into(),
        why: "Ubuntu 24.04 ships kernel.apparmor_restrict_unprivileged_userns=1. \
              It lets an unprivileged process create a user namespace and then \
              refuses the first mount inside it, which is the first thing every \
              sandbox does — so every sandbox dies on `mount --make-rprivate /: \
              Permission denied`, which names neither AppArmor nor user \
              namespaces."
            .into(),
        cost: Some(
            "this turns the restriction off for every process on the machine, not \
             only for Zygo's. It is a defence against kernel bugs reachable \
             through user namespaces; docs/threat-model.md says what it was \
             protecting. The narrower alternative is an AppArmor profile for the \
             `zygo` binary alone, which this cannot write for you because it \
             depends on where you installed it."
                .into(),
        ),
        commands: vec![
            Command::root(&[
                "sysctl",
                "-w",
                "kernel.apparmor_restrict_unprivileged_userns=0",
            ]),
            // `tee` rather than a shell redirect: the redirect would be
            // performed by *this* user's shell, which cannot write
            // /etc/sysctl.d, and `sudo sh -c '…'` is a shell this does not
            // need to start. A `sysctl -w` alone lasts until the next boot,
            // which makes for a machine that works today and fails on Monday
            // with no record of why.
            Command::root(&["tee", SYSCTL_FILE]).writing(sysctl_file_contents()),
        ],
    }
}

/// What the persistent sysctl file should contain.
fn sysctl_file_contents() -> String {
    "# Written by `zygo doctor --fix`.\n\
         #\n\
         # Ubuntu 24.04's AppArmor restriction on unprivileged user namespaces\n\
         # refuses the first mount inside one, which is the first thing every\n\
         # Zygo sandbox does. Removing this file restores the distribution's\n\
         # default at the next boot.\n\
     kernel.apparmor_restrict_unprivileged_userns = 0\n"
        .to_string()
}

fn delegation_fix() -> Fix {
    let home = std::env::var("HOME").unwrap_or_else(|_| "~".into());
    let dir = format!("{home}/.config/systemd/user/user@.service.d");
    Fix {
        check: "cgroup v2",
        what: "delegate cgroup controllers to your user session".into(),
        why: "without a cgroup it can own, a sandbox has no memory limit, no pid \
              limit and no CPU quota. Zygo refuses to start one rather than run it \
              unlimited (requirement N4)."
            .into(),
        cost: None,
        commands: vec![
            Command::user(&["mkdir", "-p", &dir]),
            Command::user(&["tee", &format!("{dir}/delegate.conf")])
                .writing(delegate_file_contents()),
            Command::user(&["systemctl", "--user", "daemon-reexec"]),
        ],
    }
}

/// What the systemd drop-in should contain.
fn delegate_file_contents() -> String {
    "# Written by `zygo doctor --fix`.\n\
     [Service]\n\
     Delegate=cpu cpuset io memory pids\n"
        .to_string()
}

/// Install `passt` and `nftables`, when `doctor` said they are missing and
/// this host has a package manager whose name we can spell.
fn egress_fix(detail: &str) -> Option<Fix> {
    let mut packages = Vec::new();
    if detail.contains("passt") {
        packages.push("passt");
    }
    if detail.contains("nftables") {
        packages.push("nftables");
    }
    if packages.is_empty() {
        return None;
    }

    let manager = package_manager()?;
    let mut argv: Vec<&str> = match manager {
        "apt-get" => vec!["apt-get", "install", "-y", "--no-install-recommends"],
        "dnf" => vec!["dnf", "install", "-y"],
        "pacman" => vec!["pacman", "-S", "--noconfirm"],
        "apk" => vec!["apk", "add"],
        _ => return None,
    };
    argv.extend(packages.iter().copied());

    let mut commands = Vec::new();
    // apt refuses to install from an index it does not have, which on a fresh
    // image or a machine that has not updated in a while is every package.
    if manager == "apt-get" {
        commands.push(Command::root(&["apt-get", "update"]));
    }
    commands.push(Command::root(&argv));

    Some(Fix {
        check: "egress",
        what: format!("install {}", packages.join(" and ")),
        why: "network = \"egress\" moves packets with `pasta` and installs the \
              allowlist with `nft`. A networked sandbox does not start without \
              them, rather than starting unconfined."
            .into(),
        cost: None,
        commands,
    })
}

/// Stop AppArmor confining `pasta`, which is the second Ubuntu policy.
///
/// `aa-complain` rather than unloading the profile: the profile stays loaded
/// and keeps logging, so what it *would* have denied is still visible in
/// `dmesg`, and switching it back is one command.
fn pasta_profile_fix() -> Option<Fix> {
    if !pasta_is_enforced() {
        return None;
    }
    Some(Fix {
        check: "egress",
        what: "stop AppArmor confining `pasta`".into(),
        why: "the distribution's profile denies `pasta` the sandbox's user \
              namespace, so a networked sandbox fails with \"Couldn't open user \
              namespace … Permission denied\" — which has nothing to do with \
              /dev/net/tun, the usual suspect."
            .into(),
        cost: Some(
            "`pasta` is the process that moves packets between the sandbox and \
             the host; in complain mode its profile logs what it would have \
             denied instead of denying it. `sudo aa-enforce /usr/bin/pasta` puts \
             it back."
                .into(),
        ),
        commands: vec![Command::root(&["aa-complain", "/usr/bin/pasta"])],
    })
}

#[cfg(target_os = "linux")]
fn apparmor_restricts_userns() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .is_ok_and(|v| v.trim() != "0")
}

#[cfg(not(target_os = "linux"))]
fn apparmor_restricts_userns() -> bool {
    false
}

/// Whether an AppArmor profile for `pasta` is loaded in enforce mode.
///
/// Read from `/sys/kernel/security/apparmor/profiles`, which is the same
/// place `aa-status` reads and needs no `apparmor-utils` installed to ask.
/// Lines are `name (mode)`.
fn pasta_is_enforced() -> bool {
    let Ok(text) = std::fs::read_to_string("/sys/kernel/security/apparmor/profiles") else {
        return false;
    };
    text.lines().any(|line| {
        (line.contains("pasta") || line.contains("passt")) && line.contains("(enforce)")
    })
}

fn package_manager() -> Option<&'static str> {
    let path = std::env::var_os("PATH")?;
    let directories: Vec<_> = std::env::split_paths(&path).collect();
    ["apt-get", "dnf", "pacman", "apk"]
        .into_iter()
        .find(|candidate| directories.iter().any(|d| d.join(candidate).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Check;

    fn report(checks: Vec<Check>) -> Report {
        Report { checks }
    }

    #[test]
    fn a_healthy_host_has_nothing_to_fix() {
        let r = report(vec![
            Check::ok("user namespaces", "one can be built and mounted in"),
            Check::ok("cgroup v2", "delegated (cpu io memory pids)"),
        ]);
        assert!(plan(&r).is_empty());
    }

    #[test]
    fn a_cgroup_that_cannot_delegate_is_one_users_own_file() {
        let r = report(vec![Check::failed("cgroup v2", "not delegated", "…")]);
        let fixes = plan(&r);
        if !cfg!(target_os = "linux") {
            assert!(fixes.is_empty(), "there is nothing to fix off Linux");
            return;
        }
        let fix = fixes
            .iter()
            .find(|f| f.check == "cgroup v2")
            .expect("a fix");
        assert!(
            !fix.needs_root(),
            "delegation is the user's own systemd configuration: {:?}",
            fix.commands
        );
        assert!(
            fix.commands
                .iter()
                .any(|c| c.argv.iter().any(|a| a.contains("delegate.conf"))),
            "{:?}",
            fix.commands
        );
    }

    #[test]
    fn every_command_that_touches_the_system_says_it_needs_root() {
        let fix = apparmor_userns_fix();
        assert!(fix.needs_root());
        assert!(
            fix.cost.is_some(),
            "a fix that weakens the host for every process has to say so"
        );
        assert!(fix.commands.iter().all(|c| c.root));
        assert_eq!(fix.commands[0].display().split(' ').next(), Some("sudo"));
    }

    #[test]
    fn nothing_is_run_through_a_shell() {
        // The whole point of `argv` rather than a string: a path with a space
        // in it cannot become two arguments, and nothing can be a `;`.
        let fix = delegation_fix();
        for command in &fix.commands {
            assert!(
                !command.argv.is_empty(),
                "an empty command would run the shell's idea of nothing"
            );
            assert!(
                !command.argv[0].contains(['|', ';', '&', '>']),
                "{:?} looks like a shell line, not a program",
                command.argv
            );
        }
    }

    #[test]
    fn the_persistent_sysctl_says_what_wrote_it_and_what_removing_it_does() {
        let fix = apparmor_userns_fix();
        let writer = fix
            .commands
            .iter()
            .find(|c| c.stdin.is_some())
            .expect("the fix writes a file");
        assert_eq!(writer.argv[1], SYSCTL_FILE);
        assert!(SYSCTL_FILE.starts_with("/etc/sysctl.d/"));
        let body = writer.stdin.clone().unwrap();
        assert!(body.contains("zygo doctor --fix"));
        assert!(body.contains("kernel.apparmor_restrict_unprivileged_userns = 0"));
        assert!(
            body.contains("Removing this file"),
            "a file that changes a kernel default has to say how to undo it"
        );
    }
}
