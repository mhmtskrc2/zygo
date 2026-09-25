// SPDX-License-Identifier: Apache-2.0
//! Namespace sets and user-namespace id mapping.
//!
//! Split out from the launcher itself because it is pure computation: what
//! `clone3` is asked for, and what goes into `uid_map`/`gid_map`. Both are
//! testable on any host, and getting the id map wrong is a security bug rather
//! than a crash.

use std::fmt;
use std::path::Path;

use crate::spec::Network;

/// The namespaces a sandbox is unshared into.
///
/// Held as a set rather than a raw flag word so the choice is inspectable and
/// testable; the flag word is produced by [`NamespaceSet::clone_flags`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamespaceSet {
    pub user: bool,
    pub pid: bool,
    pub mount: bool,
    pub net: bool,
    pub ipc: bool,
    pub uts: bool,
    pub cgroup: bool,
}

impl NamespaceSet {
    /// The full set, which is what every sandbox gets except where the network
    /// policy explicitly opts out.
    pub fn full() -> Self {
        Self {
            user: true,
            pid: true,
            mount: true,
            net: true,
            ipc: true,
            uts: true,
            cgroup: true,
        }
    }

    /// The set for a given network policy. `host` networking is the one case
    /// where a namespace is dropped, and it takes `--allow-host-net` to reach.
    pub fn for_network(network: Network) -> Self {
        Self {
            net: network != Network::Host,
            ..Self::full()
        }
    }

    /// `clone3`/`unshare` flag word.
    pub fn clone_flags(self) -> u64 {
        let mut flags = 0u64;
        // User namespace first: in a rootless run it is what grants the
        // privilege to create the others.
        if self.user {
            flags |= libc::CLONE_NEWUSER as u64;
        }
        if self.pid {
            flags |= libc::CLONE_NEWPID as u64;
        }
        if self.mount {
            flags |= libc::CLONE_NEWNS as u64;
        }
        if self.net {
            flags |= libc::CLONE_NEWNET as u64;
        }
        if self.ipc {
            flags |= libc::CLONE_NEWIPC as u64;
        }
        if self.uts {
            flags |= libc::CLONE_NEWUTS as u64;
        }
        if self.cgroup {
            flags |= libc::CLONE_NEWCGROUP as u64;
        }
        flags
    }
}

/// One line of `/proc/<pid>/uid_map`: sandbox id, host id, count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdMapEntry {
    pub inside: u32,
    pub outside: u32,
    pub count: u32,
}

impl fmt::Display for IdMapEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", self.inside, self.outside, self.count)
    }
}

/// A subordinate id range from `/etc/subuid` or `/etc/subgid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubIdRange {
    pub start: u32,
    pub count: u32,
}

/// Parse `/etc/subuid`-style content for one user.
///
/// Format is `name:start:count`, where `name` may be a login name or a numeric
/// uid. Malformed lines are skipped rather than failing the parse: these files
/// are edited by hand and by three different distro tools.
pub fn parse_subid(content: &str, user: &str, uid: u32) -> Vec<SubIdRange> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or(line).trim();
            let mut parts = line.split(':');
            let name = parts.next()?;
            if name != user && name != uid.to_string() {
                return None;
            }
            let start = parts.next()?.trim().parse().ok()?;
            let count = parts.next()?.trim().parse().ok()?;
            if count == 0 {
                None
            } else {
                Some(SubIdRange { start, count })
            }
        })
        .collect()
}

/// Build the id map for a sandbox.
///
/// Two entries: the sandbox's own uid maps to the caller's real uid, and the
/// rest of the range maps into the subordinate block. That gives a sandbox that
/// believes it is root (uid 0 inside) while being an unprivileged, *distinct*
/// uid on the host — which is what keeps one tenant from touching another's
/// files (design doc §3.10, "seeing another tenant").
///
/// With no subordinate range, the map degenerates to a single identity entry:
/// the sandbox works, but every tenant shares one host uid, so uid-level
/// separation between tenants is lost.
pub fn id_map(sandbox_uid: u32, host_uid: u32, sub: Option<SubIdRange>) -> Vec<IdMapEntry> {
    let mut map = vec![IdMapEntry {
        inside: sandbox_uid,
        outside: host_uid,
        count: 1,
    }];

    if let Some(range) = sub {
        // Map everything below the sandbox uid, then everything above, so the
        // single-uid entry above is not overlapped — the kernel rejects
        // overlapping map entries.
        if sandbox_uid > 0 {
            map.push(IdMapEntry {
                inside: 0,
                outside: range.start,
                count: sandbox_uid,
            });
        }
        let remaining = range.count.saturating_sub(sandbox_uid);
        if remaining > 1 {
            map.push(IdMapEntry {
                inside: sandbox_uid + 1,
                outside: range.start + sandbox_uid,
                count: remaining - 1,
            });
        }
    }

    map.sort_by_key(|e| e.inside);
    map
}

/// Serialise an id map into the form `newuidmap` and `/proc/<pid>/uid_map`
/// expect: one entry per line.
pub fn render_id_map(map: &[IdMapEntry]) -> String {
    map.iter()
        .map(IdMapEntry::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The one line of a rendered map an unprivileged process may write itself:
/// the entry that maps to its own host id.
///
/// Without `newuidmap` the kernel allows exactly one entry, and only for the
/// writer's own uid. That entry is not necessarily the first line — the map
/// is sorted by the inside id, and the default sandbox user of 1000 puts the
/// subordinate range for 0..1000 first — so it is picked by the outside id,
/// not by position. Found on a host with `/etc/subuid` configured but the
/// `uidmap` package missing: the first line asked for the subordinate range,
/// and the answer was EPERM.
pub fn identity_line(map: &str, outside: u32) -> String {
    let wanted = outside.to_string();
    map.lines()
        .find(|l| l.split_whitespace().nth(1) == Some(wanted.as_str()))
        .or_else(|| map.lines().next())
        .unwrap_or("")
        .to_string()
}

/// Read the subordinate range configured for the current user.
///
/// Without one, every tenant maps to the same host uid and the uid-level
/// separation between tenants described in design doc §3.10 is lost; the
/// sandbox still runs, and `zygo doctor` reports the degradation.
pub fn subuid_range_for_current_user() -> Option<SubIdRange> {
    // SAFETY: getuid cannot fail and has no preconditions.
    let uid = unsafe { libc::getuid() };
    let user = std::env::var("USER").unwrap_or_default();
    let content = std::fs::read_to_string(Path::new("/etc/subuid")).ok()?;
    parse_subid(&content, &user, uid).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_full_namespace_set_covers_every_boundary() {
        let flags = NamespaceSet::full().clone_flags();
        for (name, flag) in [
            ("user", libc::CLONE_NEWUSER),
            ("pid", libc::CLONE_NEWPID),
            ("mount", libc::CLONE_NEWNS),
            ("net", libc::CLONE_NEWNET),
            ("ipc", libc::CLONE_NEWIPC),
            ("uts", libc::CLONE_NEWUTS),
            ("cgroup", libc::CLONE_NEWCGROUP),
        ] {
            assert!(flags & flag as u64 != 0, "{name} namespace missing");
        }
    }

    #[test]
    fn only_host_networking_drops_the_network_namespace() {
        for net in [Network::None, Network::Egress, Network::Full] {
            assert!(
                NamespaceSet::for_network(net).net,
                "{net} must keep its own netns"
            );
        }
        let host = NamespaceSet::for_network(Network::Host);
        assert!(!host.net);
        assert!(
            host.user && host.pid && host.mount,
            "the rest still applies"
        );
        assert_eq!(host.clone_flags() & libc::CLONE_NEWNET as u64, 0);
    }

    #[test]
    fn subid_files_parse_by_name_or_uid() {
        let content = "\
# a comment
alice:100000:65536
bob:165536:65536
1000:231072:65536
malformed-line
carol:notanumber:65536
dave:300000:0
";
        assert_eq!(
            parse_subid(content, "alice", 1001),
            [SubIdRange {
                start: 100_000,
                count: 65_536
            }]
        );
        assert_eq!(
            parse_subid(content, "nobody", 1000),
            [SubIdRange {
                start: 231_072,
                count: 65_536
            }],
            "a numeric uid also matches"
        );
        assert!(
            parse_subid(content, "carol", 9).is_empty(),
            "malformed skipped"
        );
        assert!(
            parse_subid(content, "dave", 9).is_empty(),
            "zero-length skipped"
        );
        assert!(parse_subid(content, "absent", 9).is_empty());
    }

    #[test]
    fn id_map_gives_the_sandbox_a_distinct_host_identity() {
        let map = id_map(
            1000,
            501,
            Some(SubIdRange {
                start: 100_000,
                count: 65_536,
            }),
        );

        // The sandbox's own uid maps to the caller.
        assert!(map.contains(&IdMapEntry {
            inside: 1000,
            outside: 501,
            count: 1
        }));
        // Everything below it comes from the subordinate block, so uid 0 inside
        // is an unprivileged host uid.
        assert!(map.contains(&IdMapEntry {
            inside: 0,
            outside: 100_000,
            count: 1000
        }));
        // And so does everything above.
        assert!(map.contains(&IdMapEntry {
            inside: 1001,
            outside: 101_000,
            count: 64_535
        }));
    }

    /// The kernel rejects a map whose entries overlap; this is the invariant
    /// that is easy to break when adjusting the range arithmetic.
    #[test]
    fn id_map_entries_never_overlap() {
        for sandbox_uid in [0u32, 1, 999, 1000, 65_535] {
            let map = id_map(
                sandbox_uid,
                501,
                Some(SubIdRange {
                    start: 100_000,
                    count: 65_536,
                }),
            );
            for pair in map.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                assert!(
                    a.inside + a.count <= b.inside,
                    "uid {sandbox_uid}: inside ranges overlap: {a:?} then {b:?}"
                );
                let (lo, hi) = if a.outside <= b.outside {
                    (a, b)
                } else {
                    (b, a)
                };
                assert!(
                    lo.outside + lo.count <= hi.outside,
                    "uid {sandbox_uid}: outside ranges overlap: {lo:?} and {hi:?}"
                );
            }
        }
    }

    #[test]
    fn without_a_subordinate_range_the_map_is_a_single_identity_entry() {
        let map = id_map(1000, 501, None);
        assert_eq!(
            map,
            [IdMapEntry {
                inside: 1000,
                outside: 501,
                count: 1
            }]
        );
    }

    #[test]
    fn id_maps_render_one_entry_per_line() {
        let map = id_map(
            1000,
            501,
            Some(SubIdRange {
                start: 100_000,
                count: 2000,
            }),
        );
        let text = render_id_map(&map);
        assert_eq!(text.lines().count(), map.len());
        assert!(text.lines().next().unwrap().starts_with("0 100000 1000"));
    }

    /// The helper-less fallback must write the caller's own identity entry,
    /// which with a sandbox user of 1000 is the *second* line, not the first.
    #[test]
    fn the_identity_line_is_found_by_host_id_not_position() {
        let map = id_map(
            1000,
            501,
            Some(SubIdRange {
                start: 100_000,
                count: 2000,
            }),
        );
        let text = render_id_map(&map);
        assert_eq!(identity_line(&text, 501), "1000 501 1");
        // No subordinate range: the single entry is the identity.
        assert_eq!(identity_line("0 501 1", 501), "0 501 1");
        // An id that is not in the map at all falls back to the first line
        // rather than to nothing, so the kernel's error names the real cause.
        assert_eq!(
            identity_line("0 100000 1000\n1000 501 1", 7),
            "0 100000 1000"
        );
    }
}
