# Threat model

The threat model, written against what is actually built and measured rather
than what is planned. Where a control is not
implemented, or is implemented and unverified, it says so — a threat model that
overstates its coverage is worse than none, because it is what people plan
around.

Reporting: [SECURITY.md](../SECURITY.md).

## What is being protected

* The **host's integrity** — its filesystem, its kernel, its other processes.
* **Other tenants**: their code, their data, their secrets.
* The **supervisor** itself, which holds every function's spec and secrets.
* The host's **resources**: memory, CPU, pids, disk, network.

## Who the attacker is

A tenant who can run arbitrary code inside a sandbox. That is the *design
assumption*, not a worst case: running code you did not write is what Zygo is
for. Three trust classes, from the design:

| Class | Who | Backend | Residual risk accepted |
|---|---|---|---|
| T1 | Your own team, CI | `ns`, relaxed seccomp | A kernel CVE |
| T2 | Authenticated, contracted customers | `ns` + strict seccomp + Landlock + a network allowlist | A kernel local-privilege-escalation CVE — historically a few critical ones a year |
| T3 | Anonymous, hostile | `vm` | A VMM or KVM CVE, which are much rarer |

**The `vm` backend runs one-shot sandboxes.** A guest boots, the program runs
under a kernel of its own, and the root filesystem is read-only at the device
rather than by a mount option the guest could change. What it does not have
yet: any writable scratch inside the guest, networking, warm functions, and
its own in-guest cgroups, seccomp and Landlock — so a tenant's limits are the
VMM's host-side cgroup and nothing finer. Until those land, T3 workloads do not have the boundary the
design assigns them, and the honest answer for anonymous code today is a
separate machine.

**The `gvisor` backend runs one-shot sandboxes.** It moves the syscall surface
from the host kernel into gVisor's Sentry, which is a smaller and more
defensible target than `ns` on a host without KVM, and `make gvisor-linux`
checks that the same spec behaves the same way on both. It is not a T3 answer
either: it holds no warm functions, has no networking, and a rootless `runsc`
cannot write cgroups, so its resource limits are advisory. A tenant there can
still exhaust host memory.

## Vectors and what stands against them

Every row marked **attempted** is exercised by `make escape-linux`, which runs
the escape rather than inspecting a setting — a test that reads a flag also
passes on a kernel that ignores that flag. The suite currently reports
**16 blocked, 0 escaped, 1 skipped**.

Beside it, `make fuzz-linux` sweeps *every* syscall number the architecture
has — 469 of them — against all three profiles, each call made in a forked
child so a syscall that blocks or exits takes nothing with it. The escape
suite attempts the vectors somebody thought of; the sweep needs no
imagination, which is what makes it the check for a filter whose branch
offsets are computed. It asserts the profiles are ordered on a real kernel
(`permissive` ⊋ `default` ⊋ `strict`), that no syscall kills the process, that
`clone3` answers `ENOSYS` rather than `EPERM`, and that every syscall named in
the table below is refused.

| Vector | Control | Status |
|---|---|---|
| Kernel syscall surface | seccomp allowlist (~190 syscalls named; about 170 of them exist on a given architecture); `bpf`, `io_uring`, `userfaultfd`, `keyctl`, `perf_event_open` and `ptrace` refused | **attempted** — and swept: all 469 syscall numbers under each profile, 300 refused with EPERM under `default` |
| Overwriting the runtime binary (CVE-2019-5736 shape) | read-only root; `/proc/self/exe` is not writable | **attempted** |
| `cgroup` `release_agent` | cgroupfs is not mounted in the sandbox at all | **attempted** |
| `mount()` to reach the host | `CAP_SYS_ADMIN` dropped; seccomp refuses `mount` | **attempted** |
| `setns` into the host's namespaces | refused: no capability in the host's user namespace | **attempted** |
| Regaining capabilities via a new user namespace | `unshare(CLONE_NEWUSER)` refused by seccomp | **attempted** |
| Rewriting `uid_map` to become another uid | the map is written by the parent and is then read-only | **attempted** |
| Creating a block device to read the host's disk | `mknod` refused; no block devices in `/dev` | **attempted** |
| Reading kernel memory (`/dev/mem`, `/proc/kcore`) | masked and not present | **attempted** |
| Seeing or signalling host processes | separate pid namespace; only the sandbox's own processes are visible | **attempted** |
| Writing through a read-only bind mount | `MS_RDONLY` remount after the bind | **attempted** |
| Escaping a writable mount by symlink | `pivot_root`; the symlink resolves inside the sandbox root | **attempted** |
| Regaining privilege through a setuid binary | `no_new_privs`, `nosuid` | **skipped** where the test image has no setuid binary to try |
| Reaching the host's filesystem | `pivot_root` with the old root detached | **attempted** |
| Tampering with shared image layers | the store is not reachable from inside; layers are bound read-only | **attempted** |
| Resource exhaustion | mandatory cgroup limits; `pids.max` always set; `memory.oom.group` | **attempted** separately by `make verify-linux` (the fork bomb is cut off at `pids.max`; the memory hog is OOM-killed inside its own cgroup and the host loses 0 MB) |
| Reaching the host over the network | default `network = "none"`; under `egress`/`full`, RFC1918, CGNAT, link-local and loopback are rejected *above* every allow rule, so a hostname that resolves into one is refused too | **attempted** by the supervisor suite |
| Using a resolver of one's own to dodge the allowlist | DNS is forced to one address; port 53 to anything else is rejected | **attempted** |
| Secrets | never in `EXEC`, never in the zygote: written by the supervisor from *outside* the sandbox to `/run/secrets/<name>` (0400) between `FORKED` and `GO`, removed when the last request in flight finishes. A warm-exec sandbox is reached through a directory descriptor its own init hands out before it hardens, not through `/proc` — which a non-dumpable process does not offer an unprivileged supervisor at all | **attempted** — the file is absent between requests, the value is absent from the agent's `environ`, and both paths are exercised as an ordinary user |
| The supervisor's socket | unix socket 0600 inside a 0700 directory, plus an `SO_PEERCRED` uid check | **attempted** |
| The HTTP API | bearer token from the environment only, compared in constant time; refuses to start unauthenticated on a reachable address | **attempted** |
| Zygote contamination | the zygote never handles a request itself; every request is a fresh process that ends in `_exit` | by construction |
| Timing / microarchitectural side channels | **out of scope** | — |

## Where the boundary is weaker than it looks

Said plainly, because `zygo doctor` says it too:

* **`ns` is one kernel.** Every control above is a kernel feature. A kernel
  local-privilege-escalation bug defeats all of them at once. That is the
  accepted residual risk for T1 and T2, and the reason `vm` exists for T3.
  `zygo doctor` now says how old the running kernel's *series* is and warns
  past two years, because that residual risk grows with the gap. Age is not
  the same as unpatched — a long-term series gets backports without changing
  its version, and the warning says which kind it is looking at — but a host
  four years behind is four years of hardening behind, and every control here
  rests on it.
* **Without `newuidmap`, tenants share a uid.** With no subordinate range the
  id map degenerates to a single identity entry, so two sandboxes run as the
  same host uid and are protected from each other by namespaces and file modes
  rather than by uid separation as well. `zygo doctor` reports this as degraded.
* **Landlock needs 5.13, and its network rules 6.7.** On older kernels the
  filesystem allowlist is absent and the mount plan is the only filesystem
  boundary. The process-level network rules — `bind` denied everywhere,
  `connect` limited to the allowlist's ports under `egress` — are built and
  unit-tested, but have not been exercised on a 6.7+ kernel by this project's
  own suite; CI's ubuntu-24.04 runner is where that happens. Below 6.7 the
  nftables allowlist inside the namespace is the only egress control, and it
  is the one every check here runs against.
* **`cgroup.kill` needs 5.14.** Below it, killing a request's process tree
  falls back to freeze → signal → thaw, which is a race the freeze closes but
  which is more machinery than one write.
* **The seccomp profiles were only recently run against real packages.**
  The [compatibility matrix](seccomp-profiles.md) now exercises five packages
  under `default` and `strict`, and its first run found the filter denying
  every thread and `strict` denying the agent its own control socket. Both
  are fixed and pinned by tests — but a control that has been run against five
  packages has been run against five packages. The agent's forked child is
  tightened further under `strict` (no `execve`, no new process) — by the
  agent, at the supervisor's request, which means only agents that honour
  `ZYGO_CHILD_SECCOMP` have it. The Python reference agent does; the Node and
  sh examples do not.
* **On Ubuntu 24.04 and later, Zygo asks you to turn something off.**
  `kernel.apparmor_restrict_unprivileged_userns=1` stops an unprivileged
  process mounting inside a user namespace, which is the first thing every
  sandbox does. `zygo doctor` detects it — by attempting the mount, not by
  reading the sysctl — and prints `sysctl -w
  kernel.apparmor_restrict_unprivileged_userns=0` as the remedy. Be clear
  about what that is: a host-wide protection against a class of local
  privilege escalation that starts with an unprivileged user namespace, and
  turning it off removes it for **every** process on the machine, not only
  Zygo's. It is the right call on a host whose job is running sandboxes, and
  it is what `shim/lima.yaml` does inside Zygo's own VM. On a shared
  workstation the narrower answer is an AppArmor profile that permits it for
  the `zygo` binary alone; Zygo does not ship one yet, and that is a gap.

* **Egress needs `pasta` and `nft`.** If either is missing, a networked sandbox
  refuses to start — it does not start unconfined — but that is a liveness
  failure you should know about before it happens.
* **A derived system layer installs packages as root inside the sandbox's user
  namespace, with host networking**, once, before any tenant code exists. The
  package list is validated against the Debian name alphabet and passed as
  positional parameters, never through a shell.

## What has not been reviewed

**No external security audit has been done.** That is the largest gap in this
document, and it needs an auditor, not a commit. The hardening that could be
done without one is in place: the syscall sweep described above, and the
kernel-age warning in `zygo doctor`.

Until an audit happens, the strongest honest statement is the one at the top
of this file: every vector listed above is attempted by a suite that runs on
every change, none of them currently succeed, and the syscall surface they
rest on is swept in full rather than sampled.
