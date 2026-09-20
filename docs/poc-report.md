# Phase 0 — Measurement report

Source: [ahmed.md](../ahmed.md) §7 Phase 0 · Plan: [todo.md](../todo.md) §0.2
Date: 18 September 2026

## Decision

**Continue.** The architecture's core assumption — that the fork-based warm path
stays under 2 ms — was verified over 100,000 requests. All three acceptance
criteria passed. One PoC (1) missed its own target, but the cause was isolated
and it does not touch the product's actual promise; it is turned into an
architectural recommendation below.

| PoC | Subject | Result |
|---|---|---|
| 1 | Sandbox setup cost | **FAIL** — p50 3.96 ms (target < 3 ms); cause isolated |
| 2 | cgroup v2 limits | **PASS** — 4/4 |
| 3 | Warm request overhead | **PASS** — p50 1.89 ms, p99 3.66 ms (100k requests) |
| 4 | CoW with `gc.freeze()` | **PASS** — 0.81 MB per request |
| 5 | seccomp allowlist + packages | **PASS** — 8/8 |
| 6 | overlayfs in a userns | Measured — absent on 5.10, the flatten fallback is mandatory |
| 7 | `zygo-core` API sketch | Partly — the PoC 3 driver used the crate by embedding it |
| 8 | libkrun boot cost | **Could not be run** — no KVM in this environment |
| 9 | Is a netns pool feasible? | **No** — incompatible with a per-sandbox userns; PoC 1's recommendation withdrawn |

**Acceptance criteria (document §7):** PoC 3 p50 < 2 ms and p99 < 10 ms ✓ ·
the host is unaffected in PoC 2 ✓ · all five packages work in PoC 5 ✓

---

## The measuring environment, and how far it can be trusted

```
kernel   5.10.104-linuxkit aarch64   (Docker Desktop LinuxKit VM)
host     Apple Silicon, macOS 24.1, Virtualization.framework
cpu/mem  5 cores / 8 GB
python   3.12.14
```

Ways this environment makes the report **pessimistic**:

- The measurements run under nested virtualisation (macOS → VM → container). On
  bare Linux, syscall and fork costs are lower.
- The user's other services were running in the same container at the time
  (elasticsearch, mysql, postgres, minio); some of the spikes in p99.9 and max
  come from that.

Ways this environment **narrowed the scope** — worth stating plainly:

- **Kernel 5.10 is below our own minimum of 5.11.** So Landlock (5.13),
  `cgroup.kill` (5.14), `memory.peak` (5.19) and overlayfs in a userns (5.11)
  could not be tested at all in this report. PoC 6's multi-kernel matrix
  (5.15 / 6.1 / 6.8) could not be run.
- **There is no KVM**, so the `vm` backend and PoC 8 could not be measured.
- PoC 1 and 2 ran as root inside a privileged container; the **real rootless
  scenario** (userns + `/etc/subuid` + `newuidmap`) was not verified.

These three gaps have to close in the phase 1 CI matrix; they are tied to
`todo.md` §1.5.

---

## PoC 3 — Warm request overhead (the acceptance gate)

Measured: a full round trip from the supervisor against the **real** reference
agent (not a throwaway zygote). The handler is empty, so what is measured is
pure overhead.

```
EXEC ──► agent ──fork()──► child
     ◄── FORKED                        │ fork phase
  [create the request cgroup, move pid]│ cgroup phase
GO   ──►                               │
     ◄── DONE                          │ run phase
  [remove the cgroup]
```

**100,000 requests, 1,000 warm-up:**

| Phase | p50 | p90 | p99 | p99.9 | max |
|---|---|---|---|---|---|
| **total (EXEC → DONE)** | **1887 µs** | 2331 | **3664** | 9149 | 60212 |
| fork (EXEC → FORKED) | 610 | 819 | 1187 | 2588 | 36946 |
| cgroup (FORKED → GO) | 97 | 189 | 352 | 955 | 15997 |
| run (GO → DONE) | 1144 | 1495 | 2480 | 5830 | 38990 |

Throughput: 508 req/s (single stream, sequential). Zygote RSS: 15 MB, warm-up
0.3 ms.

**Acceptance: PASS** (p50 1887 < 2000 µs; p99 3664 < 10000 µs).

**Worth noticing:** there is only **6% of headroom** at p50. On its own that says
the target was chosen correctly but the margin is thin. When phase 2 adds the
supervisor's real work to this path (queue, timeout timer, metrics, logging) the
budget may be exceeded; a regression test is essential (todo.md §2.9).

The per-request cgroup was measured at **97 µs** — the same order as the
document's "~50 µs" estimate. For open question A2 ("a cgroup per request or one
per tenant"): a per-request cgroup is 5% of the total budget, which is cheap in
exchange for killing the whole tree in one write with `cgroup.kill`. **Keep it
per request by default.**

> **Correction (phase 2.2).** That 97 µs is not the cost of a per-request
> cgroup. The pid the agent reports is namespace-local, so the write into
> `cgroup.procs` moved nothing and the directory stayed empty — 97 µs is the
> cost of creating and removing an empty directory. The bug is described under
> [phase 2.2](#phase-22--the-pid-in-forked-was-namespace-local-so-nothing-was-ever-moved)
> and is fixed; the real cost has not been re-measured, so **A2's conclusion is
> not yet supported by a measurement.**

### The bug this PoC found

The first measurement gave p50 **11.8 ms** — six times the target. The cause was
lazy `import`s on the agent's child path, paid again on every request:

| Module | Cost on top of an empty interpreter |
|---|---|
| `inspect` (for `isawaitable`) | ~10.0 ms |
| `base64` (for bytes results) | ~7.2 ms |
| `random` (for reseeding) | ~2.1 ms |
| `asyncio` (for async handlers) | ~51.3 ms |

The fix: all of them moved into the zygote (module level), so the child inherits
them through CoW. `inspect.isawaitable` was replaced with a `types`-based check.
`asyncio` is imported at load time when the handler is async.

Result: p50 11.76 → 5.72 ms on macOS. Final p50 on Linux: 1.89 ms.

**The general lesson, written into the protocol:** nothing on the warm path may
be lazily loaded. This applies to third-party agent authors too — it is the
easiest mistake to make.

---

## PoC 1 — Sandbox setup cost

Measured: `unshare` → id map → fork into the pid ns → mount plan →
`pivot_root`. `execve` excluded (that cost belongs to the program being run, and
the warm path never pays it).

| Phase | p50 | p90 | p99 |
|---|---|---|---|
| namespaces (`unshare`, 7 ns) | 2.491 ms | 3.287 | 7.933 |
| writing the id map | 0.182 | 0.512 | 1.694 |
| fork into the pid ns | 0.540 | 0.819 | 3.871 |
| mount plan (6 mounts) | 0.327 | 0.476 | 0.788 |
| `pivot_root` + detach | 0.299 | 0.687 | 3.260 |
| **total** | **3.961 ms** | 5.331 | 17.833 |

**Acceptance: FAIL** (3.961 > 3 ms).

### Cause isolated: `CLONE_NEWNET`

Per-namespace `unshare` cost (200 iterations):

| Namespace | p50 |
|---|---|
| `CLONE_NEWNET` | **2.466 ms** |
| `CLONE_NEWUSER` | 0.135 |
| `CLONE_NEWIPC` | 0.132 |
| `CLONE_NEWNS` | 0.119 |
| `CLONE_NEWUTS` | 0.115 |
| `CLONE_NEWCGROUP` | 0.113 |
| `CLONE_NEWPID` | 0.110 |
| **all six together (without NET)** | **0.148** |
| all seven together | 2.707 |

The network namespace is **94%** of the setup cost. The other six together are
0.15 ms — the document's "a namespace set costs ~1 ms" estimate holds with room
to spare without NET, and does not hold with it. This is known kernel behaviour:
creating and destroying a netns involves RCU synchronisation and per-subsystem
initialisation.

### What it means

- **The warm path is unaffected.** Sandbox setup happens once per tenant, not
  per request. The product's actual promise is already verified by PoC 3.
- **The cold `zygo run` target (< 50 ms) is not in danger**: 4 ms is 8% of the
  budget.
- Without NET the total would be ~1.6 ms, so **the target is in effect drawn
  around everything except NET.**

### Recommendation: a netns pool — **WITHDRAWN**, see PoC 9

My original recommendation here was: since `network = "none"` netns instances
are identical to one another, they could be created in advance and pooled,
taking 2.5 ms out of setup.

**That recommendation was wrong.** PoC 9 measured it: `setns(CLONE_NEWNET)`
requires the caller to hold `CAP_SYS_ADMIN` in the user namespace that *owns*
the target namespace. Because every Zygo sandbox creates its own user namespace,
a pooled netns always belongs to somebody else's userns and `setns` returns
EPERM. Detail below.

### Two launcher traps this PoC found

Both would have hit us verbatim in phase 1.3:

1. **uid/gid must be captured before `unshare`.** Inside a new, unmapped userns,
   `getuid()` returns the overflow uid (65534), and writing that into `uid_map`
   is refused. A process may only map the uid it genuinely holds outside.
2. **A second fork is needed before mounting `/proc`.**
   `unshare(CLONE_NEWPID)` does not move the caller; it only makes its
   *children* the first members of the new namespace. Only a process that is a
   member of that namespace can mount a fresh `proc`; before that you get EPERM.

---

## PoC 2 — cgroup v2 limit enforcement

| Test | Result |
|---|---|
| Fork bomb (400 fork attempts) against `pids.max = 16` | **PASS** — cut off at exactly 16; `fork()` refused after 15 |
| Unbounded allocation against `memory.max = 64M` | **PASS** — SIGKILL (137), `memory.events: oom=1 oom_kill=2` |
| Effect on the host | **PASS** — MemAvailable 4988 MB → 4988 MB, **0 MB lost** |
| Infinite loop against `cpu.max = 0.5 core` | **PASS** — 1001 ms of CPU over 2 s of wall clock |

Requirement N4 ("a sandbox cannot affect the host or its neighbours") holds on
this kernel with cgroups alone, without any namespaces.

**Kernel feature state:** `cgroup.freeze` ✓ · `memory.oom.group` ✓ ·
`cgroup.kill` ✗ (needs 5.14) · `memory.peak` ✗ (needs 5.19). So on 5.10 the
timeout kill falls back to per-pid SIGKILL and peak RSS measurement falls back
to `getrusage` — both already have fallbacks in `zygo-core`.

### Two launcher traps this PoC found

1. **Controllers must be written into `subtree_control` at every level.**
   Enabling them at the root is not enough for a grandchild cgroup; every link
   of the `zygo.slice` → `tenants` → `tenant-A` chain needs it, or the limit
   files never appear at all.
2. **The "no internal processes" rule.** A cgroup may either hold processes or
   enable controllers for its children, never both. If the root holds processes
   they have to move into an `init` child cgroup first.

---

## PoC 4 — `gc.freeze()` and copy-on-write

50,000 objects of application data + 10 stdlib modules (116,921 tracked objects
in total), the child's `Private_Dirty` after forking
(`/proc/self/smaps_rollup`), 20 samples:

| Scenario | Median copied |
|---|---|
| No `gc.freeze()`, GC in the child | **14.96 MB** |
| With `gc.freeze()`, GC in the child | **0.81 MB** |
| With `gc.freeze()`, no GC in the child (floor) | 0.80 MB |

`gc.freeze()` saves **14.15 MB per request** — a 95% reduction — and the result
lands 0.01 MB above the theoretical floor. The technique does exactly what it is
supposed to.

**Acceptance: PASS** (0.81 < 2 MB).

This is the assumption the N3 density target (≥ 800 warm tenants in 64 GB) rests
on: without `gc.freeze()` every request would dirty 15 MB and the sharing claim
would collapse.

---

## PoC 5 — Real packages under the seccomp allowlist

The `default` profile from appendix B was written as a Docker seccomp profile
([`poc/seccomp-default.json`](../poc/seccomp-default.json)) — the same
libseccomp semantics, giving a real test without writing our own BPF first.

| Exercise | Result |
|---|---|
| numpy — 256×256 matmul (BLAS), `eigvals` | PASS |
| pandas — write/read a 1000-row CSV, groupby | PASS |
| Pillow — PNG/JPEG/WEBP encode-decode, thumbnail | PASS |
| pydantic — model validation (Rust `pydantic_core`) | PASS |
| requests — session, TLS context, adapter | PASS |
| stdlib — ssl, socket, subprocess, threading | PASS |
| `fork()` | PASS — the warm path itself |
| `unshare(CLONE_NEWUSER)` | PASS — **correctly refused** (EPERM) |

**Acceptance: PASS** — all five packages work.

### The omission fixed in the profile

The first draft did not include the `setgroups`/`setuid`/`setgid` family and the
container did not start at all. Appendix B does not exclude them — the draft was
incomplete, and the profile was not loosened. Identity and scheduling calls
(`set*id`, `sched_set*`) were added.

`clone3` deliberately returns `ENOSYS`; glibc sees that and falls back to
`clone()`, where the filter that masks the `CLONE_NEW*` bits takes effect. So
forking is permitted and creating namespaces is not.

---

## PoC 6 — overlayfs inside a userns

On kernel 5.10.104:

| Test | Result |
|---|---|
| Read-only overlay (lowerdir only) | **unsupported** |
| Writable overlay (upperdir) | **unsupported** |
| Whiteout `mknod c 0 0` | **unsupported** (EPERM) |

As expected: unprivileged overlayfs arrived in 5.11. This independently confirms
two design decisions:

1. **The flatten fallback is mandatory**, not optional (risk R3).
   `Store::flatten` was already written and worked on a real `python:3.12-slim`
   image.
2. **Recording whiteouts in a sidecar is the right call.** overlayfs's char
   device 0:0 form cannot be written rootless; `Store` records them in
   `.zygo-whiteouts.json` and applies them during flatten.

**Missing:** the 5.15 / 6.1 / 6.8 matrix could not be run — there is only one
kernel in this environment. Left to the phase 1 CI matrix.

---

## PoC 7 — `zygo-core` API sketch

Not run separately as a full PoC, but PoC 3's driver
([`crates/zygo-core/examples/poc3_warm_path.rs`](../crates/zygo-core/examples/poc3_warm_path.rs))
used the crate by **embedding** it: a complete supervisor side written with
`zygo_core::protocol::{FrameReader, FrameWriter, Message}`, which then drove
100k requests. The first concrete evidence for ADR-008 ("the library is the
product, the CLI is a thin client").

The `Pool` and `Fn::call` APIs arrive with the supervisor (phase 2.2/2.6); at
that point the real PoC 7 will be this example collapsing into `Pool`.

---

## PoC 8 — libkrun

**Could not be run.** There is no `/dev/kvm` in this environment (Docker Desktop
on Apple Silicon does not offer nested virtualisation). Since the `vm` backend
is first-class in phase 2 (ADR-010), this measurement has to happen on a Linux
machine with KVM **before phase 2 starts**. Its risk: document §6 R6 (virtiofs
import cost).

---

## Items carried into phase 1

1. **netns pool** — takes 2.5 ms out of setup (PoC 1). `todo.md` §1.3.
2. **Capture uid/gid before `unshare`** — a launcher contract (PoC 1).
3. **Fork into the pid ns before mounting `/proc`** — a launcher contract (PoC 1).
4. **`subtree_control` at every level + "no internal processes"** — cgroup setup
   (PoC 2).
5. **No lazy imports on the warm path** — written into the protocol document
   (PoC 3).
6. **A CI matrix on kernels 5.15/6.1/6.8 + a runner with KVM** — the gaps this
   report could not close (PoC 6, 8).
7. **A warm path regression test** — there is only 6% of headroom at p50 (PoC 3).

## Reproducing this

```bash
# PoC 1, 2, 4, 5, 6 — in a Linux container
docker run --rm --privileged -v "$PWD/poc:/poc:ro" python:3.12-slim python3 /poc/poc1_namespace_setup.py
docker run --rm --privileged -v "$PWD/poc:/poc:ro" python:3.12-slim sh     /poc/poc2_cgroup_limits.sh
docker run --rm            -v "$PWD/poc:/poc:ro" python:3.12-slim python3 /poc/poc4_cow_pollution.py
docker run --rm --security-opt seccomp="$PWD/poc/seccomp-default.json" -v "$PWD/poc:/poc:ro" zygo-poc5 \
    python3 /poc/poc5_seccomp_packages.py
docker run --rm --privileged -v "$PWD/poc:/poc:ro" debian:12 sh /poc/poc6_overlayfs_userns.sh

# PoC 3 — directly on macOS, in a container on Linux
cargo run --release --example poc3_warm_path -- --n 100000
```

---

## Appendix: what came out of actually running the phase 1.3 launcher

The launcher was written in the same environment and verified with
`poc/verify_launcher.sh` (**19/19 passing** at the time: pid/uts/net/mount
namespace isolation, capability drop, a read-only root, `/proc` masking,
pids/memory/timeout limits, stdin/stdout and exit code passthrough). The
findings that running it produced, and that changed the design:

**1. `clone3` makes PoC 1's second fork unnecessary.** `unshare(CLONE_NEWPID)`
does not move the caller, whereas `clone3(CLONE_NEWPID)` makes the child pid 1
in the new namespace directly. So that is why the document said `clone3`; the
0.54 ms "fork into the pid ns" phase PoC 1 measured does not exist in the real
launcher.

**2. `memory.high` + `swap.max=0` = a livelock instead of an OOM.** Document
§3.5 says `memory.high = max × 0.9`. What we measured: in a sandbox limited to
128 MB, asking for 400 MB pinned `memory.current` 3 MB below the limit, drove
`high` events into the thousands, and **`oom_kill` stayed at 0 for 14 seconds**.
With no swap, anonymous memory cannot be reclaimed, so `memory.high` throttles
the process and never lets it reach `memory.max`; in the end the wall-clock
limit stops it. The result: a memory overrun looks to the user like a
**timeout**, and the tenant burns its entire time budget.

Decision: `memory.high` is only written when `swap.max > 0`. Afterwards:
**exit 137 (SIGKILL), in 0 seconds.** This condition should be added to the
document's §3.5 table.

**3. cgroup v2's "no internal processes" rule bites in three separate places.**
PoC 2 found it at one level; the launcher hit it three times:
- The tenant cgroup carries the limits *and* parents the request cgroups, so it
  cannot hold processes → the zygote has to sit in a `tenant/zygote` child
  cgroup. (The §3.6 diagram in the document already shows this; it had been
  missed in the code.)
- `subtree_control` has to be written on `zygo.slice`'s parent, which makes that
  cgroup unusable for later `zygo` invocations → `discover()` now reuses an
  enclosing `zygo.slice` if there is one rather than nesting a new one. Repeated
  invocations became idempotent.
- The supervisor has to move itself into `zygo.slice/system` — which the design
  already said, but the reason is not only OOM protection: it is a precondition
  for delegation.

**4. Mounting a tmpfs over `/dev` hides the skeleton's subdirectories.**
`/dev/shm` and `/dev/pts` have to be recreated on top of the fresh tmpfs.

**5. Flags cannot go in a tmpfs option string.** `nosuid`/`nodev`/`noexec` are
`MS_*` flags; put into tmpfs's option string the mount is refused with EINVAL.
The mount plan now keeps the two apart.

**6. `execve` does not search PATH.** `zygo run python:3.12 python3` returned
ENOENT. `execvp` allocates and so cannot be used in the child; candidates are
built from `PATH` in the parent and tried in order in the child. The image's own
`ENV` is now merged underneath the spec's as well (Docker's `-e` behaviour).

**7. Two of the verification tests were asserting the wrong thing** — and
looking green:
- "the sandbox should see ≤1 network interface" is wrong: the kernel creates
  `tunl0` and `ip6tnl0` in every new netns. The correct assertion is that the
  host's real interface (`eth0`) must not be visible.
- "`/proc/kcore` must be unreadable" is wrong: masking bind-mounts the file onto
  `/dev/null`, which makes it **readable but empty**. The correct assertion is
  that it must be a character device and return 0 bytes.
- The memory test also said "exit code is not zero", which was equally true of
  the livelock in finding 2. It now requires "exactly 137, in under 10 seconds".

**Left unverified at that point:** seccomp and Landlock were not yet wired into
the launcher (the profile was verified separately in PoC 5, the code path was
not); there was no `--tty`; real rootless operation (`newuidmap`) was still
untested — we run as root in this environment.

### Landlock — what could be verified on kernel 5.10

Landlock ABI v1 arrived in Linux 5.13; the measuring environment is 5.10. The
code was written and wired in, but **the restriction itself cannot be verified
on this kernel**. Separating what was verified from what was not matters:

| Verified (on 5.10) | How |
|---|---|
| ABI detection says "absent" | the `landlock_create_ruleset` version query returns 0 |
| The syscall really reaches 444 | errno is exactly **`ENOSYS`** — any other errno would mean the number landed on some *other* syscall and we would silently be restricting nothing |
| `zygo doctor` is honest | "landlock unavailable, degraded" — it does not claim a protection it does not have |
| Graceful degradation | the sandbox starts normally; Landlock is defence in depth, not the boundary itself (the boundary is the mount namespace) |
| Rule derivation | unit tests: per-ABI masks (v2 REFER, v3 TRUNCATE, v5 IOCTL_DEV), a read-only `/` plus writable paths derived from the mount plan, `MAKE_CHAR`/`MAKE_BLOCK` granted nowhere, and `landlock_path_beneath_attr` being **packed** (12 bytes) |
| The network rule policy | with `network="none"` TCP bind/connect are handled but no rule is added (= refused); on egress/full Landlock is not touched and the allowlist lives in nftables/pasta |

| Not verified | Why |
|---|---|
| That the restriction is actually enforced | needs a 5.13+ kernel |
| That `landlock_add_rule` / `restrict_self` are accepted | same |
| The effect of ABI v4 network rules | needs 6.7+ |

This adds a **5.13+ runner** requirement to the phase 1.5 CI matrix. Until that
runner exists, Landlock's status is honestly "written, unit tested, not verified
in the field".

### Two decisions made while writing Landlock

**1. Rules are derived from the mount plan, not written separately.** Writable
paths come from `MountPlan::writable_targets()` — when a new `rw` mount is added
to the spec the Landlock rule appears automatically, and the two can never drift
apart.

**2. `MAKE_CHAR`/`MAKE_BLOCK` are not granted even on writable paths.** A sandbox
that can create a device node in its own scratch area can reach hardware the
mount plan deliberately kept away from it.

---

## PoC 9 — Is a netns pool feasible? (phase 1.3)

After PoC 1's `CLONE_NEWNET = 2.47 ms` finding, I measured the pool idea I had
recommended before writing it. Just as well.

**The question:** can a sandbox enter a network namespace it did not create?

`setns(CLONE_NEWNET)` requires the caller to hold `CAP_SYS_ADMIN` in the user
namespace that **owns** the target namespace.

| Situation | Result |
|---|---|
| Pool in the init userns · caller in the same userns | **usable** |
| Pool in the init userns · caller opened its own userns | **EPERM** |
| Pool in its own userns · caller in the parent (the launcher's) userns | **usable** |
| Pool in its own userns · caller in a *different* userns | **EPERM** |

The gain would have been real and large:

| Operation | p50 |
|---|---|
| `unshare(CLONE_NEWNET)` | 3.353 ms |
| `setns(CLONE_NEWNET)` | **0.132 ms** |
| difference | 3.221 ms |

**But it is out of reach.** What the two working rows have in common is that the
sandbox does not create its own user namespace. Yet in Zygo the userns is the
pole holding up two things:

- **N5 — rootless operation.** An unprivileged user can only create netns,
  mntns and pidns from inside a user namespace.
- **§3.10 — separation between tenants.** Mapping each tenant to a different
  host uid is the only mechanism behind "B cannot read A's files".

Giving up both for 3.2 ms is out of the question. **The pool idea does not fit
the architecture; I withdraw the recommendation from the PoC 1 report.**

### What there is instead

**What the design already had: the warm pool.** When a tenant is served, the
netns is created once and all of that tenant's requests share it — and the
request path is a `fork()` anyway, not a new namespace. In the 1887 µs warm path
PoC 3 measured, the netns cost is **zero**. So the 2.5 ms only affects *cold*
`zygo run` and tenant creation; both sit in the < 50 ms budget and eat 8% of it.

One narrow variant is worth noting: **a per-tenant pool.** Several sandboxes for
the same tenant already share a uid; if the tenant's userns is kept alive, extra
sandboxes belonging to that tenant can enter both that userns and a pooled
netns. Since the phase 2 supervisor will keep the tenant userns alive anyway,
this may come close to free. Noted in `todo.md` §2.2.

### A caveat about the validity of this measurement

The 3.3 ms was measured under nested virtualisation on Apple Silicon (macOS →
LinuxKit VM → container). Creating a netns involves RCU synchronisation, which
can be disproportionately expensive when nested. **On bare Linux the "94%" ratio
is probably not this large.** The phase 1.5 CI matrix should re-measure this on
bare metal; if the ratio is small, PoC 1's "FAIL" should be reconsidered too.

### Two mistakes of its own that this PoC found

1. I had again called `getuid()` **after** `unshare(CLONE_NEWUSER)` — the same
   bug I found in PoC 1 and fixed in the launcher, this time in the test code.
   The result: the "pool in its own userns" scenario could never be set up and
   the table came out incomplete.
2. `stdout` was not flushed before forking, so the child copied the parent's
   buffered text and printed it as well — the output appeared three times.

### The terminal: what daemonlessness gives free, and what it hides

Writing `--tty` surfaced a situation that is inverted compared with Docker.

**The free gain.** Docker `-t` has to allocate a *new* pty, because the
`dockerd` that starts the container is not attached to your terminal. In Zygo
the launcher is a direct child of your shell and `clone3` passes descriptors
down: the sandbox sees your real terminal, and colours, prompts and job control
work with no effort at all. Measured — the sandbox's terminal device is the same
as the caller's (minor 0).

**The hidden cost.** That same inheritance hands tenant code a **writable**
handle to your terminal. Measured: before the fix, from inside the sandbox

```
ioctl(1, TIOCSTI, "X")   →  succeeded
```

that is, code in the sandbox could push characters into the user's terminal
*input* queue, and the shell reads them as if typed after zygo exited. **It
worked on both the default and the strict profile.** Kernel 6.2 closes this with
`dev.tty.legacy_tiocsti=0`, but the measuring environment is 5.10 and one cannot
rely on the host kernel having closed it.

Fixed in two layers:

1. **A seccomp argument filter** — `ioctl` itself stays permitted (the terminal,
   sockets and half of libc depend on it); what is refused is the *request
   number*: `TIOCSTI` and `TIOCLINUX` → EPERM. Ordinary ioctls (TCGETS = every
   `isatty`, TIOCGWINSZ) are unaffected; both are pinned by tests.
2. **`--tty`** — the sandbox gets a pty of its own (`setsid` + `TIOCSCTTY`), so
   the caller's terminal is not filtered, it is *never visible*. Measured: minor
   0 by default (the caller's), minor 1 with `--tty`.

### The BPF generator moved to symbolic labels

Adding the `ioctl` argument check introduced a second branching block into the
program. Computing `clone`'s jump distance by hand, I wrote one too few and
produced a filter that **refused every fork** — while all the structural tests
passed. To make that class of bug impossible, the generator was moved to a small
assembler that names branch targets and resolves distances in a second pass.
Backward jumps and distances beyond the 8-bit limit now return an error.

### The third measurement mistake I found in my own tests in this section

The check comparing terminal devices was written wrongly three times: `$(...)`,
`| grep` and `>/dev/null` all redirect fd 1 — so the measurement itself
destroyed the thing being measured and the sandbox appeared to be `/dev/null`.
In the end the device's minor number was carried in the **exit code**; no
redirection is needed at all.

The same pattern has now appeared four times in this project (`$$` in a POSIX
`sh` subshell, `tar::Builder` refusing `..`, `isatty` through a pipe, and now
this). The shared lesson: **make sure a test does not touch what it measures.**

---

## Phase 1.5 — escape suite, fuzzing, static binary

**Escape suite** (`poc/escape_suite.sh`): every vector from document §3.10 is
**actually attempted** — nothing is read off a setting. 16 vectors, 0 escapes:
overwriting `/proc/self/exe` (the CVE-2019-5736 shape) · cgroup `release_agent` ·
`mount()` · `setns` · `unshare(CLONE_NEWUSER)` · rewriting `uid_map` ·
`mknod` + visible block devices · `/dev/mem`, `/dev/kmem`, `/proc/kcore` ·
host processes · `bpf`/`perf_event_open`/`keyctl`/`userfaultfd`/`io_uring`/
`ptrace`/`process_vm_readv`/`kcmp` · writing to a read-only mount · escaping via
a symlink from a writable mount · a setuid binary · the host filesystem · the
image store.

**Fuzzing** (`crates/zygo-core/tests/fuzz_parsers.rs`): everything that handles
untrusted input — the spec parser, resolution, protocol decoding, framing, image
references, scalar types. ~25,000 generated inputs, reproducible from an
xorshift seed, no nightly required. **No panics found.**

**Static binary**: 4.5 MB, zero dynamic dependencies (N6; target < 15 MB).

## Phase 2 — the warm path's p99: a tail chased for a day was the tenant's own quota

`zygo bench warm` was giving p50 1426 µs (budget 2000 ✓) but p99 46320 µs
(budget 10000 ✗) over 3000 requests. The phase breakdown put the tail in the
"plumbing" between `GO` and `DONE`: handler p99 168 µs, plumbing p99 42836 µs.
The tail was bursty — every phase's max rose at the same moment.

### The experiment that showed the tail was not in Zygo

I took Zygo out of the equation entirely: a plain Python process with the
agent's heap profile, in a `fork` → child writes and exits → parent reads EOF
loop.

| | to-EOF p99 | max |
|---|---|---|
| on the host, agent-sized heap | 1103 µs | 9343 |
| **inside a sandbox**, same loop | **48059 µs** | 55225 |

Same code, same heap, the only difference being the sandbox. So the tail is not
in our protocol, our pipe plumbing or our agent.

### Which property of the sandbox — eliminated one at a time

| variable | p99 | conclusion |
|---|---|---|
| `--seccomp permissive` | 49638 | ❌ not seccomp (`default` 50367) |
| `--mem 4G` | 50319 | ❌ not the memory limit |
| `unshare -Urpmfn` (with the net ns) | 1347 | ❌ not the network namespace |
| `unshare -Urpmf` | 2545 | ❌ not the other namespaces |
| **`--cpu 1` → `--cpu 2`** | 48825 → **1570** | ✅ **the CPU quota** |

Read while the loop runs, `cpu.stat` says so directly:

| quota | `nr_periods` | `nr_throttled` | `throttled_usec` |
|---|---|---|---|
| `--cpu 1` | 28 | **27** | **2 257 080** |
| `--cpu 2` | 15 | 2 | 1 858 |

### Why: fork-per-request wants slightly more than one core

A warm request forks. While the parent is sending the answer the child is still
being torn down — the kernel has to unmap the CPython child's ~16 MB of CoW
pages. So for a moment the tenant has two runnable tasks. Back to back, that
lands at almost exactly 1.0 core of demand; a `cpu = 1.0` tenant meets its own
quota and CFS stops it until the next period. With a 100 ms period the expected
wait is half a period: ~50 ms. The measured p99 is 48–50 ms.

(This also explains why the "deferred `waitpid`" fix for p50 brought p50 from
1814 → 1426 µs while making the tail worse: teardown left the critical path but
became **concurrent** with the next request, so the contention grew.)

### Below saturation the budget holds comfortably

The same loop, `cpu = 1.0` fixed, the only variable being think time:

| think | demand | p50 | p99 | cpu/request |
|---|---|---|---|---|
| 0 ms | **1.01 cores** | 982 µs | 48154 | — |
| 2 ms | 0.50 | 684 | **1948** | 1.48 ms |
| 5 ms | 0.20 | 526 | 1935 | 1.16 ms |
| 10 ms | 0.11 | 519 | 1730 | 1.14 ms |

One warm request costs **~1.15 ms of CPU** → a one-core tenant's capacity is
**~500 req/s**. Below that, p99 ≈ 1.9 ms, a fifth of the budget.

### The same result, in the product itself

Corrected `zygo bench warm`, 3000 requests, the only variable being offered load:

| | saturated (unpaced) | **paced to 250 req/s** |
|---|---|---|
| CPU used | 1.00 / 1.00 quota (100%) | 0.51 / 1.00 (51%) |
| throttling | **76 / 76 periods**, 6458 ms | **0 / 119 periods**, 0 ms |
| p50 | 1408 µs | **1316 µs** |
| p90 | 1663 | 1562 |
| p99 | 46986 → `NOT MEASURED` | **1965 µs** |
| p99.9 | 50804 | 2478 |
| max | 52526 | **3264** |

Below saturation even `max` is a third of the p99 budget, and the headroom at
p50 is 34%. The phase breakdown clears up too: in the saturated run `fork` p99
is 41507 and `run` p99 43979, while at a fixed rate they are 942 and 1038. So
the "bursty tail, every phase at once" observation was throttling's signature —
when CFS stops a running task, whichever phase happens to be open gets longer.

The sandbox's own cost (at 250 req/s): `fork` 495 µs, `admit` 62, handler 33,
plumbing 613, `release` 38. The host's bare `fork`+`wait` floor is p50 243 µs —
so a fifth of the p50 is the machine itself.

### The option of shortening the period: measured, rejected

Raising the quota weakens N4 (limits are mandatory). The alternative is to
enforce the same quota at a finer grain. The measurement confirms the tail
tracks the period one for one (p99 ≈ half of it) — but it has a price:

| period | p90 | p99 | total throttled |
|---|---|---|---|
| 100 ms | 1313 µs | 48962 | 2.3 s |
| 50 ms | 1147 | 26412 | 4.4 s |
| 20 ms | 1286 | 12846 | 6.5 s |
| 10 ms | **5588** | 8386 | 9.0 s |
| 5 ms | 4095 | 5640 | 11.7 s |
| 2 ms | 3076 | 4672 | 14.5 s |

Shortening the period **creates no CPU that was not there**; it splits one long
stall into many short ones, and because the unsaturated workload genuinely wants
more than one core the total time spent throttled goes up. Since 100 ms is
already not a problem below saturation (p99 1.9 ms), shortening it would damage
the ordinary case's p90 to fix only the pathological one. **The default is
unchanged.**

### What changed in the product

The defect was in the measuring tool, not the product, so that is where the fix
went:

- `pool::CpuAccounting` — reads `cpu.stat` + `cpu.max`, takes the difference
  between two readings, judges saturation, reports demand in cores. It lives in
  the library because the supervisor's queueing and backpressure decisions will
  want it too.
- `zygo bench warm --rate` (fixes the offered load) and `--cpu`.
- The bench now prints CPU used, the quota, CPU per request, the throttling
  counters and the tenant's capacity.
- **On a saturated run the p99 budget is reported as `NOT MEASURED`** and does
  not break CI; p50 is judged either way. On a saturated run the p99 is not a
  number about this code, and reporting it as a FAIL sends the reader to exactly
  the wrong place — the one I spent a day in.

---

## Phase 2.2 — the supervisor: `PDEATHSIG` watches the thread, not the process

On the supervisor's first end-to-end attempt everything looked healthy:
`serve` ✓, `ps` showing the function `warm` ✓ — but the first `exec` returned
**broken pipe**. `/proc/<pid>/stat` gave the answer: the agent was `Z`, a
zombie. The sandbox had died the moment the `serve` command returned.

The cause is not in Zygo. `prctl(PR_SET_PDEATHSIG)` — the mechanism that stops a
crashed supervisor leaving sandboxes behind with the tenant's mounts still
attached — fires when the **thread that created the child** dies, not when the
process does. The supervisor was starting the sandbox on a connection thread;
that thread closed when the CLI command finished and the kernel sent the sandbox
a SIGKILL.

I verified the mechanism with Zygo taken out of the equation entirely — a bare
`fork` + `prctl`, 5.10 / aarch64:

| who created the child | 300 ms later |
|---|---|
| a thread, which then exited | **killed** |
| the main thread, still alive | running |

### The fix: change the creating thread, not the guarantee

Removing `PDEATHSIG` would have fixed the symptom immediately — and sacrificed
the very security property it provides. Instead, `supervisor::Launcher`:
sandboxes are created by **a single thread that lives as long as the
supervisor**, and connection threads hand the work to it over a channel and wait
for the result. Warming is serialised as a consequence; that is acceptable
because it is a few hundred milliseconds once per function and `exec` never
comes through there.

### Why only an end-to-end test could have caught this

`zygo run` is single-threaded and could never have shown this bug. Nor can a
unit test: the bug lives in the relationship between the sandbox's *lifetime*
and the *client process exiting*. That is why `poc/verify_supervisor.sh` was
written — 22 checks with the client process exiting at every step — and wired
into CI. Its second check looks for this bug directly and names the cause if it
sees a broken pipe.

The suite also genuinely attempts backpressure: 12 simultaneous clients against
a function with `--concurrency 1` → **3 served, 9 told to retry, 0 failed**.
`BUSY` is its own response type and has its own CLI exit code (75), because the
caller's correct reaction is to retry, not to give up.

---

## Phase 2.2 — the pid in `FORKED` was namespace-local, so nothing was ever moved

Building the request deadline exposed a bug that had been invisible since the
warm pool was written, and it invalidates one of PoC 3's numbers.

The agent is pid 1 in its own pid namespace, so the pid it reports in `FORKED`
is the child's number *in that namespace*. The supervisor was writing it
straight into `cgroup.procs`, and would have passed it to `kill`.

Measured on 5.10, with a handler that reports its own `os.getpid()` while a
probe reads the cgroup from outside:

| | |
|---|---|
| pid the agent reported | **2** |
| the child's actual host pid | **28** |
| contents of `req-.../cgroup.procs` | **empty** |

So the per-request cgroup — the entire reason the protocol has the `FORKED`/`GO`
handshake — had never contained a request. It was a directory being created and
removed, and the **97 µs attributed to it in PoC 3 is the cost of that and
nothing else**; the real cost of a populated request cgroup has not been
measured yet. Open question A2 should be revisited once it has been.

`admit()` ignores errors deliberately — a failed move should not fail a request,
because the tenant cgroup still bounds the child — so nothing ever complained.

**PoC 3 could not have caught it.** There the agent was a plain host subprocess
with no pid namespace of its own, so the number it reported happened to be
correct. The bug only exists once the agent is really sandboxed, which is
exactly the configuration that shipped.

### The fix, and two consequences

The pid is translated before use, via `NSpid` in `/proc/<host>/status` — whose
last field is the pid in the innermost namespace — with candidates taken from
the agent's own `children` rather than all of `/proc`, so it stays two small
reads on the request path. Re-measured: `req-.../cgroup.procs` holds the host
pid and the zygote cgroup holds only the agent.

1. **A SIGKILL to an untranslated pid would have hit an unrelated process.**
   Nothing had a deadline before, so it never fired; the deadline work is what
   made it reachable.
2. **Killing one pid is not enough anyway.** Below kernel 5.14 there is no
   `cgroup.kill`, and a handler that forks helpers leaves them running and
   holding the result pipe open, so the agent never sees end of file and the
   function wedges. Measured with a handler that forks four spinners: all four
   survived. `cgroup::kill` now falls back to freeze → signal every member →
   thaw. The freeze is the part that matters: without it a process can fork
   between reading `cgroup.procs` and sending the signal, and the new child is
   born unsignalled. Re-measured: 0 survivors, and the request is killed at its
   deadline instead of wedging the function.

### The deadline itself

The timeout is the **function's** own, not the caller's. `zygo exec` waits 60 s
by default; a function served with `--timeout 2s` is stopped at 2 s regardless,
because N4 makes the spec's limits mandatory and a client must not be able to
buy more time by asking for it. Measured: 2009 ms for a handler that never
returns, 2075 ms for one that forked four helpers.

The connection survives a kill, which is what stops every timeout costing a
rewarm: the supervisor kills the request, the agent notices EOF on the result
pipe and sends `DONE` by itself, and the next request goes down the same
connection. Verified by a second request reaching the same sandbox with the
function still `warm`.

---

## Phase 2.2 — idle tiering: what pausing is actually worth

The design document's F12 asks for idle sandboxes to be put to sleep and woken
on demand, in two tiers. The question worth measuring is whether the middle tier
earns its place: if waking a paused function costs what a cold start costs,
there is no reason to have it.

Measured on 5.10, a function served with `--idle-timeout 1s`, left alone, then
called:

| | |
|---|---|
| state after the idle timeout | `paused` |
| time to answer once woken | **7 ms** |
| a cold start, for comparison | ~300 ms |

So pausing keeps what matters. The asset a warm function represents is its
resident pages — they are the whole reason a request costs a `fork()` — and
`cgroup.freeze` gives up the CPU while keeping them. Going cold gives up both,
which is why it is a separate, much later threshold.

Two rules the policy follows, both of which would be bugs if it did not:

- **A function with a request in flight is never tiered**, whatever the clock
  says. The idle clock is read after a call finishes rather than before it
  starts, so a slow request cannot make its own function look idle.
- **A cold function is still registered.** It appears in `zygo ps` as `cold`
  with its counters intact, and a request for it is a cold start rather than a
  "not found" — which is exactly the trade `cold_after` was configured to make.
  `zygo stop` deregisters it, or it would come back on the next request having
  been explicitly stopped.

### Cgroups outlive their processes

A supervisor that dies takes its sandboxes with it (`PDEATHSIG`) but not their
cgroups. Verified directly rather than assumed: after `kill -9` on the
supervisor, `tenants/<name>` was still there. Without cleanup a restarted
supervisor would accumulate one dead tenant tree per previous lifetime and reuse
their stale limits for any name it served again.

`Hierarchy::clean_stale_tenants` removes tenant cgroups whose subtree holds no
process, checked by reading `cgroup.procs` at every level — a check that only
looked at the top would delete a tenant whose request cgroup was busy. Anything
still holding a process is left alone: `Listener::bind` has already established
that no other supervisor is listening, so this should find nothing, and if it
does, the honest reading is not to kill it.

This half could not be unit tested. `rmdir` on a real cgroup succeeds with its
control files in place and on an ordinary filesystem it does not, so a temporary
directory cannot stand in for cgroupfs. The decision (`is_empty_subtree`) is
unit tested; the removal is checked end to end in `poc/verify_supervisor.sh`
against a real kernel.

---

## Phase 2.9 — the throughput criterion, measured

`zygo bench cold` and `zygo bench load` exist to put numbers on two
requirements that had never been measured.

**N2, a cold `run` with the image cached: p50 18.4 ms** against a 50 ms budget
(p90 21.2, p99 33.7, 30 runs, `python3 -c pass` in `python:3.12-slim`). Worth a
caveat the tool prints itself: this host has no unprivileged overlayfs, so the
rootfs is flattened — the same bind mount every run, which is the cheap case. A
number measured with a real overlay would be higher.

**The phase 2 criterion is "≥ 600 requests/s at a concurrency of 4".** Measured
over 6 s runs with an empty handler:

| | requests/s | CPU used | longest wait for the connection |
|---|---|---|---|
| concurrency 1, `cpu = 1.0` | 424 | 1.00 / 1.00 (saturated) | 0 |
| concurrency 4, `cpu = 1.0` | 394 | 1.00 / 1.00 (saturated) | — |
| concurrency 1, `cpu = 4.0` | 607 | 1.46 / 4.00 | 0 |
| concurrency 2, `cpu = 4.0` | 592 | 1.49 / 4.00 | — |
| concurrency 4, `cpu = 4.0` | 599–609 | 1.45 / 4.00 | **2.2–3.7 s** |

Two findings, and they point in different directions.

**At the default quota the criterion is unreachable, and not because of Zygo.**
A warm request costs ~2.4 ms of CPU, so one core is about 420 requests/s. 600
needs at least 1.5 cores. This is the same lesson as §2.1b in a different shape,
and `bench load` reports the CPU accounting so the number is attributable rather
than mysterious.

**Concurrency contributes nothing.** With the quota lifted, throughput is flat
at ~600 requests/s whether 1 or 4 clients call, while only 1.5 of 4 cores are
used — so neither the quota nor the machine is the constraint. The agent handles
one `EXEC` to completion before reading the next, so `concurrency` bounds what
the *supervisor admits*, not what the agent can overlap.

So the criterion is met numerically at concurrency 4, by a single serialised
stream, with the "concurrency 4" part doing no work. The honest reading is that
it is **not satisfied as intended**, and what it needs is agent work — a
`select` over the wire and the in-flight result pipes — rather than anything in
the supervisor.

### A fairness problem found on the way

At concurrency 4 the requests-per-client split is only moderately uneven (684
against 1166), but one client waited **2.2–3.7 s** for the connection.
`WarmFn`'s wire is guarded by a plain mutex, which is not fair, so waiting is
unbounded and badly skewed.

This nearly went unnoticed, and the reason is worth recording: the *percentiles*
of connection-wait time were **zero at p99** while the maximum was 3.7 seconds.
A distribution that skewed defeats a percentile — almost every request acquires
the lock instantly and a handful wait for seconds. `bench load` therefore prints
the maximum and the per-client request counts, not just percentiles. My first
hypothesis from the percentiles alone was total starvation; the per-client
counts disproved it, and the maximum located the real problem.

---

## Phase 2.9 — concurrency, and what it cost

The measurement above said the agent's sequential loop was the ceiling. Fixing
it meant changing both ends: the agent's `serve` now waits on the control socket
**and** every in-flight request's result pipe together, and the supervisor runs
one thread per function that routes replies to their callers by request id.
Nothing holds the connection for longer than a single frame takes to write.

Re-measured, `--cpu 4` so the quota is not the constraint:

| | before | after |
|---|---|---|
| concurrency 1 | 607 req/s | 538 |
| concurrency 2 | 592 | **912** |
| concurrency 4 | 599–609 | **981** |
| longest wait for the connection, c4 | **3.7 s** | **494 µs** |
| requests per client, c4 | 684 vs 1166 | **1464 vs 1477** |
| CPU used, c4 | 1.45 / 4 cores | 2.35 / 4 |

The criterion is now met *because of* concurrency rather than in spite of it,
and the fairness problem is gone — the split is even to within 1% and the worst
wait is under half a millisecond.

### What it cost, isolated

Single-stream p50 went from 1316 µs to 1695 µs; headroom at p50 from 34% to 15%.
Two separate causes, separated by measuring with the per-request cgroup turned
off:

| | p50 | admit phase |
|---|---|---|
| before, with the request cgroup silently empty | 1316 µs | 62 µs |
| now | 1702 | 213 |
| now, `--no-cgroup` | 1574 | 0 |

- **~150 µs** is the pid translation making the per-request cgroup actually
  contain the request. That is a correctness fix being paid for, not a
  regression: the old 62 µs bought an empty directory.
- **~300 µs** is multiplexing — `FORKED` and `DONE` each cross a channel to
  reach the calling thread, instead of being read on it.

A fast path that reads the socket on the calling thread when it is the only
caller in flight would recover most of the 300 µs. Not done: it needs a careful
answer to who owns the socket, and 15% headroom is passing. Recorded here so the
trade is visible rather than discovered later.

### Two bugs the design made possible

Both are the kind that only exist once requests overlap, and both are now
covered by tests:

- **A child inherits the other requests' pipes.** Forked while another request
  is in flight, it holds the write end of that request's result pipe, so the
  reader never reaches end of file and the two requests deadlock. The child now
  closes every other in-flight descriptor — collected *before* the fork, because
  afterwards it cannot ask the parent what was open.
- **`SHUTDOWN` must not abandon forked work.** It stops new work being accepted
  and lets what is already running finish, or a `zygo stop` during a request
  loses its answer.

The conformance suite gained a test that fails on a sequential agent by
construction: a slow request and a fast one issued in that order, where the fast
one has to answer first. Writing it the other way round — the quick request
first, the long one forked before the quick one is released — is what catches
the inherited-pipe deadlock.

---

## Phase 3 — `zygo up`, and the first time two functions shared an image

Bringing a whole spec file up is the first thing that runs several functions
from one image, and it found two bugs immediately.

**The flattened rootfs was shared, mount points and all.** `rootfs_view` keyed
the flattened directory on the image layers alone and then created the mount
points *inside* it — so every function using that image shared one directory and
the first to run decided the shape of each mount point.

Measured on a three-function spec. Names are ordered, so `broken` ran first, and
its handler did not exist; a mount source that is not there is treated as a
directory (what `docker run -v` does), so `/zygo/handler.py` was created as a
**directory**. The other two functions then failed with **ENOTDIR** binding a
file onto it.

The flat rootfs is now keyed on the layers *and* the mount points — exactly as
the overlay skeleton already was — and the mount points are created before the
done marker, so nothing ever observes a rootfs whose mount points are half
there. Identical shapes still share one copy, which is the common case and the
reason sharing was tempting in the first place.

Reachable before `up` existed: two `zygo serve` calls on one image with
different mounts would have done it. It had simply never happened, because every
test until now used one function per image.

**A missing handler failed with ENOTDIR instead of naming the file.** Now
checked in `Pool::serve`, which reports
`fn.broken.entry: /proj/does-not-exist.py does not exist`.

Where that check went is worth recording. The obvious home is resolution — but
resolution is filesystem-free on purpose: it normalises paths without touching
them, which is what lets the entire spec layer be tested on a host with no
sandboxes at all. There is a test named for exactly that property, and it failed
when the check was put there. That is the test doing its job, and the check
belongs at the first point that genuinely needs the file.

---

## Phase 3 — blue/green: `up` as a deploy rather than a restart

`up` restarted every function on every run. That is the wrong default for a
deploy command: a project with ten functions where one handler was edited
would lose nine warm sandboxes — their resident pages, their request counters —
for nothing. The fix has two halves, and the second turned out to be the
interesting one.

**Deciding "unchanged".** The supervisor now compares what a `SERVE` asks for
against what it holds under that name: the resolved spec, the secret values,
and a SHA-256 of the handler and requirements files as they are on disk now.
The file hash is what makes it work — the spec cannot see an edit to
`handler.py`, and that edit is why anyone runs `up`. The hash is taken before
the warm-up, so an edit that lands during it is counted as a change next time
rather than guessed about. Cold functions are compared too, and an unchanged
cold function is woken rather than rebuilt.

Eleven end-to-end checks on a three-function project. A second `up` with
nothing edited: `replaced: []`, `unchanged: [other, s, v]`, request counters
intact. Edit one handler: `replaced: [v]`, the other two untouched, and the
next request sees the new code. Append `timeout = "9s"` to one function's
section: that one replaced. Rotate a secret value: the function that names it
replaced, and the new value is what it reads from `/run/secrets`.

**The queued request.** Replacement was already blue/green — the new sandbox
is warm before the old gate closes, and the old sandbox lives until the last
in-flight request drops its `Arc` — and the checks confirm it: with a
2-second request in flight, `up` over an edited handler returned, a new request
was answered by the replacement with the new code, and the in-flight one
finished on the old sandbox with the old code, exit 0.

But a request *queued* behind the old gate (`concurrency = 1`) got
"`v` is shutting down". The gate closes for two reasons — stopped or replaced —
and a caller cannot tell which. Now, on `Closed`, the request looks the name up
once more and, if a different entry holds it, tries that one; the queued
request in the check ran on the replacement and returned the new version.
One redirect, not a loop: a second closure in a row means someone is
redeploying faster than requests are admitted, and the honest answer beats a
retry storm.

---

## Phase 3 — the seccomp profile that had never been run

The default profile was "validated" in phase 0 against numpy, pandas, Pillow,
pydantic and requests. That validation used a JSON profile applied by a
different tool. The BPF filter that ships — generated in Rust from the same
syscall names — had never been run against any of them until the strict
compatibility matrix was written, and its venv build failed before a single
function existed.

**Every threaded program was dead.** `RuntimeError: can't start new thread`,
from `pip`'s progress bar on a 13.6 MB wheel. The filter is an allowlist that
answers `EPERM` to anything unlisted, and `clone3` was listed only in
`permissive`. glibc's `pthread_create` tries `clone3` first and falls back to
`clone` on exactly one error, `ENOSYS`; on `EPERM` it gives up. So the
profile that was supposed to let numpy's BLAS threads run refused the call
that creates them, and every program with a thread died the same way — the
launcher's own tests never started one.

The fix is not to allow `clone3`. Its flags live in a struct the filter cannot
read, and the reason `clone` is special-cased is to read the flags and refuse
`CLONE_NEWUSER`. It answers `ENOSYS` instead, which is what Docker's profile
does for the same reason, and glibc takes the `clone` path — where the flags
are checked.

**`strict` killed every function before its handler ran.** With threads
fixed, all five packages worked under `default`, and all five `strict`
functions reported "expected READY from the agent, got end of stream".
`strict` removed the whole socket family, data calls included. The agent
speaks to the supervisor over a socket it inherited at descriptor 3; a socket
it cannot `recvfrom` is a supervisor it cannot hear. The profile had confused
two things: transferring bytes on a descriptor a process already holds, which
is not a capability, and opening one, which is. `strict` now removes
`socket`, `socketpair`, `connect`, `bind`, `listen` and `accept4` and keeps
the transfer calls, and its unit test asserts both halves.

The matrix, after both fixes: ten cells, ten "works" — each a real operation
returning `{"ok": true}`, not an import. Under `strict`, `requests` gets
`EPERM` from `socket()` and reports its own `ConnectionError` at once, which
is the behaviour the profile is for.

The lesson is the one the escape suite was built on, applied to a different
artefact: a security control that has been read carefully and never run is a
list of intentions.

**The child filter, added afterwards.** The design's remaining tightening was
for the agent's *forked child*: it is already running the interpreter and
never needs another program, so `execve` and process creation can go. The
sandbox's own filter cannot take them, because the launcher `execve`s into the
agent. The split that makes it work is that the supervisor builds the BPF
program — the syscall numbers are the host's — and hands it over as bytes in
`ZYGO_CHILD_SECCOMP`, so an agent in any language installs it with one
`prctl` without knowing what is in it.

`clone` was the entry that mattered. Removing `fork` and `vfork` does nothing
on glibc, which makes both processes and threads through `clone`; the program
checks `CLONE_THREAD` and refuses only the calls without it. A handler can
still start a thread and can no longer fork at all. Re-running the matrix with
it installed was the point of having a matrix: all five `strict` cells still
work, and a handler's `subprocess.run` now fails with `PermissionError` where
it succeeded under `default` — the difference between the two profiles, run
rather than asserted.

---

## Phase 3 — `zygo shell`, and who should do the entering

A debug shell into a warm sandbox looks like it belongs to the supervisor: the
supervisor owns the sandbox, so surely it should run the shell. Following that
through gives you a terminal proxied over the control socket — a raw byte
stream through a frame protocol that carries nothing else like it, plus window
resizing, plus signal forwarding, plus a pty pair on the supervisor side. Three
hundred lines to move a terminal that was already in the right place.

The client can do all of it, because of the same property warm-exec's `enter`
rests on: a process in the parent user namespace gains a full capability set
when it enters a child one. `zygo shell` runs as the same user that started the
supervisor, so `setns` into the sandbox's user namespace gives it exactly what
the supervisor would have had. The supervisor's whole contribution is one
message answering "which pid", and the terminal never moves.

Order matters in the `setns` sequence, for the same reason it does in `enter`:
the user namespace first, because it is what grants the capability to enter the
rest, and the mount namespace last, so `/proc/<pid>/ns/*` is still resolved
against the host's filesystem while it still is the host's.

**What the shell keeps and what it drops** is the part worth stating, because a
debug tool that quietly has more power than the thing it is debugging is a
security hole with a friendly name. It keeps the namespaces — filesystem, pids,
network, hostname — so an egress allowlist is as real for the shell as for a
request, those being nftables rules *inside* the network namespace it just
entered. It drops every capability and sets `no_new_privs`. It deliberately
does not install the seccomp filter or the Landlock ruleset, and does not join
the tenant's cgroup: a debug shell killed by the tenant's memory limit, or one
that cannot run the tool you came to run, is not a debug shell. It prints that
on the way in rather than leaving it to the documentation.

Six checks, and the one that matters is the last: the function's request count
and state are read before and after, and must be unchanged. The command is only
worth having if looking at a function does not disturb it.

---

## Phase 3 — the derived system layer, built where `upperdir` cannot be

`system = ["jq"]` in a function's spec gives it `jq`, without a Dockerfile and
without touching the image anyone else uses. The mechanism the design names —
an overlay whose upper directory becomes the layer — is not available to a
rootless build on the kernel this project measures on (5.10 has no
unprivileged overlay), and where it is, the upper's whiteouts are character
devices and `trusted.*` xattrs that an unprivileged process cannot read back.

So the layer is made by **copy and diff**. The flattened base is copied with
modes and mtimes preserved; `apt-get install` runs inside a one-shot sandbox
whose root is that copy, left writable — the only sandbox Zygo ever starts
without the read-only remount; then both trees are walked. A file whose kind,
size, mode and nanosecond mtime match is unchanged, which is sound because the
copy preserved them and `dpkg` writes files with their package's timestamps.
Everything else is an addition; anything missing is a `.wh.` entry; a path
that changed kind is written as both. The tar is sorted and root-owned, so the
same inputs produce the same digest, and it goes into the store like a pulled
layer: verified by digest, unpacked, whiteouts recorded in the sidecar. A
derived image `python:3.12-slim+system.<key>` is indexed beside its base.

Measured on `python:3.12-slim`, `jq`: **5.3 s** to build (of which `apt-get
update` is most), **125 ms** for a second function naming the same package.
A function on the same image without `system` does not see `jq`, and the
layer holds no apt lists — the build cleans them, as a Dockerfile would.

**What the first run found.** `apt-get update` died with `seteuid 42 failed -
Invalid argument`. `apt` sandboxes its own download methods by switching to
the `_apt` user, and under a single-id user namespace uid 42 is not mapped.
The option that disables it, `APT::Sandbox::User=root`, was on the `install`
line — and the download happens in `update`. It is now on both, and the unit
test counts two occurrences, because this is the kind of fix that is one
refactor away from being undone.

**Ownership is root's throughout.** On the build host everything belongs to
the user who ran it; in the layer it belongs to root, because a layer that
recorded a host uid would be wrong on every other machine. A package that
`chown`s to a service user gets EINVAL under a single-id map — the same result
as a rootless `docker build`, and the same remedy: `newuidmap` with a
subordinate range, which the launcher uses when it is there.

---

## Phase 2.1 — the conformance suite, and what it found in the reference agent

"The protocol is language independent" had been an assertion since the design
document was written. `zygo agent test` makes it checkable: nine checks, taken
from `spec/protocol.md` §3 rather than from the Python agent's behaviour, run
against a real process with the control socket at descriptor 3.

On the first run the reference agent failed one of them and died. A frame whose
body was not valid JSON raised `json.JSONDecodeError` out of the read loop,
taking the agent and every request in flight with it — the exact failure the
spec's "no silent loss" rule exists to prevent, in the one place nobody had
looked.

The fix turns on whether the stream can be resynchronised. A body that arrived
whole and is not a message leaves the connection at a frame boundary, because
the length prefix was honoured: reportable, and the agent carries on. An
announced length past the 32 MiB cap is the opposite — nothing was consumed,
the next frame cannot be found, and closing is the only correct answer. The
spec gained this as requirement 6; it was implied by requirement 5 and worth
saying out loud.

**The second agent is what keeps the suite honest.** A conformance tool written
against one implementation encodes that implementation's habits. So
`examples/agents/sh` is a complete agent in POSIX sh and `jq` — about 130 lines
including its comments, sharing no code with Zygo, in a language with no JSON
support, no threads and no `fork` primitive beyond `&`. It passes the same nine
checks. Writing it forced one clarification: it serves one request at a time
and answers a second `EXEC` with `overloaded`, which is conforming —
concurrency is optional, losing a request is not — so the suite accepts a
refusal as an answer and reports it as one.

And a check on the checker, in the supervisor suite: an agent that sends
`READY` and then sleeps is run through the tool, and the run fails if it
*passes*.

---

## Phase 4 — the first run on a real host, and what a container was hiding

Every measurement in this report until now was taken in Docker on macOS:
aarch64, kernel 5.10.104-linuxkit, one cgroup, root inside the container. A
Raspberry Pi running Ubuntu 23.10 on kernel 6.5 is a different machine in the
ways that matter — a systemd user session, an unprivileged user, real
`subuid` ranges, and two kernel features this project has always had code for
and never executed:

| | Docker on macOS | the Pi |
|---|---|---|
| kernel | 5.10 | 6.5 |
| overlayfs in a user namespace (5.11+) | absent, layers always flattened | **supported** |
| `cgroup.kill` (5.14+) | absent, freeze-signal-thaw every time | **present** |
| Landlock | ABI 0 | not compiled into this kernel at all |
| privilege | root in a container | an ordinary user |

Three things were wrong, and all three had been wrong for the whole project.

**`zygo doctor` said the host was fine where `zygo run` failed.** The cgroup
check read `cgroup.controllers` and found `cpu memory pids`, so it printed
`cgroup v2 … ok`. The next command refused to start: an ssh login sits in a
`session-N.scope`, which lists everything its parent delegated and still
refuses `mkdir`, because the scope itself is not delegated. Risk R2 is that a
host without delegation silently applies no limits; requirement N4 says that
must never be silent. It was not silent — but the tool whose job is to
predict it was confidently wrong, which is worse than saying nothing. The
check now *attempts* what a sandbox will do: create a child cgroup, remove it.
That is the project's own first rule, applied to the file that had been
exempt from it.

The remedy was incomplete too. It named the user manager's `Delegate=`, which
is necessary and not sufficient: after applying it, the same ssh session fails
identically, because the session scope is still a leaf. Both halves are
printed now, the second being `systemd-run --user --scope -p Delegate=yes`.

**`zygo doctor` offered a backend that does not exist.** It printed
`backends available: ns, vm` on a machine with `/dev/kvm`, while
`zygo backend list` said — correctly — that `vm` is not built. Two causes: a
`/dev/kvm` the user cannot open was `degraded` rather than absent, and the
report answered "does the host have what this needs" while printing an answer
to "can I use this". The first is now absent; the second is joined in the CLI,
which is the layer allowed to know both.

Fixing that introduced a fourth bug, worth recording because of how it
presented: `Report::supports` calling `backend::for_isolation` recursed,
because the `ns` backend's own availability check calls `doctor::run()`. The
symptom was `zygo doctor` exiting 139 — a `SIGSEGV` from a blown stack — and
only inside a *working* delegated scope, because anywhere else the `&&`
short-circuited before reaching the cycle. A data type reaching back into the
layer above it deserved that.

**A client talking to a wedged supervisor waited for ever.** This is the one
that cost the most, because it disguised all the others.

The control socket had no timeout anywhere: not in `send`, not in the
greeting. So when a supervisor thread stopped — a deadlocked `place_secrets`,
later a launcher thread parked on a futex — every client that spoke to it
blocked in `unix_stream_data_wait` and never came back. From the outside that
is indistinguishable from the host having locked up, and three separate
investigations here began by ruling that out. `zygo exec --timeout` did not
help: it bounded the *supervisor's* budget for the work, not the client's wait
for the answer, though the comment beside it claimed otherwise.

One timeout would not do, because the honest budgets differ by three orders of
magnitude:

| request | budget | why |
|---|---|---|
| `serve` | 20 min | warming may run `pip` or `apt` inside a sandbox |
| `exec` | the request's own deadline, plus 10 s | the supervisor enforces the deadline and then still has to reply |
| everything else | 30 s | `ps`, `stop`, `logs` read state the supervisor already holds |

The budget is a *liveness* check rather than a deadline for the work, and the
test says so by construction: a fake supervisor greets, then answers nothing,
and the client must come back with an error naming the request inside its
budget rather than blocking.

**The sh agent's framing was wrong on any host with `gawk`.** The example
agent in POSIX sh exists to keep the "language independent protocol" claim
honest. It turns out to have been keeping it honest against one distribution.

It writes the protocol's four-byte length prefix with
`awk 'BEGIN { printf "%c%c%c%c", … }'`. In a UTF-8 locale, `gawk`'s `%c`
encodes a value above 127 as a **two-byte UTF-8 sequence**; `mawk` has no
multibyte notion and writes one byte. Debian's minimal images ship `mawk`,
Ubuntu ships `gawk` — so the conformance suite passed in the container and
failed on the Pi, on exactly the checks whose frames exceeded 127 bytes:

```
PASS  READY          (~90 bytes)
PASS  PING/PONG      (short)
PASS  FORKED         (short)
FAIL  DONE           (~130 bytes)  ← the first frame over 127
FAIL  DONE with stdout/stderr
PASS  ERROR overloaded (short)
PASS  SHUTDOWN       (short)
```

Which checks failed was the diagnosis: the boundary was a byte value, not a
message type. `export LC_ALL=C` at the top of the script fixes it, and says
why — every byte-level tool in that file has to agree that the length is a
count of bytes.

The lesson is the protocol claim's own: an implementation tested on one
machine is a claim about that machine. The Node agent is unaffected — it
writes `Buffer`s — and the Python one uses `struct`.

**And the supervisor had two more waits with no end.** With the client
timeout in place the next wedge was legible instead of mysterious: a
supervisor whose `zygo-launcher` thread sat in `pipe_read` while a `serve`
queued behind it for ever.

The launcher is deliberately one thread — warming is serial, so two `serve`s
cannot race for the same name — and the cost of that choice is that **any**
unbounded wait inside a warm-up stops the supervisor warming anything, ever
again. There were two:

* the launcher reading the sandbox's status pipe, which sees end of file when
  the child `execve`s and bytes when it fails, and nothing at all when the
  child does neither;
* the thread waiting for the agent's `READY` frame, for an agent that starts
  and then says nothing.

Both are bounded now — 60 s to reach `execve`, 120 s for an agent to announce
itself — and both report what did not happen rather than an I/O error. A pipe
has no read timeout, so the first needed `poll` with a deadline; the second is
a socket and took `set_read_timeout`.

The budgets are deliberately far above anything real: a sandbox reaches
`execve` in milliseconds, and an agent's imports run after the venv and the
derived layer are already built. They exist to turn "never" into "failed",
which is the whole of the lesson these three fixes share.

**Secrets do not work for a warm-exec function without privilege — and the
failure was a permanent hang.** This is the one that mattered.

The supervisor suite stalled on the Pi and never finished. The client sat in
`unix_stream_data_wait`; a supervisor thread sat in `futex_wait_queue`, which
is a mutex nobody was going to release. Bisecting the spec showed that
*declaring* a secret was enough — a warm-exec function that never read one hung
just the same — which put it in `place_secrets` rather than in the handler.

The deadlock is four lines of Rust and entirely self-inflicted:

```rust
let mut guard = secrets.lock()?;        // the lock
let lease = SecretsLease { secrets, .. }; // dropping this re-locks it
std::fs::create_dir_all(&dir)?;          // <- early return on failure
```

`SecretsLease::drop` calls `withdraw_secrets`, which locks the same mutex, and
`Mutex` is not reentrant. So a *failed write* dropped the lease on a thread
that still held the lock, and that thread stopped for ever — one leaked thread
per request, and a client that waits for ever. It only fires when the write
fails, which is why root in a container never saw it. The lease is created
after the guard is dropped now, and the regression test asserts the call
*finishes* rather than asserting its value; against the old code it fails with
"place_secrets deadlocked on the error path".

Then the real error appeared:
`i/o error on /proc/9447/root/run/secrets: Permission denied`.

A held sandbox's init sets `PR_SET_DUMPABLE` off, on purpose and with a
comment saying why: it is a fork of the supervisor, it still maps the
supervisor's memory — which holds every function's secrets — and it sits in
the same namespaces as tenant code under the same uid. Non-dumpable is what
refuses `ptrace` and `/proc/1/mem` to that code. The cost, unnoticed until
now, is that `/proc/<init>/root` becomes root-owned, so an unprivileged
supervisor cannot write a secret through it either.

| | secrets, rootless |
|---|---|
| runtime agent (`runtime = "python"`) | **works** — `execve` resets dumpable, so the agent's `/proc/<pid>/root` is writable |
| warm-exec (`cmd`) | **refused**, with that explanation |

Rootless is principle P2, so this is a real gap in the headline mode, and every
test this project had ran as root in a container where the check is bypassed.
The request now fails immediately and says which combination is unsupported
and what to use instead.

The obvious cheaper fix does not work, and why is the useful part. Reaching
through the *request's* own parked process instead of the held init was
tried: same `EACCES`. `/proc/<pid>/root` is traversable only while the
process is **dumpable**, and writing the id map clears that, because it
changes credentials. The launcher already lives on the other side of the same
window — it opens `/proc/<pid>/ns/*` "while the child is guaranteed still
dumpable", which is before the map is written and therefore before anything is
mounted. There is no moment in a sandbox's life at which an unprivileged
supervisor can reach its `/run` by path.

So it was a choice, not a patch: either the child hands a directory descriptor
out over `SCM_RIGHTS` before it hardens — keeping the property that nothing
inside ever holds a value, at the cost of a handshake in the clone child — or
the request's own helper writes the files just before `execve`, which is far
simpler and means a process inside the sandbox briefly holds them.

**The first was built.** The sandbox's init creates `/run/secrets` and sends
its descriptor out, in the only moment such a thing can be taken: after the
root is committed, so the directory can exist, and before `harden`, which
drops the capabilities creating it needs and may install a Landlock ruleset
that forbids it. The supervisor writes with `openat` and never touches
`/proc` again. The `SCM_RIGHTS` helpers were already in the tree for the DNS
socket, so this is a second use of code that was already carrying its own
weight rather than a new mechanism.

It also removes a difference that should never have existed: root and
unprivileged now take the same path, and the reason this bug survived so long
was precisely that they did not.

```
on the Pi, as an ordinary user, before:  error: … Permission denied
                                 after:  {"k": "sk_live_9"}
```

**A day spent on a bug that was not ours, and how it was settled.** With the
secrets path fixed, `zygo up` on a function with `network = "egress"` still
refused on the Raspberry Pi: `pasta could not configure the sandbox's
network: Couldn't open user namespace /proc/<pid>/ns/user: Permission
denied`. That is the sentence above in different clothes, and the obvious
reading was that it *is* the sentence above — `pasta` runs after the id map
is written, and `/proc/<pid>/ns` had just been shown to close at exactly that
moment. Acting on the reading, the launcher was changed to take the
namespaces early and hand `pasta` descriptors instead of paths.

It was wrong, and two measurements said so. The first: the container, running
as root, went from twenty-odd passing egress checks to `Couldn't open user
namespace /proc/self/fd/3: No such device or address` — `pasta` had already
put a socket of its own on that descriptor. The second is the one that
mattered. `/proc/<pid>/ns` on a parked sandbox turns out to be *readable*: it
is owned by the starting uid, mode `dr-x--x--x`, and an ordinary sibling
shell opens it without ceremony.

```
$ unshare -Ur --net --pid --fork sleep 30 & sleep 2
$ head -c0 /proc/$!/ns/user && echo readable
readable
$ pasta --config-net --userns /proc/$!/ns/user --netns /proc/$!/ns/net
Couldn't open user namespace /proc/66312/ns/user: Permission denied
```

A shell can open the file; `pasta` cannot. On the same host `pasta
--config-net -- /bin/true`, which involves no Zygo at all, fails too — `mount
/: Permission denied`, inside its own self-sandboxing. That machine carries
the passt Ubuntu 23.10 shipped in **June 2023**, and it simply does not work
rootless. The launcher change was reverted in full.

Two things are worth keeping from it. The suite now tells the cases apart: a
failure whose text begins `pasta could not configure` is reported as a
prerequisite that is present but does not work, and the section is skipped
rather than counted against Zygo — the same treatment a missing package
already got. And the diagnosis discipline held only because the container was
re-run: without that second environment the change would have looked like a
fix, because the environment that could not disprove it was the only one
being asked.

Rootless egress therefore remains **unproven**, not broken: no environment
available to this project can run `pasta` rootless. CI's ubuntu-24.04 runner
is current and unprivileged, and is where the answer comes from.

**The verification suites only knew how to run in a container.** Each one
hard-coded `/sys/fs/cgroup` as the place to build its harness, which is the
root of the hierarchy in a container and a directory nobody may write to on a
host. `verify_launcher.sh` reported **24 failures on a host where the launcher
worked perfectly** — every one of them an empty string where output should
have been, because the wrapper was breaking the invocation. The seven suites
now share `poc/cgroup_harness.sh`, which reads the starting cgroup from
`/proc/self/cgroup` and builds relative to it. The same file runs in both
places, and the reason it has to is not cosmetic: cgroup v2 forbids a cgroup
from holding processes *and* delegating to its children, so the shell running
the suite has to step out of the way or `zygo` cannot enable the controllers
its tenants need. That rule is why the container harness existed; reading the
root from `/proc` is all it took to make it general.

Reading the root was not all it took to make it *reliable*. For a while the
harness printed a remedy — start the suite under `systemd-run --user --scope
-p Delegate=yes` — and carried on when nobody had. A step a human has to
remember is a step a human forgets, and forgetting it does not look like a
forgotten step: the next Raspberry Pi run reported **121 passed, 13 failed**,
and every one of the thirteen was the missing scope. So the harness now does
what `zygo` itself does and steps into a scope of its own, once, guarded
against re-execing forever.

A CI runner has neither a writable hierarchy nor a user bus to ask, so it
gets the third route: `poc/ci_cgroup.sh`, sourced by each suite step, makes a
cgroup with `sudo`, hands it to the job's user, and has root move the shell
into it — a migration the kernel will not let an unprivileged process make
across a root-owned ancestor. And when all three routes fail, every suite's
summary now carries a note saying the count above it is not a verdict, which
is the part that was missing all along.

---

## Phase 4 — the `gvisor` backend, and three ways a runtime says no

The mount plan has been pure data since phase 1, with a comment saying the
reason: `ns` executes it, and `gvisor` would translate it into an OCI
`config.json` instead. That translation is a few hundred lines and it is
unit-tested on macOS, which is the payoff being collected. Then it was run,
and the running is where the report starts — three failures in a row, none of
which a unit test could have produced.

**The documented download does not exist.** gVisor's install instructions
describe `…/releases/release/latest/${ARCH}/runsc` beside a `runsc.sha512`.
The bucket has neither; it serves `gvisor.tar.zstd`. Zstd rather than the
bz2 alternative is free here, because the image store already carries a zstd
decoder for layers and carries no bzip2 at all.

**`runsc` alone is not a runtime.** The first working download installed the
100 MB binary and nothing else. It answers `runsc --version` perfectly and
then refuses to start anything: `sidecar "gvisor_sentry" not usable …
--sidecar-usage-policy is set to STRICT`. The release's `gvisor-bin/`
directory is required. The 41 MB containerd shim beside it is not, and is
left behind.

**`--rootless` and a user namespace are the same request made twice.** With
the sidecars in place, every run died in the gofer with `fork/exec
/proc/self/exe: invalid argument` — a message that names nothing. `runsc do`,
its own minimal path, worked. So the fault was in the generated bundle, and
finding it meant starting from a baseline `runsc spec` and adding this
bundle's differences one at a time:

| added to a working baseline | result |
|---|---|
| cgroup namespace | ok |
| unknown `_zygo*` keys | ok |
| an OCI seccomp section | ok |
| **user namespace + id mappings** | **the gofer's EINVAL** |

`runsc --rootless` creates a user namespace and writes its own id map; a spec
that also declares one makes the gofer clone with `CLONE_NEWUSER` a second
time. The bundle omits it now, and the uid still applies — gVisor's Sentry
implements `process.user` itself, which a test confirms by asking `id -u`
inside.

**And one difference that was not a failure.** `ns` `chdir`s to the spec's
working directory and falls back to `/` when the image lacks it, on the
grounds that refusing to start is worse. An OCI runtime instead *creates* the
directory — and fails on a read-only root. So `zygo run --isolation gvisor
alpine:3 echo hi` died where `ns` had run it. Requirement N8 is that the same
spec means the same thing on every backend, so the fallback moved into the
bundle rather than staying a property of one launcher.

What it does now, checked by 19 end-to-end checks in `make gvisor-linux`:

```
uname -r  on ns      5.10.104-linuxkit
uname -r  on gvisor  4.19.0-gvisor
```

Three probes give byte-identical answers and exit codes on both backends, and
the kernel differs — which is the pair of facts the acceptance criterion
actually asks for. A comparison where everything matched would only prove both
backends ran the same binary.

What it does not do is refused rather than weakened: warm functions (entering
one is `runsc exec`, not `setns`), the agent (its socket is an inherited
descriptor, and an OCI runtime closes everything but stdio), and networking
(gVisor's netstack is its own). Rootless `runsc` also cannot write cgroups, so
limits there are advisory — said in a warning on every start, because a limit
written down and not enforced is worse than one never promised.

---

## Phase 4 — the syscall sweep, and two bugs in the sweep itself

The escape suite attempts the vectors somebody thought of. The seccomp filter
has a failure mode that nobody thinks of: its branch offsets are *computed*,
and one wrong offset silently allows a syscall no test names. The unit tests
run a BPF interpreter over the program, which catches that — against the
program, not against the kernel.

`make fuzz-linux` calls every syscall number the architecture has, 469 of
them, under each profile, each one in a forked child with zero arguments so a
call that blocks, exits or changes process state takes nothing with it. What
it asserts needs no copy of the allowlist, deliberately: a check that compared
against the list would only be testing the list against itself. Instead it
asserts relationships — `permissive` ⊋ `default` ⊋ `strict` on a real kernel,
no syscall kills the process, `clone3` answers `ENOSYS` and not `EPERM` — and
the differences come out as exactly the 16 and 6 syscalls the constants claim.

| profile | refused with EPERM, of 469 |
|---|---|
| `permissive` | 284 |
| `default` | 300 |
| `strict` | 306 |

The first run of it found two bugs, both in the sweep rather than in the
filter, and both of the kind this project keeps finding in its own tests. The
SIGALRM handler returned instead of raising, so PEP 475 restarted the
interrupted read and the sweep hung for ever on the first syscall that waits —
it ran for eleven minutes without producing a line. And the set comparison
sorted numerically before handing the lists to `comm`, which compares as
strings: it had been answering from unsorted input, warning about it only on
stderr, where nothing was looking. The comparison now fails loudly if `comm`
complains at all.

Those are the fifteenth and sixteenth entries in the inventory below, and the
same lesson as all the others: the test is also code.

---

## Phase 4 — dating a kernel without a feed

The design asks `zygo doctor` to warn about a kernel old enough to have open
CVEs. The obvious implementation — fetch a vulnerability feed — is the wrong
one for this tool: a check that needs the network fails closed on exactly the
air-gapped hosts most likely to be running something ancient.

What needs no network is the release date of each upstream series, which is a
fact that never changes. `doctor` now dates the running series against a table
of them and warns past two years, saying whether it is a long-term series,
because a long-term series two years old and a development series two years
old are different propositions.

Two things keep it honest. A series *newer* than the table is never called
old: the table is a floor on what this build knows, and guessing would turn
the warning into a lie the moment the binary is a year old. And the wording
never says "unpatched" — a distro backports fixes into an old series without
changing its version, which is what the long-term series are for. What age
actually measures is how much kernel hardening the host is behind, which
matters because every `ns` control is a kernel feature.

On the host this was written on it reads:

```
kernel age   5.10 is 5 years old, a long-term series   degraded
             → a long-term series still gets stable updates, so this is
               about features and hardening rather than open CVEs
```

Running it rather than trusting the unit tests paid for itself immediately,
though not in the way intended: `zygo doctor | head` printed the report and
then `Aborted`. Rust's runtime sets `SIGPIPE` to `SIG_IGN`, so writing to a
pipe whose reader has gone returns `EPIPE`, `println!` panics on it, and this
binary is built with `panic = "abort"`. Every command in the CLI did it —
`zygo ps | grep -q` included, which is how several verification scripts read
its output. The fix is one line restoring the default disposition, and the
lesson is the project's own: a feature that has only ever been unit-tested has
only ever been unit-tested.

---

## Phase 3 — `zygo.lock`, and the digest that was being thrown away

A tag is a pointer. `image = "python:3.12-slim"` today and next month are two
different images, and until now nothing in a deploy noticed. `zygo up` writes
what each function resolved to beside the spec, with `Cargo.lock`'s
semantics: absent means write it, an edited spec means rewrite that entry
silently, and an image that moved under a spec nobody edited is **refused**
with both digests and a `--relock` that accepts it. The file changes nothing
by itself; refusing to let something change silently is the whole feature.

Writing it found something already wrong. The registry client resolves a
multi-platform index to this host's manifest and had been discarding the
index digest — it only ever needed the one it was about to pull. But the
index digest is the one that means the same image on every architecture, and
a lock file naming this host's manifest fails on a colleague's laptop for no
reason other than its CPU. That is a lock file that teaches people to delete
lock files. The client keeps both now, the store records both, and where an
image genuinely exists for one platform only the refusal message says why the
line cannot travel.

`apt` versions are the deliberate exception: a move is recorded with a
warning rather than refused, because Debian's archive does not keep old
versions and refusing would strand every host that was not built the same
week.

---

## Phase 3 — OTLP, without the dependency that usually comes with it

The design asks for an OpenTelemetry export beside the Prometheus endpoint.
The obvious route — `opentelemetry-otlp` — brings `tonic`, `prost` and code
generation for a binary with a 15 MB budget, which is why this sat unbuilt
for a phase. The way through is that OTLP defines *two* stable encodings, and
the JSON one over HTTP is accepted by every collector on the same port as
protobuf. So the exporter is `serde_json` building one document, and the HTTP
client the registry pull already links: no new dependency at all.

What is exported comes from the same snapshot `/metrics` renders, so the two
views cannot disagree — the refactor that made `/metrics` read a snapshot was
most of the change. The JSON mapping has traps that a hand-built document
gets wrong quietly: 64-bit integers are strings, sums must declare cumulative
temporality and carry a start time, gauges must not. The test receives the
real POST on a real socket and asserts on the bytes that arrived, because a
payload that is never sent exports nothing.

Metrics only. The request spans §3.12 also names need a trace context carried
on the request path, which is a protocol change, and are not built.

---

## Phase 3 — egress, and the capability that `execve` takes back

`network = "egress"` was the last large piece of the design that had never been
attempted. The design says: rootless `pasta` for transport, an allowlist by
host/CIDR/port, DNS forced to a resolver Zygo controls. Everything here was
probed against a real kernel before a line of it was written, because two of
those three could plausibly have been impossible rootless.

**Both halves work, unprivileged.** `pasta`, given `--netns` and `--userns`
paths, configures a namespace that already exists — copying the host's
addresses and routes into it — and forwards TCP and UDP in userspace. `nft`,
run inside that namespace, actually filters: with a default-drop output chain
allowing only `1.1.1.1:443`, a connection there returned `HTTP/2 301` and
connections to `8.8.8.8:443` and to the host's own bridge address both failed
to connect. DNS to the address `pasta` intercepts returned a 33-byte answer.
None of it needed a capability on the host.

**`pasta` is given namespace paths rather than a pid on purpose.** Given a pid
it joins the target's *mount* namespace too, so it can rewrite
`/etc/resolv.conf`. At the moment it runs, the sandbox has not pivoted yet and
its mount namespace is still a copy of the host's — and `make-rprivate` changes
propagation, not which inode a write lands on. So that write would have hit the
host's own `/etc/resolv.conf`. With explicit paths it never enters a mount
namespace at all; Zygo supplies `resolv.conf` as a read-only bind mount, which
also means the host's search domains never reach a tenant.

**The failure worth recording.** `nft` reported
`cache initialization failed: Operation not permitted`, which reads like a
kernel without nftables in user namespaces — and the identical ruleset loaded
fine under `nsenter -U -n`. The difference is `execve`. Entering a user
namespace Zygo created grants a full capability set inside it, but `execve`
recomputes capabilities and keeps them only for uid 0. Inside the sandbox's
namespace the supervisor is the *mapped* uid — 1000 by default — and uid 0
there is not mapped to anything when the host has no subordinate range, so
`setuid(0)` is not a way out either. The fix is the ambient set, the one set
that survives `execve` for an unprivileged uid: raise `CAP_NET_ADMIN` there
after `setns` and before the exec. `nsenter` hides the whole problem by setting
uid 0 for you.

The second failure was smaller and the same shape: `pasta` started as root
drops to `nobody` *before* opening `/proc/<pid>/ns/user`, and `nobody` cannot
read it. Zygo passes `--runas <uid>:<gid>` explicitly now — a no-op for the
rootless case the project targets, and the fix for the containers and CI
runners where it is not.

**A test bug the same run exposed.** The negative checks were written as
`case "$out" in *'"other":"ok"'*) bad ;; *) ok ;; esac`, so "the destination was
refused" was the fallthrough — and when `up` failed, three of them passed
against an error message. They read a named field out of the handler's answer
now and fail when it is absent. The check that no `pasta` leaks was worse: it
passed because none had ever started. It asserts one per networked sandbox
while they are up, before asserting none afterwards.

**What holds the allowlist together.** The rules are generated in Rust and
handed to `nft` on stdin — no shell, no tenant string in the ruleset — and the
order *is* the policy: loopback, established, DNS to the forced address only,
the private and link-local rejects, then what the spec allowed, then reject.
Putting the private rejects above the allow rules is what makes a *hostname*
that resolves into a private range refused; resolution alone could only ever
catch a CIDR written in the file.

**Wildcards, and the resolver that had to move inside.** A wildcard rule
cannot be turned into addresses ahead of time, and the filter works on
addresses — so enforcing `*.example.com` means Zygo answering the sandbox's
DNS itself and admitting each answer to the filter before it is sent. The
constraint that decides where that resolver lives is that `resolv.conf` cannot
name a port: it has to be on port 53 of an address the sandbox can reach,
which is *inside the sandbox's own network namespace*. A forked helper enters
the namespaces, binds `127.0.0.53:53`, and hands the socket back over
`SCM_RIGHTS`; a socket belongs to the namespace it was created in whoever
holds it, so the supervisor serves it from an ordinary thread. Measured: a
`*.cloudflare.com:443` rule resolves and serves; `example.com` gets `NXDOMAIN`;
`pasta`'s forwarder, which would be a second resolver and therefore a way
around the first, does not answer.

**The download limit that broke `pasta`.** `bandwidth` on what the sandbox
sends is a token bucket on the tap and does what the arithmetic says — 500 KB
at 100 KB/s took 4.89 s against 1.09 s unlimited. What it receives is another
matter. The rootless tool for ingress is a policer that drops packets over the
rate, and every attempt, at every burst, ended in `Connection reset by peer`:
`pasta` is a userspace TCP stack and treats that loss as a dead peer. The
alternative is to queue rather than drop, which needs an `ifb` device to
redirect ingress through — a kernel module this host lacks and one that cannot
be autoloaded from a user namespace. The limit therefore shapes what is sent,
shapes what is received where `ifb` exists, and says so where it does not.

---

## Phase 3 — the examples, run rather than read

An example is a promise: its README says what happens, and nothing checks
that it does. `make examples-go-linux` now runs them. The Go program comes up
as a warm-exec function on `alpine:3` and answers ten requests through the
CLI in 7 ms each, client start-up included; the LLM tool comes up under
`seccomp = "strict"`, evaluates an expression, refuses the injection its
README shows, and is killed at its two-second deadline on `9 ** 9 ** 9`.

That last line is where the run earned its keep. The tool's README promised
`exit 137: the deadline killed the whole process tree`, and the request *was*
killed — the supervisor reported SIGKILL — but `zygo exec` exited 1, because it
had always folded every failure into 1. A caller could not tell "the tool
ran out of time" from "the tool raised" without parsing stderr, which is
precisely what the README told them they would not have to do. `exec` now
exits with the request's own status: 137 for a deadline kill, the program's
own code for a warm-exec function that exited, 1 for a handler that raised.
The README had described the behaviour the design intended; the example test
was the first thing to hold the binary to it.

---

## Phase 2.4 and 2.8 — secrets, and the HTTP front door

**Secrets are delivered without the agent ever seeing them.** The design says
"as a file, to the child only, and the zygote does not see them" (§3.10). The
obvious implementation — put the values in `EXEC` and have the child write
them out — satisfies the first half and breaks the second: the agent parses
`EXEC`, so it has held every value in memory.

Instead the supervisor writes `/run/secrets/<name>` from *outside* the
sandbox, through `/proc/<agent-pid>/root`, between `FORKED` and `GO`, and
removes the files when the last request in flight finishes. No protocol change,
no agent code, and ownership needs no `chown`: the supervisor's host uid is
exactly the uid the sandbox's user namespace maps to the handler. Verified by
looking rather than trusting — the handler reads the value, the file is absent
once nothing is running, present for exactly the life of a request, and the
agent's own `/proc/<pid>/environ` never contains it.

Where the values come from matters as much as where they go. The supervisor is
started by whichever command needed one first and inherits *that* environment,
which is nobody's idea of where `STRIPE_KEY` lives. So the client reads them:
`zygo serve` and `zygo up` resolve the spec locally — resolution is pure, so
this is free and cannot disagree with the supervisor — take the secret names
from it, read the values from their own environment, and send them in `SERVE`.
A name with no value is a spec error naming the variable, reported before any
sandbox exists, and no value ever appears in an error message.

**The HTTP API is a client of the supervisor, not a second copy of it.**
`zygo api` turns each route into one control request over the unix socket —
the same boundary the CLI crosses — so ADR-005's single RPC boundary on the
request path is kept: HTTP → supervisor *replaces* CLI → supervisor rather than
adding to it. A pool of control connections, one per request in flight, is what
turns concurrent HTTP requests into concurrent sandbox requests.

The status codes are §4.6's, and one of them needed a fact the API did not
have. A deadline kill and an OOM kill both arrive as exit 137 with the same
message; only the supervisor knows which it enforced. So `Outcome` gained a
`timed_out` flag set by the side that killed the request, and a 408 is now a
statement rather than a guess. Measured: a handler that never returns, served
with `--timeout 2s`, answers 408 in 2053 ms over HTTP.

Two refusals, both P6: an unauthenticated listener is refused on anything but
loopback or a unix socket, because "no auth" on a reachable address is code
execution for anyone who can route to it; and bearer auth with no
`ZYGO_API_TOKEN` is refused rather than silently open. Seventeen end-to-end
checks cover the routes, the auth, and both refusals.

---

## Phase 2.7 — the venv cache, built where it will run

`requirements.txt` becomes `cache/venvs/<hash>/`, bound read-only at `/venv`
with `/venv/bin` first on `PATH`. The decision that matters is *where* it is
built: inside a one-shot sandbox from the same image, with that image's own
`pip`. Building on the host would produce a venv for the host's Python, and a
wheel compiled against the wrong interpreter or libc fails at import time
inside the sandbox with a message that points nowhere useful.

That decision also dictates the cache key — the image's manifest digest plus
the file's bytes. The same requirements against a different image are a
different venv; the same requirements from two projects are one. Measured:
**3970 ms** to build, **111 ms** for a second function to reuse it, one
directory in the cache, and a write attempt from inside the sandbox refused.

The design called for an embedded `uv`. Not done, on arithmetic: `uv` is about
30 MB and requirement N6 caps the entire binary at 15 MB. The image's `pip` is
slower and already there.

The build sandbox is given host networking — the one place Zygo does that
without `--allow-host-net`, justified narrowly: it installs the user's own
requirements file, once, before any tenant code exists, and the warm sandbox
keeps the spec's `network`.

One launcher assumption surfaced: `SandboxConfig.stdio` was documented as a pty
slave and step 4 made the process its session leader with `TIOCSCTTY`; the
builder hands it a pipe to capture `pip`, and the ioctl said ENOTTY. It now
adopts a controlling terminal only when the descriptor is one, which is what
the field should always have meant.

---

## Phase 2.3 — warm-exec: a held sandbox, and a fresh process per request

The other half of the warm model (§3.4, layer 1), for everything that is not an
interpreter worth keeping warm. The sandbox — namespaces, mounts, cgroup,
hardening — is built once and *held*; each request is a fresh process entered
into it. There is no agent in the box, and nothing from the image has to exist
for the sandbox to stay up: the init is Zygo's own code, a reaping loop that
never `execve`s.

Two hops, because two rules cannot be satisfied by one process. The supervisor
forks a helper; the helper enters the user, pid, net, ipc, uts and cgroup
namespaces and forks the request; the request, after `GO`, enters the mount
namespace itself, wires its pipes onto stdin/stdout/stderr, runs exactly the
hardening the init ran, and `execve`s `cmd`. Entering `pid` affects children,
so the request is born inside — and because the helper is still in the host's
pid namespace, the pid `fork()` returns is the **host** pid. The path that
needed translation in §2.2b needs none here.

This is rootless for the reason PoC 9's third row measured: `setns` into a user
namespace needs `CAP_SYS_ADMIN` in it, and a process in the parent namespace
with the creator's uid has every capability there. The supervisor created it;
the helper is its fork.

### What it costs

5.10/aarch64, 250 requests/s, through the library:

| | p50 | p99 | max |
|---|---|---|---|
| `sh -c cat` | **2210 µs** | 2950 | 3368 |
| `python3 -c 'json.load(sys.stdin)'` | **54 550 µs** | 59 868 | 60 902 |
| the agent path, for comparison | 1684 | 2926 | 4390 |

The first row is the design's "1–3 ms plus the program's own start-up", with
`sh` starting as part of it: enter (fork, six `setns`, fork, report) 480 µs,
admit 168 µs, run — mount namespace, hardening, `execve`, the program, end of
file — 1499 µs. Through the CLI, ten round trips averaged 7 ms each with the
client process's own start-up included.

The second row is the whole argument for the agent model in one number.
Starting an interpreter per request is 54 ms; forking a warm one is 1.7 ms.
Warm-exec is for the programs that start in a millisecond and would gain
nothing from an agent, and `bench warm -- CMD` judges it against the phase 2
criterion for that case — 3 ms — rather than the agent's 2 ms.

### The held init

It is a fork of the supervisor that never execs, sitting in the same namespaces
as tenant code under the same uid, and it still maps the supervisor's memory.
Three things make that safe, checked rather than assumed: it drops every
capability (`CapEff` is zero); it sets itself non-dumpable, which refuses
`ptrace` and `/proc/1/mem` to same-uid code (the seccomp profile denies
`ptrace` as well, and this holds even if it did not); and it closes every
descriptor from 3 up the moment it has signalled ready. It costs ~0 MB
resident.

### The bug the first run found

`sh -c cat` printed the right JSON and then waited out the full 30 s deadline.
`cat` echoes as it reads and exits on end of file — and end of file never came.
`fork` had given the helper the supervisor's entire descriptor table,
including the *parent's* write end of the request's own stdin; the helper
closed the child-side ends it knew about and kept the rest until the request
exited, which the request could not do until the helper let go.

The same inheritance is a concurrency hazard — a second request's helper would
have held the first request's stdin — so it was fixed as a rule rather than a
list: the helper `dup`s exactly the thirteen descriptors it and the request
need down to 3–15 (copies first, so no `dup2` overwrites one not yet copied)
and `close_range`s everything above. After that, 2.2 ms.

Twelve end-to-end checks cover the mode: the init's identity and capabilities,
stdin in and stdout out, fresh pids, one process at rest, the deadline, the
sandbox still serving afterwards, and secrets arriving at `/run/secrets` by the
same mechanism the agent path uses.

---

## Phase 5 — macOS, and what a third environment found

macOS has no user namespaces, no cgroups, no seccomp and no Landlock, and
will not grow them. There is no port to write. `zygo` on a Mac forwards: the
command, its arguments, its working directory and its streams go to a Linux
`zygo` in a VM, and that command's exit status comes back out. Docker answers
the same wall the same way.

The provider is **Lima**, not a Virtualization.framework helper. It needs no
signed binary, `brew install lima` is one line, and everything above the
provider — what forwards, what a path maps to, what the VM is asked — is
provider-independent and unit-tested on every platform in the matrix.

What makes forwarding usable is one rule: the VM mounts `$HOME` at the same
path it has on the host, writable, so `./handler.py` is one file seen from two
sides and nothing in the command line has to be rewritten. The rule is also
the limit, and it is enforced rather than hoped for — a command run from
outside `$HOME` is refused, and the message names both directories, because
forwarding it would run against a directory that is not the one in front of
the user.

| | measured |
|---|---|
| no VM at all → output from a Linux sandbox | **51–59 s** (target: 60 s) |
| the same again, first time ever | + ~40 s for Lima's image download |
| `zygo run python:3.12-slim python -c pass`, VM up | **117 ms** (target: 100 ms) |
| of which, the `limactl shell` hop | 46 ms |
| `zygo serve` → warm | 104 ms |
| warm `zygo exec`, round trip from the Mac | 96 ms |

The 17 ms over target is the ssh hop; vsock is where that goes if it matters.
`make verify-shim` is 14 checks against a real VM — a Linux kernel answers,
stdin crosses, an exit status of 7 arrives as 7, a handler in the current
directory is found, and `ps` lists a function running inside.

CI cannot run it: GitHub's hosted macOS runners are themselves virtual
machines with no nested virtualization, so Lima cannot boot there. The suite
skips and says why.

### Two product bugs, and why no suite could see them

Neither is about macOS. Both would stop a first-time user on Linux at the
first command they type, and both survived because this project had been
measured in exactly two places: a container it was root in, and a Raspberry
Pi running the verification suites.

**The client and the supervisor looked for different sockets.** `zygo serve`
worked its paths out from the XDG environment — data under `~/.local/share`,
socket under `$XDG_RUNTIME_DIR` — and then started the supervisor with
`--data-root <data>`, which selects the self-contained layout whose socket is
`<root>/run/supervisor.sock`. The supervisor came up and listened; the client
waited ten seconds on a path nothing was bound to and reported that it "did
not answer". Containers set `ZYGO_DATA_HOME` and have no `XDG_RUNTIME_DIR`,
so there both halves came from one place and happened to agree.

The layout now travels as both halves or not at all, and the timeout names
the socket it waited on — the sentence that would have made this a one-minute
diagnosis instead of an afternoon.

**The supervisor could not delegate out of a cgroup its own client was
sitting in.** `zygo serve` re-executes into a transient scope and spawns the
supervisor there. The supervisor steps aside into `zygo.slice/system`, but
the client is still in the scope, so writing the scope's `subtree_control`
fails with `EBUSY` and every tenant cgroup arrives with no `memory.max`. The
error blamed delegation on a host where everything *was* delegated.

This one is worth dwelling on: `poc/cgroup_harness.sh` has been moving
processes out of the starting cgroup by hand since the first Raspberry Pi
run. The harness was supplying what the product was missing, which is why 151
supervisor checks pass either way. A test fixture that compensates for a
product defect hides it exactly as well as no test at all.

`ensure` now moves Zygo's *own* processes out of the parent first, matched by
`/proc/<pid>/exe` so nobody else's are touched.

### AppArmor, and a check that read a setting

Ubuntu 24.04 ships `kernel.apparmor_restrict_unprivileged_userns=1`. An
unprivileged process may still *create* a user namespace; what it may not do
is mount inside one. So `zygo doctor` said `user namespaces  enabled  ok` —
it had read `/proc/sys/user/max_user_namespaces` — and the sandbox died four
calls later on `mount --make-rprivate /: Permission denied`, which names
neither AppArmor nor user namespaces.

The check attempts it now, in a child, in the order the launcher does: make a
user and mount namespace, have the parent write the id map, then make the
mount tree private. Each step is reported separately, because they fail for
different reasons — and the first version of this check stopped after the id
map and still said `ok`, since the map is written by the *parent* and that is
the one part the restriction permits.

```
restriction on:   user namespaces  the mount tree could not be made private
                                   inside the namespace                     FAIL
                  → AppArmor is restricting unprivileged user namespaces on
                    this host: sudo sysctl -w
                    kernel.apparmor_restrict_unprivileged_userns=0
restriction off:  user namespaces  one can be built and mounted in            ok
```

Rule 1 from the README, in the place it mattered most: the front door.

---

### An inventory of the bugs found in the tests themselves

Twenty-six times in this project a test looked green because it measured the
wrong thing — or showed the wrong thing red. All of them fall into one of
a few patterns:

| # | Where | What the test thought | The reality |
|---|---|---|---|
| 1 | PoC 2 | the fork bomb was cut off by the cgroup | in a POSIX `sh` subshell `$$` is the parent shell's pid; the load never entered the cgroup |
| 2 | Store | the path traversal defence works | `tar::Builder` refuses to write `..`; the test could never produce the malicious archive |
| 3 | Launcher | the memory limit is enforced | the hog was not OOM-killed but hit the 30 s timeout; "exit code ≠ 0" is satisfied by both |
| 4 | Launcher | `/proc/kcore` must be unreadable | masking bind-mounts the file onto `/dev/null`: readable but **empty** |
| 5 | Escape suite | the sandbox could rewrite its `uid_map` | Python's buffered `write` swallowed the error in `__del__`; the kernel had returned EPERM |
| 6 | Escape suite | a symlink escaped to the host root | the baseline had been taken from a *different* sandbox; the difference was only the `/rw` mount point |
| 7 | `bench warm` | the warm path's p99 exceeds the budget | a closed loop with no think time sat the tenant on **its own CPU quota**; what was measured was the quota, not the runtime |
| 8 | Supervisor suite | the per-request cgroup is populated | `[ -s cgroup.procs ]` tests file size, and cgroupfs reports 0 for every file however full it is; only the content is evidence |
| 9 | Supervisor suite | a timed-out function stays warm | the JSON grep assumed `"name"` is followed by `"state"`, but serde sorts object keys; it could only ever have passed by accident |
| 10 | Egress suite | a destination off the allowlist was refused | "refused" was the `case` *fallthrough*, so when `up` failed and the function did not exist, the error text matched nothing and three negative checks passed against it |
| 11 | Egress suite | no `pasta` process leaks after `down` | none had ever started; the check now asserts one per networked sandbox **while they are up**, before asserting none afterwards |
| 12 | `make test-linux` | the Linux suite passes | the recipe ended in `\|\| true`, so the container's exit code was always zero — a build that did not compile reported success, and had done for as long as the target existed |
| 13 | Examples suite | ten warm-exec requests averaged 3 ms | the function had never come up; `exec` was failing in 3 ms and the loop counted time, not answers. Every request is now checked for a real result first |
| 14 | Examples suite | the Go program's result came back | the pattern assumed `"words"` before `"squared"`; serde sorts keys. Number 9 again, on the first day a new script existed |
| 15 | Syscall sweep | the per-syscall timeout would cut a blocking call short | the `SIGALRM` handler returned instead of raising, so PEP 475 restarted the interrupted read; the sweep hung for eleven minutes without producing a line |
| 16 | Syscall sweep | the profiles were compared as sets | `sort -n` then `comm`, which compares as *strings*: the comparison was answering from input it considered unsorted, and said so only on stderr, where nothing was looking. It now fails if `comm` writes anything at all |
| 17 | gVisor suite | `backend list --json` said gvisor was available | the pattern assumed `"backend"` then `"status"`; serde sorts keys and puts `detail` between them. **Number 9 and number 14 for the third time**, on the first day of another new script — so the rule needs to be a habit and not a memory: never match more than one JSON key in one pattern |
| 18 | gVisor suite | the `ns` baseline would be there to compare against | `ns` was run *after* a dozen gvisor runs, by which point the harness's cgroup wrapper could no longer place it, and every comparison reported a vacuous failure. The baseline is captured first now, and checked before it is used |
| 19 | Every suite | the harness had put `zygo` in a usable cgroup | it hard-coded `/sys/fs/cgroup`, which is the hierarchy root in a container and unwritable on a real host. On the first run on a Raspberry Pi, `verify_launcher.sh` reported 24 failures on a launcher that worked — the wrapper was breaking every invocation, and each check dutifully reported the empty output as the launcher's fault. The harness is shared now and reads its root from `/proc/self/cgroup`; it also says which branch it took, because a run that silently prepared nothing proves nothing |
| 20 | Supervisor suite | exactly one venv directory meant the cache had deduplicated | it asserted on a cache without ensuring the cache started empty. In a container that is free — the container is new — and on a host `/tmp` survives, so it counted a previous run's half-built venv and reported a caching bug that did not exist. An isolated repro deduplicated correctly. The suites clear their data directory first now, and the check means what it says |
| 21 | Supervisor suite | a function was there to exercise | `serve` was called for its side effect with its output thrown away, so when it failed the ten checks that used the function each reported `no function named \`spin\``. Ten red lines, no cause, and the cause was one line above them. Serving goes through a `served()` helper now that says why once and returns a status the section can branch on |
| 22 | Every suite | `ZYGO_HARNESS` being `unusable` would be noticed | the harness set the flag, printed a remedy, and carried on — and nothing read it. A run started outside a delegated scope reported *121 passed, 13 failed* on a build with no bug in it. The harness now steps into a scope of its own the way `zygo` does, and when it still cannot, every summary carries a note saying the number above it is not a verdict |
| 23 | Supervisor suite | a bandwidth limit could be shown by comparing against an unlimited upload | the assertion was `limited ≥ 3.5 s AND limited > 2 × unlimited`. The first half is arithmetic — 500 KB at 100 KB/s cannot finish sooner — and the second is a control for a slow endpoint. On a busy link the control *inverted*: the unlimited upload took 3.6 s, the ratio failed, and a working limit was reported broken. A control that cannot control for anything has to say so rather than vote |
| 24 | Supervisor suite | a networked function that did not start was Zygo's fault | the section counted `pasta could not configure the sandbox's network` as a failure. `pasta` on that host cannot bring up a namespace for an ordinary user *at all* — `pasta --config-net -- /bin/true` fails there with no Zygo involved — so the suite was reporting a three-year-old package as a runtime bug, and a day went into fixing something that was not broken. A prerequisite that is present but does not work now skips the section, the way a missing one already did |
| 25 | `zygo doctor` | reading `max_user_namespaces` said whether a sandbox could be built | it says whether one can be *created*. Ubuntu 24.04 permits that and refuses the first mount inside it, so `doctor` reported `ok` and `zygo run` died on `mount --make-rprivate /: Permission denied`. Rule 1, at the front door: the check now makes the namespace, writes the map and attempts the mount |
| 26 | Supervisor suite | the harness moving processes out of the starting cgroup was test setup | it was **compensating for a missing product behaviour**. The supervisor could not delegate controllers out of a cgroup its own client was sitting in, and the harness had been emptying that cgroup by hand since the first Raspberry Pi run — so 151 checks passed on a build where `zygo serve` failed on any ordinary machine. A fixture that papers over a defect hides it as well as no test at all |

Written up as three rules and put in the README:

1. **A test must attempt the thing — not read a setting.** A test that reads a
   flag also passes on a kernel that ignores that flag.
2. **A test must not disturb what it measures.** Checking `isatty(1)` through a
   pipe measures the pipe, not the sandbox.
3. **A latency measurement must be able to say whether it hit its own limit.**
   Under a hard quota a closed-loop benchmark measures the quota; reporting that
   without knowing makes the number look like a product defect.

The first five were found after the first two rules were applied; the sixth
(uid_map) produced the rule itself, and the seventh produced the third rule.
Eight and nine are both the first rule again, in its subtlest form: `-s` and a
key-order grep are each *reading a property* rather than looking at the thing
itself, and both would have reported a working feature as broken.

Ten, eleven and twelve are a fourth pattern, and the most dangerous one,
because all three fail **open**:

4. **A negative check must first prove the thing ran.** "The connection was
   refused", "no process leaked" and "the suite exited zero" are all satisfied
   by nothing having happened at all. Every one of them now establishes the
   positive case first — the handler answered, a `pasta` is running, the
   command's status is the command's — and only then asserts the negative.
