// SPDX-License-Identifier: Apache-2.0
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

/// The unit that remounts cgroup2 with `favordynmods` at every boot. A
/// `mount -o remount` lasts until the next reboot, for the same reason as the
/// sysctl above; systemd mounts the hierarchy itself, early, with options no
/// file controls, so a oneshot unit after it is the durable form.
const FAVORDYNMODS_UNIT: &str = "zygo-cgroup-favordynmods.service";

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
                // A profile for this binary where AppArmor can load one; the
                // host-wide sysctl only where it cannot.
                let exe = std::env::current_exe()
                    .and_then(std::fs::canonicalize)
                    .ok()
                    .filter(|_| can_load_apparmor_profiles());
                fixes.push(match exe {
                    Some(exe) => apparmor_profile_fix(&exe.to_string_lossy()),
                    None => apparmor_userns_fix(),
                });
            }
            "cgroup v2" if check.status == Status::Failed => {
                fixes.push(delegation_fix());
            }
            // Only on the host: in a container the hierarchy is the host's,
            // and remounting it from inside is not this tool's to do.
            "cgroup moves" if check.status == Status::Degraded && !in_a_container() => {
                fixes.push(favordynmods_fix());
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

/// Where `zygo doctor --fix` writes the profile, and the name it loads under.
pub const APPARMOR_PROFILE_FILE: &str = "/etc/apparmor.d/zygo";

/// Let this binary, and only it, use user namespaces under Ubuntu's
/// restriction — the narrow fix, and the one Ubuntu itself uses for the
/// programs that need them (its own profiles for browsers and container
/// tools have the same shape).
///
/// The profile is `unconfined` apart from the one permission it adds, so it
/// takes nothing away from `zygo`, and it is attached by path, so it applies to
/// the binary that is running now. A binary installed somewhere else needs its
/// own; `doctor` says so when it finds the restriction again.
fn apparmor_profile_fix(exe: &str) -> Fix {
    Fix {
        check: "user namespaces",
        what: format!("an AppArmor profile that lets {exe} use user namespaces"),
        why: "Ubuntu 24.04 ships kernel.apparmor_restrict_unprivileged_userns=1. \
              It lets an unprivileged process create a user namespace and then \
              refuses the first mount inside it, which is the first thing every \
              sandbox does. A profile with the `userns` permission is how Ubuntu \
              lets a named program past it."
            .into(),
        cost: Some(format!(
            "only {exe} gains the permission; the restriction stays on for every \
             other process. The profile adds nothing else and removes nothing. \
             A zygo binary at another path needs its own — run `zygo doctor \
             --fix` with that one. `sudo apparmor_parser -R {APPARMOR_PROFILE_FILE} \
             && sudo rm {APPARMOR_PROFILE_FILE}` removes it."
        )),
        commands: vec![
            Command::root(&["tee", APPARMOR_PROFILE_FILE]).writing(apparmor_profile(exe)),
            Command::root(&["apparmor_parser", "-r", APPARMOR_PROFILE_FILE]),
        ],
    }
}

/// The profile itself. Public so the copy shipped in `packaging/apparmor/`
/// can be checked against it.
pub fn apparmor_profile(exe: &str) -> String {
    format!(
        "# Written by `zygo doctor --fix`.\n\
         #\n\
         # Ubuntu restricts unprivileged user namespaces\n\
         # (kernel.apparmor_restrict_unprivileged_userns=1), and every Zygo sandbox\n\
         # starts with one. This lets the zygo binary below, and nothing else, use\n\
         # them. It is unconfined otherwise: it adds one permission and takes none\n\
         # away. Remove with: apparmor_parser -R {APPARMOR_PROFILE_FILE}\n\
         \n\
         abi <abi/4.0>,\n\
         include <tunables/global>\n\
         \n\
         profile zygo \"{exe}\" flags=(unconfined) {{\n\
         \x20 userns,\n\
         \n\
         \x20 include if exists <local/zygo>\n\
         }}\n"
    )
}

/// Whether this host can take a profile: AppArmor 4's policy ABI (the one
/// with `userns`) and the tool that loads it.
fn can_load_apparmor_profiles() -> bool {
    std::path::Path::new("/etc/apparmor.d/abi/4.0").exists()
        && ["/usr/sbin/apparmor_parser", "/sbin/apparmor_parser"]
            .iter()
            .any(|p| std::path::Path::new(p).exists())
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
             through user namespaces; docs/book/23-security.md says what it was \
             protecting. The narrower fix, an AppArmor profile for the `zygo` \
             binary alone, is offered instead wherever AppArmor 4 and \
             apparmor_parser are installed; they are not here."
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

fn favordynmods_fix() -> Fix {
    let unit = format!("/etc/systemd/system/{FAVORDYNMODS_UNIT}");
    Fix {
        check: "cgroup moves",
        what: "mount cgroup2 with favordynmods, now and at every boot".into(),
        why: "from Linux 6.0, moving a process into a cgroup waits for an RCU grace \
              period after a quiet spell. A warm request forked by an agent is moved \
              into its own cgroup before it runs, so about 1 request in 100 waits \
              several milliseconds — measured at ~9 ms on a 6.8 VM, against 0.85 ms \
              with this option. Warm-exec and one-shot runs never move and are not \
              affected."
            .into(),
        cost: Some(
            "this is a setting of the whole machine's cgroup hierarchy, not only \
             Zygo's. It keeps the kernel's thread-group lock ready for writers, which \
             makes every fork and every exit on the machine take a slightly slower \
             path. It was the kernel's only behaviour before Linux 6.0. Disabling \
             the unit and rebooting restores the default."
                .into(),
        ),
        commands: vec![
            Command::root(&["mount", "-o", "remount,favordynmods", "/sys/fs/cgroup"]),
            // `tee` for the same reason as the sysctl file: no shell to start.
            Command::root(&["tee", &unit]).writing(favordynmods_unit_contents()),
            Command::root(&["systemctl", "daemon-reload"]),
            Command::root(&["systemctl", "enable", FAVORDYNMODS_UNIT]),
        ],
    }
}

/// What the boot-time unit should contain.
fn favordynmods_unit_contents() -> String {
    "# Written by `zygo doctor --fix`.\n\
     #\n\
     # From Linux 6.0, moving a process into a cgroup waits for an RCU grace\n\
     # period unless cgroup2 is mounted with favordynmods, and Zygo moves every\n\
     # warm request it forks. This remounts the hierarchy with the option at\n\
     # boot. Disable this unit and reboot to restore the kernel's default.\n\
     [Unit]\n\
     Description=Mount cgroup2 with favordynmods (Zygo)\n\
     DefaultDependencies=no\n\
     Before=sysinit.target\n\
     ConditionPathIsMountPoint=/sys/fs/cgroup\n\
     \n\
     [Service]\n\
     Type=oneshot\n\
     ExecStart=/bin/mount -o remount,favordynmods /sys/fs/cgroup\n\
     \n\
     [Install]\n\
     WantedBy=sysinit.target\n"
        .to_string()
}

#[cfg(target_os = "linux")]
fn in_a_container() -> bool {
    super::probe::in_a_container()
}

#[cfg(not(target_os = "linux"))]
fn in_a_container() -> bool {
    false
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
///
/// Only root may read that file — its mode says 0444, and the kernel refuses
/// everybody else anyway — and `doctor --fix` is usually run as the user who
/// wants sandboxes. Reading it was all this did, so on Ubuntu 24.04 the fix
/// was never offered to the person it is for. Unreadable, the answer comes
/// from the profile on disk instead: see [`pasta_profile_on_disk_enforces`].
fn pasta_is_enforced() -> bool {
    match std::fs::read_to_string("/sys/kernel/security/apparmor/profiles") {
        Ok(text) => loaded_profiles_enforce_pasta(&text),
        Err(_) => pasta_profile_on_disk_enforces(
            std::fs::read_to_string("/sys/module/apparmor/parameters/enabled").ok(),
            std::fs::read_to_string(PASST_PROFILE).ok(),
            std::path::Path::new(PASST_FORCE_COMPLAIN).exists(),
        ),
    }
}

/// The profile the `passt` package installs, which confines `pasta` too.
const PASST_PROFILE: &str = "/etc/apparmor.d/usr.bin.passt";
/// Where `aa-complain` leaves a marker for a profile it cannot edit in place.
const PASST_FORCE_COMPLAIN: &str = "/etc/apparmor.d/force-complain/usr.bin.passt";

fn loaded_profiles_enforce_pasta(text: &str) -> bool {
    text.lines().any(|line| {
        (line.contains("pasta") || line.contains("passt")) && line.contains("(enforce)")
    })
}

/// Whether the profile on disk will enforce: AppArmor is on, the package's
/// profile is there, and nothing has put it in complain mode — `aa-complain`
/// writes `flags=(complain)` into the file, or a marker beside it. What is
/// loaded can differ from the file until the next reload; the file is what the
/// next boot loads, so it is the better guess of the two a user can read.
fn pasta_profile_on_disk_enforces(
    enabled: Option<String>,
    profile: Option<String>,
    force_complain: bool,
) -> bool {
    let on = enabled.is_some_and(|v| v.trim() == "Y");
    let enforcing = profile.is_some_and(|p| !p.contains("complain"));
    on && enforcing && !force_complain
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
    fn slow_cgroup_moves_are_a_root_fix_that_names_its_cost() {
        let r = report(vec![Check::degraded(
            "cgroup moves",
            "without favordynmods",
            "…",
        )]);
        let fixes = plan(&r);
        if !cfg!(target_os = "linux") || in_a_container() {
            assert!(
                fixes.iter().all(|f| f.check != "cgroup moves"),
                "nothing to offer off Linux or inside a container"
            );
            return;
        }
        let fix = fixes
            .iter()
            .find(|f| f.check == "cgroup moves")
            .expect("a fix");
        assert!(fix.needs_root());
        assert!(
            fix.cost
                .as_deref()
                .is_some_and(|c| c.contains("whole machine")),
            "a host-wide change says so: {:?}",
            fix.cost
        );
        assert_eq!(
            fix.commands[0].argv,
            ["mount", "-o", "remount,favordynmods", "/sys/fs/cgroup"]
        );
    }

    #[test]
    fn favoured_cgroup_moves_need_nothing() {
        let r = report(vec![Check::ok("cgroup moves", "favordynmods")]);
        assert!(plan(&r).iter().all(|f| f.check != "cgroup moves"));
    }

    #[test]
    fn the_boot_unit_remounts_the_hierarchy_it_checks() {
        let unit = favordynmods_unit_contents();
        assert!(unit.contains("ExecStart=/bin/mount -o remount,favordynmods /sys/fs/cgroup"));
        assert!(unit.contains("WantedBy=sysinit.target"));
        assert!(unit.lines().all(|l| !l.starts_with(' ')), "{unit}");
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
    fn the_profile_names_the_binary_and_adds_only_userns() {
        let text = apparmor_profile("/opt/my tools/zygo");
        assert!(text.contains("profile zygo \"/opt/my tools/zygo\" flags=(unconfined) {"));
        assert!(text.contains("\n  userns,\n"), "{text}");
        assert!(text.contains("abi <abi/4.0>,"));
        let rules: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(
            rules,
            [
                "abi <abi/4.0>,",
                "include <tunables/global>",
                "profile zygo \"/opt/my tools/zygo\" flags=(unconfined) {",
                "userns,",
                "include if exists <local/zygo>",
                "}",
            ]
        );
        let fix = apparmor_profile_fix("/usr/local/bin/zygo");
        assert!(fix.needs_root());
        assert_eq!(fix.commands[0].argv, ["tee", APPARMOR_PROFILE_FILE]);
        assert_eq!(
            fix.commands[1].argv,
            ["apparmor_parser", "-r", APPARMOR_PROFILE_FILE]
        );
        assert!(
            fix.cost
                .as_deref()
                .is_some_and(|c| c.contains("stays on for every"))
        );
    }

    /// The profile in packaging/apparmor/ is this one, for the install path
    /// the book uses, so a package or an image can ship it as it is.
    #[test]
    fn the_shipped_profile_is_the_generated_one() {
        let shipped =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/apparmor/zygo");
        let Ok(text) = std::fs::read_to_string(&shipped) else {
            return; // a packaged crate has no packaging/ beside it
        };
        assert_eq!(text, apparmor_profile("/usr/local/bin/zygo"));
    }

    #[test]
    fn a_user_who_cannot_read_securityfs_is_still_told_about_pasta() {
        const SHIPPED: &str = "abi <abi/4.0>,\ninclude <tunables/global>\n\
                               profile passt /usr/bin/passt{,.avx2} {\n}\n";
        let yes = || Some("Y\n".to_string());
        assert!(pasta_profile_on_disk_enforces(
            yes(),
            Some(SHIPPED.into()),
            false
        ));
        // After `aa-complain`, in either of the two forms it leaves.
        let complained = SHIPPED.replace("{,.avx2} {", "{,.avx2} flags=(complain) {");
        assert!(!pasta_profile_on_disk_enforces(
            yes(),
            Some(complained),
            false
        ));
        assert!(!pasta_profile_on_disk_enforces(
            yes(),
            Some(SHIPPED.into()),
            true
        ));
        // No AppArmor, or no passt profile: nothing to fix.
        assert!(!pasta_profile_on_disk_enforces(
            Some("N".into()),
            Some(SHIPPED.into()),
            false
        ));
        assert!(!pasta_profile_on_disk_enforces(
            None,
            Some(SHIPPED.into()),
            false
        ));
        assert!(!pasta_profile_on_disk_enforces(yes(), None, false));
    }

    #[test]
    fn loaded_profiles_are_read_by_mode() {
        assert!(loaded_profiles_enforce_pasta(
            "passt (enforce)\nfoo (complain)\n"
        ));
        assert!(!loaded_profiles_enforce_pasta("passt (complain)\n"));
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
