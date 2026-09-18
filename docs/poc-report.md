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

### An inventory of the bugs found in the tests themselves

Seven times in this project a test looked green because it measured the wrong
thing — or showed the wrong thing red. All of them fall into one of two
patterns:

| # | Where | What the test thought | The reality |
|---|---|---|---|
| 1 | PoC 2 | the fork bomb was cut off by the cgroup | in a POSIX `sh` subshell `$$` is the parent shell's pid; the load never entered the cgroup |
| 2 | Store | the path traversal defence works | `tar::Builder` refuses to write `..`; the test could never produce the malicious archive |
| 3 | Launcher | the memory limit is enforced | the hog was not OOM-killed but hit the 30 s timeout; "exit code ≠ 0" is satisfied by both |
| 4 | Launcher | `/proc/kcore` must be unreadable | masking bind-mounts the file onto `/dev/null`: readable but **empty** |
| 5 | Escape suite | the sandbox could rewrite its `uid_map` | Python's buffered `write` swallowed the error in `__del__`; the kernel had returned EPERM |
| 6 | Escape suite | a symlink escaped to the host root | the baseline had been taken from a *different* sandbox; the difference was only the `/rw` mount point |
| 7 | `bench warm` | the warm path's p99 exceeds the budget | a closed loop with no think time sat the tenant on **its own CPU quota**; what was measured was the quota, not the runtime |

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
