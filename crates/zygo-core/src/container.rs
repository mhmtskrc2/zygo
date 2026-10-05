// SPDX-License-Identifier: Apache-2.0
//! What kind of container this process runs in, as far as choosing advice goes.
//!
//! Nothing here decides whether a sandbox may start — the attempts in
//! [`crate::cgroup::probe_delegation`] and [`crate::net::availability`] do —
//! it decides which sentence to print when one may not. The same failure has
//! a different fix in a Kubernetes pod with its own user namespace, in one
//! without, in `docker run` and on a host, and advice written for the wrong
//! one is how `zygo doctor` sent a pod with `hostUsers: false` to mount
//! `/dev/net/tun` as a hostPath volume, which such a pod cannot start with.
//!
//! The readers take text rather than paths, so every case is tested against
//! what a kernel really prints rather than on a host that happens to be one
//! of them.

use std::path::Path;

/// The facts the remedies depend on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Surroundings {
    /// A container of any kind, by [`Surroundings::here`]'s rough reading.
    pub container: bool,
    /// A Kubernetes pod: the kubelet sets `KUBERNETES_SERVICE_HOST` in every
    /// container unless the pod opts out of service links, and the cgroup path
    /// says `kubepods` either way.
    pub kubernetes: bool,
    /// This process's user namespace is not the host's: a pod with
    /// `hostUsers: false`, a rootless runtime, a nested sandbox.
    pub user_namespace: bool,
    /// The cgroup2 mount covering this process's own cgroup is read-only, as
    /// a container runtime mounts it for an unprivileged container.
    pub cgroupfs_read_only: bool,
}

impl Surroundings {
    /// Read from `/proc` and the environment. Anything unreadable reads as
    /// "no", which only ever costs a less specific sentence.
    pub fn here() -> Self {
        let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let own = own_cgroup_path(&cgroup);
        let kubernetes = std::env::var_os("KUBERNETES_SERVICE_HOST").is_some()
            || std::fs::read_to_string("/proc/1/cgroup").is_ok_and(|c| c.contains("kubepods"));
        let container = kubernetes
            || Path::new("/.dockerenv").exists()
            || Path::new("/run/.containerenv").exists()
            || std::fs::read_to_string("/proc/1/cgroup").is_ok_and(|c| c.contains("/docker/"));
        Self {
            container,
            kubernetes,
            user_namespace: std::fs::read_to_string("/proc/self/uid_map")
                .is_ok_and(|m| in_user_namespace(&m)),
            cgroupfs_read_only: std::fs::read_to_string("/proc/self/mountinfo")
                .ok()
                .and_then(|m| cgroupfs_read_only(&m, &own))
                .unwrap_or(false),
        }
    }
}

/// Whether a `/proc/<pid>/uid_map` is anything but the host's own identity
/// map, `0 0 4294967295`.
pub fn in_user_namespace(uid_map: &str) -> bool {
    let lines: Vec<Vec<&str>> = uid_map
        .lines()
        .map(|l| l.split_whitespace().collect())
        .filter(|f: &Vec<&str>| !f.is_empty())
        .collect();
    !(lines.len() == 1 && lines[0] == ["0", "0", "4294967295"])
}

/// This process's cgroup as a path under `/sys/fs/cgroup`, from the text of
/// `/proc/self/cgroup`. `/sys/fs/cgroup` itself when there is no v2 line.
pub fn own_cgroup_path(proc_self_cgroup: &str) -> String {
    let rel = proc_self_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .unwrap_or("/")
        .trim()
        .trim_start_matches('/');
    if rel.is_empty() {
        "/sys/fs/cgroup".to_string()
    } else {
        format!("/sys/fs/cgroup/{rel}")
    }
}

/// Whether the cgroup2 mount that `path` lives on is mounted read-only, from
/// the text of `/proc/self/mountinfo`. `None` when no cgroup2 mount covers it.
///
/// The mount that counts is the one with the longest mount point above
/// `path`, and the last such line when several are stacked on one point:
/// `docker run -v /sys/fs/cgroup/zygo:/sys/fs/cgroup/zygo:rw` puts a writable
/// mount on top of a read-only one, and the sandbox goes under the writable
/// one. The flag read is the **mount's** (`ro` in the sixth field), not the
/// superblock's at the end of the line, which says `rw` either way.
pub fn cgroupfs_read_only(mountinfo: &str, path: &str) -> Option<bool> {
    let mut best: Option<(usize, bool)> = None;
    for line in mountinfo.lines() {
        let Some((left, right)) = line.split_once(" - ") else {
            continue;
        };
        if right.split_whitespace().next() != Some("cgroup2") {
            continue;
        }
        let fields: Vec<&str> = left.split_whitespace().collect();
        let (Some(point), Some(options)) = (fields.get(4), fields.get(5)) else {
            continue;
        };
        let covers = path == *point
            || point.ends_with('/') && path.starts_with(point)
            || path
                .strip_prefix(point)
                .is_some_and(|rest| rest.starts_with('/'));
        if !covers {
            continue;
        }
        let ro = options.split(',').any(|o| o == "ro");
        if best.is_none_or(|(len, _)| point.len() >= len) {
            best = Some((point.len(), ro));
        }
    }
    best.map(|(_, ro)| ro)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_map_is_not_a_user_namespace() {
        assert!(!in_user_namespace("         0          0 4294967295\n"));
    }

    #[test]
    fn a_pod_with_host_users_false_is_one() {
        // What `cat /proc/self/uid_map` printed in a k3s 1.36 pod.
        assert!(in_user_namespace("         0 2121334784      65536\n"));
        assert!(in_user_namespace("0 1000 1\n1 100000 65536\n"));
    }

    #[test]
    fn the_own_cgroup_is_found_under_the_mount() {
        assert_eq!(own_cgroup_path("0::/\n"), "/sys/fs/cgroup");
        assert_eq!(
            own_cgroup_path("0::/bench.slice/docker-ca87.scope\n"),
            "/sys/fs/cgroup/bench.slice/docker-ca87.scope"
        );
        assert_eq!(own_cgroup_path(""), "/sys/fs/cgroup");
    }

    // The two lines k3s 1.36 / containerd 2.3 printed: an ordinary pod, and
    // one on a runtime handler with `cgroup_writable = true`.
    const POD_RO: &str =
        "1617 1616 0:31 / /sys/fs/cgroup ro,nosuid,nodev,noexec,relatime - cgroup2 cgroup rw\n";
    const POD_RW: &str =
        "1739 1738 0:31 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime - cgroup2 cgroup rw\n";

    #[test]
    fn an_unprivileged_pod_has_it_read_only() {
        assert_eq!(cgroupfs_read_only(POD_RO, "/sys/fs/cgroup"), Some(true));
    }

    #[test]
    fn a_writable_runtime_handler_has_it_read_write() {
        // The superblock says `rw` on both lines; only the mount differs.
        assert_eq!(cgroupfs_read_only(POD_RW, "/sys/fs/cgroup"), Some(false));
        assert_eq!(
            cgroupfs_read_only(POD_RW, "/sys/fs/cgroup/zygo.slice"),
            Some(false)
        );
    }

    #[test]
    fn a_writable_bind_on_top_of_a_read_only_mount_wins() {
        let docker = "\
900 899 0:31 /docker/abc /sys/fs/cgroup ro,nosuid,nodev,noexec,relatime - cgroup2 cgroup rw
901 900 0:31 /zygo /sys/fs/cgroup/zygo rw,nosuid,nodev,noexec,relatime - cgroup2 cgroup rw
";
        assert_eq!(
            cgroupfs_read_only(docker, "/sys/fs/cgroup/zygo/zygo.slice"),
            Some(false)
        );
        assert_eq!(cgroupfs_read_only(docker, "/sys/fs/cgroup"), Some(true));
        // A sibling whose name only begins the same way is not covered.
        assert_eq!(
            cgroupfs_read_only(docker, "/sys/fs/cgroup/zygo2"),
            Some(true)
        );
    }

    #[test]
    fn the_last_of_two_mounts_on_one_point_counts() {
        let stacked = format!("{POD_RO}{POD_RW}");
        assert_eq!(cgroupfs_read_only(&stacked, "/sys/fs/cgroup"), Some(false));
    }

    #[test]
    fn no_cgroup2_mount_is_no_answer() {
        let v1 = "30 22 0:26 / /sys/fs/cgroup/memory ro,nosuid - cgroup cgroup rw,memory\n";
        assert_eq!(cgroupfs_read_only(v1, "/sys/fs/cgroup/memory"), None);
        assert_eq!(cgroupfs_read_only("", "/sys/fs/cgroup"), None);
    }
}
