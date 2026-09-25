//! Scalar types used by `sandbox.toml` and by CLI flags.
//!
//! Each type parses from the same string syntax in both places, so
//! `--mem 512M` and `mem = "512M"` cannot drift apart.

use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Parse error for a scalar field. Carries the offending input so callers can
/// point at it.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid {kind} `{input}`: {reason}")]
pub struct ParseError {
    pub kind: &'static str,
    pub input: String,
    pub reason: String,
}

impl ParseError {
    fn new(kind: &'static str, input: &str, reason: impl Into<String>) -> Self {
        Self {
            kind,
            input: input.to_string(),
            reason: reason.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Bytes
// ---------------------------------------------------------------------------

/// A byte quantity: `"256M"`, `"1.5G"`, `"64K"`, or a bare integer.
///
/// Suffixes are binary (1M = 1 MiB), matching `docker run --memory` and cgroup
/// v2 semantics. A bare number is bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bytes(pub u64);

impl Bytes {
    pub const fn from_mib(mib: u64) -> Self {
        Bytes(mib * 1024 * 1024)
    }
    pub const fn get(self) -> u64 {
        self.0
    }
    /// Scale by a factor, saturating. Used for `memory.high = max * 0.9`.
    pub fn scaled(self, factor: f64) -> Self {
        Bytes((self.0 as f64 * factor) as u64)
    }
}

impl FromStr for Bytes {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        if t.is_empty() {
            return Err(ParseError::new("byte size", s, "empty"));
        }
        let digits_end = t
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(t.len());
        let (num, suffix) = t.split_at(digits_end);
        let value: f64 = num
            .parse()
            .map_err(|_| ParseError::new("byte size", s, "expected a number, e.g. `256M`"))?;
        if value < 0.0 || !value.is_finite() {
            return Err(ParseError::new(
                "byte size",
                s,
                "must be a finite, non-negative number",
            ));
        }

        let suffix = suffix.trim();
        let mult: u64 = match suffix.to_ascii_uppercase().as_str() {
            "" | "B" => 1,
            "K" | "KB" | "KIB" => 1 << 10,
            "M" | "MB" | "MIB" => 1 << 20,
            "G" | "GB" | "GIB" => 1 << 30,
            "T" | "TB" | "TIB" => 1u64 << 40,
            other => {
                return Err(ParseError::new(
                    "byte size",
                    s,
                    format!("unknown suffix `{other}` (use B, K, M, G or T)"),
                ));
            }
        };
        Ok(Bytes((value * mult as f64) as u64))
    }
}

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(u64, &str); 4] = [
            (1u64 << 40, "T"),
            (1 << 30, "G"),
            (1 << 20, "M"),
            (1 << 10, "K"),
        ];
        for (mult, unit) in UNITS {
            if self.0 >= mult {
                let v = self.0 as f64 / mult as f64;
                return if (v.fract()).abs() < f64::EPSILON {
                    write!(f, "{}{unit}", v as u64)
                } else {
                    write!(f, "{v:.1}{unit}")
                };
            }
        }
        write!(f, "{}B", self.0)
    }
}

// ---------------------------------------------------------------------------
// Duration
// ---------------------------------------------------------------------------

/// A duration: `"30s"`, `"10m"`, `"1h"`, `"500ms"`. A bare number is seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Duration(pub std::time::Duration);

impl Duration {
    pub const fn from_secs(s: u64) -> Self {
        Duration(std::time::Duration::from_secs(s))
    }
    pub const fn from_millis(ms: u64) -> Self {
        Duration(std::time::Duration::from_millis(ms))
    }
    pub fn as_millis(self) -> u64 {
        self.0.as_millis() as u64
    }
    pub const fn get(self) -> std::time::Duration {
        self.0
    }
}

impl FromStr for Duration {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        if t.is_empty() {
            return Err(ParseError::new("duration", s, "empty"));
        }
        let digits_end = t
            .find(|c: char| !c.is_ascii_digit() && c != '.')
            .unwrap_or(t.len());
        let (num, suffix) = t.split_at(digits_end);
        let value: f64 = num
            .parse()
            .map_err(|_| ParseError::new("duration", s, "expected a number, e.g. `30s`"))?;
        if value < 0.0 || !value.is_finite() {
            return Err(ParseError::new(
                "duration",
                s,
                "must be a finite, non-negative number",
            ));
        }

        let millis = match suffix.trim() {
            "ms" => value,
            "" | "s" => value * 1_000.0,
            "m" => value * 60_000.0,
            "h" => value * 3_600_000.0,
            "d" => value * 86_400_000.0,
            other => {
                return Err(ParseError::new(
                    "duration",
                    s,
                    format!("unknown unit `{other}` (use ms, s, m, h or d)"),
                ));
            }
        };
        Ok(Duration(std::time::Duration::from_millis(millis as u64)))
    }
}

impl fmt::Display for Duration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ms = self.as_millis();
        if ms == 0 {
            return write!(f, "0s");
        }
        for (unit_ms, unit) in [
            (86_400_000u64, "d"),
            (3_600_000, "h"),
            (60_000, "m"),
            (1_000, "s"),
        ] {
            if ms.is_multiple_of(unit_ms) {
                return write!(f, "{}{unit}", ms / unit_ms);
            }
        }
        write!(f, "{ms}ms")
    }
}

// ---------------------------------------------------------------------------
// Cpu
// ---------------------------------------------------------------------------

/// CPU quota expressed in cores: `0.5` means half a core.
///
/// Translated to `cpu.max = <quota> <period>` with a 100 ms period, which is
/// what both Docker and systemd use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cpu(pub f64);

impl Cpu {
    /// The CFS enforcement period.
    ///
    /// This sets the worst-case latency a quota can add: a tenant that runs out
    /// of quota waits out the rest of the period, so a throttled request costs
    /// an expected half-period — 50 ms here. That is the whole of the warm
    /// path's p99 when a tenant is driven past its own quota, which is why
    /// [`crate::pool::CpuAccounting`] exists.
    ///
    /// Shortening it was measured and rejected. The tail does track the period
    /// (p99 ≈ half of it at every value from 100 ms down to 2 ms), but a
    /// shorter period creates no CPU that was not there: it splits one long
    /// stall into many short ones. On the measured workload, going to 10 ms cut
    /// p99 from 49 ms to 8.4 ms and pushed p90 the other way, 1.3 ms to 5.6 ms,
    /// while total time spent throttled rose from 2.3 s to 9.0 s. Below
    /// saturation the 100 ms period costs nothing (p99 1.9 ms), so the trade
    /// would make the ordinary case worse to improve a case that means the
    /// tenant needs a larger quota.
    pub const PERIOD_US: u64 = 100_000;

    pub fn cores(self) -> f64 {
        self.0
    }

    /// `cpu.max` value: `"50000 100000"` for half a core.
    pub fn cgroup_max(self) -> String {
        let quota = (self.0 * Self::PERIOD_US as f64).round() as u64;
        format!("{quota} {}", Self::PERIOD_US)
    }
}

impl Eq for Cpu {}

impl FromStr for Cpu {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let v: f64 = s.trim().parse().map_err(|_| {
            ParseError::new("cpu quota", s, "expected a number of cores, e.g. `0.5`")
        })?;
        if !v.is_finite() || v <= 0.0 {
            return Err(ParseError::new("cpu quota", s, "must be greater than zero"));
        }
        Ok(Cpu(v))
    }
}

impl fmt::Display for Cpu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

macro_rules! str_enum {
    // The common shape: one spelling per variant, and serde derives the
    // (de)serialisation from the same names.
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }, default = $default:ident, kind = $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name {
            $($variant),+
        }

        str_enum!(@common $name { $($variant => $text),+ }, default = $default, kind = $kind, aliases = {});
    };
    // With aliases: other spellings accepted on the way *in* — the flag, the
    // spec file, the API — and never produced on the way out, so a name
    // borrowed from another tool never appears in Zygo's own output.
    // Deserialisation goes through `FromStr` so a spec file and the API take
    // the alias too, which a derived `Deserialize` would refuse.
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }, default = $default:ident, kind = $kind:literal, aliases = { $($alias:literal => $target:ident),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name {
            $($variant),+
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                s.parse().map_err(de::Error::custom)
            }
        }

        str_enum!(@common $name { $($variant => $text),+ }, default = $default, kind = $kind, aliases = { $($alias => $target),+ });
    };
    (@common $name:ident { $($variant:ident => $text:literal),+ }, default = $default:ident, kind = $kind:literal, aliases = { $($alias:literal => $target:ident),* }) => {
        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
            pub const ALL: &'static [$name] = &[$($name::$variant),+];
            /// Spellings accepted besides [`Self::as_str`]'s, and what they mean.
            pub const ALIASES: &'static [(&'static str, $name)] = &[$(($alias, $name::$target)),*];
        }

        impl Default for $name {
            fn default() -> Self { Self::$default }
        }

        impl FromStr for $name {
            type Err = ParseError;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s.trim().to_ascii_lowercase().as_str() {
                    $($text => Ok(Self::$variant),)+
                    $($alias => Ok(Self::$target),)*
                    _ => Err(ParseError::new(
                        $kind,
                        s,
                        format!("expected one of: {}", [$($text),+].join(", ")),
                    )),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

str_enum! {
    /// Where the isolation boundary is drawn (design doc §3.9).
    Isolation {
        Ns => "ns",
        Gvisor => "gvisor",
        Vm => "vm",
    },
    default = Ns,
    kind = "isolation backend"
}

str_enum! {
    /// Network policy (design doc §3.8). `Host` additionally requires
    /// `--allow-host-net`, since it removes the network boundary entirely.
    ///
    /// `bridge` is accepted as a spelling of `full`: it is the word every
    /// migrating Docker configuration already contains, and the first
    /// adoption report's runner failed every networked run on it before
    /// anybody read the usage error. It is read and never written — a
    /// resolved `full` prints as `full`.
    Network {
        None => "none",
        Egress => "egress",
        Full => "full",
        Host => "host",
    },
    default = None,
    kind = "network mode",
    aliases = { "bridge" => Full }
}

str_enum! {
    /// seccomp profile (design doc appendix B).
    SeccompProfile {
        Permissive => "permissive",
        Default => "default",
        Strict => "strict",
    },
    default = Default,
    kind = "seccomp profile"
}

str_enum! {
    /// Handler calling convention for agent-backed runtimes.
    HandlerMode {
        Function => "function",
        Stdin => "stdin",
    },
    default = Function,
    kind = "handler mode"
}

/// Warm-process strategy. Absent means warm-exec: no agent, the sandbox spawns
/// `cmd` per request (design doc §3.4, layer 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runtime {
    /// Built-in agent shipped with Zygo.
    Builtin(BuiltinRuntime),
    /// Third-party agent speaking the wire protocol; path is inside the sandbox.
    Agent(PathBuf),
}

str_enum! {
    /// Runtimes with an agent in the box.
    BuiltinRuntime {
        Python => "python",
        Node => "node",
        Go => "go",
    },
    default = Python,
    kind = "runtime"
}

impl Runtime {
    /// Infer the runtime from a handler file extension, as documented for the
    /// `runtime` field ("inferred from the `entry` extension").
    pub fn infer_from_entry(entry: &std::path::Path) -> Option<Runtime> {
        match entry.extension()?.to_str()? {
            "py" => Some(Runtime::Builtin(BuiltinRuntime::Python)),
            "js" | "mjs" | "cjs" | "ts" => Some(Runtime::Builtin(BuiltinRuntime::Node)),
            "go" => Some(Runtime::Builtin(BuiltinRuntime::Go)),
            _ => None,
        }
    }
}

impl fmt::Display for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Runtime::Builtin(b) => f.write_str(b.as_str()),
            Runtime::Agent(p) => write!(f, "agent:{}", p.display()),
        }
    }
}

// `runtime` is either a bare string (`"python"`) or a table
// (`{ agent = "/app/zygo-agent" }`).
impl<'de> Deserialize<'de> for Runtime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Name(String),
            Agent { agent: PathBuf },
        }
        match Repr::deserialize(d)? {
            Repr::Name(s) => s
                .parse::<BuiltinRuntime>()
                .map(Runtime::Builtin)
                .map_err(de::Error::custom),
            Repr::Agent { agent } => Ok(Runtime::Agent(agent)),
        }
    }
}

/// `python`, `node`, or the path to an agent of your own.
///
/// The same two shapes the spec file's `runtime` takes, so a flag and a table
/// key cannot disagree about syntax. A path is told apart by having a
/// separator in it: an agent lives at an absolute path inside the sandbox, and
/// a built-in is one word.
impl std::str::FromStr for Runtime {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Runtime, ParseError> {
        if s.contains('/') {
            return Ok(Runtime::Agent(PathBuf::from(s)));
        }
        s.parse::<BuiltinRuntime>().map(Runtime::Builtin)
    }
}

impl Serialize for Runtime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Runtime::Builtin(b) => s.serialize_str(b.as_str()),
            Runtime::Agent(p) => {
                use serde::ser::SerializeMap;
                let mut m = s.serialize_map(Some(1))?;
                m.serialize_entry("agent", p)?;
                m.end()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mount
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountMode {
    Ro,
    Rw,
}

/// A bind mount: `host:guest[:ro|rw]`.
///
/// Read-only unless `rw` is spelled out (principle P6: loosening takes a flag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub mode: MountMode,
}

impl FromStr for Mount {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split(':').collect();
        let (source, target, mode) = match parts.as_slice() {
            [src, dst] => (*src, *dst, MountMode::Ro),
            [src, dst, mode] => {
                let mode = match mode.trim().to_ascii_lowercase().as_str() {
                    "ro" => MountMode::Ro,
                    "rw" => MountMode::Rw,
                    other => {
                        return Err(ParseError::new(
                            "mount",
                            s,
                            format!("unknown mode `{other}` (use `ro` or `rw`)"),
                        ));
                    }
                };
                (*src, *dst, mode)
            }
            _ => {
                return Err(ParseError::new(
                    "mount",
                    s,
                    "expected `host:guest` or `host:guest:ro|rw`",
                ));
            }
        };

        if source.is_empty() || target.is_empty() {
            return Err(ParseError::new(
                "mount",
                s,
                "host and guest paths must be non-empty",
            ));
        }
        if !target.starts_with('/') {
            return Err(ParseError::new(
                "mount",
                s,
                format!("guest path `{target}` must be absolute"),
            ));
        }
        Ok(Mount {
            source: PathBuf::from(source),
            target: PathBuf::from(target),
            mode,
        })
    }
}

impl fmt::Display for Mount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}",
            self.source.display(),
            self.target.display(),
            match self.mode {
                MountMode::Ro => "ro",
                MountMode::Rw => "rw",
            }
        )
    }
}

// ---------------------------------------------------------------------------
// Egress allowlist
// ---------------------------------------------------------------------------

/// One entry of the egress allowlist: `api.stripe.com:443`, `*.example.com:443`,
/// `10.0.0.0/8:5432`, or a host with no port (all ports).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowRule {
    pub host: HostPattern,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostPattern {
    /// Exact hostname match.
    Exact(String),
    /// `*.example.com` — matches any subdomain, but not `example.com` itself.
    Wildcard(String),
    /// A CIDR block.
    Cidr(Cidr),
}

/// Minimal CIDR block. Written by hand rather than pulled in as a dependency:
/// the only operations needed are parse, display and `contains`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix)
                };
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix)
                };
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }

    /// Blocks that stay denied even when listed, unless `--allow-private-net`
    /// is given: "network access to the host".
    pub fn is_private_or_link_local(&self) -> bool {
        is_private_addr(self.addr)
    }
}

/// The addresses that stay denied in every namespaced network mode.
///
/// One definition, because there were three and they disagreed (B-18). The
/// spec's validator had no carrier-grade NAT range and no unspecified
/// address; the resolver had both; the nftables set had CGNAT. So
/// `allow = ["100.64.0.1:443"]` passed validation, told the user nothing, and
/// was then dropped by the ruleset — a rule that looks accepted and is dead.
///
/// These are the same blocks `PRIVATE_V4` and `PRIVATE_V6` install, and the
/// test below holds the two lists to each other rather than to a comment.
pub fn is_private_addr(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                // 0.0.0.0/8, "this network".
                || o[0] == 0
                // 100.64.0.0/10, carrier-grade NAT: the range a cloud provider
                // puts its own services on.
                || (o[0] == 100 && (o[1] & 0xC0) == 64)
                // 224.0.0.0/4 multicast and 240.0.0.0/4 reserved, broadcast
                // included.
                || o[0] >= 224
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // fe80::/10 link-local, fc00::/7 unique-local.
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                // ff00::/8 multicast.
                || v6.is_multicast()
        }
    }
}

impl FromStr for Cidr {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = s
            .split_once('/')
            .ok_or_else(|| ParseError::new("CIDR", s, "expected `address/prefix`"))?;
        let addr: IpAddr = addr
            .parse()
            .map_err(|_| ParseError::new("CIDR", s, "invalid IP address"))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_| ParseError::new("CIDR", s, "invalid prefix length"))?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(ParseError::new(
                "CIDR",
                s,
                format!("prefix must be between 0 and {max}"),
            ));
        }
        Ok(Cidr { addr, prefix })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl FromStr for AllowRule {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let t = s.trim();
        if t.is_empty() {
            return Err(ParseError::new("allow rule", s, "empty"));
        }

        let parse_port = |p: &str| -> Result<u16, ParseError> {
            let port: u16 = p
                .parse()
                .map_err(|_| ParseError::new("allow rule", s, "port must be 1–65535"))?;
            if port == 0 {
                return Err(ParseError::new("allow rule", s, "port must be 1–65535"));
            }
            Ok(port)
        };

        // Host and port, which an IPv6 address makes less obvious than it
        // looks: the address is full of colons, so "split on the last one"
        // turns `2001:db8::1` into the host `2001:db8:` on **port 1** — a rule
        // that parses, reports nothing, and permits something nobody asked
        // for (B-17). Brackets are the standard way to say which colons belong
        // to the address, and a bare literal is recognised as one.
        let (host_part, port) = if let Some(rest) = t.strip_prefix('[') {
            let (inside, after) = rest.split_once(']').ok_or_else(|| {
                ParseError::new(
                    "allow rule",
                    s,
                    "a `[` needs a matching `]`: `[2001:db8::1]:443`",
                )
            })?;
            let port = match after {
                "" => None,
                other => match other.strip_prefix(':') {
                    Some(p) => Some(parse_port(p)?),
                    None => {
                        return Err(ParseError::new(
                            "allow rule",
                            s,
                            "after `]` only `:port` may follow",
                        ));
                    }
                },
            };
            (inside, port)
        } else if t.parse::<std::net::Ipv6Addr>().is_ok() {
            // A bare IPv6 literal: every colon belongs to the address.
            (t, None)
        } else {
            match t.rsplit_once(':') {
                Some((h, p)) if !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => {
                    (h, Some(parse_port(p)?))
                }
                _ => (t, None),
            }
        };

        let host = if host_part.contains('/') {
            HostPattern::Cidr(host_part.parse()?)
        } else if let Some(suffix) = host_part.strip_prefix("*.") {
            if suffix.is_empty() || suffix.contains('*') {
                return Err(ParseError::new(
                    "allow rule",
                    s,
                    "wildcard must be `*.domain.tld` with a single leading `*.`",
                ));
            }
            HostPattern::Wildcard(suffix.to_ascii_lowercase())
        } else if host_part.contains('*') {
            return Err(ParseError::new(
                "allow rule",
                s,
                "`*` is only allowed as a leading `*.` label",
            ));
        } else if host_part.is_empty() {
            return Err(ParseError::new("allow rule", s, "host must be non-empty"));
        } else {
            HostPattern::Exact(host_part.to_ascii_lowercase())
        };

        Ok(AllowRule { host, port })
    }
}

impl AllowRule {
    /// Whether a connection to `host:port` is permitted by this rule.
    pub fn matches(&self, host: &str, port: u16) -> bool {
        if let Some(p) = self.port
            && p != port
        {
            return false;
        }
        self.matches_host(host)
    }

    /// Whether this rule names `host`, whatever the port.
    ///
    /// What the egress resolver asks: a DNS query has a name and no port, and
    /// the answer decides whether the name resolves at all. A CIDR rule never
    /// names a host — it names addresses, and those need no resolving.
    pub fn matches_host(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        match &self.host {
            HostPattern::Exact(h) => *h == host,
            HostPattern::Wildcard(suffix) => host
                .strip_suffix(suffix)
                .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1),
            HostPattern::Cidr(c) => host.parse::<IpAddr>().is_ok_and(|ip| c.contains(ip)),
        }
    }
}

impl fmt::Display for AllowRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            HostPattern::Exact(h) => write!(f, "{h}")?,
            HostPattern::Wildcard(s) => write!(f, "*.{s}")?,
            HostPattern::Cidr(c) => write!(f, "{c}")?,
        }
        if let Some(p) = self.port {
            write!(f, ":{p}")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// serde glue: every scalar above deserialises from its string form
// ---------------------------------------------------------------------------

macro_rules! serde_from_str {
    ($($t:ty),+ $(,)?) => {$(
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                String::deserialize(d)?.parse().map_err(de::Error::custom)
            }
        }
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.collect_str(self)
            }
        }
    )+};
}

serde_from_str!(Mount, AllowRule, Cidr);

// Bytes and Duration additionally accept a bare integer, which TOML users
// reach for naturally (`pids = 64`, `timeout = 30`).
impl<'de> Deserialize<'de> for Bytes {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Int(u64),
            Str(String),
        }
        match Repr::deserialize(d)? {
            Repr::Int(v) => Ok(Bytes(v)),
            Repr::Str(s) => s.parse().map_err(de::Error::custom),
        }
    }
}

impl Serialize for Bytes {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Duration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Int(u64),
            Str(String),
        }
        match Repr::deserialize(d)? {
            Repr::Int(v) => Ok(Duration::from_secs(v)),
            Repr::Str(s) => s.parse().map_err(de::Error::custom),
        }
    }
}

impl Serialize for Duration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Cpu {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Num(f64),
            Str(String),
        }
        let c = match Repr::deserialize(d)? {
            Repr::Num(v) => Cpu(v),
            Repr::Str(s) => s.parse().map_err(de::Error::custom)?,
        };
        if !c.0.is_finite() || c.0 <= 0.0 {
            return Err(de::Error::custom("cpu quota must be greater than zero"));
        }
        Ok(c)
    }
}

impl Serialize for Cpu {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_f64(self.0)
    }
}

#[cfg(test)]
mod tests {

    /// An IPv6 address in `allow` is an address, not a host and a port.
    ///
    /// `2001:db8::1` used to parse as the host `2001:db8:` on port 1: it
    /// looked accepted, matched nothing anyone meant, and opened port 1 on a
    /// prefix instead (B-17).
    #[test]
    fn an_ipv6_literal_in_allow_keeps_all_of_its_colons() {
        let bare: AllowRule = "2001:db8::1".parse().expect("a bare v6 literal");
        assert_eq!(bare.port, None, "a bare literal names no port");
        match &bare.host {
            HostPattern::Exact(h) => assert_eq!(h, "2001:db8::1"),
            other => panic!("expected an exact host, got {other:?}"),
        }

        let with_port: AllowRule = "[2001:db8::1]:443".parse().expect("a bracketed v6");
        assert_eq!(with_port.port, Some(443));
        match &with_port.host {
            HostPattern::Exact(h) => assert_eq!(h, "2001:db8::1"),
            other => panic!("expected an exact host, got {other:?}"),
        }

        let bracketed_bare: AllowRule = "[::1]".parse().expect("brackets without a port");
        assert_eq!(bracketed_bare.port, None);

        // And the ordinary cases still split where they always did.
        let v4: AllowRule = "10.0.0.1:5432".parse().expect("a v4 host and port");
        assert_eq!(v4.port, Some(5432));
        let name: AllowRule = "api.example.com:443".parse().expect("a name and port");
        assert_eq!(name.port, Some(443));
        let no_port: AllowRule = "api.example.com".parse().expect("a name alone");
        assert_eq!(no_port.port, None);

        // Malformed brackets are refused rather than guessed at.
        assert!(
            "[2001:db8::1".parse::<AllowRule>().is_err(),
            "unclosed bracket"
        );
        assert!(
            "[2001:db8::1]x".parse::<AllowRule>().is_err(),
            "junk after `]`"
        );
        assert!("[2001:db8::1]:0".parse::<AllowRule>().is_err(), "port zero");
    }
    use super::*;

    #[test]
    fn bytes_roundtrip() {
        assert_eq!("256M".parse::<Bytes>().unwrap(), Bytes(256 << 20));
        assert_eq!("1G".parse::<Bytes>().unwrap(), Bytes(1 << 30));
        assert_eq!("1.5G".parse::<Bytes>().unwrap(), Bytes(1610612736));
        assert_eq!("64".parse::<Bytes>().unwrap(), Bytes(64));
        assert_eq!("512k".parse::<Bytes>().unwrap(), Bytes(512 << 10));
        assert_eq!(Bytes(256 << 20).to_string(), "256M");
        assert_eq!(Bytes(1610612736).to_string(), "1.5G");
        assert_eq!(Bytes(512).to_string(), "512B");
    }

    #[test]
    fn bytes_rejects_nonsense() {
        for bad in ["", "M", "12X", "-5M", "abc"] {
            assert!(bad.parse::<Bytes>().is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn duration_roundtrip() {
        assert_eq!("30s".parse::<Duration>().unwrap(), Duration::from_secs(30));
        assert_eq!("10m".parse::<Duration>().unwrap(), Duration::from_secs(600));
        assert_eq!("1h".parse::<Duration>().unwrap(), Duration::from_secs(3600));
        assert_eq!(
            "500ms".parse::<Duration>().unwrap(),
            Duration::from_millis(500)
        );
        assert_eq!("5".parse::<Duration>().unwrap(), Duration::from_secs(5));
        assert_eq!(Duration::from_secs(600).to_string(), "10m");
        assert_eq!(Duration::from_millis(1500).to_string(), "1500ms");
    }

    #[test]
    fn cpu_to_cgroup_max() {
        assert_eq!(Cpu(0.5).cgroup_max(), "50000 100000");
        assert_eq!(Cpu(1.0).cgroup_max(), "100000 100000");
        assert_eq!(Cpu(2.5).cgroup_max(), "250000 100000");
    }

    #[test]
    fn mount_defaults_to_readonly() {
        let m: Mount = "./cache:/cache".parse().unwrap();
        assert_eq!(m.mode, MountMode::Ro);
        let m: Mount = "./cache:/cache:rw".parse().unwrap();
        assert_eq!(m.mode, MountMode::Rw);
        assert_eq!(m.to_string(), "./cache:/cache:rw");
    }

    #[test]
    fn mount_requires_absolute_guest_path() {
        assert!("./a:b".parse::<Mount>().is_err());
        assert!("./a".parse::<Mount>().is_err());
        assert!("./a:/b:xx".parse::<Mount>().is_err());
    }

    #[test]
    fn allow_rule_exact_and_wildcard() {
        let r: AllowRule = "api.stripe.com:443".parse().unwrap();
        assert!(r.matches("api.stripe.com", 443));
        assert!(!r.matches("api.stripe.com", 80));
        assert!(!r.matches("evil.com", 443));

        let r: AllowRule = "*.example.com:443".parse().unwrap();
        assert!(r.matches("a.example.com", 443));
        assert!(r.matches("a.b.example.com", 443));
        // The bare apex is not covered by `*.`, matching TLS wildcard semantics.
        assert!(!r.matches("example.com", 443));
        // And no suffix-splicing: `notexample.com` must not match.
        assert!(!r.matches("notexample.com", 443));
    }

    #[test]
    fn allow_rule_without_port_matches_any_port() {
        let r: AllowRule = "example.com".parse().unwrap();
        assert!(r.matches("example.com", 443));
        assert!(r.matches("example.com", 8080));
    }

    #[test]
    fn allow_rule_cidr() {
        let r: AllowRule = "10.0.0.0/8:5432".parse().unwrap();
        assert!(r.matches("10.1.2.3", 5432));
        assert!(!r.matches("11.1.2.3", 5432));
        assert!(!r.matches("10.1.2.3", 5433));
    }

    #[test]
    fn allow_rule_rejects_interior_wildcard() {
        assert!("a.*.com:443".parse::<AllowRule>().is_err());
        assert!("*:443".parse::<AllowRule>().is_err());
        assert!("example.com:0".parse::<AllowRule>().is_err());
        assert!("example.com:99999".parse::<AllowRule>().is_err());
    }

    #[test]
    fn private_ranges_are_recognised() {
        assert!(
            "10.0.0.0/8"
                .parse::<Cidr>()
                .unwrap()
                .is_private_or_link_local()
        );
        assert!(
            "192.168.0.0/16"
                .parse::<Cidr>()
                .unwrap()
                .is_private_or_link_local()
        );
        assert!(
            "169.254.0.0/16"
                .parse::<Cidr>()
                .unwrap()
                .is_private_or_link_local()
        );
        assert!(
            "127.0.0.0/8"
                .parse::<Cidr>()
                .unwrap()
                .is_private_or_link_local()
        );
        assert!(
            !"8.8.8.0/24"
                .parse::<Cidr>()
                .unwrap()
                .is_private_or_link_local()
        );
    }

    #[test]
    fn runtime_inferred_from_extension() {
        use std::path::Path;
        assert_eq!(
            Runtime::infer_from_entry(Path::new("./resize.py")),
            Some(Runtime::Builtin(BuiltinRuntime::Python))
        );
        assert_eq!(
            Runtime::infer_from_entry(Path::new("./tool.mjs")),
            Some(Runtime::Builtin(BuiltinRuntime::Node))
        );
        assert_eq!(Runtime::infer_from_entry(Path::new("./bin/parser")), None);
    }

    #[test]
    fn enums_parse_case_insensitively() {
        assert_eq!("VM".parse::<Isolation>().unwrap(), Isolation::Vm);
        assert_eq!("Egress".parse::<Network>().unwrap(), Network::Egress);
        assert!("kvm".parse::<Isolation>().is_err());
    }

    /// `bridge` is Docker's word for "a network, NAT'd", and every migrating
    /// configuration contains it. Read as `full` on every way in — the
    /// flag, a spec file, the API's JSON — and never written back out.
    #[test]
    fn bridge_is_read_as_full_and_never_printed() {
        assert_eq!("bridge".parse::<Network>().unwrap(), Network::Full);
        assert_eq!("Bridge".parse::<Network>().unwrap(), Network::Full);
        assert_eq!(Network::Full.to_string(), "full");
        assert_eq!(Network::ALIASES, [("bridge", Network::Full)]);

        // Through serde, which is what a spec file and an API request use.
        let from_toml: Network = toml::from_str::<toml::Value>("n = \"bridge\"")
            .unwrap()
            .get("n")
            .cloned()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(from_toml, Network::Full);
        let from_json: Network = serde_json::from_str("\"bridge\"").unwrap();
        assert_eq!(from_json, Network::Full);
        assert_eq!(serde_json::to_string(&Network::Full).unwrap(), "\"full\"");

        // The others still parse, and nonsense is still refused with the
        // canonical names — the alias is not advertised.
        assert_eq!(
            serde_json::from_str::<Network>("\"egress\"").unwrap(),
            Network::Egress
        );
        let err = "sideways".parse::<Network>().unwrap_err().to_string();
        assert!(err.contains("none, egress, full, host"), "{err}");
        assert!(!err.contains("bridge"), "{err}");
    }
}
