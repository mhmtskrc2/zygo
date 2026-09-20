//! Resource limits and their translation to kernel knobs (design doc §3.5).
//!
//! The struct is platform independent and so is [`Limits::cgroup_writes`]; the
//! `ns` backend just writes what it is handed. That keeps the interesting part
//! — *which* values end up in `memory.max` and friends — unit testable on any
//! host, which matters because getting these wrong is a security bug, not a
//! performance one.

use crate::spec::{Bytes, Cpu, Duration};

/// The full limit set for one sandbox. There is no "unlimited" variant for the
/// fields that matter: principle P7 says a sandbox without limits does not
/// exist.
#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    /// `memory.max` — hard limit; exceeding it OOM-kills this cgroup only.
    pub mem: Bytes,
    /// `memory.high` — throttle and reclaim before killing.
    pub mem_high: Bytes,
    /// `memory.swap.max` — zero by default so a tenant cannot drown the host's swap.
    pub swap: Bytes,
    /// `memory.oom.group` — kill the whole request tree, not a random child.
    pub oom_group: bool,
    /// `cpu.max`, in cores.
    pub cpu: Cpu,
    /// `pids.max` — the fork-bomb limit, and the most important one.
    pub pids: u32,
    /// Wall-clock budget, enforced by the supervisor plus `cgroup.kill`.
    pub timeout: Duration,
    /// `/tmp` tmpfs size. Counts against `mem`.
    pub scratch: Bytes,
    /// `/tmp` tmpfs inode count, so a tenant cannot exhaust inodes cheaply.
    pub scratch_inodes: u64,
    /// `io.max` read bandwidth, bytes/s. `None` leaves I/O unlimited.
    pub io_read: Option<Bytes>,
    /// `io.max` write bandwidth, bytes/s.
    pub io_write: Option<Bytes>,
    /// `RLIMIT_NOFILE`.
    pub nofile: u64,
    /// `RLIMIT_FSIZE`.
    pub fsize: Bytes,
    /// Concurrent TCP connections, enforced by an nftables connection count
    /// inside the sandbox's network namespace. Meaningless for a sandbox
    /// without a network, and harmless there.
    pub connections: u32,
    /// Bytes per second the sandbox may **send**, enforced by a `tc` token
    /// bucket inside the namespace. What it receives is shaped the same way
    /// only where the host has an `ifb` device to queue ingress through;
    /// without one it is not shaped, and the supervisor says so. `None` leaves
    /// the link unlimited.
    pub bandwidth: Option<Bytes>,
}

/// One `(file, contents)` pair to write under a cgroup v2 directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgroupWrite {
    pub file: &'static str,
    pub value: String,
}

impl Limits {
    /// The cgroup v2 files to write, in the order they must be written.
    ///
    /// `memory.max` comes before `memory.high` deliberately: writing a `high`
    /// above the current `max` is rejected on some kernels, and the reverse
    /// order is always accepted.
    pub fn cgroup_writes(&self) -> Vec<CgroupWrite> {
        let mut w = vec![CgroupWrite {
            file: "memory.max",
            value: self.mem.get().to_string(),
        }];

        // `memory.high` throttles and reclaims before the hard limit is hit —
        // but only helps when there is somewhere to reclaim *to*. With
        // `memory.swap.max = 0`, anonymous memory cannot be reclaimed at all,
        // so a runaway allocation is throttled harder and harder without ever
        // reaching `memory.max`: it never gets OOM-killed, it simply stops
        // making progress until the wall-clock limit fires. Measured on a real
        // kernel: `memory.current` pinned 3 MB under the limit, `high` events
        // climbing into the thousands, `oom_kill` still 0 after 14 s.
        //
        // A prompt OOM kill is the better outcome — it is accurate, it frees
        // the slot immediately, and the tenant gets "out of memory" instead of
        // "timed out".
        if self.swap.get() > 0 {
            w.push(CgroupWrite {
                file: "memory.high",
                value: self.mem_high.get().to_string(),
            });
        }

        w.extend([
            CgroupWrite {
                file: "memory.swap.max",
                value: self.swap.get().to_string(),
            },
            CgroupWrite {
                file: "memory.oom.group",
                value: u8::from(self.oom_group).to_string(),
            },
            CgroupWrite {
                file: "pids.max",
                value: self.pids.to_string(),
            },
            CgroupWrite {
                file: "cpu.max",
                value: self.cpu.cgroup_max(),
            },
            CgroupWrite {
                file: "cpu.weight",
                value: "100".to_string(),
            },
        ]);

        // `io.max` is per-device and needs a `major:minor`, which only the
        // launcher knows. Emitted separately by `io_max_for_device`.
        let _ = (&self.io_read, &self.io_write);
        w.retain(|e| !e.value.is_empty());
        w
    }

    /// `io.max` line for a specific block device, or `None` when no I/O limit
    /// is configured.
    pub fn io_max_for_device(&self, major: u32, minor: u32) -> Option<CgroupWrite> {
        if self.io_read.is_none() && self.io_write.is_none() {
            return None;
        }
        let mut parts = vec![format!("{major}:{minor}")];
        if let Some(r) = self.io_read {
            parts.push(format!("rbps={}", r.get()));
        }
        if let Some(w) = self.io_write {
            parts.push(format!("wbps={}", w.get()));
        }
        Some(CgroupWrite {
            file: "io.max",
            value: parts.join(" "),
        })
    }

    /// `mount -o` options for the `/tmp` tmpfs.
    pub fn scratch_mount_options(&self) -> String {
        format!(
            "size={},nr_inodes={},mode=1777",
            self.scratch.get(),
            self.scratch_inodes
        )
    }

    /// rlimits to apply just before `execve`, as `(resource, value)` pairs.
    ///
    /// `RLIMIT_NPROC` mirrors `pids.max` as a backstop: on a host without
    /// cgroup delegation the pids controller may be unavailable, and an
    /// unbounded `fork()` is the one failure that takes the whole box down.
    pub fn rlimits(&self) -> Vec<(RlimitKind, u64)> {
        vec![
            (RlimitKind::NoFile, self.nofile),
            (RlimitKind::FSize, self.fsize.get()),
            (RlimitKind::Core, 0),
            (RlimitKind::NProc, self.pids as u64),
        ]
    }
}

/// The rlimits Zygo sets. Kept as an enum rather than raw `libc` constants so
/// the limit plan is inspectable (and testable) on non-Linux hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RlimitKind {
    NoFile,
    FSize,
    Core,
    NProc,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            mem: Bytes::from_mib(256),
            mem_high: Bytes::from_mib(256).scaled(0.9),
            swap: Bytes(0),
            oom_group: true,
            connections: 256,
            bandwidth: None,
            cpu: Cpu(0.5),
            pids: 64,
            timeout: Duration::from_secs(30),
            scratch: Bytes::from_mib(64),
            scratch_inodes: 10_000,
            io_read: None,
            io_write: None,
            nofile: 1024,
            fsize: Bytes::from_mib(64),
        }
    }

    #[test]
    fn cgroup_writes_cover_every_mandatory_knob() {
        let w = limits().cgroup_writes();
        let files: Vec<&str> = w.iter().map(|e| e.file).collect();
        for required in [
            "memory.max",
            "memory.swap.max",
            "memory.oom.group",
            "pids.max",
            "cpu.max",
        ] {
            assert!(files.contains(&required), "missing {required} in {files:?}");
        }
    }

    /// Without swap there is nothing to reclaim, so `memory.high` would
    /// throttle a runaway allocation into a livelock instead of letting
    /// `memory.max` kill it. Measured on a real kernel; see docs/poc-report.md.
    #[test]
    fn memory_high_is_only_set_when_swap_can_absorb_the_reclaim() {
        let no_swap = limits();
        assert_eq!(no_swap.swap, Bytes(0));
        assert!(
            !no_swap
                .cgroup_writes()
                .iter()
                .any(|w| w.file == "memory.high"),
            "memory.high with swap.max=0 turns an OOM into a hang"
        );

        let with_swap = Limits {
            swap: Bytes::from_mib(64),
            ..limits()
        };
        let w = with_swap.cgroup_writes();
        assert!(w.iter().any(|e| e.file == "memory.high"));
        // `memory.high` above the current `memory.max` is rejected, so the
        // hard limit has to be written first.
        let pos = |f: &str| w.iter().position(|e| e.file == f).unwrap();
        assert!(pos("memory.max") < pos("memory.high"));
    }

    #[test]
    fn values_are_plain_byte_counts() {
        let w = limits().cgroup_writes();
        let get = |f: &str| &w.iter().find(|e| e.file == f).unwrap().value;
        assert_eq!(get("memory.max"), "268435456");
        assert_eq!(get("memory.swap.max"), "0");
        assert_eq!(get("memory.oom.group"), "1");
        assert_eq!(get("cpu.max"), "50000 100000");
        assert_eq!(get("pids.max"), "64");
    }

    #[test]
    fn io_max_is_omitted_when_unset_and_formatted_when_set() {
        assert!(limits().io_max_for_device(8, 0).is_none());

        let l = Limits {
            io_read: Some(Bytes::from_mib(50)),
            io_write: Some(Bytes::from_mib(10)),
            ..limits()
        };
        let w = l.io_max_for_device(259, 0).unwrap();
        assert_eq!(w.file, "io.max");
        assert_eq!(w.value, "259:0 rbps=52428800 wbps=10485760");

        let l = Limits {
            io_read: None,
            io_write: Some(Bytes::from_mib(10)),
            ..limits()
        };
        assert_eq!(
            l.io_max_for_device(8, 0).unwrap().value,
            "8:0 wbps=10485760"
        );
    }

    #[test]
    fn scratch_options_bound_both_size_and_inodes() {
        let o = limits().scratch_mount_options();
        assert!(o.contains("size=67108864"), "{o}");
        assert!(o.contains("nr_inodes=10000"), "{o}");
        assert!(o.contains("mode=1777"), "{o}");
    }

    #[test]
    fn rlimits_include_a_fork_backstop_and_disable_core_dumps() {
        let r = limits().rlimits();
        assert!(r.contains(&(RlimitKind::NProc, 64)));
        assert!(r.contains(&(RlimitKind::Core, 0)));
        assert!(r.contains(&(RlimitKind::NoFile, 1024)));
    }
}
