# 23. Security: the threat model

A *threat model* is a written answer to three questions: what are we
protecting, from whom, and what stands in the way. This one is written against
what is actually built and measured, not what is planned. Where a control is
not built, or is built but not yet checked, this chapter says so, because a
threat model that claims too much is worse than none: it is what people plan
around.

## Reporting a vulnerability

If you find a way out of a sandbox, **do not open a public issue**. Report it
privately, through GitHub's private vulnerability reporting on the repository
(*Security → Report a vulnerability*) or by e-mail. [SECURITY.md](../../SECURITY.md)
has the address, what to include, how fast you will hear back, what is in
scope and what is not. Zygo is pre-1.0, only the latest release is supported,
and there is no bug bounty: reports are answered, fixed and credited, not paid
for. No external audit has been done; one is planned.

## Words used in this chapter

| word | meaning |
|---|---|
| tenant | whoever supplies the code that runs in a sandbox |
| escape | code inside a sandbox reaching something outside it |
| control | one lock that stands against an attack, such as a seccomp filter |
| vector | one way an attacker might try to get out |
| CVE | a public, numbered record of a known security bug |
| LPE | *local privilege escalation*: a bug that lets a normal process become root |
| residual risk | the risk that is left after every control, and that you accept |
| backend | how Zygo isolates a sandbox: `ns` (namespaces), `gvisor` or `vm` |

## What is being protected

- The **host's integrity**: its files, its kernel, its other processes.
- **Other tenants**: their code, their data, their secrets.
- The **supervisor** itself, which holds every function's spec and secrets.
- The host's **resources**: memory, CPU, pids, disk, network.

## Who the attacker is

The attacker is a tenant who can run any code they like inside a sandbox.
That is the *design assumption*, not the worst case. Running code you did not
write is what Zygo is for. So every control below is judged by one question:
what can hostile code, already running inside, do next?

## The three trust classes

Not all code is equally hostile. The design sorts it into three *trust
classes*, and gives each one a backend and a risk that is accepted.

| Class | Who | Backend | Residual risk accepted |
|---|---|---|---|
| T1 | Your own team, CI | `ns`, relaxed seccomp | A kernel CVE |
| T2 | Authenticated, contracted customers | `ns` + strict seccomp + Landlock + a network allowlist | A kernel local-privilege-escalation CVE — historically a few critical ones a year |
| T3 | Anonymous, hostile | `vm` — today one-shot only, no network, no in-guest cgroups, seccomp or Landlock; needs KVM and a `make vm-build` binary | A VMM or KVM CVE, which are much rarer |

T2 is a setting, not the default. A function ships with `seccomp = "default"`;
T2 means `seccomp = "strict"` in its `[fn.<name>]` table or `--seccomp
strict`, an `egress` allowlist rather than `full`, and Landlock, which is on
wherever the kernel has it. A runtime pool is `strict` by default, because
its zygotes are shared between tenants ([chapter 20](20-sandbox-toml.md#limits)).

```text
  who wrote the code?            backend                     what could still break it
  ───────────────────            ───────                     ─────────────────────────
  ┌─────────────────────┐        ┌──────────────────────┐
  │ T1  your team, CI   │ ─────▶ │ ns, relaxed seccomp  │ ─▶ a kernel CVE
  └─────────────────────┘        └──────────────────────┘
  ┌─────────────────────┐        ┌──────────────────────┐
  │ T2  paying,         │ ─────▶ │ ns + strict seccomp  │ ─▶ a kernel LPE CVE
  │     known customers │        │ + Landlock           │    (a few critical a year)
  └─────────────────────┘        │ + network allowlist  │
                                 └──────────────────────┘
  ┌─────────────────────┐        ┌──────────────────────┐
  │ T3  anonymous,      │ ─────▶ │ vm                   │ ─▶ a VMM or KVM CVE
  │     hostile         │        │ (not complete today; │    (much rarer)
  └─────────────────────┘        │  see below)          │
                                 └──────────────────────┘
```

## The `vm` backend today

**The `vm` backend runs one-shot sandboxes.** A guest boots, the program runs
under a kernel of its own, and the root filesystem is read-only at the device,
not by a mount option the guest could change. What it does not have yet:
networking, warm functions, and its own in-guest cgroups, seccomp and
Landlock. So a tenant's limits are the VMM's cgroup on the host side, and
nothing finer. (A *VMM*, or virtual machine monitor, is the host program that
runs the virtual machine.) Until those land, T3 workloads do not have the
boundary the design gives them. The honest answer for anonymous code today is
a separate machine.

## The `gvisor` backend today

**The `gvisor` backend runs one-shot sandboxes.** gVisor runs its own kernel
in user space, called the *Sentry*, so the program's syscalls reach the Sentry
instead of the host kernel. On a host without KVM, that is a smaller and
easier-to-defend target than `ns`. `make gvisor-linux` checks that the same
spec behaves the same way on both. It is not a T3 answer either: it holds no
warm functions and has no networking. A rootless `runsc` cannot write cgroups,
so its resource limits are advisory, and a tenant there can still use up the
host's memory.

## The layers an attacker must pass

On the `ns` backend, hostile code has to beat every one of these locks to get
out, or find a bug in the kernel underneath them all.
[Chapter 4](04-other-locks.md) explains each lock.

```text
  ┌─────────────────────────────────────────────────────────────────────┐
  │ host: the kernel, your files, other tenants, the supervisor         │
  │  ┌───────────────────────────────────────────────────────────────┐  │
  │  │ user namespace: root inside, nobody outside; uid_map fixed    │  │
  │  │  ┌─────────────────────────────────────────────────────────┐  │  │
  │  │  │ pid, mount, net, ipc, uts, cgroup namespaces            │  │  │
  │  │  │  ┌───────────────────────────────────────────────────┐  │  │  │
  │  │  │  │ cgroup limits: memory, cpu, pids.max always set   │  │  │  │
  │  │  │  │  ┌─────────────────────────────────────────────┐  │  │  │  │
  │  │  │  │  │ pivot_root, read-only root, no cgroupfs     │  │  │  │  │
  │  │  │  │  │  ┌───────────────────────────────────────┐  │  │  │  │  │
  │  │  │  │  │  │ no capabilities, no_new_privs, nosuid │  │  │  │  │  │
  │  │  │  │  │  │  ┌─────────────────────────────────┐  │  │  │  │  │  │
  │  │  │  │  │  │  │ Landlock (5.13+), nftables      │  │  │  │  │  │  │
  │  │  │  │  │  │  │  ┌───────────────────────────┐  │  │  │  │  │  │  │
  │  │  │  │  │  │  │  │ seccomp allowlist         │  │  │  │  │  │  │  │
  │  │  │  │  │  │  │  │    ┌──────────────┐       │  │  │  │  │  │  │  │
  │  │  │  │  │  │  │  │    │ tenant code  │       │  │  │  │  │  │  │  │
  │  │  │  │  │  │  │  │    └──────────────┘       │  │  │  │  │  │  │  │
  │  │  │  │  │  │  │  └───────────────────────────┘  │  │  │  │  │  │  │
  │  │  │  │  │  │  └─────────────────────────────────┘  │  │  │  │  │  │
  │  │  │  │  │  └───────────────────────────────────────┘  │  │  │  │  │
  │  │  │  │  └─────────────────────────────────────────────┘  │  │  │  │
  │  │  │  └───────────────────────────────────────────────────┘  │  │  │
  │  │  └─────────────────────────────────────────────────────────┘  │  │
  │  └───────────────────────────────────────────────────────────────┘  │
  └─────────────────────────────────────────────────────────────────────┘
     every layer is a feature of ONE kernel: a kernel LPE bug skips them all
```

## How the controls are tested

Every row below marked **attempted** is run by `make escape-linux`. It runs
the escape itself, not a check of a setting, because a test that reads a flag
also passes on a kernel that ignores that flag. The suite attempts 19 vectors in 27
checks, and on Linux 5.10 and 6.8 reports **27 blocked, 0 escaped, 0
skipped**. Run rootless, it skips one: setting up a file capability to try
needs root on the host.

Beside it, `make fuzz-linux` sweeps *every* syscall number the architecture
has — 469 of them — against all three seccomp profiles. Each call is made in a
forked child, so a syscall that blocks or exits takes nothing else with it.
The escape suite tries the attacks somebody thought of; the sweep needs no
imagination. That is what makes it the right check for a filter whose jump
offsets are computed by a program. It checks that the profiles are ordered on
a real kernel (`permissive` ⊋ `default` ⊋ `strict`), that no syscall kills the
process, that `clone3` answers `ENOSYS` rather than `EPERM`, and that every
syscall named in the tables below is refused.

```text
  make escape-linux                        make fuzz-linux
  ─────────────────                        ───────────────
  every known attack, really tried         all 469 syscall numbers
  → 27 blocked, 0 escaped, 0 skipped       × 3 profiles, one forked child each
                                           → profiles ordered, nothing kills
                                             the process, clone3 → ENOSYS
```

## Vectors: the kernel and privileges

| Vector | Control | Status |
|---|---|---|
| Kernel syscall surface | seccomp allowlist (`default`: ~215 syscalls named; 190 of them exist on aarch64, all on x86_64); `bpf`, `io_uring`, `userfaultfd`, `keyctl`, `perf_event_open` and `ptrace` refused | **attempted** — and swept: all 469 syscall numbers under each profile, 271 refused with EPERM under `default` (Linux 6.8, aarch64); the numbers above the table answer ENOSYS |
| `mount()` to reach the host | `CAP_SYS_ADMIN` dropped; seccomp refuses `mount` | **attempted** |
| `setns` into the host's namespaces | refused: no capability in the host's user namespace | **attempted** |
| Regaining capabilities via a new user namespace | `unshare(CLONE_NEWUSER)` refused by seccomp | **attempted** |
| Rewriting `uid_map` to become another uid | the map is written by the parent and is then read-only | **attempted** |
| Regaining privilege through a setuid or file-capability binary | one uid mapped, so setuid has no other identity to switch to; every mount `nosuid`; `no_new_privs`; an empty bounding set | **attempted** — a copy of `python3` given `CAP_DAC_OVERRIDE` reads a mode-000 file outside a sandbox and cannot inside; with the three controls switched off in a test build, it could |
| Reading kernel memory (`/dev/mem`, `/proc/kcore`) | masked and not present | **attempted** |
| Creating a block device to read the host's disk | `mknod` refused; no block devices in `/dev` | **attempted** |

## Vectors: files

| Vector | Control | Status |
|---|---|---|
| Overwriting the runtime binary (CVE-2019-5736 shape) | read-only root; `/proc/self/exe` is not writable | **attempted** |
| `cgroup` `release_agent` | cgroupfs is not mounted in the sandbox at all | **attempted** |
| Writing through a read-only bind mount | read-only for the mount *and every mount below it*: `mount_setattr(AT_RECURSIVE)` on 5.12+, one remount per submount (read from the host's mount table) below that | **attempted** — including a tmpfs mounted inside the read-only source |
| Setuid binaries or device nodes in a shared folder | every bind, `:rw` as well, is `nosuid,nodev`, recursively | **attempted** |
| Escaping a writable mount by symlink | `pivot_root`; the symlink resolves inside the sandbox root | **attempted** |
| Reaching the host's filesystem | `pivot_root` with the old root detached | **attempted** |
| Tampering with shared image layers | the store is not reachable from inside; layers are bound read-only | **attempted** |

## Vectors: processes and resources

| Vector | Control | Status |
|---|---|---|
| Seeing or signalling host processes | separate pid namespace; only the sandbox's own processes are visible | **attempted** |
| Resource exhaustion | mandatory cgroup limits; `pids.max` always set; `memory.max` and `memory.oom.group` on each request's own cgroup | **attempted** separately by `make verify-linux` (the fork bomb is cut off at `pids.max`; the memory hog is OOM-killed inside its own cgroup and the host loses 0 MB) and `make verify-oom-linux` (in a warm function, the hog dies and the three requests beside it, and the zygote, do not) |
| Zygote contamination | the zygote never handles a request itself; every request is a fresh process that ends in `_exit` | by construction |
| A file left in the temp folder for the next request (or tenant) | each request's `TMPDIR` is its own folder under `/work`, which cannot be listed, removed when the request ends; a literal `/tmp/...` path is still shared by the sandbox | **attempted** (case 18, in a runtime pool) |

## Vectors: the network

| Vector | Control | Status |
|---|---|---|
| Reaching the host over the network | default `network = "none"`; under `egress`/`full`, RFC1918, CGNAT, link-local, loopback, multicast and reserved ranges are rejected *above* every allow rule, so a hostname that resolves into one is refused too | **attempted** by the supervisor suite, and measured from outside by the first consumer: under `--net full` the cloud metadata address, the host's own Postgres and the LAN router are all *no route*, where Docker's default bridge reaches two of the three (see [below](#measured-against-docker)) |
| Using a resolver of one's own to dodge the allowlist | DNS is forced to one address; port 53 to anything else is rejected | **attempted** |

## Vectors: secrets and the supervisor

| Vector | Control | Status |
|---|---|---|
| Secrets | never in `EXEC`, never in the zygote: written by the supervisor from *outside* the sandbox to `/run/secrets/<name>` (0400) between `FORKED` and `GO`, removed when the last request in flight finishes. A warm-exec sandbox is reached through a directory descriptor its own init hands out before it hardens, not through `/proc` — which a non-dumpable process does not offer an unprivileged supervisor at all | **attempted** — the file is absent between requests, the value is absent from the agent's `environ`, and both paths are exercised as an ordinary user |
| The supervisor's socket | unix socket 0600 inside a 0700 directory, plus an `SO_PEERCRED` uid check | **attempted** |
| The HTTP API | bearer token from the environment only, compared in constant time; refuses to start unauthenticated on a reachable address | **attempted** |

## Out of scope

| Vector | Control | Status |
|---|---|---|
| Timing / microarchitectural side channels | **out of scope** | — |

Timing side channels, such as Spectre, let code learn secrets by measuring
how long things take on a shared CPU. Tenants who need protection from that
belong on the `vm` backend and on separate hosts.

## The stated guarantee for egress

*Egress* is traffic from the sandbox out to the network. Under
`network = "egress"` or `"full"`, a sandbox **cannot reach** the cloud
metadata endpoint (`169.254.169.254`), any RFC1918 address (the host, its
neighbours, the LAN's router), any CGNAT, link-local, multicast or reserved
address, or the host's loopback — whatever name they resolve from — unless the operator passes
`--allow-private-net`. This is enforced inside the sandbox's own network
namespace by nftables rules that sit *above* every allow rule, and by a
resolver that admits only what the allowlist names. What Zygo does *not*
guarantee is anything about the public internet under `full`: that mode means
"the internet and nothing of yours".

```text
  sandbox wants to connect to …
        │
        ▼
  ┌───────────────────────────────────────────┐
  │ nftables rule 1 (checked first):          │
  │ 169.254.x, 10.x, 172.16-31.x, 192.168.x,  │── match ──▶ refused, "no route"
  │ CGNAT, link-local, loopback               │   (unless --allow-private-net)
  └─────────────────────┬─────────────────────┘
                        │ no match
                        ▼
  ┌───────────────────────────────────────────┐
  │ allow rules (egress: only the allow list; │── match ──▶ connected
  │ full: the public internet)                │
  └─────────────────────┬─────────────────────┘
                        │ no match
                        ▼
                     refused
```

## Measured against Docker

This was measured, not read from the code, with a small connection probe on
one host. The probe through Zygo's `--net full` found
*no route* to the metadata address, the host's Postgres, the LAN router and
`10.0.0.1`, and reached `1.1.1.1:53`. Docker's `--network bridge` on the same
host reached the host's Postgres and the LAN router, and routed the metadata
address (it was refused by the host, not blocked). Docker needs four
`iptables -I DOCKER-USER … -j DROP` rules for the same result, one for each of
`169.254.0.0/16`, `10.0.0.0/8`, `172.16.0.0/12` and `192.168.0.0/16`.
[Chapter 5](05-docker.md) has more on Docker's network.

| destination | Zygo `--net full` | Docker `--network bridge` |
|---|---|---|
| cloud metadata `169.254.169.254` | no route | routed (refused by the host, not blocked) |
| the host's Postgres | no route | reached |
| the LAN router | no route | reached |
| `10.0.0.1` | no route | not reported |
| `1.1.1.1:53` | reached | not reported |

## Where the boundary is weaker than it looks

This part is said plainly, because `zygo doctor` says it too. Each item is a
place where the protection is thinner than the tables above might suggest.

### `ns` is one kernel

Every control above is a feature of the kernel. A kernel
local-privilege-escalation bug defeats all of them at once. That is the
accepted residual risk for T1 and T2, and the reason `vm` exists for T3.
`zygo doctor` says how old the running kernel's *series* is, and warns past
two years, because that risk grows with the gap. Age is not the same as
unpatched: a long-term series gets fixes backported without changing its
version, and the warning says which kind it is looking at. But a host four
years behind is four years of hardening behind, and every control here rests
on it.

### Without `newuidmap`, tenants share a uid

`newuidmap` is the small setuid helper (from the `uidmap` package) that gives
each sandbox its own range of user ids. With no such range, the id map falls
back to a single entry, so two sandboxes run as the same host uid. They are
then kept apart by namespaces and file modes, but not also by different uids.
`zygo doctor` reports this as degraded.

### Landlock needs 5.13, and its network rules 6.7

On older kernels the filesystem allowlist is absent, and the mount plan is the
only filesystem boundary. The process-level network rules — `bind` refused
everywhere, `connect` limited to the allowlist's ports under `egress` — are
built and unit-tested, and enforced for real in one place: CI's
`landlock-network` job, on an ubuntu-24.04 runner (Linux 6.8, Landlock ABI
v4), runs `tests/linux/verify_landlock_net.sh` (`make landlock-net-linux` runs the
same script in a container). It picks the two refusals nftables cannot
produce: a `bind()` on a TCP port, which sends no packet, and a loopback
`connect()` to a port off the allowlist, which the packet filter accepts on its
first line. Both must fail with `EACCES`; a loopback connect on an allowed
port must get past Landlock; under `network = "none"` both are refused. On a
kernel below 6.7 the script says `SKIP` and exits 0 — which is why the job is
not mixed into one that also runs on 22.04. Neither of this project's own
development machines can run it: Docker Desktop's 5.10 kernel reports ABI 0,
and the Raspberry Pi's kernel has no Landlock at all. Below 6.7, the nftables
allowlist inside the namespace is the only egress control, and it is the one
every other check here runs against.

### `cgroup.kill` needs 5.14

`cgroup.kill` is one file write that kills every process in a cgroup. Below
5.14, killing a request's process tree falls back to freeze → signal → thaw.
That has a race which the freeze closes, but it is more machinery than one
write.

### The seccomp profiles have been run against a sample, not a population

The [compatibility matrix](24-seccomp-profiles.md#the-compatibility-matrix)
runs seven Python packages and three Node cases under `default` and `strict`.
Each round of widening it has found something:

- the filter refusing every thread;
- `strict` refusing the agent its own control socket;
- when Node was added, `strict` removing `socketpair`, which libuv uses for
  every pipe, so a `strict` Node function could not start a worker at all.

All are fixed and pinned by tests. But a control that has been run against ten
cases has been run against ten cases.

### The child filter is installed by the agent

Under `strict`, the agent's forked child is locked down further: no `execve`,
no new process. The *agent* does this, at the supervisor's request, so it is
only as good as the agent. `zygo agent test` now checks it rather than
trusting it: an agent that ignores `ZYGO_CHILD_SECCOMP` and runs the request
anyway fails conformance. The Python agent installs the filter. The Node agent
installs it when the image has the helper object, and otherwise falls back to
Node's permission model, which is weaker against a V8 escape and says so in
`READY`. The `sh` example refuses the request outright.
[Chapter 24](24-seccomp-profiles.md#the-child-filter) has the details.

### On Ubuntu 24.04 and later, Zygo asks you to turn something off

`kernel.apparmor_restrict_unprivileged_userns=1` stops a normal process from
mounting inside a user namespace, which is the first thing every sandbox does.
`zygo doctor` finds this by trying the mount, not by reading the sysctl.
`zygo doctor --fix` then installs an AppArmor profile that gives the `zygo`
binary alone the `userns` permission — the way Ubuntu lets its own browsers
and container tools past the same rule — and the restriction stays on for
every other process. Only where AppArmor cannot load a profile does it offer
`sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` instead. Be clear
about what that is. It is a host-wide protection against a class of
local privilege escalation that starts with an unprivileged user namespace,
and turning it off removes it for **every** process on the machine, not only
Zygo's. It is the right call on a host whose job is running sandboxes, and it
is what `shim/lima.yaml` does inside Zygo's own VM. On a shared workstation,
the profile is the right answer; [`packaging/apparmor/zygo`](../../packaging/apparmor/zygo)
is the file.
[Troubleshooting](22-troubleshooting.md#applying-a-bind-mount-from-the-spec-failed-no-such-file-or-directory)
shows `zygo doctor --fix`, which applies it after asking.

### Egress needs `pasta` and `nft`

If either is missing, a networked sandbox refuses to start. It does not start
without a firewall. But that is a liveness failure — the service stops working
— and you should know about it before it happens.

### A derived system layer is built as root, with host networking

A *derived layer* is an image layer Zygo builds by installing system packages
on top of an image. It installs them as root inside the sandbox's user
namespace, with host networking, once, before any tenant code exists. The
package list is checked against the Debian package-name alphabet and passed as
separate arguments, never through a shell.

## What has not been reviewed

**No external security audit has been done.** That is the largest gap in this
chapter, and it needs an auditor, not a commit. The hardening that could be
done without one is in place: the full syscall sweep described above, and the
kernel-age warning in `zygo doctor`.

Until an audit happens, the strongest honest statement is this: every vector
listed above is attempted by a suite that runs on every change, none of them
currently succeed, and the syscall surface they rest on is swept in full
rather than sampled.

## Hardening your deployment

These points come from [SECURITY.md](../../SECURITY.md):

- Prefer `isolation = "vm"` for code you did not write. The `ns` backend leans
  on one kernel, and does not hide it.
- Keep `network = "none"` unless a function really needs egress, and keep the
  `allow` list to the hosts it needs.
- Do not run Zygo as root. It does not need it, and `pasta`, `newuidmap` and
  cgroup delegation all behave better without it.
- Bind the HTTP API to loopback or a unix socket. It refuses to start without
  a token on a reachable address, but the safe default is worth keeping.
- Install `uidmap`, so tenants get separate subordinate uid ranges rather than
  sharing one identity map. Zygo does not let a host become multi-tenant
  without one: registering a tenant is refused on a host whose user has no
  range in `/etc/subuid`, unless the supervisor was started with
  `ZYGO_ALLOW_SHARED_UID=1`, which is the operator saying they accept that
  every tenant's sandbox shares one host uid.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [22. Troubleshooting](22-troubleshooting.md) · [Contents](README.md) · **Next: [Fork safety, question by question](fork-safety.md) →**
