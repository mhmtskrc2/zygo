//! Sandbox networking (design doc §3.8): `none`, `egress`, `full`, `host`.
//!
//! `none` is the default and needs nothing — the sandbox gets an empty network
//! namespace with a loopback interface, which the launcher brings up itself.
//! This module is about the other two namespaced modes, where the sandbox has
//! to reach the outside world without Zygo gaining a single privilege.
//!
//! Three external programs, all standard in the rootless-container world:
//!
//! * **`pasta`** (from `passt`) moves packets between the sandbox's network
//!   namespace and the host, in userspace, as the ordinary user. It copies the
//!   host's addresses and routes into the namespace, so the sandbox sees a
//!   plausible network rather than a translated one. The root-only
//!   alternative in the design — a veth pair — is not reachable rootless,
//!   which is P5.
//! * **`nft`** installs the allowlist *inside* the sandbox's network
//!   namespace. Zygo has no capabilities on the host, but it created the
//!   sandbox's user namespace, so entering that namespace grants a full
//!   capability set inside it — and the network namespace is owned by it.
//!   This is the same property [`crate::backend::ns::enter`] relies on.
//! * **`tc`** (from `iproute2`), only when a `bandwidth` is set: a token
//!   bucket on what the sandbox sends, and — where the host has an `ifb`
//!   device — the same on what it receives.
//!
//! None is invoked through a shell and none is given a tenant-controlled
//! string: the ruleset is generated here and handed to `nft` on stdin.
//!
//! **Names.** Under `egress` the sandbox's resolver is Zygo's own
//! ([`dns`]), bound inside the namespace: a name the allowlist does not cover
//! does not resolve, and a name it does cover has its addresses added to the
//! filter's sets before the answer is sent. That is what makes a wildcard rule
//! enforceable, and what stops a service changing address from taking a
//! function down. Under `full` there is no allowlist, and `pasta`'s own
//! forwarder answers.
//!
//! **Fail closed.** If a program is missing, or any step fails, the sandbox
//! does not start. A sandbox whose allowlist could not be installed would have
//! the host's whole network, which is the opposite of what the spec asked for.
//!
//! `pasta` never enters the sandbox's *mount* namespace: it is given
//! `--netns`/`--userns` paths rather than a pid. Given a pid it would also
//! join the mount namespace to rewrite `/etc/resolv.conf` — and at the moment
//! it runs, the sandbox's mount namespace is still a copy of the host's, so
//! that write would land on the host's own file. Zygo supplies
//! `/etc/resolv.conf` as a read-only mount instead.

pub mod dns;

use std::net::IpAddr;
use std::path::{Path, PathBuf};

use crate::error::{Error, IoContext, Result};
use crate::spec::{AllowRule, HostPattern, Network};

/// Where a `full` sandbox sends DNS. `pasta` intercepts this address and
/// forwards to the host's own resolver.
///
/// Link-local on purpose: it is not a real destination, it cannot collide with
/// anything the tenant might legitimately reach, and it is inside the range the
/// ruleset rejects — the accept for it sits above that reject, so DNS works and
/// the rest of 169.254/16 stays unreachable.
pub const DNS_ADDR: &str = "169.254.1.1";

/// Where an `egress` sandbox sends DNS: Zygo's resolver, on the namespace's
/// own loopback. Loopback because `resolv.conf` cannot name a port, so the
/// resolver has to be on port 53 of an address only this sandbox can reach.
pub const PROXY_ADDR: &str = "127.0.0.53";

/// The tap interface `pasta` creates inside the namespace. Named explicitly so
/// `tc` has something to name; the default follows the host's interface,
/// which is anyone's guess.
pub const SANDBOX_IFNAME: &str = "eth0";

/// How long an address a query admitted stays in the filter's sets.
///
/// Longer than any answer's TTL ([`dns::ANSWER_TTL`]) by a wide margin, so a
/// connection made on a cached answer is still permitted; short enough that an
/// address a service has moved away from does not stay open for ever.
pub const ADMIT_TIMEOUT_SECS: u64 = 600;

/// How often an address still in use is re-added, refreshing its timeout.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const ADMIT_REFRESH: std::time::Duration = std::time::Duration::from_secs(120);

/// What Zygo writes into the sandbox's `/etc/resolv.conf`.
///
/// One nameserver, no search domains, no options: whatever the host's resolver
/// is configured with stays on the host, where a tenant cannot read it.
pub fn resolv_conf(network: Network) -> String {
    let (server, note) = match network {
        Network::Egress => (
            PROXY_ADDR,
            "Zygo's own resolver: only names on the allowlist resolve.",
        ),
        _ => (DNS_ADDR, "Queries are forwarded to the host's resolver."),
    };
    format!("# Written by Zygo. {note}\nnameserver {server}\n")
}

/// Path of the `resolv.conf` Zygo mounts into a sandbox of this mode.
///
/// One file per mode for the whole data root rather than one per function:
/// its contents do not depend on the function, and a file that every sandbox
/// binds read-only is cheaper to reason about than one per tenant.
pub fn resolv_conf_path(paths: &crate::paths::Paths, network: Network) -> PathBuf {
    paths
        .data()
        .join("etc")
        .join(format!("resolv-{network}.conf"))
}

/// Write the shared `resolv.conf` if it is not already there. Idempotent.
pub fn ensure_resolv_conf(paths: &crate::paths::Paths, network: Network) -> Result<PathBuf> {
    let path = resolv_conf_path(paths, network);
    let wanted = resolv_conf(network);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(wanted.as_str()) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).at(parent)?;
        }
        std::fs::write(&path, wanted).at(&path)?;
    }
    Ok(path)
}

/// Whether this mode needs `pasta` and a ruleset.
pub fn needs_configuration(network: Network) -> bool {
    matches!(network, Network::Egress | Network::Full)
}

/// Blocks that reach the host and its neighbours. Rejected before any allow
/// rule unless `--allow-private-net` was given (design doc §3.10).
const PRIVATE_V4: &[&str] = &[
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
    "127.0.0.0/8",
    "100.64.0.0/10",
];

const PRIVATE_V6: &[&str] = &["::1/128", "fc00::/7", "fe80::/10"];

/// The allowlist's addresses known before the sandbox starts.
///
/// Exact names are resolved once here, on the host, so a client that connects
/// to a literal address the list names — or that has the name cached from
/// elsewhere — is permitted from the first packet. Wildcards contribute
/// nothing here: they are matched by the resolver as the sandbox asks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Allowed {
    /// `(address, port)`, with `None` meaning every port.
    pub entries: Vec<(IpAddr, Option<u16>)>,
    /// Exact names that resolved to nothing right now. Under `egress` the
    /// resolver tries again whenever the sandbox asks, so this is a note, not
    /// a failure.
    pub unresolved: Vec<String>,
}

/// Resolve every exact-name and CIDR rule into addresses.
pub fn resolve_allow(rules: &[AllowRule]) -> Allowed {
    let mut allowed = Allowed::default();
    for rule in rules {
        match &rule.host {
            HostPattern::Cidr(c) => allowed.entries.push((c.addr, rule.port)),
            HostPattern::Exact(name) => {
                let addrs = dns::resolve_on_host(name);
                if addrs.is_empty() {
                    allowed.unresolved.push(name.clone());
                }
                for ip in addrs {
                    allowed.entries.push((ip, rule.port));
                }
            }
            HostPattern::Wildcard(_) => {}
        }
    }
    allowed.entries.sort();
    allowed.entries.dedup();
    allowed
}

/// A CIDR rule keeps its prefix; a resolved name is a single address.
fn cidr_of(rule: &AllowRule) -> Option<String> {
    match &rule.host {
        HostPattern::Cidr(c) => Some(c.to_string()),
        _ => None,
    }
}

/// The named set an address belongs in: by family, and by whether the rule
/// names a port.
fn set_name(ip: IpAddr, port: Option<u16>) -> &'static str {
    match (ip.is_ipv4(), port.is_some()) {
        (true, true) => "allow4",
        (true, false) => "allow4any",
        (false, true) => "allow6",
        (false, false) => "allow6any",
    }
}

/// One element of a named set, as `nft` writes it.
fn element(ip: IpAddr, port: Option<u16>) -> String {
    match port {
        Some(p) => format!("{ip} . {p}"),
        None => ip.to_string(),
    }
}

/// Build the ruleset for one sandbox.
///
/// Pure, so the policy can be read and tested without a kernel. Order is the
/// policy:
///
/// 1. loopback, so a sandbox can always talk to itself — and, under `egress`,
///    to its resolver, which lives there;
/// 2. established and related, so replies to permitted connections come back;
/// 3. the connection count: a new connection past `connections` is refused;
/// 4. under `full`, DNS to [`DNS_ADDR`] — any other resolver is a way around
///    the policy. Under `egress` no such line exists: the only resolver is on
///    loopback, already permitted, and `pasta`'s forwarder is deliberately
///    *not* reachable;
/// 5. the private and link-local rejects, unless `allow_private`;
/// 6. what the spec allowed — CIDRs as written, names through the sets the
///    resolver fills;
/// 7. reject, with ICMP rather than a silent drop so a blocked program fails
///    immediately instead of waiting out a connect timeout.
pub fn ruleset(
    network: Network,
    rules: &[AllowRule],
    allowed: &Allowed,
    allow_private: bool,
    connections: u32,
) -> String {
    let mut out = String::from("table inet zygo {\n");

    if network == Network::Egress {
        // Four sets rather than one: a concatenation with a port and a bare
        // address are different types, and so are the two families.
        for (name, ty) in [
            ("allow4", "ipv4_addr . inet_service"),
            ("allow4any", "ipv4_addr"),
            ("allow6", "ipv6_addr . inet_service"),
            ("allow6any", "ipv6_addr"),
        ] {
            let elements: Vec<String> = allowed
                .entries
                .iter()
                .filter(|(ip, port)| set_name(*ip, *port) == name)
                .map(|(ip, port)| element(*ip, *port))
                .collect();
            out.push_str(&format!(
                "  set {name} {{\n    type {ty};\n    flags timeout;\n"
            ));
            if !elements.is_empty() {
                out.push_str(&format!("    elements = {{ {} }}\n", elements.join(", ")));
            }
            out.push_str("  }\n");
        }
    }
    // `ct count` needs a dynamic set to count into, one per family.
    out.push_str("  set conn4 {\n    type ipv4_addr;\n    flags dynamic;\n  }\n");
    out.push_str("  set conn6 {\n    type ipv6_addr;\n    flags dynamic;\n  }\n");

    out.push_str("  chain output {\n");
    out.push_str("    type filter hook output priority 0; policy drop;\n");
    out.push_str("    oifname \"lo\" accept\n");
    out.push_str("    ct state established,related accept\n");
    // TCP only: a UDP "connection" is a conntrack entry per query, and a
    // resolver would eat the budget. A reset rather than ICMP, so the program
    // sees `ECONNREFUSED` at once instead of waiting on a route error.
    out.push_str(&format!(
        "    meta nfproto ipv4 meta l4proto tcp ct state new \
         add @conn4 {{ ip saddr ct count over {connections} }} reject with tcp reset\n"
    ));
    out.push_str(&format!(
        "    meta nfproto ipv6 meta l4proto tcp ct state new \
         add @conn6 {{ ip6 saddr ct count over {connections} }} reject with tcp reset\n"
    ));

    if network != Network::Egress {
        out.push_str(&format!("    ip daddr {DNS_ADDR} udp dport 53 accept\n"));
        out.push_str(&format!("    ip daddr {DNS_ADDR} tcp dport 53 accept\n"));
    }

    if !allow_private {
        out.push_str(&format!(
            "    ip daddr {{ {} }} reject with icmp type admin-prohibited\n",
            PRIVATE_V4.join(", ")
        ));
        out.push_str(&format!(
            "    ip6 daddr {{ {} }} reject with icmpv6 type admin-prohibited\n",
            PRIVATE_V6.join(", ")
        ));
    }

    match network {
        // Unrestricted egress: everything above still applies, so `full` is
        // "no allowlist", not "no policy".
        Network::Full => out.push_str("    accept\n"),
        _ => {
            // CIDR rules go in as written, so a `/8` stays a `/8` rather than
            // being flattened into the one address it was parsed from.
            for rule in rules {
                if let Some(cidr) = cidr_of(rule) {
                    let family = if cidr.contains(':') { "ip6" } else { "ip" };
                    out.push_str(&format!("    {family} daddr {cidr}"));
                    if let Some(port) = rule.port {
                        out.push_str(&format!(" tcp dport {port}"));
                    }
                    out.push_str(" accept\n");
                }
            }
            out.push_str("    ip daddr . tcp dport @allow4 accept\n");
            out.push_str("    ip daddr @allow4any accept\n");
            out.push_str("    ip6 daddr . tcp dport @allow6 accept\n");
            out.push_str("    ip6 daddr @allow6any accept\n");
        }
    }

    out.push_str("    reject with icmp type admin-prohibited\n");
    out.push_str("  }\n}\n");
    out
}

/// What a networked function needs beyond its resolved spec.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    /// The `/etc/resolv.conf` to bind into the sandbox. `None` when the mode
    /// needs no configuration.
    pub mount: Option<crate::spec::Mount>,
    pub allowed: Allowed,
    pub pid_file: Option<PathBuf>,
    /// Non-fatal notes for the caller to print.
    pub warnings: Vec<String>,
}

/// Prepare the host-side half of a sandbox's network.
///
/// Resolution happens here, on the host, before the sandbox exists — so what
/// the filter permits from the first packet cannot be influenced from inside
/// it. A name that does not resolve now is a warning rather than an error: the
/// resolver tries again whenever the sandbox asks.
pub fn setup(
    paths: &crate::paths::Paths,
    tenant: &str,
    f: &crate::spec::ResolvedFn,
) -> Result<Setup> {
    if !needs_configuration(f.network) {
        return Ok(Setup::default());
    }
    // Before anything is built, so a host without the programs says so once.
    availability(f.limits.bandwidth.is_some())?;

    let allowed = resolve_allow(&f.allow);
    let mut warnings = Vec::new();
    if !allowed.unresolved.is_empty() {
        warnings.push(format!(
            "fn.{}.allow: {} did not resolve just now; the sandbox's resolver will try \
             again when asked",
            f.name,
            allowed.unresolved.join(", "),
        ));
    }

    Ok(Setup {
        mount: Some(crate::spec::Mount {
            source: ensure_resolv_conf(paths, f.network)?,
            target: PathBuf::from("/etc/resolv.conf"),
            mode: crate::spec::MountMode::Ro,
        }),
        allowed,
        pid_file: Some(pid_file(paths, tenant)),
        warnings,
    })
}

/// A pid file nothing else will claim.
///
/// Unique per sandbox rather than per name: a replaced function has two
/// sandboxes alive at once (blue/green), and a shared path would mean stopping
/// the old one killed the new one's `pasta`.
fn pid_file(paths: &crate::paths::Paths, tenant: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    paths
        .runtime()
        .join("tenants")
        .join(tenant)
        .join(format!("pasta-{}-{n}.pid", crate::process_token()))
}

/// How long `pasta` gets to configure a sandbox's network. It takes tens of
/// milliseconds; the rest is room for a host under load, and the bound is what
/// matters — past it the launch fails with what `pasta` said.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const PASTA_START_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// The device `pasta` gives a sandbox its interface through.
pub const TUN_DEVICE: &str = "/dev/net/tun";

/// Why a host with no tun device cannot give a sandbox a network, and what to
/// do about it. Shared with `zygo doctor`, which asks the same question.
pub fn tun_remedy() -> String {
    format!(
        "`pasta` needs {TUN_DEVICE}. A container runtime does not create it: \
         docker run --device {TUN_DEVICE}, or in Kubernetes mount the node's \
         {TUN_DEVICE} as a hostPath volume of type CharDevice (runc and crun allow \
         the device by default; they only leave the node out). On a host, \
         `sudo modprobe tun`. Or use `network = \"none\"`, which needs none of it"
    )
}

/// The programs on `PATH`, with the reason when one is not.
#[derive(Debug, Clone)]
pub struct Programs {
    pub pasta: PathBuf,
    pub nft: PathBuf,
    /// `tc` and `ip`, both from `iproute2`. Only required by a bandwidth limit.
    pub tc: Option<PathBuf>,
    pub ip: Option<PathBuf>,
}

/// Whether the programs a networked sandbox needs are all on `PATH`.
pub fn availability(with_bandwidth: bool) -> std::result::Result<Programs, Error> {
    let pasta = which("pasta").ok_or_else(|| missing("pasta", "passt"))?;
    let nft = which("nft").ok_or_else(|| missing("nft", "nftables"))?;
    // Checked here rather than left to `pasta`, which on a host without it
    // prints "Failed to set up tap device in namespace" and — in passt
    // 2025_01 — does not exit. Only on Linux: elsewhere there is no `pasta` to
    // find either, and the error above has already said so.
    if cfg!(target_os = "linux") && !Path::new(TUN_DEVICE).exists() {
        return Err(Error::BackendUnavailable {
            backend: "network",
            reason: format!("{TUN_DEVICE} does not exist on this host"),
            remedy: tun_remedy(),
        });
    }
    let (tc, ip) = if with_bandwidth {
        (
            Some(which("tc").ok_or_else(|| missing("tc", "iproute2"))?),
            Some(which("ip").ok_or_else(|| missing("ip", "iproute2"))?),
        )
    } else {
        (which("tc"), which("ip"))
    };
    Ok(Programs { pasta, nft, tc, ip })
}

fn missing(binary: &'static str, package: &str) -> Error {
    Error::BackendUnavailable {
        backend: "network",
        reason: format!("`{binary}` is not on PATH, so a networked sandbox cannot be confined"),
        remedy: format!(
            "install it (Debian/Ubuntu: `sudo apt install {package}`; Fedora: \
             `sudo dnf install {package}`), or use `network = \"none\"`"
        ),
    }
}

fn which(binary: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(binary))
            .find(|p| p.is_file())
    })
}

/// The last non-empty line of a program's stderr, for an error message.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn last_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("no output")
        .trim()
        .to_string()
}

/// Stop the `pasta` that was started for a sandbox.
///
/// `pasta` outlives the process it was pointed at — it holds the namespace
/// open — so something has to end it. Called when the sandbox is torn down;
/// a pid file that is not there means it already exited.
pub fn stop_pasta(pid_file: &Path) {
    let pid: i32 = match std::fs::read_to_string(pid_file) {
        Ok(text) => match text.trim().parse() {
            Ok(pid) => pid,
            Err(_) => return,
        },
        Err(_) => return,
    };
    if pid > 1 {
        // SAFETY: `kill` with a positive pid and SIGTERM; the worst case for a
        // recycled pid is a signal this user could already send.
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let _ = std::fs::remove_file(pid_file);
}

/// The egress resolver's thread, alive as long as the sandbox.
///
/// Dropping it stops the thread. Nothing else has to: the socket it serves is
/// bound inside the sandbox's namespace and goes with it.
pub struct DnsProxy {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for DnsProxy {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::configure;

#[cfg(target_os = "linux")]
pub(crate) mod linux {
    use std::io::Write;
    use std::net::{IpAddr, UdpSocket};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use std::time::Instant;

    use super::*;

    /// The two namespaces every network operation enters: user first, because
    /// it is what grants the capability to enter the network namespace it owns.
    pub struct NetNs {
        user: OwnedFd,
        net: OwnedFd,
    }

    impl NetNs {
        fn open(pid: u32) -> Result<NetNs> {
            let open = |kind: &str| -> Result<OwnedFd> {
                let path = PathBuf::from(format!("/proc/{pid}/ns/{kind}"));
                Ok(OwnedFd::from(std::fs::File::open(&path).at(&path)?))
            };
            Ok(NetNs {
                user: open("user")?,
                net: open("net")?,
            })
        }
    }

    /// Configure a started sandbox's network namespace.
    ///
    /// Called by the launcher after the id maps are written — `pasta` enters
    /// the user namespace, so it needs a mapping to enter as — and before the
    /// sandbox is told to proceed, so no tenant code ever runs with the
    /// namespace unconfined.
    ///
    /// Returns the resolver's handle for an `egress` sandbox; the caller keeps
    /// it for the sandbox's lifetime.
    #[allow(clippy::too_many_arguments)]
    pub fn configure(
        pid: u32,
        network: Network,
        rules: &[AllowRule],
        allowed: &Allowed,
        allow_private: bool,
        limits: &crate::sandbox::Limits,
        pasta_pid_file: &Path,
    ) -> Result<Option<DnsProxy>> {
        let programs = availability(limits.bandwidth.is_some())?;
        let ns = NetNs::open(pid)?;

        start_pasta(&programs.pasta, pid, pasta_pid_file)?;

        let text = ruleset(network, rules, allowed, allow_private, limits.connections);
        let output = run_in_namespace(&ns, &programs.nft, &["-f", "-"], Some(&text))?;
        if !output.status.success() {
            return Err(Error::BackendUnavailable {
                backend: "network",
                reason: format!(
                    "the egress allowlist could not be installed: {}",
                    last_line(&output.stderr)
                ),
                remedy: "this kernel may lack nftables in a user namespace; \
                         use `network = \"none\"` until it is available"
                    .into(),
            });
        }

        if let Some(bandwidth) = limits.bandwidth {
            let tc = programs.tc.as_deref().expect("checked by availability");
            let ip = programs.ip.as_deref().expect("checked by availability");
            if !apply_bandwidth(&ns, tc, ip, bandwidth.get())? {
                tracing::warn!(
                    "bandwidth limit shapes what the sandbox sends only: this host has no \
                     `ifb` device, so what it receives is not shaped (load the `ifb` module \
                     to shape both directions)"
                );
            }
        }

        if network == Network::Egress {
            return Ok(Some(start_resolver(
                ns,
                programs.nft,
                rules.to_vec(),
                allow_private,
            )?));
        }
        Ok(None)
    }

    /// Hand the namespace to `pasta`.
    ///
    /// `--netns`/`--userns` rather than a pid: see the module documentation
    /// for why the mount namespace must stay out of this. `pasta` backgrounds
    /// itself once the namespace is configured, so the process started here
    /// exits when the sandbox can reach the network — the pid file is what
    /// lets the sandbox take the daemon down again.
    ///
    /// Waited for with a deadline, and only for the process — never for its
    /// output to end. This used to be `Command::output()`, which returns when
    /// `pasta` closes its stdout and stderr, and a `pasta` that fails without
    /// exiting never does: passt 2025_01 in a container with no
    /// `/dev/net/tun` prints "Failed to set up tap device in namespace" and
    /// then sits in its event loop. The launch blocked for ever with the
    /// sandbox's child parked on its ready pipe, and `--timeout` — enforced
    /// by the same blocked parent — never fired.
    fn start_pasta(pasta: &Path, pid: u32, pid_file: &Path) -> Result<()> {
        start_pasta_within(pasta, pid, pid_file, PASTA_START_DEADLINE)
    }

    pub(super) fn start_pasta_within(
        pasta: &Path,
        pid: u32,
        pid_file: &Path,
        within: std::time::Duration,
    ) -> Result<()> {
        use std::process::Stdio;

        if let Some(parent) = pid_file.parent() {
            std::fs::create_dir_all(parent).at(parent)?;
        }
        // A stale file from a previous sandbox would be read as this one's.
        let _ = std::fs::remove_file(pid_file);

        // SAFETY: neither call takes an argument or can fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };

        let mut child = std::process::Command::new(pasta)
            .arg("--config-net")
            .arg("--quiet")
            // Keep our identity. Started as root, `pasta` drops to `nobody`
            // before it opens the namespace files — and `nobody` may not read
            // `/proc/<pid>/ns/user`, so it fails with `Permission denied`.
            // Zygo is rootless by design and then this is a no-op, but it
            // runs as root in containers and CI often enough that the default
            // is a trap.
            .arg("--runas")
            .arg(format!("{uid}:{gid}"))
            // No inbound forwarding at all: the design gives `egress` and
            // `full` egress only, and a listening port would be reachable
            // from the host.
            .args(["--tcp-ports", "none"])
            .args(["--udp-ports", "none"])
            .args(["--dns-forward", DNS_ADDR])
            .args(["--ns-ifname", SANDBOX_IFNAME])
            .arg("--pid")
            .arg(pid_file)
            .arg("--userns")
            .arg(format!("/proc/{pid}/ns/user"))
            .arg("--netns")
            .arg(format!("/proc/{pid}/ns/net"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::primitive("start pasta", "could not run `pasta`", e))?;

        let deadline = Instant::now() + within;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(_)) => {
                    let said = drain_last_line(child.stderr.take());
                    return Err(Error::BackendUnavailable {
                        backend: "network",
                        reason: format!("pasta could not configure the sandbox's network: {said}"),
                        remedy: pasta_remedy(&said),
                    });
                }
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let said = drain_last_line(child.stderr.take());
                    return Err(Error::BackendUnavailable {
                        backend: "network",
                        reason: format!(
                            "pasta did not finish configuring the sandbox's network within {} s \
                             and was stopped; it said: {said}",
                            within.as_secs()
                        ),
                        remedy: pasta_remedy(&said),
                    });
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(2)),
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::primitive(
                        "wait for pasta",
                        "could not wait for `pasta`",
                        e,
                    ));
                }
            }
        }
    }

    /// What `pasta` wrote to stderr before it exited or was stopped, without
    /// waiting for more: the pipe is read non-blocking, so a daemon that kept
    /// it open cannot hold the launch up the way it did through `output()`.
    fn drain_last_line(stderr: Option<std::process::ChildStderr>) -> String {
        use std::io::Read;

        let Some(mut stderr) = stderr else {
            return "no output".to_string();
        };
        // SAFETY: `fcntl` on a descriptor this function owns.
        unsafe {
            let fd = stderr.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
        let mut said = Vec::new();
        let mut chunk = [0u8; 4096];
        while said.len() < 64 * 1024 {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => said.extend_from_slice(&chunk[..n]),
            }
        }
        last_line(&said)
    }

    /// Run a host program inside the sandbox's user and network namespaces.
    ///
    /// The process enters the user namespace — which Zygo created, and
    /// therefore holds every capability in — and then the network namespace,
    /// where that capability set is what the program needs. Nothing is
    /// granted on the host, and the program is the host's own binary, run from
    /// the host's filesystem: the mount namespace is not entered.
    pub fn run_in_namespace(
        ns: &NetNs,
        program: &Path,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<std::process::Output> {
        use std::os::unix::process::CommandExt;

        let (user, net) = (ns.user.as_raw_fd(), ns.net.as_raw_fd());
        let mut command = std::process::Command::new(program);
        command
            .args(args)
            .stdin(if stdin.is_some() {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // SAFETY: between `fork` and `execve`, so this process is
        // single-threaded — which `setns` into a user namespace requires and
        // the supervisor itself could never satisfy. Every call is
        // async-signal-safe and nothing allocates.
        unsafe {
            command.pre_exec(move || {
                for fd in [user, net] {
                    if libc::setns(fd, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                keep_net_admin_across_exec()
            });
        }
        let name = program
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut child = command.spawn().map_err(|e| {
            Error::primitive("run in namespace", "could not run a network program", e)
        })?;
        if let Some(text) = stdin {
            child
                .stdin
                .take()
                .expect("piped")
                .write_all(text.as_bytes())
                .map_err(|e| Error::primitive("write", "internal networking error", e))?;
        }
        child.wait_with_output().map_err(|e| {
            Error::primitive(
                "run in namespace",
                format!("`{name}` did not finish").leak(),
                e,
            )
        })
    }

    /// Carry `CAP_NET_ADMIN` through an `execve`.
    ///
    /// Entering the sandbox's user namespace grants a full capability set
    /// inside it — but `execve` recomputes capabilities, and it keeps them
    /// only for uid 0. Inside the sandbox's namespace this process is the
    /// *mapped* uid (1000 by default), not 0, and uid 0 there may not be
    /// mapped to anything at all when there is no subordinate range — so
    /// becoming root first is not available either. The ambient set is what
    /// survives `execve` for an unprivileged uid: a capability may be raised
    /// there when it is already permitted and inheritable, which is exactly
    /// the state `setns` just produced.
    ///
    /// Without this, `nft` starts with an empty set and reports
    /// `cache initialization failed: Operation not permitted` — a message
    /// that reads like a missing kernel feature rather than a lost capability.
    fn keep_net_admin_across_exec() -> std::io::Result<()> {
        const LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;
        const CAP_NET_ADMIN: u32 = 12;
        const CAP_NET_RAW: u32 = 13;

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

        let header = CapHeader {
            version: LINUX_CAPABILITY_VERSION_3,
            pid: 0,
        };
        let mut data = [CapData::default(); 2];
        // SAFETY: both structures are the shapes `capget`/`capset` expect and
        // outlive the calls.
        if unsafe { libc::syscall(libc::SYS_capget, &header, data.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let wanted = (1 << CAP_NET_ADMIN) | (1 << CAP_NET_RAW);
        data[0].inheritable |= data[0].permitted & wanted;
        // SAFETY: as above.
        if unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for cap in [CAP_NET_ADMIN, CAP_NET_RAW] {
            // SAFETY: `prctl` with constant arguments.
            let rc = unsafe {
                libc::prctl(
                    libc::PR_CAP_AMBIENT,
                    libc::PR_CAP_AMBIENT_RAISE,
                    libc::c_ulong::from(cap),
                    0,
                    0,
                )
            };
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    /// Shape the sandbox's interface with `tc`. Returns whether what the
    /// sandbox *receives* was shaped too.
    ///
    /// What the sandbox sends is a token bucket (`tbf`) on the interface's
    /// root qdisc: measured on `pasta`, 500 KB at 100 KB/s takes the 4.9 s
    /// the arithmetic says. What it receives is harder. The obvious tool, an
    /// ingress policer that drops packets over the rate, makes `pasta` reset
    /// the connection — it is a userspace TCP stack, and it treats that much
    /// loss on its tap side as a dead peer, whatever the burst. Queueing
    /// instead of dropping needs an `ifb` device to redirect ingress through,
    /// and that is a kernel module a host may not have loaded — and cannot
    /// autoload from inside a user namespace. So: `ifb` when the host has it,
    /// and an honest warning when it does not, rather than a limit that
    /// silently breaks every download.
    fn apply_bandwidth(ns: &NetNs, tc: &Path, ip: &Path, bytes_per_second: u64) -> Result<bool> {
        let rate = format!("{}bit", bytes_per_second.saturating_mul(8));
        // A tenth of a second's worth, and never below what a single full
        // frame needs: a bucket smaller than a packet never passes one.
        let burst = format!("{}b", (bytes_per_second / 10).max(32 * 1024));
        let dev = SANDBOX_IFNAME;
        let shape = |device: &str| -> Vec<String> {
            [
                "qdisc", "add", "dev", device, "root", "tbf", "rate", &rate, "burst", &burst,
                "latency", "400ms",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect()
        };
        let tc_run = |args: Vec<String>| -> Result<()> {
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            let output = run_in_namespace(ns, tc, &refs, None)?;
            if output.status.success() {
                Ok(())
            } else {
                Err(Error::BackendUnavailable {
                    backend: "network",
                    reason: format!(
                        "the bandwidth limit could not be applied (`tc {}`): {}",
                        args.join(" "),
                        last_line(&output.stderr)
                    ),
                    remedy: "this kernel may lack the `sch_tbf` or `sch_ingress` modules; \
                             remove `bandwidth` until they are available"
                        .into(),
                })
            }
        };

        // Sent: always.
        tc_run(shape(dev))?;

        // Received: only through an `ifb`, and only if this host can make one.
        let made = run_in_namespace(ns, ip, &["link", "add", "ifb0", "type", "ifb"], None)?;
        if !made.status.success() {
            tracing::debug!("no ifb device: {}", last_line(&made.stderr));
            return Ok(false);
        }
        let up = run_in_namespace(ns, ip, &["link", "set", "ifb0", "up"], None)?;
        if !up.status.success() {
            return Err(Error::BackendUnavailable {
                backend: "network",
                reason: format!("could not bring ifb0 up: {}", last_line(&up.stderr)),
                remedy: "remove `bandwidth` until the host's `ifb` device works".into(),
            });
        }
        let strings =
            |items: &[&str]| -> Vec<String> { items.iter().map(|s| s.to_string()).collect() };
        tc_run(strings(&[
            "qdisc", "add", "dev", dev, "handle", "ffff:", "ingress",
        ]))?;
        tc_run(strings(&[
            "filter", "add", "dev", dev, "parent", "ffff:", "matchall", "action", "mirred",
            "egress", "redirect", "dev", "ifb0",
        ]))?;
        tc_run(shape("ifb0"))?;
        Ok(true)
    }

    /// Bind a UDP socket inside the sandbox's network namespace and bring it
    /// back here.
    ///
    /// A socket belongs to the namespace it was created in, for ever, whoever
    /// holds it. So a helper process enters the namespaces, creates and binds
    /// the socket, hands it back over a socketpair with `SCM_RIGHTS`, and
    /// exits; what this returns is a socket on the sandbox's loopback that the
    /// supervisor can serve from its own thread. A thread cannot do this
    /// alone: entering a user namespace needs a single-threaded process.
    ///
    /// Port 53 needs `CAP_NET_BIND_SERVICE` in the namespace, which entering
    /// its user namespace grants.
    fn bind_in_namespace(ns: &NetNs, addr: std::net::SocketAddrV4) -> Result<UdpSocket> {
        let mut pair = [0 as RawFd; 2];
        // SAFETY: a plain socketpair into a live array.
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        } != 0
        {
            return Err(Error::primitive(
                "socketpair",
                "internal networking error",
                std::io::Error::last_os_error(),
            ));
        }
        // SAFETY: both ends are fresh descriptors this function owns.
        let (ours, theirs) =
            unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };

        let sockaddr = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: addr.port().to_be(),
            sin_addr: libc::in_addr {
                s_addr: u32::from(*addr.ip()).to_be(),
            },
            sin_zero: [0; 8],
        };
        let (user, net) = (ns.user.as_raw_fd(), ns.net.as_raw_fd());
        let their_fd = theirs.as_raw_fd();

        // SAFETY: the child calls only async-signal-safe functions — `setns`,
        // `socket`, `bind`, `sendmsg`, `_exit` — and allocates nothing; every
        // value it touches was computed before the fork.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(Error::primitive(
                "fork",
                "internal networking error",
                std::io::Error::last_os_error(),
            ));
        }
        if pid == 0 {
            // The child. Any failure is an exit code; the parent reports it.
            unsafe {
                if libc::setns(user, 0) != 0 || libc::setns(net, 0) != 0 {
                    libc::_exit(2);
                }
                let sock = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
                if sock < 0 {
                    libc::_exit(3);
                }
                if libc::bind(
                    sock,
                    (&sockaddr as *const libc::sockaddr_in).cast(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                ) != 0
                {
                    libc::_exit(4);
                }
                if send_fd(their_fd, sock).is_err() {
                    libc::_exit(5);
                }
                libc::_exit(0);
            }
        }

        drop(theirs);
        let received = recv_fd(ours.as_raw_fd());
        let mut status = 0;
        // SAFETY: waiting for the child this function forked.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let code = if libc::WIFEXITED(status) {
            libc::WEXITSTATUS(status)
        } else {
            -1
        };
        match (received, code) {
            (Ok(fd), 0) => Ok(UdpSocket::from(fd)),
            (_, code) => Err(Error::BackendUnavailable {
                backend: "network",
                reason: format!(
                    "could not bind the resolver at {addr} inside the sandbox (helper step {code})"
                ),
                remedy: "this is a Zygo bug or a kernel that refuses `setns`; \
                         use `network = \"full\"` or `\"none\"` meanwhile"
                    .into(),
            }),
        }
    }

    /// `SCM_RIGHTS`: one descriptor over a unix socket, one byte of payload.
    ///
    /// Shared with the launcher's clone child, which uses it to hand out
    /// `/run/secrets` — the same constraints apply there, and one careful
    /// implementation is better than two.
    ///
    /// # Safety
    /// Called in a forked child; touches only the stack.
    pub(crate) unsafe fn send_fd(sock: RawFd, fd: RawFd) -> std::io::Result<()> {
        let mut payload = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: payload.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut control = [0u8; 32];
        // SAFETY: `msghdr` is plain data; every pointer is to a live local.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(4) as _;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(4) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
            if libc::sendmsg(sock, &msg, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub(crate) fn recv_fd(sock: RawFd) -> std::io::Result<OwnedFd> {
        let mut payload = [0u8; 1];
        let mut iov = libc::iovec {
            iov_base: payload.as_mut_ptr().cast(),
            iov_len: 1,
        };
        let mut control = [0u8; 32];
        // SAFETY: as in `send_fd`; the descriptor read out is one the kernel
        // just installed in this process.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(4) as _;
            // `MSG_CMSG_CLOEXEC`, so the descriptor the kernel installs is
            // close-on-exec from the moment it exists. Without it the
            // sandbox's `/run/secrets` directory descriptor was inherited by
            // every later `Command` the supervisor spawned — `pasta`, `nft`,
            // `newuidmap`, a re-exec of itself (S-04, the code review). The
            // flag has to be set here rather than afterwards: between
            // `recvmsg` and an `fcntl` there is a window in which another
            // thread can fork.
            if libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            if cmsg.is_null()
                || (*cmsg).cmsg_level != libc::SOL_SOCKET
                || (*cmsg).cmsg_type != libc::SCM_RIGHTS
            {
                return Err(std::io::Error::other("no descriptor arrived"));
            }
            let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>());
            Ok(OwnedFd::from_raw_fd(fd))
        }
    }

    /// Start the egress resolver for one sandbox.
    fn start_resolver(
        ns: NetNs,
        nft: PathBuf,
        rules: Vec<AllowRule>,
        allow_private: bool,
    ) -> Result<DnsProxy> {
        let addr: std::net::SocketAddrV4 = format!("{PROXY_ADDR}:53")
            .parse()
            .expect("a constant address");
        let socket = bind_in_namespace(&ns, addr)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);

        let thread = std::thread::Builder::new()
            .name("zygo-dns".into())
            .spawn(move || {
                let mut admitted: std::collections::HashMap<(IpAddr, Option<u16>), Instant> =
                    Default::default();
                let mut admit = |ip: IpAddr, ports: &[Option<u16>]| -> std::io::Result<()> {
                    for &port in ports {
                        let key = (ip, port);
                        if admitted
                            .get(&key)
                            .is_some_and(|at| at.elapsed() < ADMIT_REFRESH)
                        {
                            continue;
                        }
                        admit_address(&ns, &nft, ip, port)?;
                        admitted.insert(key, Instant::now());
                    }
                    Ok(())
                };
                dns::serve(
                    socket,
                    rules,
                    allow_private,
                    thread_stop,
                    &dns::resolve_on_host,
                    &mut admit,
                );
            })
            .map_err(|e| Error::primitive("spawn", "resolver thread", e))?;

        Ok(DnsProxy {
            stop,
            thread: Some(thread),
        })
    }

    /// Put one address into the filter's set, with a fresh timeout.
    ///
    /// Two invocations rather than one transaction: `nft` refuses to re-add
    /// an element that exists, and a transaction that deletes an element that
    /// does not exist fails as a whole. Delete, ignoring the outcome; then add.
    fn admit_address(ns: &NetNs, nft: &Path, ip: IpAddr, port: Option<u16>) -> std::io::Result<()> {
        let set = set_name(ip, port);
        let elem = element(ip, port);
        let _ = run_in_namespace(
            ns,
            nft,
            &[
                "delete",
                "element",
                "inet",
                "zygo",
                set,
                &format!("{{ {elem} }}"),
            ],
            None,
        );
        let output = run_in_namespace(
            ns,
            nft,
            &[
                "add",
                "element",
                "inet",
                "zygo",
                set,
                &format!("{{ {elem} timeout {ADMIT_TIMEOUT_SECS}s }}"),
            ],
            None,
        )
        .map_err(|e| std::io::Error::other(e.to_string()))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(std::io::Error::other(last_line(&output.stderr)))
        }
    }
}

/// What to try, given what `pasta` said.
///
/// One remedy for every failure sent people to `/dev/net/tun` for a problem
/// that had nothing to do with it. The one below was found by the use-case
/// sweep on a Raspberry Pi running Ubuntu: `pasta` is confined by an AppArmor
/// profile that denies it `/proc/<pid>/ns/user`, so a networked sandbox cannot
/// start and the message pointed at a device that was present and working.
// Compiled and tested on every host, called on one. Which remedy answers
// which failure is a decision, and a decision is worth checking wherever the
// tests run — `shim.rs` and `scope.rs` are kept the same way.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn pasta_remedy(said: &str) -> String {
    let lower = said.to_ascii_lowercase();
    if lower.contains("user namespace") && lower.contains("permission denied") {
        return "`pasta` was refused access to the sandbox's user namespace. On Ubuntu \
            and Debian that is usually AppArmor: a profile confines `pasta` and \
            denies it `/proc/<pid>/ns/user`.\n  \
            → check `sudo aa-status | grep -i passt` and, if it is enforcing, \
            `sudo aa-complain /usr/bin/pasta`\n  \
            → or run the sandbox with `network = \"none\"`, which needs no `pasta` \
            at all"
            .to_string();
    }
    if lower.contains("pid file") && lower.contains("permission denied") {
        return "`pasta` was refused its own pid file. Ubuntu and Debian ship an \
            AppArmor profile, `passt`, that attaches to /usr/bin/pasta by path — \
            inside containers too, since the host's profiles are what the kernel \
            enforces — and lets it write only where it expects.\n  \
            → on the host: `sudo aa-complain passt`, or add the data directory \
            (`ZYGO_DATA_HOME`) to the profile's local override\n  \
            → in a container image: install `pasta` somewhere else on `PATH`, \
            e.g. /usr/local/bin, where the profile does not attach\n  \
            → or run the sandbox with `network = \"none\"`, which needs no `pasta`"
            .to_string();
    }
    if lower.contains("tun") || lower.contains("tap device") || lower.contains("no such device") {
        return tun_remedy();
    }
    "`zygo doctor` reports what `network = \"egress\"` and `\"full\"` need; \
 `network = \"none\"` needs none of it"
        .to_string()
}

#[cfg(test)]
mod tests {

    /// The ruleset's blocks and the one `is_private_addr` answers for are the
    /// same set, checked address by address rather than by reading both lists.
    ///
    /// They were not (B-18): the spec's validator had no carrier-grade NAT
    /// range, so `allow = ["100.64.0.1:443"]` was accepted, said nothing, and
    /// was then dropped by nftables. A rule that looks accepted and is dead is
    /// worse than one that is refused.
    #[test]
    fn what_nftables_drops_is_what_the_validator_refuses() {
        use crate::spec::types::is_private_addr;

        // One address from inside every block the ruleset installs.
        let inside = [
            "10.1.2.3",
            "172.16.5.6",
            "192.168.1.1",
            "169.254.169.254",
            "127.0.0.1",
            "100.64.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
        ];
        for a in inside {
            let addr: std::net::IpAddr = a.parse().expect("an address");
            assert!(
                is_private_addr(addr),
                "{a} is dropped by the ruleset and accepted by the validator"
            );
        }

        // And addresses outside every one of them, so the check is not
        // satisfied by a function that always says true.
        for a in ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:2800:220:1::1"] {
            let addr: std::net::IpAddr = a.parse().expect("an address");
            assert!(!is_private_addr(addr), "{a} is public and was refused");
        }

        // Every block named in the ruleset has a representative above; a new
        // one without a case here fails this rather than passing quietly.
        assert_eq!(
            PRIVATE_V4.len() + PRIVATE_V6.len(),
            inside.len(),
            "a block was added to the ruleset without an address in this test"
        );
    }
    use super::*;

    fn rules(list: &[&str]) -> Vec<AllowRule> {
        list.iter().map(|s| s.parse().expect("rule")).collect()
    }

    fn egress(list: &[&str], allowed: &Allowed, private: bool) -> String {
        ruleset(Network::Egress, &rules(list), allowed, private, 256)
    }

    #[test]
    fn under_egress_the_only_resolver_is_on_loopback() {
        // `pasta`'s forwarder must not be reachable, or a tenant resolves any
        // name it likes; loopback is accepted first, and that is where Zygo's
        // resolver is.
        let text = egress(&[], &Allowed::default(), false);
        assert!(!text.contains(DNS_ADDR), "{text}");
        let lo = text.find("oifname \"lo\" accept").expect("lo rule");
        let private = text.find("127.0.0.0/8").expect("private reject");
        assert!(
            lo < private,
            "loopback is accepted before 127/8 is rejected: {text}"
        );
    }

    #[test]
    fn under_full_dns_is_permitted_above_the_link_local_reject() {
        // The forwarder's address is inside a range the policy rejects, so the
        // order of these two lines is the difference between working DNS and a
        // sandbox that cannot resolve anything.
        let text = ruleset(Network::Full, &[], &Allowed::default(), false, 256);
        let dns = text.find(DNS_ADDR).expect("dns rule");
        let reject = text.find("169.254.0.0/16").expect("link-local reject");
        assert!(dns < reject, "{text}");
        let dns_lines: Vec<&str> = text.lines().filter(|l| l.contains("dport 53")).collect();
        assert_eq!(dns_lines.len(), 2, "{text}");
        assert!(dns_lines.iter().all(|l| l.contains(DNS_ADDR)), "{text}");
    }

    #[test]
    fn the_default_is_reject_at_both_ends() {
        let list = rules(&["1.2.3.4:443"]);
        let text = ruleset(Network::Egress, &list, &resolve_allow(&list), false, 256);
        assert!(text.contains("policy drop"), "{text}");
        let last = text
            .lines()
            .rfind(|l| l.trim().starts_with("reject"))
            .expect("a final reject");
        assert!(last.contains("admin-prohibited"), "{text}");
        // A bare address is an `Exact` host, resolved to itself, and lands in
        // the set as an element — above the final reject.
        let elem = text.find("1.2.3.4 . 443").expect("the element");
        assert!(elem < text.rfind("reject").expect("reject"), "{text}");
    }

    #[test]
    fn a_cidr_rule_keeps_its_prefix() {
        // Resolution turns a name into one address; a CIDR must not be reduced
        // to its network address the same way.
        let text = egress(&["203.0.113.0/24:5432"], &Allowed::default(), true);
        assert!(
            text.contains("ip daddr 203.0.113.0/24 tcp dport 5432 accept"),
            "{text}"
        );
    }

    #[test]
    fn private_ranges_are_rejected_unless_they_were_asked_for() {
        let closed = egress(&[], &Allowed::default(), false);
        for block in PRIVATE_V4.iter().chain(PRIVATE_V6) {
            assert!(closed.contains(block), "{block} missing from {closed}");
        }
        // With `--allow-private-net` the rejects have to go: a rule naming
        // 10.0.0.0/8 would otherwise be dead code below them.
        let open = egress(&[], &Allowed::default(), true);
        assert!(!open.contains("10.0.0.0/8"), "{open}");
        assert!(!open.contains("fe80::/10"), "{open}");
    }

    #[test]
    fn full_is_no_allowlist_rather_than_no_policy() {
        let text = ruleset(Network::Full, &[], &Allowed::default(), false, 256);
        assert!(text.contains("\n    accept\n"), "{text}");
        assert!(
            text.contains("10.0.0.0/8"),
            "still not the host's own networks: {text}"
        );
        assert!(!text.contains("set allow4"), "no sets to fill: {text}");
    }

    #[test]
    fn an_empty_allowlist_permits_nothing_but_loopback_and_replies() {
        let text = egress(&[], &Allowed::default(), false);
        let accepts: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|l| l.ends_with("accept") && !l.contains('@'))
            .collect();
        assert_eq!(
            accepts,
            [
                "oifname \"lo\" accept",
                "ct state established,related accept",
            ]
        );
        // The set rules are there, and the sets are empty.
        assert!(
            text.contains("ip daddr . tcp dport @allow4 accept"),
            "{text}"
        );
        assert!(!text.contains("elements"), "{text}");
    }

    #[test]
    fn resolved_addresses_become_set_elements_in_the_right_set() {
        let allowed = Allowed {
            entries: vec![
                ("93.184.216.34".parse().unwrap(), Some(443)),
                ("93.184.216.35".parse().unwrap(), None),
                (
                    "2606:2800:220:1:248:1893:25c8:1946".parse().unwrap(),
                    Some(443),
                ),
            ],
            unresolved: vec![],
        };
        let text = egress(&[], &allowed, false);
        let set = |name: &str| -> String {
            let start = text.find(&format!("set {name} {{")).expect(name);
            let end = text[start..].find("\n  }").expect("end") + start;
            text[start..end].to_string()
        };
        assert!(set("allow4").contains("93.184.216.34 . 443"), "{text}");
        assert!(set("allow4any").contains("93.184.216.35"), "{text}");
        assert!(
            set("allow6").contains("2606:2800:220:1:248:1893:25c8:1946 . 443"),
            "{text}"
        );
        assert!(!set("allow6any").contains("elements"), "{text}");
        for name in ["allow4", "allow4any", "allow6", "allow6any"] {
            assert!(
                set(name).contains("flags timeout"),
                "{name} must accept timed elements"
            );
        }
    }

    #[test]
    fn the_connection_limit_is_counted_per_family_on_new_connections() {
        let text = ruleset(Network::Egress, &[], &Allowed::default(), false, 3);
        assert!(
            text.contains("set conn4 {\n    type ipv4_addr;\n    flags dynamic;"),
            "{text}"
        );
        assert!(
            text.contains(
                "meta l4proto tcp ct state new add @conn4 { ip saddr ct count over 3 } reject with tcp reset"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "meta l4proto tcp ct state new add @conn6 { ip6 saddr ct count over 3 } reject with tcp reset"
            ),
            "{text}"
        );
        // Counted before anything is accepted, so an allowed destination is
        // subject to it too — but after established, so replies are not.
        let established = text.find("established,related accept").unwrap();
        let count = text.find("ct count over").unwrap();
        let sets = text.find("@allow4 accept").unwrap();
        assert!(established < count && count < sets, "{text}");
    }

    #[test]
    fn a_cidr_rule_needs_no_resolver_and_a_wildcard_contributes_nothing_yet() {
        let allowed = resolve_allow(&rules(&["203.0.113.0/24:5432", "*.example.com:443"]));
        assert_eq!(
            allowed.entries,
            [("203.0.113.0".parse().unwrap(), Some(5432))]
        );
        assert!(
            allowed.unresolved.is_empty(),
            "a wildcard is not unresolved, it is dynamic"
        );
    }

    #[test]
    fn a_name_that_does_not_resolve_is_reported_rather_than_silently_dropped() {
        // `.invalid` is reserved by RFC 2606 and never resolves, so this needs
        // no network to be deterministic.
        let allowed = resolve_allow(&rules(&["nothing.invalid:443"]));
        assert!(allowed.entries.is_empty());
        assert_eq!(allowed.unresolved, ["nothing.invalid"]);
    }

    #[test]
    fn the_resolver_file_names_one_nameserver_per_mode_and_nothing_else() {
        let egress = resolv_conf(Network::Egress);
        assert!(
            egress.contains(&format!("nameserver {PROXY_ADDR}")),
            "{egress}"
        );
        let full = resolv_conf(Network::Full);
        assert!(full.contains(&format!("nameserver {DNS_ADDR}")), "{full}");
        for text in [&egress, &full] {
            assert_eq!(
                text.lines().filter(|l| l.starts_with("nameserver")).count(),
                1
            );
            assert!(
                !text.contains("search"),
                "the host's search domains stay on the host"
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(tmp.path());
        assert_ne!(
            resolv_conf_path(&paths, Network::Egress),
            resolv_conf_path(&paths, Network::Full)
        );
    }

    #[test]
    fn set_names_follow_family_and_port() {
        let v4: IpAddr = "1.2.3.4".parse().unwrap();
        let v6: IpAddr = "::1".parse().unwrap();
        assert_eq!(set_name(v4, Some(443)), "allow4");
        assert_eq!(set_name(v4, None), "allow4any");
        assert_eq!(set_name(v6, Some(443)), "allow6");
        assert_eq!(set_name(v6, None), "allow6any");
        assert_eq!(element(v4, Some(443)), "1.2.3.4 . 443");
        assert_eq!(element(v6, None), "::1");
    }

    #[test]
    fn only_the_namespaced_modes_need_configuring() {
        assert!(!needs_configuration(Network::None));
        assert!(!needs_configuration(Network::Host));
        assert!(needs_configuration(Network::Egress));
        assert!(needs_configuration(Network::Full));
    }

    #[test]
    fn a_missing_program_says_which_one_and_how_to_get_it() {
        let e = missing("pasta", "passt");
        let text = e.to_string();
        assert!(text.contains("pasta"), "{text}");
        assert!(text.contains("apt install passt"), "{text}");
        assert_eq!(e.exit_code(), 125);
        let e = missing("tc", "iproute2");
        assert!(e.to_string().contains("iproute2"));
    }

    /// A `pasta` that neither finishes nor exits is stopped at the deadline,
    /// and what it said comes back.
    ///
    /// passt 2025_01 in a container with no `/dev/net/tun` does exactly this,
    /// and the launch used to wait for its output to end — for ever, with the
    /// sandbox's own `--timeout` unable to fire.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_pasta_that_hangs_is_stopped_at_the_deadline_with_what_it_said() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("pasta");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho 'Failed to set up tap device in namespace' >&2\nexec sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = std::time::Instant::now();
        let err = linux::start_pasta_within(
            &fake,
            std::process::id(),
            &dir.path().join("pasta.pid"),
            std::time::Duration::from_millis(300),
        )
        .unwrap_err()
        .to_string();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{err}"
        );
        assert!(err.contains("tap device"), "{err}");
        assert!(err.contains("did not finish"), "{err}");
    }

    /// A `pasta` that exits non-zero is reported with its last line, even when
    /// something it started keeps its stderr open.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_pasta_that_fails_is_reported_without_waiting_for_its_children() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("pasta");
        std::fs::write(
            &fake,
            "#!/bin/sh\nsleep 60 &\necho \"Couldn't open PID file /x.pid: Permission denied\" >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = std::time::Instant::now();
        let err = linux::start_pasta_within(
            &fake,
            std::process::id(),
            &dir.path().join("pasta.pid"),
            std::time::Duration::from_secs(10),
        )
        .unwrap_err()
        .to_string();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "{err}"
        );
        assert!(err.contains("PID file"), "{err}");
    }

    /// The remedy has to match the failure.
    ///
    /// One remedy for every `pasta` failure sent people to `/dev/net/tun` for
    /// an AppArmor confinement — a device that was present, working, and
    /// nothing to do with it. Found by the use-case sweep on a Raspberry Pi
    /// running Ubuntu, where `network = "full"` could not start at all.
    #[test]
    fn a_pasta_failure_is_answered_with_the_remedy_for_that_failure() {
        let confined =
            pasta_remedy("Couldn't open user namespace /proc/124388/ns/user: Permission denied");
        assert!(confined.contains("AppArmor"), "{confined}");
        assert!(confined.contains("aa-complain"), "{confined}");
        assert!(
            !confined.contains("/dev/net/tun"),
            "the old catch-all remedy is still being given: {confined}"
        );

        let no_device = pasta_remedy("Couldn't open /dev/net/tun: No such device");
        assert!(no_device.contains("/dev/net/tun"), "{no_device}");

        // The shape a tun-less container actually produces, which names the
        // tap device rather than the tun one.
        let no_tap = pasta_remedy("Failed to set up tap device in namespace");
        assert!(no_tap.contains("--device /dev/net/tun"), "{no_tap}");

        // Ubuntu's `passt` profile, attached by path inside a container.
        let pid_file = pasta_remedy(
            "Couldn't open PID file /zygo-data/run/tenants/run/pasta-13-1.pid: Permission denied",
        );
        assert!(pid_file.contains("passt"), "{pid_file}");
        assert!(pid_file.contains("/usr/local/bin"), "{pid_file}");

        // Anything else gets something true rather than something specific and
        // wrong.
        let unknown = pasta_remedy("something nobody has seen before");
        assert!(unknown.contains("doctor"), "{unknown}");
        assert!(!unknown.contains("AppArmor"), "{unknown}");
    }
}
