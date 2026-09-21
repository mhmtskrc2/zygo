# Zygo — Roadmap

Source: [ahmed.md](ahmed.md) (Design Document v0.2). This file is the
**executable** form of that document's roadmap: order, dependencies and status
live here.

**Status key:** `[ ]` to do · `[~]` in progress · `[x]` done · `[-]` out of scope / dropped

---

## What works today

| Command | Status |
|---|---|
| `zygo doctor` | Works (full probe set on Linux, one honest line elsewhere) |
| `zygo pull <image>` | Works — multi-arch index, Hub token auth, digest verification, layer extraction, cache |
| `zygo images` / `zygo image prune` | Works |
| `zygo run --dry-run` | Works — resolved spec + rootfs view + mount plan + cgroup values |
| `zygo bench warm` | **Works** — against the real `Pool`; at 250 req/s p50 1316 µs, p99 1965 µs, both inside budget (see 2.1b) |
| `zygo bench cold` / `load` | **Works** — cold start p50 18.4 ms against N2's 50 ms; throughput ~600 req/s, see 2.9a |
| `zygo serve` / `exec` / `ps` / `stop` | **Works** — against the real supervisor; auto-start, registry, queue, backpressure, request deadlines, rewarm after a crash, idle pause/wake |
| `zygo supervisor run` / `status` | Works — hidden; `serve` starts one when it needs one |
| `zygo run` (for real) | **Works** — namespaces, cgroup, pivot_root, capability drop, seccomp, Landlock (5.13+), `--tty`, PATH resolution |
| `zygo spec validate` / `explain` | Works |
| `zygo backend list` | Works |
| Python reference agent | Works — 27 unit tests, and 9/9 on `zygo agent test`; holds several forks at once (fork path + spawn fallback) |
| `zygo up` / `down` | **Works** — brings up every `[fn.*]` in the spec, agent or warm-exec; `down` stops only what that spec declares |
| warm-exec (`cmd`, no runtime) | **Works** — held sandbox, fresh process per request, p50 2.2 ms for `sh -c cat` |
| secrets | **Works** — `/run/secrets/<name>`, written from outside the sandbox, present only while a request runs |
| venv cache (`requirements`) | **Works** — built inside the image with its own pip, keyed on image + file, shared read-only |
| `zygo api` | **Works** — bearer auth, loopback by default, every route in §4.6; a client of the supervisor |
| `system = [...]` (apt layer) | **Works** — installed once in a writable copy of the image, diffed into an OCI layer, shared by key; `nix` not yet |
| `network = "egress"` / `"full"` | **Works** — rootless `pasta` + an nftables allowlist inside the sandbox's own namespace, forced DNS, private ranges refused; wildcards not yet |
| `zygo agent test` | **Works** — nine protocol checks; `examples/agents/sh` is a complete agent in POSIX sh that passes them |
| `zygo shell` | **Works** — a debug fork into the function's namespaces; the warm agent is untouched |
| `zygo completion` | **Works** — bash, zsh, fish, elvish, powershell, generated from the parser |
| `zygo logs` | **Works** — the zygote's output and one entry per request, per name, across replacements; `-f`, `-n`, `--failed`, `--json` |
| Node agent / Go template | **Works** — `examples/agents/node` passes the nine checks; `examples/warm-exec/go` runs for real on alpine |
| `seccomp = "strict"` | **Works** — all five reference packages, after `clone3` → `ENOSYS` and the socket data calls were kept (3.0g) |
| `zygo login` | **Works** — verified against the registry before it is stored, `auth.json` at 0600, Docker's file read and never written |
| `zygo stats` | **Works** — counters since the warm-up beside latencies over the log window, with the two labelled apart; no `p99` under a hundred samples |
| `zygo top` | **Works** — `ps` on a timer plus the rate columns one sample cannot have; the first frame says `—` rather than inventing a zero |
| `zygo mcp` | **Works** — the Model Context Protocol over stdio; `run_code`, `list_functions`, `call_function`, `function_logs`; the tools expose a program and nothing that widens the sandbox; 26 checks against a real kernel |
| `zygo run --quiet` | **Works** — separates Zygo's own progress output from the sandbox's, which share a descriptor; what `oneshot` and the MCP server need |
| `zygo run --requirements` | **Works** — the same venv cache `serve` uses, so a one-shot job and a warm function with identical requirements share one build. Documented in `examples/ci-job` before it existed |
| `zygo run --outcome` | **Works** — writes why the sandbox ended as JSON: `timed_out` from the launcher, `oom_killed` from the kernel's own counter. Both kills are exit 137, so the status cannot carry it |
| Python SDK (`sdk/python`) | **Works** — sync and async, no dependencies, unix socket or TCP; 20 checks against a stand-in API |
| Node SDK (`sdk/node`) | **Works** — no dependencies and no build step, types beside the JavaScript; 16 checks |

Test status: **591 Rust tests on Linux** (498 on macOS, run; the Linux figure
adds the unchanged 93 Linux-only tests and wants CI to confirm it) + 37 Python +
**296 Linux integration / escape / supervisor / backend / example / registry
checks** + **15 macOS shim checks**
(36 launcher, 16 escape vectors, 13 syscall-sweep, 19 gvisor, 157 supervisor,
12 examples, 15 registry credentials, 10 seccomp-matrix cells, 9 Python and
9 Node conformance),
`clippy -D warnings`.

Run in **two environments**: a privileged container, and a Raspberry Pi
(kernel 6.5, an ordinary user, a systemd session, real `subuid` ranges). The
second one exercises overlayfs in a user namespace and `cgroup.kill`, which
5.10 has neither of — and found five bugs in a day. Landlock is exercised by
neither: 5.10 has ABI 0 and the Pi's kernel does not compile it in. CI's
`ubuntu-24.04` is the first place it will run, and the first x86_64 of any
kind.
and `rustfmt --check` clean. `make check-linux` type-checks the Linux-only
code from macOS.

**Phase 0 verification complete — decision: continue.** All three acceptance
criteria passed; for the detail and the limits of the measuring environment see
[docs/poc-report.md](docs/poc-report.md).

| PoC | Result |
|---|---|
| 3 — warm request overhead (**the gate**) | PASS — p50 1887 µs, p99 3664 µs over 100k requests |
| 2 — cgroup limits | PASS 4/4 — the host lost 0 MB |
| 4 — `gc.freeze()` CoW | PASS — 0.81 MB per request (down from 14.96 MB) |
| 5 — seccomp + 5 packages | PASS 8/8 |
| 1 — sandbox setup | FAIL — 4.08 ms on bare metal. The `CLONE_NEWNET` share is **18%**, not the 94% measured under nested virtualisation. Does not touch the warm path |
| 6 — userns overlayfs | Absent on 5.10; flatten + sidecar whiteout verified instead |
| 8 — libkrun | No KVM, could not be run → required before phase 2 completes |

---

## 0. Development environment decisions

This project is Linux-first (namespaces, cgroup v2, seccomp, Landlock); the
development machine is macOS/arm64. The strategy in use:

- Everything compiles on macOS. All Linux-specific code sits behind
  `#[cfg(target_os = "linux")]`; on macOS the backend returns
  `UnsupportedPlatform` (a shim comes in phase 5).
- Linux code is type-checked with
  `cargo check --target aarch64-unknown-linux-musl` (no linking needed).
- Real sandbox behaviour is tested inside a `linux/arm64` container
  (`make test-linux` → Docker Desktop's VM, privileged + cgroup v2 delegation).
- Platform-independent layers (spec, protocol, image store, registry client) are
  fully tested on macOS — which is where most of the code lives.

---

## Phase 0 — Verification

Goal: prove that the fork-based warm path stays under 2 ms and that the
environmental constraints (rootless cgroups, overlayfs in a userns) hold.
Throwaway PoC code.

### 0.1 Scaffolding
- [x] Cargo workspace: `zygo-core` (library) + `zygo` (CLI)
- [x] Error type hierarchy (`ZygoError`), exit code mapping
- [x] XDG directory layout (`$XDG_DATA_HOME/zygo`, `$XDG_RUNTIME_DIR/zygo`)
- [x] `tracing` setup, `--json` log output
- [~] Linux syscall wrappers: the namespace set and uid/gid map plan are ready
      (`backend/ns.rs`); `clone3`/`pivot_root`/`mount` calls land in 1.3

### 0.2 The PoCs (with a measurement report)
- [x] PoC 1 — **FAIL**: setup p50 3.96 ms (target < 3 ms). Cause isolated:
      `CLONE_NEWNET` alone is 2.47 ms, the other six namespaces total 0.15 ms.
      Does not touch the warm path; 8% of the cold `run` budget. The netns pool
      I proposed as the fix was disproved by PoC 9 — the warm pool already pays
      the cost once per tenant
- [x] PoC 2 — **PASS** 4/4: the fork bomb was cut off exactly at `pids.max`, the
      memory hog was SIGKILLed inside its own cgroup, the host lost 0 MB, CPU
      was 1001 ms over 2 s
- [x] PoC 3 — **PASS**: p50 **1887 µs**, p99 **3664 µs** over 100k requests,
      measured against the real reference agent. Breakdown: fork 610 µs,
      cgroup 97 µs, run 1144 µs. **Only 6% of headroom at p50**
- [x] PoC 4 — **PASS**: `gc.freeze()` cuts per-request copying from
      14.96 MB → **0.81 MB** (95%; the theoretical floor is 0.80 MB)
- [x] PoC 5 — **PASS** 8/8: all five packages work, `fork()` is permitted,
      `unshare(CLONE_NEWUSER)` is refused. Profile: `poc/seccomp-default.json`
- [~] PoC 6 — measured on 5.10: overlayfs in a userns is **absent**, whiteout
      `mknod` is **absent**. That confirms the flatten fallback and the sidecar
      whiteout decisions. The 5.15/6.1/6.8 matrix could not be run → phase 1 CI
- [~] PoC 7 — the PoC 3 driver embedded the crate and wrote a complete
      supervisor side (`examples/poc3_warm_path.rs`); the first evidence for
      ADR-008. `Pool`/`Fn::call` ship with the supervisor (2.2/2.6)
- [-] PoC 8 — **could not be run**: no `/dev/kvm` in this environment. The `vm`
      backend is first-class in phase 2, so this has to happen on a KVM machine
      **before phase 2 completes**
- [x] PoC 9 (added during phase 1.3) — is a netns pool feasible? **No**:
      `setns(CLONE_NEWNET)` requires `CAP_SYS_ADMIN` in the user namespace that
      owns the target namespace, which is incompatible with a per-sandbox userns
- [x] Measurement report → [docs/poc-report.md](docs/poc-report.md);
      **decision: continue**

### 0.3 The name
- [ ] Trademark / GitHub / crates.io / PyPI / npm collision check for `zygo` and
      the alternatives (`ember`, `hearth`, `kindle`, `cell`, `hull`); reserve the
      org

**Acceptance: PASSED.** PoC 3 p50 1887 µs & p99 3664 µs ✓ · PoC 2 host lost
0 MB ✓ · PoC 5 all five packages work ✓

Because the measuring environment is kernel 5.10 without KVM, Landlock,
`cgroup.kill`, `memory.peak`, userns-overlayfs and the `vm` backend **could not
be tested at all**; those gaps are tied to the §1.5 CI matrix.

---

## Phase 1 — Core runtime: `zygo run`

Goal: a daemonless, rootless, one-shot sandbox. Usable on its own as "the light
equivalent of `docker run`".

### 1.1 Spec (`sandbox.toml`)
- [x] Type system: `Bytes`, `Duration`, `CpuQuota`, `Mount`, `Network`,
      `Isolation`, `Runtime`, `SeccompProfile`
- [x] `[defaults]` + `[fn.*]` + `[api]` parsing, error on an unknown field
- [x] Layer merging: **flag > fn > defaults > built-in default**
- [x] Validation: limit floors and ceilings, `image`/`entry`/`cmd` consistency,
      mount syntax, `allow` entries, `--allow-host-net` for `network = "host"`
- [x] Human-readable error messages carrying line and column
- [x] `zygo spec validate`, `zygo spec explain <fn>` (the resolved effective spec)

### 1.2 Image store
- [x] Image reference parsing (`python:3.12`, `ghcr.io/x/y@sha256:...`)
- [x] Content-addressed store layout + blob writing (sha256-verified)
- [x] OCI Distribution client: `/v2/`, manifest, index (multi-arch), blob
- [x] Auth: `~/.docker/config.json`, WWW-Authenticate → token flow
- [x] Layer extraction: tar + gzip/zstd, hard link and device node handling,
      path traversal + symlink escape defences (tested)
- [~] Whiteouts: overlayfs's char device 0:0 form cannot be written rootless, so
      whiteouts are recorded in a sidecar and applied during flatten. Images
      with whiteouts therefore fall back to flatten even on the overlay path —
      to be solved with fuse-overlayfs or a direct char device (as root)
- [x] Concurrent pull lock (file lock)
- [x] Flatten fallback (when overlayfs is unavailable) + a `cache/flat/<hash>` cache
- [x] `zygo pull`, `zygo images`
- [x] `zygo image prune` (reference counting + GC, `--dry-run`)
- [x] **`zygo login <registry>`.** Checked against the registry before it is
      stored — a `login` that writes what it was told is a setting, and the
      typo surfaces hours later on a `pull` looking like a wrong image name.
      Written to Zygo's own `auth.json` at mode 0600, in Docker's shape;
      `~/.docker/config.json` is read and never edited, which is what P4
      actually promises. There is deliberately no `--password` flag: an
      argument is visible in `ps` to every process on the machine. 15 checks
      in `poc/verify_login.sh` against a real `registry:2` with htpasswd
      authentication, including that the stored credential is the one a
      `pull` then uses.

### 1.3 Launcher — the `ns` backend
- [x] Mount plan generation (overlayfs lowerdir, tmpfs scratch, binds, `/proc`
      masking, a minimal `/dev`, `/sys` read-only) — platform independent, tested
- [x] cgroup v2 path resolution + the two-level hierarchy
      (`zygo.slice/{system,tenants}`) + limit writing
- [x] Namespace setup with `clone3`; reading `subuid`/`subgid`, calling
      `newuidmap`. **`clone3` makes the child pid 1 in the new pid namespace
      directly** — PoC 1's second `unshare` fork is unnecessary
- [-] **netns pool** — measured to be **impossible** by PoC 9; the proposal was
      withdrawn. `setns(CLONE_NEWNET)` requires the caller to hold
      `CAP_SYS_ADMIN` in the user namespace that owns the target namespace, and
      since every sandbox creates its own userns the pooled netns always belongs
      to someone else → EPERM. The gain would have been real (3.35 ms →
      0.13 ms) but the price is rootless operation (N5) and uid separation
      between tenants (§3.10). The design already had the answer: the warm pool
      pays for the netns once per tenant, and nothing on the request path
- [x] **Capture uid/gid before `unshare`** (PoC 1)
- [-] **Fork into the pid namespace before mounting `/proc`** — unnecessary with
      `clone3`
- [x] **`subtree_control` at every level of the chain** + move ourselves into
      `system` (PoC 2); if the tenant's limit files did not appear, **the sandbox
      does not start** (N4)
- [x] Applying the mount plan; `pivot_root(".", ".")` + `MNT_DETACH` on the old root
- [x] Capability drop (bounding + ambient + capset), `no_new_privs`, rlimits
- [~] Landlock ruleset (staged ABI detection, v1–v5). Written, with unit tests
      (ABI masks, rule derivation, packed struct layout). **The measuring
      environment is kernel 5.10 and Landlock needs 5.13+** — on that kernel only
      the ABI detection and the graceful degradation were verified; the
      restriction itself was not
- [x] seccomp allowlist profile (appendix B) + `--seccomp=permissive|default|strict`.
      The BPF program is generated in the parent and installed in the child;
      syscall numbers were generated by compiling each architecture's own
      `<sys/syscall.h>` (`crates/zygo-core/src/backend/ns/syscalls.rs`). The
      tests contain a **BPF interpreter**: the policy is verified by running it,
      not by inspecting its structure
- [x] `PDEATHSIG` (+ race check), signal forwarding, exit code passthrough
- [x] stdin/stdout/stderr passthrough (descriptors are inherited through clone)
- [x] **`--tty` / a terminal of its own.** Because Zygo is daemonless the
      sandbox inherits your terminal (measured: device minor 0 = the caller's),
      which gives colours, prompts and job control for free. `--tty` gives the
      sandbox a separate pty (minor 1) and makes it the leader of its own session
      with `setsid` + `TIOCSCTTY`; the caller's terminal is never visible.
      Forwarding runs on its own thread, and raw mode is restored in `Drop`
      (including on panic)
- [x] **TIOCSTI injection closed.** Measured: before the fix, code inside the
      sandbox could push characters into the user's shell with
      `ioctl(1, TIOCSTI)` — on **both** the default and strict profiles. seccomp
      now filters `ioctl` by argument (`TIOCSTI`, `TIOCLINUX` → EPERM) while
      ordinary ioctls (TCGETS, TIOCGWINSZ) keep working. The BPF generator was
      moved to **symbolic labels** in the process — hand-computed offsets had
      produced the `clone` bug, and this closes that whole class
- [x] **File bind mounts fixed.** Mount points were created unconditionally as
      directories, so `./config.json:/app/config.json` returned ENOTDIR. The
      target's kind is now derived from the **source**
      (`MountPointKind::{Directory, File}`); a source that does not exist counts
      as a directory (`docker run -v` behaviour). The kind is part of the
      skeleton cache key
- [x] Timeouts + `cgroup.kill` (fallback: SIGKILL to the pid namespace's init)
- [x] **PATH resolution**: `zygo run python:3.12 python3` works; candidates are
      prepared in the parent and tried in order in the child (`execvp` allocates)
- [x] **Image env merging**: the image's `ENV` underneath, the spec's on top
- [x] Integration verification: `poc/verify_launcher.sh`, **36/36** passing
      (`make verify-linux`)

### 1.4 CLI
- [x] Command surface (clap): `run serve exec ps logs stop top stats pull images
      image login up down backend agent doctor shell api bench spec`
- [x] `zygo doctor`: kernel, userns, cgroup delegation, overlayfs, Landlock ABI,
      seccomp, KVM + the commands that fix each one
- [x] `zygo run` flags and their merge with the spec
- [~] Error messages: `Error::Primitive`/`BackendUnavailable` each carry a remedy
      line; documentation links arrive with the docs site (phase 3)

### 1.5 Test / CI
- [x] Unit tests for the spec, protocol, image references and mount plan
- [x] Integration suite: **52 scenarios** — `poc/verify_launcher.sh` (36:
      namespace isolation, capabilities, a read-only root, masking, seccomp,
      the terminal, mount rules, pids/memory/timeout limits) +
      `poc/escape_suite.sh` (16: every known escape vector from §3.10 is
      **actually attempted**, not read off a setting). `make verify-linux`,
      `make escape-linux`
- [x] CI matrix written (`.github/workflows/ci.yml`): ubuntu-22.04 (5.15) /
      ubuntu-24.04 (6.8) × x86_64/aarch64 × root/rootless; every runner prints
      its own kernel/LSM/cgroup state. It also checks that the syscall tables
      match a fresh generation
- [x] **CI runs, and is green on all twelve jobs.** Four `unit` (x86_64,
      x86_64 on 5.15, aarch64, macOS), four `launcher` (24.04 rootless and
      root, 22.04, 24.04 arm), two `static`, the syscall tables and the
      pending-hardware summary. **Landlock reports ABI v7 there and is
      enforced for the first time in this project's life**; so are
      `cgroup.kill`, `memory.peak`, unprivileged overlayfs and a real rootless
      `newuidmap`. KVM is still missing and the `pending-hardware` job says so.

      The first run failed every Linux job, with six bugs, five of them
      invisible on the two aarch64 machines this had been measured on: `fork`
      and `vfork` missing from the allowlist (musl uses `SYS_fork` where the
      architecture has one), `chmod`/`chown`/`lchown` allowed only under
      `permissive` while their `*at` forms were in the base list, a Landlock
      root rule that ignored a build's writable root, a syscall newer than
      the table answering `EPERM` where a libc fallback needs `ENOSYS`, and
      two checks that were right on a machine running nothing else. Plus the
      one that matters most: an escape suite reporting **16 escapes** because
      no sandbox had started. See docs/poc-report.md.
- [x] **Re-measured `CLONE_NEWNET` on bare metal**, and the suspicion was
      right. `poc1_namespace_setup.py` takes a `with_netns` flag now and runs
      the sequence twice, so the netns cost is a subtraction on one machine
      rather than a number carried over from another. On a Raspberry Pi 4
      (kernel 6.5, aarch64, 150 iterations): total p50 **4.08 ms**, without
      the netns **3.36 ms** — so the network namespace is **0.72 ms, 18%**,
      against the 94% measured under nested virtualisation.

      Two consequences. PoC 1's FAIL stands at 4.08 ms against 3 ms, but there
      is no dominant term left to remove — 1.4 ms namespaces, 1.1 ms pid-ns
      fork, 0.8 ms mounts — so it is four things to speed up rather than one
      to avoid. And the netns **pool** (PoC 9), which that 94% was the whole
      argument for, would buy back a fifth of a cost that is already off the
      warm path. Not worth handing live namespaces between tenants for.
- [x] Fuzzing: spec parser, resolution, protocol decoding, framing, image
      references, scalar types, **registry manifests and indexes, Docker's
      `config.json` and `zygo.lock`** — 13 tests, ~65k generated inputs,
      reproducible from a seed (`crates/zygo-core/tests/fuzz_parsers.rs`). No
      panics found.

      The three added last are the surfaces that arrived after the first pass
      and are the most exposed of the lot: an OCI index or manifest is the
      most *remote* input this program has, answered by whatever registry it
      was pointed at; `config.json` and `zygo.lock` are files people edit and
      merge by hand. Each is swept twice — with noise, and with a plausible
      document damaged in a few bytes, because pure noise is refused at the
      first byte and never reaches the fields
- [ ] A coverage-guided `cargo-fuzz` target (needs nightly)
- [x] Static musl binary: **4.5 MB**, zero dynamic dependencies (N6).
      `make dist-linux` checks both the staticness and the budget; it is in CI too

**Acceptance:** `zygo run python:3.12 python -c pass` under 50 ms with a cached
image · the suite green on three kernels · every limit effective when rootless.

---

## Phase 2 — The warm pool, the `vm` backend, the library API

Goal: the product's actual promise — a 1–2 ms warm path.

### 2.1 Protocol (language independent)
- [x] Message types: `READY EXEC FORKED GO RESULT DONE PING PONG SHUTDOWN ERROR`
- [x] Length-prefixed JSON framing (codec + tests)
- [x] `spec/protocol.md` v1: message schema, framing, required agent behaviour
- [x] **`spec/fixtures/protocol-v1.json`**, read by both suites: one fixture
      per message type (every `Message` variant has one, and a Rust test fails
      if one is added without a fixture), marked `canonical` where the message
      must re-serialise to exactly those bytes, plus two whole frames with
      their hex. Rust decodes, re-encodes and compares; Python drives the
      reference agent with every supervisor→agent fixture and checks the reply
      the spec prescribes, and sends/receives the frames byte for byte. Two
      implementations tested against each other drift together; against one
      file of bytes they cannot
- [x] **`zygo agent test <binary> -- [args…]` conformance tool.** Nine checks,
      the ones `spec/protocol.md` §3 lists, run against a real process: the
      agent is started with the control socket at descriptor 3 — where a
      sandboxed agent finds it too — and put through `READY`, `PING`/`PONG`,
      the `FORKED`/`GO` handshake including the *silence* before `GO`, result
      and stream separation, two requests outstanding, a malformed frame, and
      `SHUTDOWN`. It runs the agent on the host rather than in a sandbox: what
      is under test is the conversation, and a sandbox would add failure modes
      that are Zygo's rather than the agent author's. `make conformance`
- [x] **`examples/agents/sh`** — a complete agent in POSIX sh + `jq`, ~130 lines
      with its comments, which passes the same nine checks as the Python one
      and shares no code with Zygo. `sh` rather than `bash`: nothing in it needs
      bash, and the point is the smallest possible implementation.
      `examples/agents/README.md` is the third-party agent guide.
      **It found a bug in the reference agent on the first run** — see 3.0e

### 2.1b Resolved — the warm path's p99 was the tenant's own CPU quota

**The finding: not a product defect, a defect in the measuring tool.**
`zygo bench warm` ran a closed loop with no think time. A warm request forks, so
for a moment the tenant has two runnable tasks (the agent and the child being
torn down), which means back-to-back requests ask for slightly **more** than one
core. A default `cpu = 1.0` tenant therefore meets its own quota and CFS stops
it until the next period. With a 100 ms period that is a tail of tens of
milliseconds — and it is **the quota working**, not Zygo being slow.

**Corrected `zygo bench warm`, 3000 requests, 5.10 / aarch64** — the only
variable is the offered load:

| | saturated (unpaced) | **paced to 250 req/s** |
|---|---|---|
| CPU used | 1.00 / 1.00 quota (100%) | 0.51 / 1.00 (51%) |
| throttling | **76 / 76 periods**, 6458 ms stopped | **0 / 119 periods** |
| p50 | 1408 µs ✓ | **1316 µs** ✓ (34% headroom) |
| p90 | 1663 | 1562 |
| p99 | 46986 → `NOT MEASURED` | **1965 µs** ✓ |
| p99.9 | 50804 | 2478 |
| max | 52526 | **3264** |
| exit code | 0 | 0 |

Below saturation even `max` is a third of the p99 budget (10 ms). One warm
request costs **~2.0 ms of CPU** → a one-core tenant's capacity is
**~400–500 req/s**; latency is only a meaningful number below that.

**Explanations ruled out** (all measured, none of them the cause):

| | evidence |
|---|---|
| ❌ seccomp | `permissive` p99 49638 ≈ `default` 50367 |
| ❌ the memory cgroup | p99 50319 with `--mem 4G`; zero reclaim |
| ❌ the network namespace | `unshare -Urpmfn` p99 1347 |
| ❌ the other namespaces | `unshare -Urpmf` p99 2545 |
| ❌ the environment itself | bare `fork`+`wait` floor p99 513–1225 µs |
| ❌ Python's GC | zero collections over 3000 iterations |
| ❌ our own plumbing | the same tail appears in a `fork`/`exit` loop with no Zygo in it |
| ✅ **CFS bandwidth throttling** | `throttled_usec` 2 257 080 (quota 1) → 1 858 (quota 2) |

p50 also improved from **1814 → 1426 µs** during this work (headroom 9% → 29%):
the agent now sends `DONE` as soon as it has the child's result and defers
`waitpid`.

**Shortening the period was considered and rejected.** At a fixed quota the tail
tracks the period one for one (p99 ≈ half of it), but it creates no CPU that was
not there — it only splits one long stall into many short ones:

| period | p90 | p99 | total throttled |
|---|---|---|---|
| 100 ms (default) | 1313 µs | 48962 | 2.3 s |
| 20 ms | 1286 | 12846 | 6.5 s |
| 10 ms | **5588** | 8386 | 9.0 s |
| 2 ms | 3076 | 4672 | 14.5 s |

Below saturation the 100 ms period costs nothing already (p99 1.9 ms), so
shortening it would only fix the saturated case while making the ordinary
case's p90 worse. The default is unchanged.

**What was done:**
- [x] `pool::CpuAccounting` — reads `cpu.stat`/`cpu.max`, takes the difference
      between two readings, judges saturation, reports demand in cores. The
      supervisor's queueing decisions will use this too (2.2)
- [x] `zygo bench warm --rate` — fixes the offered load
- [x] `zygo bench warm --cpu` — changes the tenant's quota
- [x] The bench now prints CPU used, the quota, CPU per request and the
      throttling counters
- [x] **On a saturated run the p99 budget is reported as `NOT MEASURED`** and
      does not break CI; p50 is judged either way. The reason: on a saturated
      run the p99 is not a number about this code, and reporting it as a FAIL
      sends the reader somewhere wrong

**Rule (methodology error #7):** *a latency measurement must be able to say
whether it hit its own limit.* A closed-loop benchmark under a hard quota
measures the quota, not the runtime. Added to the README as the third rule.

**No latency gate was added to CI — deliberately.** A p50/p99 claim on a shared,
noisy runner is inherently flaky, and this session's lesson is exactly "do not
assert a number whose source you cannot show". `bench warm` remains a tool you
run by hand.

### 2.2 Supervisor
- [x] A background process in the user's session, `supervisor.sock` (0600, inside
      a 0700 directory, uid checked with `SO_PEERCRED`), auto-start from `serve`.
      A stale socket is told apart from a live one **by connecting** — the only
      test that cannot be fooled by a stale pid file or a recycled pid
- [x] Control protocol (`supervisor::protocol`): `HELLO`/`WELCOME`,
      `SERVE`/`SERVED`, `EXEC`/`EXECUTED`, `LIST`/`FUNCTIONS`, `STOP`,
      `SHUTDOWN`, `PING`, `BUSY`, `ERROR`. **Separate** from the agent wire
      protocol — even if tenant code gets hold of the agent socket it cannot say
      anything the supervisor will act on (test: an agent `EXEC` does not
      deserialise as a control message). The framing is shared, the message set
      is not
- [x] Sandbox registry: name → resolved spec, `WarmFn`, state, gate. Serving the
      same name again retires the old one and installs the new
- [x] Request routing, per-tenant queue (`supervisor::gate`), `concurrency`,
      backpressure. `BUSY` is its own response type and has **its own CLI exit
      code (75)** — the caller's correct reaction is to retry, not to give up
- [x] `zygo serve` / `exec` / `ps` / `stop` / `supervisor run|status`
- [x] `spec.resolve_for_serve()` — in `serve` the name is a *registration*, in
      `up`/`explain` it is a *lookup*. The latter still catches a typo
- [x] The base directory travels over the wire: the supervisor has a working
      directory of its own, so `--mount ./data:/data` has to resolve against
      where the user typed it
- [x] **`poc/verify_supervisor.sh` — 151 end-to-end checks**, with the client
      process exiting between each one. Wired into the Makefile and CI
- [x] Per-request cgroup create/remove, moving the pid, `GO` synchronisation.
      **The pid move was broken until now** — see 2.2b
- [x] Request deadlines enforced by the supervisor + `cgroup.kill`, with a
      freeze-and-signal fallback below kernel 5.14. The limit is the function's
      own `timeout`, not the caller's: N4 makes the spec's limits mandatory, so
      a client cannot buy more time by asking for it
- [x] Crash resilience: a function whose agent dies is rewarmed from its stored
      spec before the next request, with exponential backoff (immediate, then
      200 ms doubling to 30 s) so a handler that cannot start at all does not
      become a rewarm loop. An explicit `serve` clears the history
- [x] **A replaced function killed its own replacement** (found on a Pi, kernel
      6.5): old and new sandbox shared `tenants/<name>`, so on a kernel with
      `cgroup.kill` retiring the old one killed the new one, and every rewarm
      after it. Invisible on the 5.10 phase 0 measured on, where the fallback
      signalled a single pid. Each sandbox now gets a generation
      `tenants/<name>/g<pid>-<n>` of its own; limits stay on the tenant.
      `verify_supervisor.sh` execs after the re-serve, which is the only check
      that can see it
- [x] **Rootless without the `uidmap` package wrote the wrong id-map line**
      (Pi): with `/etc/subuid` configured but `newuidmap` missing, the fallback
      wrote the *first* line of the map, which for the default sandbox user of
      1000 is the subordinate range — EPERM. It now writes the caller's own
      identity entry, found by host id; `doctor` reports the missing helpers as
      degraded instead of "configured"
- [x] `escape_suite.sh` case 12 used one host directory for every run, so a
      rootless run after a root run found it root-owned, could not create the
      symlink, and reported an empty result as an escape. Per-uid paths now.
      Verified on the Pi, rootless: 16 blocked, 0 escaped, 1 skipped
- [x] **Rootless from a login session.** An ssh shell lives in a
      `session-N.scope` that systemd owns, so `zygo.slice` cannot be created
      there — while `doctor` said "delegated", because `cgroup.controllers`
      lists what the *user manager* was given. Both halves are fixed. `doctor`
      now **attempts** it: create a child cgroup, remove it. And a command
      that builds a sandbox re-executes itself inside a transient scope
      (`systemd-run --user --scope -p Delegate=yes`) when its own cgroup will
      not take one, which is what podman does and what turns a two-line
      remedy into nothing to type. Guarded against re-executing for ever, and
      only for the commands that need a cgroup: `zygo ps` does not pay a
      process spawn for one
- [x] Idle tiering (F12, §3.9): `idle_timeout` → `cgroup.freeze`, `cold_after` →
      drop the sandbox and keep the spec, wake on request. Measured: a paused
      function answers **7 ms** after being woken, against ~300 ms for a cold
      start — the pages that make a request a `fork()` are what pausing keeps.
      A function with a request in flight is never tiered whatever the clock
      says. The policy is one callable pass (`tier_idle`) with a thread on top,
      so it can be tested by calling it rather than by sleeping out a ten-minute
      default
- [-] **A per-tenant netns pool** (the narrow variant of PoC 9) — **deferred,
      with the arithmetic**. The variant is still sound: the supervisor keeps a
      tenant's userns alive, so further sandboxes for that tenant could enter it
      and take a pooled netns. But there is nothing to optimise yet. Every
      sandbox is created through one path (`warm_and_register`) and a tenant
      never holds two at once — `serve`, rewarm and waking from cold each build
      one and retire the other. So the saving would be PoC 9's measured
      3.353 ms against a warm-up of 95–336 ms: **1–3.5%**, on a path that runs
      once per function rather than per request. Worth revisiting when a tenant
      really does get several sandboxes — phase 3's blue/green rewarm is the
      first thing that would do it
- [x] Clean the cgroup tree on supervisor restart. Sandboxes die with their
      supervisor, but **cgroups outlive their processes**, so without this a
      restarted supervisor inherits one dead `tenants/<name>` tree per previous
      lifetime and reuses their stale limits. Only cgroups holding no process
      are removed, checked by reading `cgroup.procs` rather than assumed
- [-] Reaping dead sandboxes (`waitpid`) — not needed: `NsSandbox::drop` already
      kills and reaps, and a crashed sandbox is dropped when its function is
      rewarmed. The zombies seen during 2.2a were the `PDEATHSIG` casualties,
      still held by a live handle

#### 2.2a Bug found: `PDEATHSIG` fires on the death of the **thread**, not the process

On the first end-to-end attempt `serve` succeeded, `ps` showed the function
warm, but the first `exec` returned **broken pipe**. `/proc/<pid>/stat` showed
the agent as `Z` — a zombie: the sandbox had died the moment the `serve`
command returned.

The cause is not in Zygo but in the kernel's behaviour.
`prctl(PR_SET_PDEATHSIG)` — the thing that stops a crashed supervisor leaving
sandboxes behind — fires when the **thread that created the child** exits, not
when the process does. The supervisor was starting the sandbox on a connection
thread; that thread ended when the CLI command finished, and the kernel sent the
sandbox a SIGKILL.

I verified the mechanism with a bare `fork` + `prctl`, not with my own code
(5.10 / aarch64):

| who created the child | 300 ms later |
|---|---|
| a thread, which then exited | **killed** |
| the main thread, still alive | running |

Dropping `PDEATHSIG` would have fixed the symptom by giving up the guarantee.
Instead, `supervisor::Launcher`: sandboxes are created by **a single thread that
lives as long as the supervisor**, and connection threads hand the work to it.
Warming is serialised as a consequence — a few hundred milliseconds once per
function, and `exec` never comes through there.

`zygo run` is single-threaded and could never have shown this bug; nor can a
unit test. That is why `poc/verify_supervisor.sh` exists and runs in CI.

#### 2.2b Bug found: the pid in `FORKED` is namespace-local, so nothing was ever moved

Building the request deadline exposed a bug that had been invisible since the
warm pool was written. The agent is pid 1 in its own pid namespace, so the pid
it reports in `FORKED` is the child's number *in that namespace*. The supervisor
was writing it straight into `cgroup.procs` and would have passed it to `kill`.

Measured on 5.10 with a handler that reports `os.getpid()`:

| | |
|---|---|
| pid the agent reported | **2** |
| the child's actual host pid | **28** |
| contents of `req-.../cgroup.procs` | **empty** |

So the per-request cgroup — the entire reason the protocol has the
`FORKED`/`GO` handshake — had never contained a request. It was a directory
being created and removed, and the ~97 µs attributed to it in PoC 3 was the cost
of that and nothing else. `admit()` ignores errors on purpose (a failed move
should not fail a request, since the tenant cgroup still bounds the child), so
nothing ever complained.

PoC 3 could not have caught it: there the agent was a plain host subprocess with
no pid namespace, so the number it reported happened to be correct.

The fix translates the pid before use, via `NSpid` in `/proc/<host>/status`,
with candidates taken from the agent's own children rather than all of `/proc`.
After it, `req-.../cgroup.procs` holds the host pid and the zygote cgroup holds
only the agent.

**Two consequences beyond the fix:**

- A SIGKILL sent to an untranslated pid would have gone to whatever unrelated
  host process held that number. It never fired before because nothing had a
  deadline yet.
- Killing one pid is not enough anyway. Below kernel 5.14 there is no
  `cgroup.kill`, and a handler that forked helpers left them running and holding
  the result pipe open, so the agent never saw end of file and the function
  wedged. Measured: four spinners survived. `cgroup::kill` now falls back to
  freeze → signal every member → thaw, which kills the whole tree; the freeze is
  what stops a process forking between the read and the signal. Re-measured:
  0 survivors.

`poc/verify_supervisor.sh` now asserts directly that a running request is inside
its own cgroup — the check that would have caught this on day one.

**Phase 2 is complete except for the `vm` backend (2.5), which needs a
machine with KVM.** Warm-exec, secrets, the venv cache, the HTTP API and the
benchmarks are built and verified end to end.

#### 2.9b Concurrency, implemented: the agent now holds several forks at once

The agent handled one `EXEC` to completion before reading the next. `serve` now
waits on the control socket **and** every in-flight request's result pipe
together (`select`), so `EXEC` returns as soon as the fork is reported and `GO`
and the result arrive as ordinary events. The supervisor side matches: one
thread per function routes replies to their callers by request id, and nothing
holds the connection for longer than a single frame takes to write.

Measured, `--cpu 4` so the quota is not the constraint:

| | before | after |
|---|---|---|
| concurrency 1 | 607 req/s | 538 |
| concurrency 2 | 592 | **912** |
| concurrency 4 | 599–609 | **981** |
| longest wait for the connection, c4 | **3.7 s** | **494 µs** |
| requests per client, c4 | 684 vs 1166 | **1464 vs 1477** |
| CPU used, c4 | 1.45 / 4 cores | 2.35 / 4 |

So the throughput criterion is now met **because of** concurrency rather than in
spite of it, and the fairness problem is gone: the split is even to within 1%
and the worst wait is under half a millisecond.

**The cost, stated plainly.** Single-stream p50 went from 1316 µs to 1695 µs,
and headroom at p50 from 34% to 15%. Isolated by measurement:

| | p50 | admit |
|---|---|---|
| before, request cgroup silently empty | 1316 µs | 62 µs |
| now | 1702 | 213 |
| now, `--no-cgroup` | 1574 | 0 |

- **~150 µs** is the pid translation (2.2b) making the per-request cgroup
  actually contain the request. That is a correctness fix being paid for, not a
  regression — the old 62 µs bought nothing.
- **~300 µs** is multiplexing: two thread handoffs per request (`FORKED` and
  `DONE` each cross a channel) instead of a blocking read on the calling thread.

Worth revisiting: a fast path that reads the socket on the calling thread when
it is the only caller in flight would recover most of the 300 µs. Not done —
it needs a careful answer to "who owns the socket", and 15% headroom is passing.

**Two agent bugs the design made possible and had to be closed:**

- A child forked while another request is in flight inherits that request's
  pipes, so its reader never reaches end of file and the two requests deadlock.
  The child now closes every other in-flight request's descriptors, collected
  before the fork because afterwards it cannot ask.
- `SHUTDOWN` must stop new work without abandoning forked work, or a `zygo stop`
  during a request loses its answer.

Both are covered in `agents/python/test_zygo_agent.py`, along with a test that
fails on a sequential agent by construction: a slow request and a fast one
issued in that order, where the fast one has to answer first.

### 2.3 warm-exec (warm mode without an agent)
- [x] The sandbox is built once and **held**: its init is Zygo's own code — a
      reaping loop that never `execve`s — so nothing from the image has to
      exist for a sandbox to stay up. It drops every capability, sets itself
      non-dumpable (it maps the supervisor's memory, in the same namespaces as
      tenant code under the same uid; `PR_SET_DUMPABLE` off refuses `ptrace`
      and `/proc/1/mem` to that code), closes every inherited descriptor, and
      costs ~0 MB
- [x] Each request is a fresh process **entered** into the sandbox: the
      supervisor forks a helper, the helper `setns`es user, pid, net, ipc, uts
      and cgroup and forks the request, and the request — after `GO` — enters
      the mount namespace, wires its pipes onto stdin/stdout/stderr, runs
      exactly the hardening the init ran (`child::harden`, now shared), and
      `execve`s `cmd`. The event goes in on stdin, JSON comes out on stdout.
      Rootless by PoC 9's third row: the supervisor created the user
      namespace, so its fork has every capability in it
- [x] **No pid translation** on this path: the helper is in the host's pid
      namespace, so `fork()` returns the request's host pid — the cgroup
      attach and the deadline kill use it directly
- [x] The same cgroup, timeout, secrets and metrics path as the agent:
      `admit`, `place_secrets` and `kill_request` are shared free functions
      now; `Function` is the enum the supervisor, the CLI and the benchmarks
      see, so a function is called, paused, woken and stopped the same way
      whichever mode it is
- [x] Selected by the spec, not a flag: a function with `cmd` and no `runtime`
      is warm-exec. `zygo up` brings them up; `bench warm -- CMD` measures them
- [x] **Measured** (5.10/aarch64, 250 req/s): `sh -c cat` **p50 2210 µs**,
      p99 2950, max 3368 — inside the design's "1–3 ms plus the program". Ten
      CLI round trips averaged 7 ms *including* the client process starting.
      And `python3` per request: **p50 54.5 ms** — the interpreter's own start,
      which is the whole reason interpreters get an agent instead
- [x] 12 end-to-end checks in `poc/verify_supervisor.sh`; see 2.3a for the
      bug the first run found

#### 2.3a Bug found on the first run: the helper held the request's stdin open

`echo` (`sh -c cat`) printed the right JSON and then waited the full 30 s
deadline. `cat` echoes as it reads and exits on end of file — and end of file
never came, because `fork` had given the helper a copy of the supervisor's
*entire* descriptor table, including the **parent's** write end of the
request's own stdin. The helper closed the child-side ends it knew about and
kept everything else until the request exited, which the request could not do
until the helper let go of its stdin.

The same inheritance is a concurrency hazard: a second request's helper would
hold the first request's stdin, and a held init that never execs would keep
the control socket and every agent connection open for ever.

Fixed as a rule rather than a list: the helper `dup`s exactly the thirteen
descriptors it and the request need down to 3–15 (copies first, so no `dup2`
can overwrite one not yet copied) and `close_range`s everything above; the
hold init closes everything from 3 up the moment it has signalled ready. After
that, `sh -c cat` answers in 2.2 ms.

### 2.4 Python reference agent (`zygo-agent`)
- [x] Handler import, `gc.freeze()`, `READY`
- [x] EXEC/FORKED/RESULT/DONE + length-prefixed JSON framing
- [x] Child side: random reseed, stdout/stderr ring buffer, calling `handler`
      (sync/async), result serialisation, `_exit`
- [x] `function` and `stdin` modes
- [x] Thread detection → spawn fallback (`--oneshot` worker); closes R1 with a test
- [x] Secret delivery: `/run/secrets/<name>`, present for exactly as long as a
      request is in flight. **Written from outside the sandbox**, through
      `/proc/<agent>/root`, between `FORKED` and `GO`: the agent never receives
      a value — it is not in `EXEC`, not in the zygote's memory, and not on the
      connection — so an agent cannot leak one and needs no code for secrets to
      work. Ownership is right by construction: the supervisor's host uid is
      what the sandbox maps to the handler's uid. The first request in writes
      the files, the last one out removes them (an RAII lease, so every exit
      from the request path withdraws them). Values come from the environment
      of the shell running `zygo serve`/`up` — not the supervisor's, which
      inherited whichever command started it — and a name with no value is a
      spec error before any sandbox exists. Verified end to end: the handler
      reads it, the file is gone afterwards, the agent's `environ` never has
      it, and it is present for exactly the life of a request
- [x] Peak RSS / cpu_ms / wall_ms (`getrusage`); reading the cgroup's
      `memory.peak` is ready as `cgroup::peak_memory`, the supervisor will wire
      it up

### 2.5 The `vm` backend (libkrun) — first-class in the first release
- [ ] Static linking of libkrun + libkrunfw (licence/size); fallback
      `zygo backend install vm`
- [ ] A `krun_create_ctx / set_root / set_exec / start_enter` wrapper
- [ ] Share the overlayfs view over virtiofs
- [ ] The protocol over vsock; Landlock/seccomp inside the guest as well
- [ ] Guest memory limit = `mem` + a kernel allowance; the host cgroup applies to
      the VMM
- [ ] The shared test suite passing on `vm`; a clear error plus an `ns`
      suggestion when there is no KVM

### 2.6 The library and its bindings
- [x] `zygo-core`: `Pool` + `WarmFn` written and working end-to-end on Linux —
      `serve()` brings the sandbox up and waits for the agent's `READY`, `call()`
      does the EXEC→FORKED→cgroup→GO→DONE round. The agent is handed a
      **connected socket** (no socket file, no mount); it is embedded in the
      binary and written into the data directory on first use (N6)
- [x] `CallTiming`: a per-request phase breakdown (lock/fork/admit/run/release) —
      the metrics §3.12 asks for, and the only way to localise a tail
- [ ] Rewrite the CLI on top of this API (a thin client)
- [x] **Python and Node clients over the HTTP API** (`sdk/python`, `sdk/node`) —
      the thin layer ADR-008's target users actually want, shipped before the
      embedded one because it is what an agent framework, a worker and a
      webhook all reach for. Both have **no dependencies**: the standard
      library has an HTTP client in each language, and a unix socket is thirty
      lines on top of it. Python ships an `asyncio` client beside the
      synchronous one, because an agent framework is asynchronous and a tool
      that blocks the loop for the length of a sandbox request is unusable
      inside one; Node ships hand-written types rather than a build step, so
      what is in the repository is what executes
- [x] Every kind of failure is its own type — `Busy`, `Timeout`,
      `HandlerError`, `NotFound`, `AuthError`, `SpecError`, `TransportError` —
      because each implies a different next move. `Busy` means the request
      never ran and retrying is correct; `HandlerError` means it will fail
      again. A `batch` element is a result *or* an error, returned rather than
      raised, so one refused event does not hide the answers to the others
- [x] Both clients pool connections, and both suites prove it twice: once that
      five calls use one connection, once that eight concurrent calls against a
      200 ms server finish in about 200 ms rather than 1.6 s. A single
      connection would serialise callers behind a socket, and on a path
      measured in milliseconds that is the whole cost
- [ ] Python binding (PyO3): `zygo.Pool()`, embedded use without a supervisor.
      Still wanted for an embedder whose worker is already long-lived; much
      more expensive than the above (manylinux wheels, a Linux-only extension)
      and no longer blocking anybody
- [x] API stability policy: `api` in `GET /version` is the number a client
      checks, bumped only on an incompatible change to a route. `0.x` may
      change with a release note; after `1.0` it will not without a major
      version. Separate from the crate version and from `CONTROL_VERSION`,
      which no SDK speaks — reported anyway, because a mismatch there explains
      an API that is up and answering errors

### 2.7 venv cache
- [x] `requirements.txt` → `cache/venvs/<hash>`, read-only bind at `/venv`
      with `/venv/bin` first on `PATH`, so `python3` resolves to the venv's.
      **Keyed on the image's manifest digest plus the file's bytes**: the same
      requirements against a different image are a different venv (a wheel
      built for another Python fails at import time with a useless message),
      and two projects with identical requirements share one. Measured: built
      in **3970 ms**, reused by a second function in **111 ms**, exactly one
      directory in the cache
- [x] **Built inside a sandbox with the image's own `pip`**, not on the host —
      the only way the venv matches the interpreter it will run under. The
      build gets host networking, once, and it is the one place Zygo grants
      that without `--allow-host-net`: the operation is installing the user's
      own requirements file before any tenant code exists. The warm sandbox
      keeps the spec's `network`
- [-] Embedded `uv` — **not done, deliberately**. `uv` is ~30 MB and N6 caps
      the whole binary at 15 MB. The image's `pip` is slower and already there
- [x] Locking per key with the store's lock, taken only when the marker is
      missing; a directory without a marker is a build that did not finish and
      is rebuilt rather than trusted
- [x] An uninstallable requirement fails at `serve` time with pip's own last
      lines in the error, not from inside a broken sandbox later. The venv is
      read-only inside the sandbox (verified by trying to write it), so no
      tenant can modify what others share
- [x] Found on the way: `SandboxConfig.stdio` insisted on being a terminal —
      `TIOCSCTTY` on the pipe the builder uses to capture `pip` gave ENOTTY.
      It now becomes the controlling terminal only when it is one

### 2.8 HTTP API
- [x] `POST /fn/<name>` (200 / 408 / 429 / 500 as §4.6 specifies), `/batch`
      (every event at once, answers in order, each with its own status so one
      429 hides nothing), `GET /fn`, `GET /fn/<name>/stats`,
      `POST /fn/<name>/warm`, `GET /healthz` (unauthenticated on purpose — a
      load balancer has no business holding the token), `GET /metrics`
      (Prometheus text). `X-Zygo-Timeout-Ms` honoured; the function's own
      `timeout` still wins, as it does everywhere
- [x] Bearer auth by default, token from `ZYGO_API_TOKEN` only (never a flag —
      flags are in `ps`), constant-time comparison. `127.0.0.1:7700` by default;
      `unix://PATH` at 0600. **An unauthenticated listener is refused on any
      address that is not loopback or a unix socket** (P6), and bearer auth with
      no token set is refused rather than silently open
- [x] `zygo api` foreground mode. A **client** of the supervisor over the
      control socket, auto-starting it — not a second owner of sandboxes.
      ADR-005's one RPC boundary holds because HTTP → supervisor replaces
      CLI → supervisor rather than adding to it. A pool of control connections,
      one per request in flight, is what turns concurrent HTTP requests into
      concurrent sandbox requests. hyper was already in the tree through
      reqwest, so the server side cost code, not a dependency
- [x] `Outcome.timed_out`: the supervisor records that *it* killed the request
      for its deadline, because a deadline kill and an OOM kill both arrive as
      exit 137 and only the side that enforced the deadline can tell them
      apart. That is what makes the 408 honest instead of guessed
- [x] `WARM` control request, for `/fn/<name>/warm` and anything else that
      wants a function ready before its first real request
- [x] **The routes an SDK needs**, added in 2.10: `GET /version`,
      `PUT /fn/<name>` (serve), `DELETE /fn/<name>` (stop),
      `GET /fn/<name>/logs`, `POST /run` (one-shot)
- [x] **The deploy gate.** The three that create or destroy a sandbox are
      refused unless `zygo api --allow-deploy` says otherwise, with a 403 that
      names the flag. Without it a token reaches the functions somebody
      declared in a spec file and nothing else; with it the same token can
      serve any image with any mount, which is a shell rather than an API.
      P6 says a widened boundary is spelled out, so it is a flag, not a default
- [x] `allow_host_net`, `allow_private_net` and `allow_unlimited` are **never**
      taken from a request body, whatever the gate says. Each removes a
      guarantee, and a caller that could remove one over HTTP would make the
      flag on the server meaningless

### 2.10 The SDKs, the MCP server, and what they needed from the API

Added after the market read in `docs/`: the target user (a platform, an agent
framework) embeds an API rather than calling a process, and the two languages
they write in are Python and TypeScript. ADR-008 said this in the design
document; what was shipped until now was the Rust crate and a CLI.

- [x] `crates/zygo-cli/src/cmd/oneshot.rs` — one sandbox, one command, output
      captured. Spawns `zygo run` as a child rather than launching in-process:
      a one-shot has no supervisor and no RPC boundary, so in-process would
      save a millisecond against a cold start of eighteen, and would cost a
      second copy of resolve, pull, derive, rootfs view, network setup and
      backend selection — a copy that would drift, in the code that builds
      sandbox boundaries
- [x] Found by its own test: killing the child left its `sleep` holding the
      output pipe, and reading that pipe to end-of-file waited the full thirty
      seconds — so a 200 ms deadline returned in 30 s. The child now leads a
      process group of its own and the deadline kills the group. The test
      checks the grandchild is gone, which an elapsed-time assertion cannot:
      giving up on the pipe looks identical from outside while leaving a
      process behind
- [x] Also found there: this binary sets `SIGPIPE` back to its default
      disposition, so writing input to a child that had already exited would
      have killed the API process serving other requests. The writing thread
      blocks the signal for itself, which turns it back into `EPIPE`
- [x] **`zygo run --quiet`.** Zygo's own progress output — "pulling
      python:3.12-slim" — shares a descriptor with the sandbox's standard
      error, so a caller capturing the streams reads it as something the
      program wrote. Found by the MCP suite on the Pi, where `print(6 * 7)`
      came back as `42` followed by a pull line. The flag hides what Zygo says
      and never what the program says
- [x] **`zygo mcp`** — JSON-RPC over stdio, which is what an agent host starts
      as a child process. Four tools; each request is handled on a thread of
      its own so a thirty-second `run_code` does not block a `list_functions`
      behind it, and answers are written one line at a time so two cannot
      interleave
- [x] The tools expose a **program and nothing else**: no image, no mounts, no
      network mode, no limits. A model reads untrusted text and that text can
      ask it for things, so the boundary is set once on the command line by
      whoever installed the server. A unit test fails if any tool schema ever
      grows an `image`, `mount`, `network` or `mem` field, and the Linux suite
      attempts it for real by sending `mounts: ["/:/hostroot:rw"]` and checking
      that `/hostroot` is not there
- [x] `/work` persists between calls and `/zygo` holds the program, read-only —
      so a model can write a file in one call and read it in the next, and a
      program cannot rewrite itself mid-run. Without `--workspace` the former
      is a scratch directory removed when the server exits
- [x] A tool that fails answers with `isError`, not a JSON-RPC error: an error
      at the protocol layer is handled by the host and never reaches the model
      that could fix the traceback
- [x] `poc/verify_mcp.sh` — **26 checks against a real kernel**, driving the
      server over a pipe as a host does. Every boundary claim is attempted from
      inside: the read-only root by writing to it, the absent network by
      opening a socket, the memory limit by allocating past it (with the
      positive case first, so "it was killed" cannot be satisfied by nothing
      having run). Three of its first four failures were the test's fault, and
      one was the product's — the pull line above
- [x] `make test-sdk`, `make verify-mcp`, and `make test` now includes the
      first

### 2.9 Measurement
- [x] `zygo bench warm` — against the real `Pool`, with a phase breakdown, a
      handler/plumbing split and **the host's own fork floor** measured and
      reported (so how much of the number belongs to the machine is visible).
      Exits non-zero when a budget is missed
- [x] `zygo bench cold` — builds a sandbox, runs a program, tears it down, `n`
      times, against requirement N2. **First measurement of N2: p50 18.4 ms**
      against a 50 ms budget (5.10/aarch64, `python3 -c pass`, image cached).
      The output says when the rootfs was flattened rather than overlaid,
      because that is the cheaper case and a number without it is misleading
- [x] `zygo bench load` — sustained throughput through one warm function, with
      per-client request counts and connection-wait times. See 2.9a
- [x] Timeout exit code: **137**, not 125. 125 means "this host could not run
      it"; a timeout means it ran and was killed, which is a different thing to
      anything reading exit codes. 137 is `128 + SIGKILL` — what `docker run`
      reports and literally what happens, since the deadline is enforced by
      killing the request's cgroup. **`zygo exec` did not pass it on** — it
      folded every failure into 1 until the agent-tool example's test held it
      to what that example's README promised; it now exits with the request's
      own status (137 killed, the program's code for warm-exec, 1 raised)
- [x] **A copy-on-write pollution regression test.** Copy-on-write is the
      whole economy of the warm path: a request is a fork and what it costs is
      the pages it dirties, and pages the *zygote* dirties are worse — copied
      for every later fork and never shared again. Fifty requests through a
      handler that allocates leave the zygote **0 kB** larger (16492 → 16492),
      against a 2 MB budget that would catch a regression writing to the
      parent per request. Read through `ps`, because reading the zygote's own
      `/proc` would be the check disturbing what it measures.

#### 2.9a What `bench load` says about the phase 2 throughput criterion

The criterion is "≥ 600 requests/s at a concurrency of 4". Measured, 6 s runs,
empty handler:

| | requests/s | CPU used | connection wait, max |
|---|---|---|---|
| concurrency 1, `cpu = 1.0` | 424 | 1.00 / 1.00 (**saturated**) | 0 |
| concurrency 4, `cpu = 1.0` | 394 | 1.00 / 1.00 (**saturated**) | — |
| concurrency 1, `cpu = 4.0` | 607 | 1.46 / 4.00 | 0 |
| concurrency 2, `cpu = 4.0` | 592 | 1.49 / 4.00 | — |
| concurrency 4, `cpu = 4.0` | 599–609 | 1.45 / 4.00 | **2.2–3.7 s** |

Two separate findings:

1. **At the default `cpu = 1.0` the criterion is unreachable**, and not because
   of anything in Zygo: a warm request costs ~2.4 ms of CPU, so one core is
   ~420 requests/s. 600 requests/s needs at least 1.5 cores. The benchmark says
   so rather than just failing.
2. **Concurrency contributes nothing.** With the quota lifted, throughput is
   flat at ~600 requests/s whether 1 or 4 clients are calling, and only 1.5 of
   4 cores are used. The agent handles one `EXEC` to completion before reading
   the next, so `concurrency` bounds what the *supervisor admits*, not what the
   agent can overlap.

So the criterion was met numerically at concurrency 4 — but by a single
serialised stream, with the "concurrency 4" part doing no work. **That is what
2.2b below fixed.**

**And a fairness problem found on the way.** At concurrency 4 the
requests-per-client split was only moderately uneven (684 vs 1166), but one
client waited **2.2–3.7 s** for the connection. `WarmFn`'s wire was guarded by a
plain mutex, which is not fair, so waiting was unbounded and badly skewed — a
percentile of the pooled samples reported zero contention right up to the
maximum. `bench load` now prints the maximum and the per-client split for
exactly this reason.

**Acceptance**, with what has actually been measured:

| criterion | status |
|---|---|
| empty handler p50 < 2 ms / p99 < 10 ms | **met** — 1316 µs / 1965 µs at 250 req/s (2.1b) |
| ≥ 600 req/s at concurrency 4 | **met** — 981 req/s, and met *because of* concurrency now that the agent multiplexes (2.9b) |
| back within 500 ms after an agent crash | rewarm implemented with immediate first retry; not yet timed end to end |
| 1000 warm tenants in 64 GB | not measured — needs a host with the RAM |
| warm request < 3 ms on `vm` | **blocked** — no KVM in this environment (PoC 8) |
| a Go binary under 3 ms via warm-exec | **met, with a caveat** — `sh -c cat` at p50 2.2 ms (2.3); there is no Go binary in the image used here, and `sh` starting is part of the number |
| an embedded call through the Python binding | not built (2.6) |

---

## Phase 3 — Spec, networking, derived layers, security evidence, DX

- [x] `zygo up` / `zygo down`. `up` brings every `[fn.*]` in the spec up warm,
      reporting per function and carrying on past one that fails — a spec with
      ten functions where the sixth cannot start should say which one. `down`
      stops **only what that spec declares**, so a supervisor holding another
      project's functions keeps them; `zygo stop --all` is the blunt
      instrument. The `--allow-*` escapes are deliberately unavailable to `up`:
      a spec needing one has to be served deliberately, which is why they are
      flags. See 3.0a for two bugs it found
- [x] **Blue/green rewarm when the spec changes.** `up` is a deploy, and a
      deploy that restarts what did not change is not idempotent — every re-run
      would throw away warm pages and request counters on functions nobody
      touched. `SERVE` gained `if_changed`: the supervisor compares the resolved
      spec, the secret *values*, and a SHA-256 of the handler and requirements
      files against what is registered (warm, paused or cold), and only a
      difference replaces. A replacement was already blue/green — the new
      sandbox is warm before the old one's gate closes, and requests the old one
      accepted finish on it — and now a request *queued* behind the old gate is
      admitted to the replacement instead of being told the function is
      shutting down (one redirect, then the honest answer). `SERVED` reports
      `change: started | replaced | unchanged`; `zygo up` prints and returns
      each. Bare `zygo serve` still always replaces: the user just said what
      they want. See 3.0b for what the eleven end-to-end checks showed
- [~] **Derived system layer — `system = [...]` (apt) works end to end.**
      The packages are installed once, in a one-shot sandbox whose root is a
      private *writable copy* of the flattened image, running as root inside
      its user namespace under the `permissive` profile with host networking;
      the difference is written as an OCI layer tar (whiteouts included) into
      the content-addressed store, and a derived image `<base>+system.<key>`
      is indexed beside the base. Keyed on base manifest + architecture +
      the normalised package list, so every function naming the same packages
      on the same image shares one layer; the resolved `name=version` list is
      recorded under `cache/system/<key>`. Package names are validated at
      resolve time (Debian policy alphabet, no shell). Measured: `jq` on
      `python:3.12-slim` built in 5.3 s, reused in 125 ms; the base image is
      untouched. See 3.0c for why it is copy-and-diff rather than `upperdir`,
      and the `apt` privilege bug the first run found.
      **Not done:** `nix = [...]` (declared, refused with a pointer here);
      pruning derived images; the package-repository allowlist, which is
      `egress`'s job
- [x] **Node agent, Go warm-exec template, `examples/agents/` guide.** The
      Node agent (`examples/agents/node`) keeps a *pool* of workers because Node
      has no `fork()`: each is a process that has already loaded the handler
      and is parked; a request is handed to an idle one, which runs exactly one
      request and exits while a replacement starts off the request path. That
      is one process per request, a pid before any work, nothing until `GO`, a
      fresh process every time. A request that arrives while every worker is
      busy waits for the next one rather than being refused — the first cut
      refused, and the conformance suite's back-to-back requests caught it.
      Passes the nine checks (`make conformance-node`, in a `node:22-slim`
      container against the static musl binary, since that image's glibc is
      older than the build image's). The Go template
      (`examples/warm-exec/go`) is the other answer: a language that starts
      fast needs no agent, and the whole integration is a `cmd`. Built in a Go
      container and run for real by `make examples-go-linux`: `up` on
      `alpine:3`, the event in on stdin, the result out on stdout, ten requests
      through the CLI at 7 ms each, and a rejected event carrying the program's
      own stderr. **V8 snapshots are not used**: Node 22's startup snapshot
      API is still experimental, and a pooled process that has already loaded
      the handler gets the same result for the request path
- [x] **`egress` networking: rootless `pasta`, an nftables allowlist, forced
      DNS.** `network = "egress"`/`"full"` hand the sandbox's network namespace
      to `pasta`, which moves packets in userspace as the ordinary user, and
      then load an nftables ruleset *inside* that namespace — which works
      because entering the user namespace Zygo created grants a full capability
      set in it, the same property warm-exec's `enter` relies on. Both run
      between the id maps and the child's go-ahead, so no tenant code ever runs
      with the namespace unconfined; if either program is missing or either
      step fails, the sandbox does not start. DNS is forced to one address
      `pasta` intercepts and forwards, and `/etc/resolv.conf` is a read-only
      mount Zygo writes — so no resolver of the tenant's choosing, and none of
      the host's search domains. `zygo doctor` reports both programs.
      **Wildcards (`*.example.com`) are refused**: enforcing one needs a DNS
      proxy of Zygo's own, which is not built. See 3.0d
- [x] **Private network blocking (RFC1918, link-local, CGNAT, loopback) by
      default + `--allow-private-net`.** Rejected in the ruleset above every
      allow rule, so a *name* that resolves into a private range is refused
      too — not only a CIDR written in the spec, which is all resolution could
      catch. With the flag the rejects are omitted entirely, because a rule
      naming 10.0.0.0/8 would otherwise be dead code below them
- [x] **Wildcard allow rules, through a resolver of Zygo's own.** Under
      `egress` the sandbox's `resolv.conf` names `127.0.0.53`, and that is a
      UDP DNS server the supervisor runs — bound *inside* the sandbox's network
      namespace by a forked helper that enters it, binds, and passes the socket
      back over `SCM_RIGHTS`, because `resolv.conf` cannot name a port and a
      host-side socket is not reachable from in there. A name off the list gets
      `NXDOMAIN`; a name on it is resolved with the host's `getaddrinfo`, its
      addresses are added to the filter's named sets (`allow4`, `allow4any`,
      `allow6`, `allow6any`, with a 10-minute timeout refreshed on use) **before
      the answer is sent**, and private ranges are dropped from the answer
      unless `--allow-private-net`. `pasta`'s own forwarder is unreachable
      under `egress`, so there is exactly one resolver. Verified:
      `*.cloudflare.com:443` resolves and serves; `example.com` does not resolve
      at all; the forwarder at 169.254.1.1 does not answer. See 3.0f
- [x] **`connections` (nftables `ct count`) and `bandwidth` (`tc`).** Two new
      limits with a default of 256 connections and no bandwidth cap (warned
      about under `egress`/`full`, like disk I/O). The count is a dynamic set
      keyed on the sandbox's source address, TCP only, `ct state new`, and
      answers with a TCP reset: with `connections = 3` the fourth of four held
      connections is `ConnectionRefusedError`. Bandwidth is a token bucket on
      the tap's root qdisc and shapes **what the sandbox sends**: 500 KB at
      `100K` took 4.9 s against 1.2 s unlimited, which is the arithmetic. What
      it receives is shaped only where the host has an `ifb` device — see 3.0f
      for why the obvious tool broke `pasta`
- [x] **Landlock v4 network rules** — `bind` is denied in every namespaced mode
      (no mode has ingress) and `connect` is granted on exactly the allowlist's
      ports (plus 53) under `egress`, or left alone when a rule names no port,
      because Landlock cannot say "any port to this host". Defence in depth
      behind nftables: on ABI v4 a port off the list is refused at `connect()`
      with `EACCES` before a packet exists. Unit-tested on every host; the
      end-to-end check reads the ABI from `zygo doctor` and expects
      `PermissionError` on v4+ and the filter's error below it — **v4 has not
      been exercised here** (5.10 has ABI 0); CI's ubuntu-24.04 (6.8) is where
      it runs
- [~] **`zygo shell <name> [-- cmd…]`** — a fresh process entered into the
      function's namespaces, so the warm agent keeps its memory, its counters
      and its place in the idle policy. The *client* does the entering, not the
      supervisor: it runs as the same user, so entering the user namespace Zygo
      created grants it the same capability set inside it, and the terminal
      stays where it already is instead of being proxied through a frame
      protocol that carries nothing else like it. It drops every capability and
      sets `no_new_privs`, and deliberately skips seccomp, Landlock and the
      tenant's cgroup — a debug shell the memory limit kills is not one, and it
      says so on the way in. Six end-to-end checks, including that the agent's
      request count and state are unchanged by it.
- [x] **`zygo logs <name> [-f] [-n N] [--failed]`.** The supervisor keeps a
      bounded ring per function *name* — 500 entries, 4 KiB of text each —
      holding the zygote's own stdout/stderr (import-time warnings, a crash
      traceback; the agent's stdio is a pipe now, read line by line and also
      forwarded to tracing so nothing that was visible before is lost) and one
      entry per request with its exit code, timing, stdout, stderr and error.
      It belongs to the name rather than to the `Entry`, so it survives a
      blue/green replacement and a cold spell: a `logs -f` across a deploy
      shows the new zygote come up. `-f` polls for entries after the last
      sequence number rather than streaming, because the control socket is one
      reply per request and a stream would be a second protocol. `--json` is
      one compact object per line — the first version pretty-printed, and the
      end-to-end check for "one entry per line" caught it. `stop` forgets the
      log with the function. Ten end-to-end checks
- [x] **JSON logs, Prometheus `/metrics`, OTLP** — `--json` switches the
      tracing subscriber to JSON lines, `zygo api` serves `/metrics`, and
      `zygo api --otlp-endpoint <url>` (or `OTEL_EXPORTER_OTLP_ENDPOINT`)
      pushes the same numbers, from the same snapshot, as **OTLP/HTTP with
      the JSON encoding** every `--otlp-interval` (60 s). JSON rather than
      protobuf because the protocol defines both as stable and JSON costs no
      `prost`/`tonic`: the exporter is `serde_json` plus the HTTP client the
      registry pull already links, which is how it fits the 15 MB budget.
      `OTEL_EXPORTER_OTLP_HEADERS` for the collector's auth. A down collector
      is one warning and one recovery line, not a log full of it. Tested by
      receiving the real POST on a socket and checking the JSON mapping's
      traps (int64 as strings, cumulative sums with a start time).
      **Metrics only**: the request spans §3.12 names need a trace context
      on the request path, which is a protocol change, and are not built
- [x] **Docs**: [quickstart](docs/quickstart.md), [concepts P1–P8 with the
      cost of each](docs/concepts.md), the [`sandbox.toml`
      reference](docs/spec-reference.md), [seccomp
      profiles](docs/seccomp-profiles.md), the [threat model](docs/threat-model.md),
      a [comparison](docs/comparison.md), and a `docs/SUMMARY.md` so mdBook
      can build a site from the same files. Not a hosted site yet — that is a
      domain and a deploy, not a document
- [x] **Examples**: a [webhook](examples/webhook) behind the API with a
      per-request secret, an [LLM tool](examples/agent-tool) under `strict`
      with four pids and a two-second deadline, a [CI job](examples/ci-job)
      as a `zygo run`, the Go warm-exec program, and the agents. The specs are
      validated and the tool and the Go program are *run* by
      `make examples-go-linux`, including the two inputs the tool's README
      promises are refused. **Windmill integration** is not here: it is a
      change on Windmill's side and belongs in that repository (§4.8 says what)
- [x] **Shell completion** — `zygo completion bash|zsh|fish|elvish|powershell`,
      generated from the parser by `clap_complete` rather than written by hand,
      so it cannot drift from the flags the binary accepts. Two tests hold it
      there: one generates for every shell and looks for the commands, one
      asserts every declared subcommand is reachable — a command that is in
      `--help` and not in the dispatch is worse than one that is missing
- [x] `--json` on every command that answers something: `doctor`, `backend
      list`, `images`, `ps`, `stop`, `serve`, `exec`, `up`, `down`, `spec
      validate`/`explain`, `pull`, `bench`, `agent test`, `logs`. Not on `api`
      (a server), `shell` (a terminal) or `completion` (a script), where it
      would mean nothing. Not on `top` either, whose `--json` prints one
      frame and exits — a stream of frames is a different thing and `--once`
      is the scriptable form

- [x] **`zygo stats [name]`.** Counters and latencies are different windows
      and the table says which is which: `requests`/`failures` are counted
      since the function was warmed, the percentiles are over the entries
      still in its log, and a `samples` column joins them. No `p99` below a
      hundred samples — a percentile over nine of them is a number with a
      decimal point and no content, so the column says `—` and the footer
      says why. Killed requests are split into "overran a deadline" and
      "something else, usually the memory limit", because both arrive as exit
      137 and only the supervisor knows which.

      Writing it found a measurement bug that made it worth writing. A killed
      request logged `wall_ms: 0`, because the agent's `DONE` describes a
      request that *finished* — so every timeout read as the fastest request
      there was and dragged the percentiles down with it. Warm-exec requests
      logged `0` too: nothing was timing them at all. Both now record what
      the supervisor's own clock saw, and two checks in
      `poc/verify_supervisor.sh` hold them there.

- [x] **`zygo top`.** What justifies it beside `ps` is the pair of columns a
      single sample cannot have: a rate needs two. So the first frame prints
      `—` and says why, rather than a zero that would be a measurement. The
      rate is over the interval that actually elapsed, not the one asked for,
      because on a loaded machine those differ and dividing by the request
      would overstate every figure. A counter that goes backwards — a
      restarted supervisor — saturates to zero instead of becoming an enormous
      rate. Measured in the suite: 40 requests inside a 2 s interval read as
      **19.9 req/s**.

      Every command the CLI declares is now built. The `pending()` helper that
      answered "not implemented yet (todo.md, phase N)" has no callers left
      and is gone.

#### 3.0a Two bugs `zygo up` found — the first time functions shared an image

**The flattened rootfs was shared, mount points and all.** `rootfs_view` keyed
the flattened directory on the image layers alone and then created the mount
points *inside* it. Every function using that image therefore shared one
directory, and the first to run decided what shape each mount point had.

Measured on a three-function spec: `broken` ran first (names are ordered, and
its handler did not exist), so its `/zygo/handler.py` mount point was created as
a **directory** — what `docker run -v` does for a source that is not there. The
other two then failed with **ENOTDIR** trying to bind a file onto it.

The flat rootfs is now keyed on the layers *and* the mount points, exactly as
the overlay skeleton already was, and the mount points are created before the
done marker so nobody sees a half-built rootfs. Identical shapes still share one
copy, which is the common case and the reason sharing was tempting.

This was reachable before `up` — two `zygo serve` calls on one image with
different mounts would do it — but every test until now used one function per
image.

**A missing handler failed with ENOTDIR instead of naming the file.** The mount
source not existing is treated as a directory, so the error surfaced from deep
in the launcher with nothing in it to act on. Now checked in `Pool::serve`,
which reports `fn.broken.entry: /proj/does-not-exist.py does not exist`.

Deliberately *not* checked at resolve time: resolution is filesystem-free on
purpose — it normalises paths without touching them, which is what lets the
whole spec layer be tested on any host. There is a test named for that property,
and it failed when the check was put there, which is the test doing its job.

#### 3.0b Blue/green — what "unchanged" has to mean

The replacement half was already there: `serve` warms the new sandbox, swaps
it into the registry, closes the old gate and lets the old sandbox go when the
last in-flight request's `Arc` drops. What was missing was the *decision* —
`up` replaced everything, every time — and one gap in the drain: a request
queued behind the old gate got "`v` is shutting down".

**What is compared.** The resolved spec is not enough. Editing `handler.py`
changes nothing the spec can see, and that edit is the most common reason to
deploy at all. So the registry keeps a SHA-256 of the handler and requirements
files taken *before* the warm-up — an edit that lands while the sandbox is
starting counts as a change next time, because whether the sandbox read the old
or new bytes is unknowable and "replace" is the safe answer. Secret *values* are
compared too: a rotated key is a deploy. Mounts are deliberately not: a bind
mount is live by design, so its contents are not something a restart would
refresh. Cold functions are compared as well, so an unchanged function that
went cold is woken rather than rebuilt.

**The queued request.** On `Closed` the request looks the name up once more;
if a different entry holds it now, it tries that one. One redirect, not a
loop — two closures in a row means somebody is redeploying faster than requests
are admitted, and at that point the honest answer beats a retry storm. Verified
with `concurrency = 1`: a request queued behind a 2-second one, replaced
mid-wait, ran on the replacement and returned the *new* code; the in-flight
request finished on the old sandbox with the old code, exit 0.

**Found along the way.** `serve` over a name that had gone cold left the stale
`Cold` entry behind — invisible, because the warm registry is consulted first
and the next tiering pass overwrote it, but a second copy of a spec that nothing
would ever read. The cold entry is now removed by the same function that
registers the replacement.

**Not done.** `zygo serve` has no `--if-changed`; the flag is `up`'s because
`up` is the deploy command. A function whose *image* tag moved is not detected
— the store pins by digest at warm time and `zygo pull` is explicit.

#### 3.0c The derived system layer — copy and diff, and `apt`'s own sandbox

**Why not `upperdir`.** The design's mechanism is an overlay with a writable
upper directory, and the layer is the upper. Two things are wrong with that
rootless. An unprivileged overlay mount needs kernel 5.11, and the host phase
0 measured on (5.10) refuses it — the flatten fallback exists for exactly this.
And where it does work, what the upper holds for a deletion is a character
device 0:0, and for an opaque directory a `trusted.overlay.opaque` xattr;
turning those into `.wh.` entries needs to *read* them, which needs privileges
a rootless build does not have. So: flatten the base (cached, shared), copy
it with modes and mtimes preserved, run the install in the copy with the root
left writable, then walk both trees. Additions and modifications are found by
kind, size, mode and nanosecond mtime — sound because the copy preserved them
and `dpkg` writes files with their package's timestamps, so an identical
reinstall is identical. Deletions become `.wh.` entries; a path that changed
kind is written as a whiteout *and* the new entry, because the layer format
has no "replace" and flattening a directory over a file fails. The tar is
deterministic (sorted, root-owned, no uids from the build host), so the same
inputs give the same digest. The cost is one copy of the image per build:
about a second for a slim image, once per key.

**The writable root is the only one.** `SandboxConfig.writable_root` skips
the read-only remount of the root bind for this one sandbox. Everything else
is a tenant, and for a tenant the plan's `bind ro` is a guarantee.

**The bug the first run found.** `apt-get update` failed with `seteuid 42
failed - Invalid argument` and the http method died. `apt` sandboxes its own
download methods by switching to the `_apt` user, and under a single-id map
uid 42 does not exist. `APT::Sandbox::User=root` turns that off — and it was
on the `install` line only, while `update` is where the download happens.
Both commands carry it now, and the unit test counts two.

**Ownership.** Under a single-id map everything the install writes is the
user's on the host and root's in the layer; a package that `chown`s to another
user gets EINVAL. That is what a rootless `docker build` produces too. With
`newuidmap` and a subordinate range the launcher maps them and ownership is
kept — the Pi fix above is what makes that path honest.

**Ten end-to-end checks** (`verify_supervisor.sh`): the build, the package
present, the apt lists cleaned out of the layer, the base image unchanged for
a function without `system`, a second function reusing the layer, the derived
image listed, the version recorded, an unknown package failing with `apt`'s
own words and leaving nothing indexed, and a malformed name refused at
resolve time.

#### 3.0d Egress — two failures that only a real kernel could show

The mechanism was probed before any of it was written: `pasta` given
`--netns`/`--userns` paths configures an existing namespace, and `nft` loaded
inside it actually filters. Both held. What did not hold was the part that only
appears once Zygo is the one running them.

**`pasta` started as root drops to `nobody` first.** Its default is
`--runas nobody`, applied *before* it opens `/proc/<pid>/ns/user` — and
`nobody` may not read that file, so it failed with `Permission denied`. Zygo is
rootless by design, and as an ordinary user `pasta` keeps its identity and the
default is harmless; but it runs as root in containers and CI often enough that
the default is a trap. `--runas <our uid>:<our gid>` is now passed explicitly,
which is a no-op for the case the project targets and the fix for the one it
does not.

**`execve` threw away the capability that `setns` had just granted.** Entering
the sandbox's user namespace gives a full capability set inside it, so `nft`
should have been able to load the ruleset. It reported
`cache initialization failed: Operation not permitted` — a message that reads
like a missing kernel feature. The cause is that `execve` recomputes
capabilities and keeps them only for uid 0: inside the sandbox's namespace this
process is the *mapped* uid, 1000 by default, and uid 0 there may not be mapped
to anything at all when there is no subordinate range, so becoming root first
is not available either. `CAP_NET_ADMIN` is now raised in the **ambient** set
before the exec, which is the one set that survives it for an unprivileged uid.
`nsenter` hides this by setting uid 0 for you; doing it by hand does not.

**A test bug, found by the same run.** The first version of the checks read
`case "$out" in *'"other":"ok"'*) bad ;; *) ok ;; esac` — "the destination was
refused" was the *fallthrough*. When `up` failed and the function did not
exist, the error text matched nothing and three negative checks passed for the
wrong reason. They now read a named field out of the answer and fail when it is
missing, so "refused" means the handler ran and was refused. The same run also
showed the pasta-leak check passing vacuously, because no `pasta` had ever
started; it now asserts that one is running per networked sandbox *first*.

**What is verified** (15 checks): a `none` function still reaches nothing; the
allowed destination is reachable and one off the list is not; a private address
is refused without the flag; DNS resolves through the forwarder and a resolver
of the sandbox's own choosing does not answer; `resolv.conf` names the
forwarder and carries none of the host's search domains; `full` reaches the
public internet but still not the host's networks; a wildcard is refused with a
reason; and no `pasta` is left behind after `down`.

#### 3.0g The profile that had never been run: what the matrix found

Phase 0 "validated" the default seccomp profile against five packages — with
a JSON profile applied by a different tool. The Rust filter that ships had
never been run against them. `make seccomp-matrix-linux` did, and the venv
build failed before a single function existed.

**`clone3` and the fallback that never came.** The filter is an allowlist and
answers `EPERM` to anything unlisted; `clone3` was listed only in
`permissive`. glibc's `pthread_create` tries `clone3` first and falls back to
`clone` on exactly one error, `ENOSYS`. On `EPERM` it stops and the program
sees "can't start new thread" — which is `pip` on any download large enough
for a progress bar, `numpy` on import, and every other threaded program. The
filter cannot allow `clone3` outright: its flags live in a struct BPF cannot
read, and the whole point of the `clone` rule is to read the flags. So it
answers `ENOSYS`, which is Docker's answer for the same reason, and glibc
takes the path that is checked. A verdict test now pins `ENOSYS` for
`default` and `strict` and `Allow` for `permissive`.

**`strict` and the socket it inherited.** With the venv built, all five
packages worked under `default` and all five `strict` functions died before
their handler ran: "expected READY from the agent, got end of stream".
`strict` removed the whole socket family, data calls included, and the agent
talks to the supervisor over a socket it was handed at descriptor 3 — a socket
it could no longer `recvfrom`. The mistake was in what the profile was
supposed to deny: transferring bytes on a descriptor a process already holds
is not a capability, opening one is. `strict` now removes `socket`,
`socketpair`, `connect`, `bind`, `listen` and `accept4`, keeps `sendto` and
`recvfrom`, and the test that checks the socket family is gone also checks
the transfer calls are not.

Both are the same lesson as 3.0d's: a security profile that has been read
carefully and never run is a list of intentions. The matrix runs in under
three minutes and is part of the Linux chain now.

#### 3.0h The child filter, and who is allowed to install it

The design puts a second seccomp filter in the agent's forked child: it is
already running the interpreter and never needs another program, so `execve`
and process creation can go. The sandbox's own filter cannot take them — the
launcher `execve`s *into* the agent, so a profile without `execve` is a
sandbox that cannot start at all.

The awkward part is who builds it. The syscall numbers are the host's and the
agent may be written in anything, so neither end can do it alone. The split:
the supervisor builds the program and passes it as bytes in
`ZYGO_CHILD_SECCOMP` (base64 of the raw `sock_filter` array), and the agent
installs it with one `prctl` after `GO` and before the handler. The agent
needs no knowledge of what is in it; the Python one decodes it once at
start-up, so the child's share is a single syscall and no `ctypes` import.

`clone` is the interesting entry. Removing `fork` and `vfork` is not enough —
glibc makes processes through `clone`, and threads too. The program checks
`CLONE_THREAD`: with the flag it is a thread and is allowed, without it it is
a process and is refused. So a handler can still start a thread and cannot
fork-bomb its way to `pids.max`, which is the distinction the design wanted.
Filters stack and the kernel takes the strictest answer, so nothing here can
loosen the sandbox filter underneath.

Verified by running it rather than reading it, which is this project's rule:
under `default` a handler's `subprocess.run` works, under `strict` the same
handler fails with `PermissionError` and the zygote keeps serving; a thread
still works; a malformed value is a start-up error; the spawn fallback (for
handlers that cannot be forked) installs it too. An agent that does not
implement it is still conforming — the spec says so — and its `strict`
functions have the sandbox filter only. The Node and sh examples say so.

#### 3.0i `zygo.lock`, and the one thing it refuses

A tag is a pointer. `image = "python:3.12-slim"` today and next month are two
different images, and nothing in a deploy noticed. `up` now writes what each
function resolved to beside the spec, and the semantics are `Cargo.lock`'s,
because they are the ones nobody argues with: absent → written; the spec
changed → rewritten silently, since the user just asked for the change; the
spec is the same and the image is not → **refused**, with both digests and a
`--relock` to accept it. It changes nothing by itself. Refusing to let
something change silently is the whole feature.

What is recorded is the *index* digest where the registry served a
multi-platform index, not the platform manifest's — the registry client had
been dropping it, resolving the index and returning only the selected
manifest. A lock naming this host's manifest would fail on a colleague's
laptop for no reason other than its architecture, which is a lock file that
teaches people to delete lock files. Where an image exists for one platform
only, the manifest digest is recorded with a `platform` beside it, and the
refusal message says why it cannot travel.

`apt` versions are the exception: recorded, and a move is a warning rather
than a refusal. Debian's archive does not keep old versions, so refusing
would strand every host that was not built the same week.

Not built, deliberately, until the shape has been discussed: pinning pip's
transitive resolution (only the requirements file's hash is recorded),
pulling the locked digest, and a `--frozen` for CI.

#### 3.0f The resolver inside the sandbox, and the limit that broke `pasta`

**Why the resolver is inside.** An allowlist by name cannot be enforced by a
packet filter; the filter sees addresses, and the addresses behind a name are
the service's to change. The design's answer is to make the resolver the
policy point, and the constraint that shapes the implementation is a small
one: `resolv.conf` cannot name a port. So the resolver has to be on port 53 of
an address the sandbox can reach, which means *inside its network namespace*.
A socket belongs to the namespace it was created in, for ever, whoever holds
it — so a forked helper enters the sandbox's user and network namespaces,
binds `127.0.0.53:53`, and passes the descriptor back over `SCM_RIGHTS`. The
supervisor then serves DNS on a socket that lives in the sandbox from a thread
that does not. A thread could not have done the entering itself: joining a
user namespace needs a single-threaded process.

**Order is the guarantee.** For an allowed name the addresses are added to
the nftables sets *before* the answer is sent, so by the time the sandbox can
act on an address the filter already permits it. An address that could not be
admitted is left out of the answer rather than handed out with a connect
timeout attached. Sets rather than rules, with a 10-minute timeout and a
refresh on use, so a service that moves address keeps working and one it left
does not stay open indefinitely. Under `egress`, `pasta`'s own forwarder is
made unreachable, which the suite checks: with two resolvers the second is a
way around the first.

**The obvious download limit resets the connection.** `bandwidth` on what the
sandbox *sends* is a `tbf` on the tap's root qdisc and behaves exactly as
arithmetic says (500 KB at 100 KB/s: 4.89 s; unlimited: 1.09 s). For what it
*receives*, the standard rootless tool is an ingress policer that drops
packets over the rate — and every attempt ended in
`ConnectionResetError`, whatever the burst. `pasta` is a userspace TCP stack,
and that much loss on its tap side reads to it as a dead peer. Queueing instead
of dropping needs an `ifb` device to redirect ingress through, and that is a
kernel module a host may not have — this one does not — and one that cannot be
autoloaded from inside a user namespace. So `bandwidth` shapes what is sent,
shapes what is received where `ifb` exists, and warns where it does not,
instead of silently breaking every download.

**Six new end-to-end checks**, all with the positive case established first:
the wildcard resolves and serves; an off-list name gets `NXDOMAIN`; the
forwarder does not answer; the fourth of four held connections is refused
under `connections = 3`; the upload takes what the limit says; and `full`
still resolves anything through the forwarder.

#### 3.0e The conformance tool found the reference agent losing requests

`zygo agent test` was written against `spec/protocol.md` §3 rather than against
the Python agent, which is the only way a conformance suite is worth anything.
On its first run the reference agent failed one check and died: a frame whose
body was not valid JSON raised `json.JSONDecodeError` straight out of the read
loop, killing the agent and every request in flight with it.

The distinction the fix turns on is whether the stream can be resynchronised. A
body that arrived whole and is not a message leaves the connection sitting at a
frame boundary — the length prefix was honoured — so the agent can report
`bad_message` and carry on. An *announced length* past the 32 MiB cap is the
opposite: nothing was consumed, there is no way to find the next frame, and
closing the connection is the only correct answer. The agent now has a
`BadFrame` exception for the first and keeps raising for the second, and
`spec/protocol.md` §3 gained the rule as requirement 6 — it was implied by "no
silent loss" and worth saying out loud.

The `sh` agent is what keeps the suite honest in the other direction. A
conformance tool written and tested against one implementation tends to encode
that implementation's habits; a second agent sharing no code, in a language with
no JSON support and no threads, is what shows the wire is really the contract.
It also forced one protocol clarification: it serves one request at a time and
answers a second `EXEC` with `overloaded`, which is conforming — concurrency is
optional, losing a request is not — so the suite accepts a refusal as an answer
and says so in its output.

And a check on the checker: a deliberately lazy agent that sends only `READY`
and then sleeps is run in the suite, and the run fails if it *passes*.

**A harness bug, found while wiring this in.** `make test-linux` ended in
`|| true`, so the container's exit code was always zero — a Linux build that did
not compile reported success, and had done for as long as the target existed.
Caught because a run happened to race a `Cargo.toml` edit and "passed" with two
`E0433`s in its log. The `|| true` is gone. Tenth entry for the inventory of
bugs found in the tests themselves.

### Security (a precondition for the public release)
- [~] Escape suite: runc CVE-2019-5736-style, writing to `/proc`, mount leaks,
      cgroup `release_agent`, `/dev`, userns+setuid — **all attempted and all
      failing on `ns`** (16 vectors, `make escape-linux`, see
      [docs/threat-model.md](docs/threat-model.md)); `vm` waits for the
      backend
- [x] **The `strict` seccomp profile + a package compatibility matrix**
      ([docs/seccomp-profiles.md](docs/seccomp-profiles.md)): `make
      seccomp-matrix-linux` installs requests, pydantic, numpy, pandas and
      Pillow into one venv and exercises each under `default` and `strict`.
      All ten cells work — after two real bugs the first run found. **Every
      threaded program was dead under `default`**: `clone3` was unlisted and
      so returned `EPERM`, and glibc's `pthread_create` falls back to `clone`
      only on `ENOSYS`; `pip`'s progress-bar thread on `numpy`'s 13.6 MB wheel
      is what surfaced it. `clone3` now answers `ENOSYS`, as Docker's profile
      does, and the flag-checked `clone` path is what runs. **`strict` killed
      every function before its handler ran**: it removed the socket data
      calls, and the agent's control socket is an inherited one it could no
      longer `recvfrom`. `strict` now removes socket *creation* and keeps
      transfer. The difference against Docker's default is in the doc. **The
      child filter is installed now**: under `strict` the supervisor builds a
      second program (`execve`, `execveat`, `fork`, `vfork`, and `clone`
      without `CLONE_THREAD` → `EPERM`; the sandbox filter stays underneath)
      and hands it to the agent as `ZYGO_CHILD_SECCOMP`, base64 of the raw
      `sock_filter` array; the Python agent installs it in every child after
      `GO` with one `prctl`. Verified by running it: `subprocess.run` works
      under `default` and fails with `PermissionError` under `strict`, a
      thread still works, a malformed value is a start-up error, the spawn
      fallback installs it too. Node and sh agents cannot reach `prctl` and
      say so. See 3.0g
- [x] **`SECURITY.md` and [docs/threat-model.md](docs/threat-model.md) written.**
      The policy names what is in scope and, more usefully, what is not and why:
      `--allow-*`, `network = "host"` and `zygo shell` all remove a guarantee
      deliberately and are not vulnerabilities. The threat model is the design's
      §3.10 written against what is *built and attempted* rather than planned,
      with a section on where the boundary is weaker than it looks — one kernel,
      the shared uid without `newuidmap`, Landlock's 5.13/6.7 floors,
      `cgroup.kill`'s 5.14 floor, the uncatalogued `strict` profile.
      The reporting channels are GitHub's private advisory and an e-mail
      address, so a reporter without a GitHub account still has somewhere to
      go. **Decided: no bug bounty.** Reports are answered, fixed and credited,
      not paid for; what counts as a report is the in-scope table in the file

**Acceptance:** a stranger can bring up three functions (Python, Go, apt-layered)
with `up` from the README in ten minutes and call them from a webhook · the
escape suite reports zero successful escapes.

---

## Phase 4 — External audit + the `gvisor` backend

- [ ] An independent security review (findings and their resolution public).
      **Needs an auditor, not a commit** — everything here that can be done
      without one is done
- [x] **Fuzz-based extension of the escape suite** — `make fuzz-linux`
      (`poc/fuzz_syscalls.sh`) sweeps every syscall number the architecture
      has (469 on aarch64) against all three profiles, each call in a forked
      child with zero arguments so a syscall that blocks, exits or changes
      process state takes nothing with it. The escape suite attempts the
      vectors somebody thought of; this needs no imagination, which is what
      makes it the check for a filter whose branch offsets are *computed*.
      Asserts, without a copy of the allowlist (a check against the list would
      only test the list against itself): the profiles are ordered on a real
      kernel (`permissive` ⊋ `default` ⊋ `strict`, and the differences are
      exactly the 16 and 6 the constants claim); **no syscall kills the
      process**, so the default action is an errno rather than a dead sandbox;
      `clone3` answers ENOSYS and not EPERM; every syscall the threat model
      names is refused. 13 checks, in CI. Two bugs in its own first run, both
      of the kind this project keeps finding in tests: a SIGALRM handler that
      returned instead of raising, so PEP 475 restarted the read and the sweep
      hung for ever on the first blocking syscall; and `sort -n` feeding
      `comm`, which compares as strings and had been answering from unsorted
      input while warning only to stderr
- [x] **Kernel age warnings in `zygo doctor`** — a `kernel age` check that
      dates the running series against a table of upstream release dates and
      warns past two years, saying whether it is a long-term series. No feed
      and no network: a CVE list that needs fetching is a check that fails
      closed on an air-gapped host. The table is a floor on knowledge, so a
      series *newer* than it knows is never called old — otherwise the warning
      ages into a lie the moment the binary is a year old — and one older than
      every row is reported as a minimum. Degraded, never failed: an old
      kernel still runs sandboxes. It says plainly that age is not the same as
      unpatched, because a long-term series gets backports without changing
      its version. Running it rather than trusting its unit tests found an
      unrelated CLI bug on the first try: **every command aborted when its
      output was piped to a reader that exits early** (`zygo doctor | head`,
      `zygo ps | grep -q`). Rust ignores `SIGPIPE`, so the write returns
      `EPIPE`, `println!` panics, and `panic = "abort"` does the rest. One
      line in `main` restores the default disposition
- [x] **`runsc` download and verification (`zygo backend install gvisor`)** —
      fetches the release archive, checks its published sha512 **before**
      decompressing anything, and unpacks with the zstd decoder the image
      store already carries. There is no flag to skip verification: this
      downloads a binary that will be handed other people's code.
      gVisor's own install instructions still describe a bare `runsc` beside
      a `runsc.sha512`; **the bucket has neither**. It publishes
      `gvisor.tar.zstd`, and the first version of this command 404'd against
      the documented path. Worse, the first version that did download
      installed `runsc` *alone* — which passes `runsc --version` and then
      refuses to start a sandbox, because this release execs
      `gvisor-bin/gvisor_sentry` and its sidecar policy is `STRICT`. The
      sidecars are installed too; the 41 MB containerd shim is not
- [x] **OCI bundle generation (the same mount plan → `config.json`),
      `runsc run`** — [`oci.rs`](crates/zygo-core/src/oci.rs), pure and
      unit-tested on every host, which is the payoff for the mount plan being
      data rather than a sequence of syscalls. Device nodes and propagation
      go back to the runtime, masked and read-only paths become the two path
      lists, the seccomp allowlist becomes an OCI `seccomp` section, and an
      overlay rootfs is refused because `root.path` is one directory (the
      store flattens for this backend instead). `zygo run --isolation gvisor`
      works: 19 checks in `make gvisor-linux`
- [~] The protocol running inside runsc (warm-exec + agent). **Refused with a
      reason rather than half-built**: a warm sandbox is entered with `setns`
      on namespaces the supervisor holds, and gVisor's boundary is not those
      namespaces — entering a running one is `runsc exec`. The agent is worse:
      it takes its control socket as an inherited descriptor, and an OCI
      runtime closes everything but stdio, so it would need a socket bound
      into the bundle. Both say so and point at `--isolation ns`
- [x] **The shared suite passing on `gvisor`** — `make gvisor-linux` runs the
      same command on `ns` and on `gvisor` and compares stdout and exit code
      (requirement N8), *and* checks the one thing that must differ: `uname -r`
      reports `4.19.0-gvisor` against the host's `5.10.104-linuxkit`, which is
      the proof the boundary actually moved. **Not done:** the performance
      difference, which needs a host where both backends can hold a warm
      function, and `gvisor` cannot yet
- [x] **Firecracker snapshot/restore research (preparation for v2)** —
      written up in
      [docs/firecracker-snapshots.md](docs/firecracker-snapshots.md). The
      conclusion that matters: a restore is not a faster `fork()`, it is a
      *safer* one. Published restore figures are in the low hundreds of
      milliseconds against this project's measured 1.9 ms fork, so the case
      for it is the hardware boundary, not latency, and the docs should say
      so rather than implying a choice that is not there. Three correctness
      problems come before any of it: **every restore of one snapshot shares
      its entropy** (the same bug the reference agent had, one layer down),
      a restored guest believes it is the moment the snapshot was taken, and
      snapshots are tied to a CPU model, a VM configuration and a Firecracker
      version — so the cache key needs all three, like the derived layer's.
      Research only; the `vm` backend has to exist first

**Acceptance:** the same suite passes on three backends · external audit findings
closed and public. **Two of three:** `ns` and `gvisor` agree on the shared
probes; `vm` needs KVM.

#### 4.0 The first run on a real host, and three things it found

Every measurement before this was taken in Docker on macOS: kernel 5.10,
aarch64, root inside a container, one cgroup. A Raspberry Pi on Ubuntu 23.10
(kernel 6.5, an ordinary user, a systemd session, real `subuid` ranges) is
different in the ways that matter, and it exercised **overlayfs in a user
namespace** and **`cgroup.kill`** for the first time — both have had code
since phase 1 and neither had ever executed, because 5.10 has neither.

Landlock still has not: this kernel does not compile it in at all.

Three bugs, each of which had been there the whole time:

- [x] **`zygo doctor` said `cgroup v2 … ok` on a host where `zygo run` failed.**
      It read `cgroup.controllers`, which on an ssh login's `session-N.scope`
      lists everything the parent delegated — and the scope still refuses
      `mkdir`, because systemd owns it and it is not delegated. Risk R2 is
      that missing delegation silently means no limits; N4 says it must never
      be silent. It was not silent, but the check whose job is to predict it
      was confidently wrong, which is worse. It now **attempts** it: create a
      child cgroup, remove it. The project's own first rule, applied to the
      one file that had been exempt
- [x] **The remedy it printed was incomplete.** `Delegate=` on
      `user@.service` is necessary and not sufficient; the same ssh session
      then fails identically. Both halves are printed now, the second being
      `systemd-run --user --scope -p Delegate=yes`
- [x] **`zygo doctor` offered a backend that does not exist** —
      `backends available: ns, vm` on a machine with `/dev/kvm`, while
      `backend list` said correctly that `vm` is not built. A `/dev/kvm` the
      user cannot open is now absent rather than degraded, and "can I use
      this" is answered in the CLI, which may know both halves. Fixing it
      introduced a fourth bug worth recording: `Report::supports` calling
      `backend::for_isolation` **recursed**, because the `ns` backend's
      availability check calls `doctor::run()`. `zygo doctor` exited 139 — a
      blown stack — and only inside a *working* delegated scope, because
      anywhere else the `&&` short-circuited first
- [x] **A client talking to a wedged supervisor waited for ever.** The control
      socket had no timeout at all — not in `send`, not in the greeting — so a
      supervisor with one stopped thread blocked every client for ever, which
      from the outside is indistinguishable from the machine locking up. It
      disguised both deadlocks below and cost three investigations that began
      by ruling out the host. `--timeout` did not help: it bounded the
      supervisor's budget for the work, not the client's wait for the answer,
      though the comment beside it said otherwise. Budgets are per request now,
      because the honest ones differ by three orders of magnitude — 20 min for
      `serve` (it may run `apt`), the request's own deadline plus a reply grace
      for `exec`, 30 s for everything that only reads state. Pinned by a fake
      supervisor that greets and then answers nothing
- [x] **The sh example agent mis-framed every message over 127 bytes on a
      host with `gawk`.** It writes the four-byte length with
      `awk printf "%c"`, and in a UTF-8 locale `gawk` encodes a value above
      127 as two UTF-8 bytes while `mawk` writes one. Debian's minimal image
      ships `mawk` and Ubuntu ships `gawk`, so the conformance suite passed in
      the container and failed on the Pi — on exactly the checks whose frames
      were long enough, which is what identified it. `export LC_ALL=C` fixes
      it. The agent exists to keep the "language independent protocol" claim
      honest and had been keeping it honest against one distribution;
      `zygo agent test` also reported the timeout as a raw
      `Resource temporarily unavailable`, and now names what did not arrive
- [x] **Two more waits with no end, inside the warm-up.** The launcher is one
      thread on purpose — warming is serial — so *any* unbounded wait in a
      warm-up stops the supervisor warming anything again. On the Pi its
      thread sat in `pipe_read` while every later `serve` queued behind it.
      Both waits are bounded now: **60 s** for a sandbox to reach `execve`
      (`poll` with a deadline, because a pipe has no read timeout) and
      **120 s** for an agent to send `READY` (`set_read_timeout`). Both say
      what did not happen instead of returning an I/O error. The budgets are
      far above anything real; they exist to turn "never" into "failed"
- [x] **A warm-exec secret deadlocked the supervisor.** `place_secrets` built
      the `SecretsLease` *above* an early `?`, so a failed write dropped it on
      a thread still holding the lease's own mutex — a permanent self-deadlock,
      one leaked thread per request, and a client that waits for ever. It only
      fires when the write fails, which is why root in a container never saw
      it. Fixed by creating the lease after the guard is dropped; pinned by a
      test that asserts the call *finishes* (against the old code it reports
      "place_secrets deadlocked on the error path")
- [x] **Secrets now reach a warm-exec function without privilege.** The
      sandbox's own init creates `/run/secrets` and hands the **directory
      descriptor** out over `SCM_RIGHTS`, in the one moment such a thing can
      be taken: after the root is committed, so the directory can exist, and
      before `harden`, which drops the capabilities that creating it needs and
      may install a Landlock ruleset that forbids it. The supervisor writes
      with `openat` and never touches `/proc` again. The fd-passing helpers
      were already here for the DNS socket and are shared rather than
      duplicated. Verified where it failed: on an ordinary user's Raspberry
      Pi, a `cmd` function reads its secret, and reads it again on the next
      request after the files were withdrawn
- [x] **The verification suites only knew how to run in a container.** Each
      hard-coded `/sys/fs/cgroup` as the root to build its harness under.
      `verify_launcher.sh` reported **24 failures on a host where the launcher
      worked**, all of them empty output from a wrapper that was breaking
      every invocation. The seven suites now share
      [`poc/cgroup_harness.sh`](poc/cgroup_harness.sh), which reads the
      starting cgroup from `/proc/self/cgroup` and builds relative to it — the
      same file works in a privileged container and in a delegated systemd
      scope, and says which it did

#### 4.1 What running `runsc` found that reading about it did not

Three failures, in order, each invisible to a unit test:

1. **The documented download does not exist.** gVisor's instructions describe
   `…/latest/${ARCH}/runsc`; the bucket serves `gvisor.tar.zstd`. A 404.
2. **`runsc` alone is not a runtime.** It reports its version happily and then
   fails with `sidecar "gvisor_sentry" not usable … --sidecar-usage-policy is
   set to STRICT`. The release's `gvisor-bin/` directory is not optional.
3. **`--rootless` and a user namespace in the bundle are the same request
   made twice.** `runsc` builds a user namespace itself in rootless mode; a
   spec that also declares one makes the gofer clone with `CLONE_NEWUSER`
   again and die with `fork/exec /proc/self/exe: invalid argument` — a message
   that says nothing about the cause. Found by bisecting a working baseline
   from `runsc spec` against the generated bundle, one field at a time. The
   uid still applies: the Sentry implements it.

And a fourth that was a *behaviour* difference rather than a failure: `ns`
falls back to `/` when the image lacks the configured working directory, while
an OCI runtime tries to **create** it and fails on a read-only root. So
`zygo run --isolation gvisor alpine:3 echo hi` died where the same command on
`ns` had worked. Requirement N8 is that the same spec means the same thing on
every backend, so the fallback now happens in the bundle.

---

## Phase 5 — macOS

- [x] **Shim mode on macOS.** `crates/zygo-cli/src/shim.rs`: every command
      except `doctor`, `completion` and `agent test` is run by a Linux `zygo`
      inside a VM, with the same arguments, the same working directory and the
      same streams, and its exit status comes back out. The three that stay
      here stay for a reason each, written down beside the list. The shim also
      keeps the VM's binary level with this one, comparing a stamp on the host
      rather than asking the guest, because that check runs before every
      command.
- [x] **VM lifecycle through Lima** (`shim/lima.yaml`), not a
      Virtualization.framework helper: it needs no signed binary, `brew
      install lima` is one line, and everything above the provider is
      provider-independent if that changes. Ubuntu 24.04, kernel 6.8 —
      **Landlock ABI v4 runs here for the first time in this project**.
      Measured: **51–59 s** from no VM at all to output from a Linux sandbox
      — creating the VM, provisioning it, installing the Linux binary and
      pulling the container image, all of it — against phase 5's 60 s. Add
      about 40 s the very first time, for Lima to download the Ubuntu image.
      A command against a running VM is **79 ms**.
- [x] **virtiofs: `$HOME` at the same path, writable.** That is the whole
      path contract, and it is enforced rather than hoped for — a command run
      from outside `$HOME` is refused and the message names both directories,
      because forwarding it would run against a directory that is not the one
      in front of the user.
- [x] **Command forwarding**, over Lima's ssh rather than vsock. `ps`, `serve`,
      `exec`, `run`, `stop` all work from the Mac; stdin crosses, and an exit
      status of 7 arrives as 7.
- [x] **The VM goes when the user says they are finished.** `zygo stop
      --all` means everything, and that includes the machine Zygo started to
      do it in — verified by asking `limactl`, not by believing Zygo. Not an
      idle *timer*: noticing idleness needs something running to notice it,
      and on macOS that is a launchd agent Zygo does not install. The next
      command brings the VM back in 15 s with nothing in it.
- [x] **Apple's `container`, researched — not adoptable here yet, and the
      interesting use is not the obvious one.** Apple's runtime reached 1.0 in
      June 2026. It gives each container its own lightweight VM through
      Virtualization.framework, with an optimised kernel, a `vminitd` init
      speaking gRPC over vsock, and sub-second starts. It needs **macOS 26**
      and Apple silicon; this machine is macOS 15.1, so none of it could be
      measured rather than read, and nothing below is a result.

      The obvious reading — "a per-sandbox microVM, so use it as the `vm`
      backend on a Mac" — does not fit. Zygo's warm path is a zygote forking
      per *request* inside one sandbox; a VM per container is a VM per
      function, which is the granularity Zygo already has. It would buy a
      kernel boundary per function at the cost of the thing the project is
      for.

      The use that does fit is duller and better: `container machine`, added
      in 1.0, is a persistent Linux environment that mounts the Mac's home
      directory automatically — which is exactly the contract `shim/lima.yaml`
      spells out by hand. On macOS 26 it could replace Lima as the shim's
      provider with no change above `crates/zygo-cli/src/shim.rs`'s provider
      boundary, and one fewer thing for a user to `brew install`. Worth
      revisiting when a macOS 26 machine is available; the questions to settle
      there are whether a container machine delegates cgroup v2 controllers,
      whether it permits unprivileged user namespaces, and what its start
      latency is against Lima's 90 s and 15 s.
- [ ] CI: an end-to-end test on a macOS runner — **blocked**: GitHub's hosted
      macOS runners are themselves VMs and offer no nested virtualization, so
      Lima cannot boot there. `make verify-shim` skips itself when `limactl`
      is absent and says why; the shim's own decisions are unit-tested on
      every platform, including in CI.

**Acceptance:** first run on a Mac with no VM, **51–59 s** against a 60 s
target. Second run, `zygo run python:3.12-slim python -c pass`: **117 ms**
with the VM up, against a target of 100 ms — the ssh hop is 46 ms of it, and
vsock is where that goes if it matters. `serve` warms in **104 ms** and a warm
`exec` round-trips in **96 ms**, which is the Linux number plus the hop.
`make verify-shim` is 14 checks, all passing.

Two product bugs came out of running Zygo somewhere that was neither a
container nor a test harness, and both would have stopped a first-time user on
Linux too: the client and the supervisor computed different socket paths, and
the supervisor could not delegate cgroup controllers out of a cgroup its own
client was sitting in. Both fixed, both with regression tests. See
docs/poc-report.md.

---

## Phase 6 — Ecosystem and launch

- [ ] Python SDK (`pip install zygo`), TypeScript SDK (`@zygo/sdk`)
- [ ] Further agents: Ruby (fork), JVM (CRaC); a community catalogue and
      `agent test` badges
- [ ] TypeScript binding (napi-rs)
- [ ] `zygo-core` 1.0 on crates.io + an API stability promise
- [ ] A Windmill worker plugin / PR + a benchmark blog post
- [ ] GitHub Action: `uses: zygo/run@v1`
- [ ] `zygo import compose.yml`
- [ ] Launch: a 90-second demo (Docker vs Zygo), an HN/Reddit post, a comparison page
- [ ] SemVer policy, the `SECURITY.md` process, an LTS kernel support table
- [ ] Contribution guide, a "good first issue" set, a `docs/adr/` directory

---

## After test

Source: [docs/docker-replacement-report.md](docs/docker-replacement-report.md),
the 20 September 2026 session that used Zygo in Docker's place for a day.
Every item was checked against the code before it was written down; the two
claims the report gets wrong are recorded at the end of the report itself
rather than as work.

- [x] **`zygo bench` never entered a delegated scope.** `needs_a_cgroup`
      (`crates/zygo-cli/src/scope.rs`) listed `Run`, `Serve`, `Up` and
      `Supervisor(Run)` but not `Bench`, and every bench mode warms a sandbox
      of its own — so on an ordinary systemd login the command that
      demonstrates the warm path was the first one to fail, with
      `cannot create the cgroup …/session-N.scope/zygo.slice`. `Command::Bench(_)`
      is in the list, and all three modes are in the test that pins it
- [x] **`zygo run <image>` with no command refused to run.** `--help` promised
      the image's entrypoint and cmd, `cmd/run.rs` had the code to use them,
      and the resolver never let it get there: with neither `entry` nor `cmd`
      it returned `fn.run: nothing to run`. A one-shot resolve now carries an
      empty command and `zygo run` fills it in from the image config, which is
      the only place that can read it; an image declaring neither still fails,
      naming itself. `zygo spec explain` with no function name had the same
      bug for the same reason and is fixed with it
- [x] **`-v host:guest` got an image-reference error.** `-v` is the global
      verbose flag, so `zygo run -v $PWD:/src image` handed the pair to the
      image argument and got `repository has an empty path component` —
      accurate, and pointing at the wrong thing. A reference that fails to
      parse and is shaped like a mount now answers with `--mount` instead.
      Told apart by an absolute guest path, which is what keeps `alpine:3` and
      `localhost:5000/team/app:v2` out of it
- [x] **`stop --all` printed raw `limactl` output on macOS.** `shim::stop_vm`
      inherited stdio, so a teardown that worked ended in thirty lines of
      Lima's logging with a `level=error` among them. Captured, replaced with
      one line, and kept for `-v`, where the person debugging the shim wants
      exactly those lines. A failure still shows Lima's own words: they say
      what is holding the VM open and nothing else can
- [x] **`--mem 64M` alone was refused.** Scratch defaulted to a flat 64M and
      must be smaller than mem, so one flag with nothing else said was an
      error about a field the user never mentioned. An unset `scratch` now
      follows `mem` down — `min(64M, mem/2)` — so 64M is a ceiling rather than
      a constant and the derived default can never collide with `mem` or warn
      about itself. An explicit `scratch` that will not fit is still an error,
      and now names a size that would work
- [x] **The first traceback frame belonged to Zygo.** `format_exc()` in
      `agents/python/zygo_agent.py` started at the `try:` in `run_request`, so
      a raising handler opened with `/zygo/agent.py … in run_request` — the
      runtime explaining itself before it explained the bug, on the one output
      read when something is wrong. The leading frames from the agent's own
      file are dropped, chained causes survive, and an error raised by the
      harness itself still prints in full, because there the harness is the
      answer
- [x] **`image prune` reached only layers.** Three images with 47 MB of layers
      left a 230 MB data directory: the flattened rootfs, venv and derived
      system caches were never collected, and each is larger than the layers
      it comes from. All four are reachable now and each is reported on its
      own line. The venv marker and the system record already named what they
      were built against; a flattened rootfs did not, so its completion marker
      carries the layer digests — a directory that records none cannot be
      shown to be live and costs one re-flatten to drop
- [x] **README: the headline number is not reachable from the macOS CLI.** The
      96 ms round trip was given without saying that ~100 ms of it is the hop
      into the VM, which is also what `docker exec` costs there. The README
      now says plainly that on a Mac the ~1 ms warm path is reachable through
      the HTTP API or the library and not the CLI, and that this is the shim
      working rather than failing
- [x] **A2: settled on bare metal. The default stays.** Measured on the
      Raspberry Pi — Ubuntu 23.10, kernel 6.5, aarch64, four cores, a real
      `session-370.scope` login — at a rate below the host's own capacity, so
      neither row is a CPU quota in disguise (0.49 of 1.00 cores, zero
      throttled periods). 1000 requests at 100 req/s, back to back:

      | | per-request cgroup | none | cost |
      |---|---|---|---|
      | p50 | 3166 µs | 2928 µs | +238 µs |
      | p99 | 3426 µs | 3066 µs | +360 µs |
      | max | 3673 µs | 3126 µs | +547 µs |
      | `admit` p99 | 758 µs | 176 µs | +582 µs |
      | acceptance | p99 **PASS** | p99 PASS | — |

      **The tail is not there.** On this host the per-request cgroup costs
      about 240 µs at p50 and 360 µs at p99, both configurations pass the p99
      budget, and `admit` never exceeds 838 µs. The field report's 13.6 ms and
      its `p99 FAIL` were an artefact of nested virtualisation: under Lima on
      Apple Silicon the same phase measures 11–12 ms at p99, fifteen times
      what real hardware does, because that is where a cgroup `mkdir` and
      `rmdir` per request actually costs something.

      So the roadmap's own caution was right and its guess was wrong: the
      absolute numbers did not survive bare metal, and neither did the ratio.
      Nothing to change — per-request stays the default, and it keeps
      `cgroup.kill`, which is one write to tear down a timed-out request's
      whole tree. The three options the reopening proposed are all answers to
      a problem this host does not have.

      Two things follow. `--no-cgroup` remains a measurement flag rather than
      a production one, since what it buys is 240 µs and what it gives up is
      per-request containment. And the p99 note added to `bench warm` earns
      its keep in exactly one place — a nested-virtualisation host, where it
      now explains a failure that is about the environment; on the Pi it
      correctly stays silent, because there is no tail to explain
      (`p50 FAIL` there is the board's own fork floor of 310 µs against the
      VM's 97 µs, not the cgroup)
- [ ] **Bare-metal re-run of the report's other numbers.**- [ ] **Bare-metal re-run of the report's other numbers.** The cold and warm
      comparisons against Docker have the same problem as the A2 table: both
      runtimes paid for a VM. Re-measure where neither does, and put both rows
      in the report

---

## Second test

Source: [docs/second_test.md](docs/second_test.md), a re-test of every
"After test" item against the tree of 21 September 2026 with the fixes in.
Seven of eight closed under re-test and are not repeated here.

- [x] **Nothing a user decided they were finished with could ever be
      collected.** `prune` walked four categories and found nothing, and the
      cause was upstream of the walk: there was no `zygo image rm`, and
      `Store` had no way to drop an entry, so an image could not be
      un-referenced by intent. Its layers were therefore never unreferenced,
      and neither were the venvs, flattened rootfs and system records keyed on
      the same liveness. `prune` could collect what a crash orphaned and
      nothing a person decided. `zygo image rm <ref>` now exists — `Store::remove`
      plus the command, aliased `remove`. It refuses while a warm function is
      running on the image, naming it and the `stop` that frees it, because
      that function's rootfs is those layers mounted. Derived
      `<ref>+system.<key>` images go with their base. It then runs the same
      collection `prune` runs, immediately, because `rmi` frees disk and a
      removal that leaves the bytes behind until a second command is a
      surprise. Measured end to end in the VM: a pull plus a venv is 207 MB,
      and `image rm python:3.12-slim` leaves 188 KB
- [x] **A venv nothing asks for any more lived as long as its image.** The
      report proposed keying the venv on its requirements hash; that is
      already what `venv::cache_key` does, and it is why a changed pin
      produces a *second* venv rather than why the first survives. The real
      gap was that nothing recorded use: `venv::ensure` returned on the
      marker and touched it. The hit paths for venvs and flattened rootfs now
      stamp their marker, and `prune --unused-for <duration>` collects what
      has not been used within it. Age never re-counts what liveness already
      condemns, so the reported size is not doubled. Verified in the VM: a
      venv used a second ago survives `--unused-for 1s`; the same venv, left
      alone, is collected
- [x] **`prune` now says what it kept and what a flag would take.** The line
      that would have answered the report's "the data directory only grows"
      without reaching for `du`: `kept 27.3 MB across 2 venvs, the oldest
      unused for 33 minutes` and `kept 49.4 MB of compressed blobs beside 6
      unpacked layers`. The size has to be visible before the flag is chosen,
      or `--unused-for 7d` is a guess
- [x] **The compressed blob beside every extracted layer is now optional.**
      49.4 MB of the store on this host, and read exactly once — to unpack
      its layer. Every path afterwards works from the unpacked directory, and
      `pull` treats a present layer as cached without looking for the blob.
      `prune --blobs` drops them, costing one download if a layer directory
      is ever lost by hand; keeping them by default is what containerd does.
      Manifest and config blobs are not layers and are never offered
- [x] **`stop --all` on a stopped VM booted the VM to stop nothing.** The
      report saw the narration; the cause was worse. `shim::forward` called
      `ensure_running` before every forwarded command, so a stop against a
      stopped machine spent sixteen seconds building one, told a supervisor
      that does not exist to stop functions that do not exist, and stopped it
      again. `stop`, `stop --all` and `down` now answer `nothing is running;
      the Linux VM is stopped` and start nothing. Measured: 16 s → 48 ms.
      `make verify-shim` is 15 checks now, the new one timing a second
      `stop --all` rather than reading its output, because the output was
      already plausible while the behaviour was not
- [x] **`bench warm` now says when its own failing p99 is about A2.** Not a
      defect in the scope fix; a consequence of A2 that greets everyone who
      runs the command the README points at. When `admit` and `release` own
      most of the p99 *and* the p99 missed its budget, the run prints what
      share of the tail is the per-request cgroup rather than the handler,
      points at A2, and names `--no-cgroup` as the measurement without it.
      The same duty as the existing CPU-quota note: a reader must not
      conclude the runtime is slow from a number that is about an open
      decision. Silent on a run that passes. Measured at 78% on this host
- [ ] **A2 still has to be settled, and the acceptance line has to end up
      true.** Tracked under "After test"; repeated here only because the
      second pass is right that whatever is chosen must leave `bench warm`
      passing on a default configuration, or the budget itself has to change.
      A note explaining a failure is a stopgap, not the answer

---

## Tested against a real consumer

Source: running Zygo as the script sandbox for
an early adopter — a Phoenix application whose whole purpose is executing
somebody else's `def main(event)` in a box. It already has two such boxes
behind one behaviour (its runner interface): CPython compiled to
WebAssembly, and `docker run --rm`. Zygo is now a third driver written to
that same behaviour, so the adopter's own driver suite runs against it unchanged
— the same assertions, the same harness, the same event payload, for all
three boxes. Its its Zygo driver is the consumer; nothing
in it is written to flatter Zygo.

The workload is not synthetic: a scratch directory mounted read-write, the
event in as a file, the result out as a file, stdout left to the script,
`--mem/--cpu/--pids/--scratch/--timeout`, `--net none` by default and
`--net egress --allow` for a project's allowed hosts.

### Round 1 — every test failed on one line

- [x] **A `--mount` outside `$HOME` was forwarded to the VM instead of being
      refused.** 8 of 8 Zygo tests failed identically with `applying a bind
      mount from the spec failed: No such file or directory (os error 2)` and
      a pointer to `zygo doctor`, which has nothing to say about it. The adopter
      puts its scratch directory in the system temporary directory, which on
      macOS is under `/var/folders/…` and does not exist inside the Lima VM.
      Reduced to two lines:

      ```
      zygo run --mount /var/folders/…/tmp.X:/data:ro alpine:3 cat /data/f.txt
      → error: applying a bind mount from the spec failed: No such file or directory
      zygo run --mount $HOME/tmp.X:/data:ro         alpine:3 cat /data/f.txt
      → hello
      ```

      The shim already had this rule and applied it to one of the two things
      that need it: `workdir` refuses a command run from outside `$HOME` on
      the stated grounds that forwarding it "would silently run against a
      directory that is not the one the user is looking at", which is exactly
      what a forwarded mount does. `unmapped_mounts` now applies the same
      check to every `--mount` source of `run` and `serve`, resolving
      relative paths against the caller's directory first, and the refusal
      names the path, the rule and `TMPDIR`. The guest never sees the command

### Round 2 — the suite, green

Driver suite: 21 passed. Proved non-vacuous rather than trusted, because a
test guarded by `available?()` passes when the driver is absent: a handler
returning `platform.python_version()` answers `3.12.14 on linux` from a Mac
in 284 ms, with the script's `print` coming back beside the result.

### Round 3 — parity with the Docker driver

No defects. Eight events at once all succeeded; a script's stderr, a 1 MB
result (8000 rows in 175 ms), writing to the scratch `/tmp`, and `OSError`
on a write to the read-only image root all behaved as the container driver
does. Secrets appeared to be missing until both drivers were asked the same
question and gave the same answer: they arrive as a `secrets` dict in the
script's scope rather than through the environment, which is the harness's
design and not a driver's business.

### Round 4 — what Zygo is for

No defects, and the two claims hold against a real consumer:

| | Docker driver | Zygo driver |
|---|---|---|
| `python:3.12-slim`, one event | 516 ms median | 203 ms median |
| egress allowlist | needs a proxy the app runs | `--net egress --allow` |

With `network: "allowlist"` and one allowed host, `https://example.com`
answered 200 and `https://api.github.com` raised `URLError`, enforced in the
sandbox's own namespace with no proxy process anywhere.

### Round 5 — leaks under sustained load

50 events at a concurrency of 5 finished 50/50 in 3.15 s, and the VM was
unchanged afterwards: no zygo processes, no stray cgroups, no new mounts, no
disk growth. One defect, found by looking rather than by failing:

- [x] **A failed `zygo run` leaked its sandbox root directory.** `cmd/run.rs`
      created `tmp/root-<pid>` for the `pivot_root` target and removed it in
      one line before its `Ok` — so a run that failed after the directory was
      made left it behind, and *only* a failing run ever did. Sharpened to a
      measurement before it was believed: five failing runs left five
      directories, three succeeding runs left none. An adopter found it by
      having tests that fail on purpose. The directory is now an RAII guard,
      removed however the function leaves, and `remove_dir` rather than
      `remove_dir_all` so a mount that outlived its namespace keeps its
      contents instead of being deleted through. Re-measured after the fix:
      five failing runs, delta zero
- [x] **`prune` collects abandoned sandbox roots.** 31 had accumulated on this
      host before the fix and nothing would ever have taken them. The pid is
      in the directory's name, so liveness is answerable without bookkeeping;
      a root whose pid is still running is left alone, because skipping one
      costs a wait and being wrong the other way pulls the ground out from
      under a live sandbox. All 31 collected

### On real hardware

Run on the Raspberry Pi rather than in the Lima VM, because GitHub Actions is
out of quota and because two of the open items needed a kernel on metal. The
musl build and the suites were synced over; `doctor` reports user namespaces,
delegated cgroup v2, overlayfs, seccomp, `subuid` and `pasta` all present,
Landlock absent on 6.5 as expected.

| Suite | Result |
|---|---|
| `verify_launcher.sh` | 30 passed, 0 failed |
| `escape_suite.sh` | 16 blocked, 0 escaped |
| `fuzz_syscalls.sh` | 13 passed, 0 failed (469 syscalls × 3 profiles) |

The delegated-scope fix is confirmed where it matters: the login sits in
`session-370.scope`, undelegated, and `zygo bench warm` runs there without a
wrapper. And A2 is settled — see "After test" above; the tail that reopened
it does not exist on this host.

### Round 6 — the rest of the surface

No defects. TypeScript runs on `node:22-alpine` through the same driver
(`console.log` comes back as output, the handler's return as the result),
and a project's installed packages mount read-only with `/packages` on
`sys.path` — `requests 2.34.2` imported from it.

---

## Code health — from the 2026-09-21 review

Source: [docs/code_check.md](docs/code_check.md). A read-only pass over the
whole tree at `945e41a`; the IDs below are the report's. Nothing here changes
behaviour for inputs that work today — each item fixes a path that is wrong
today, or removes a way for the next change to go wrong. Every fix lands with
the test that would have caught it.

### Fixed (P0 — latent bugs and hygiene)

All of them, each with the test that would have caught it. Where the test is
the interesting part it is named; three of them were checked against the
original code first, and each failed there and passes now.

- [x] **B-01** A partially failed secret write left files in `/run/secrets`
      and every later request on that function failed with EACCES: the
      directory was recorded *after* the loop, so `unlink_all` had nothing to
      aim at. Recorded before it now, a failed write undoes what it managed,
      and `O_TRUNC` became `unlinkat` + `O_EXCL` — the old comment blamed "a
      file from a previous generation", which a fresh tmpfs never has; the
      real culprit was a 0400 file the supervisor cannot reopen for writing
- [x] **B-02** `reject_foreign_peer` failed **open**: the syscall and the
      policy shared one `?`, so "could not read the credentials" and "the
      credentials are ours" returned the same answer. Split into
      `peer_verdict(ours, peer)`, which refuses `None`, and tested for all
      three inputs — a socket on which `SO_PEERCRED` genuinely fails cannot be
      conjured from a process that owns both ends
- [x] **B-03** `F_DUPFD` and `dup2` both *clear* close-on-exec, so a warm-exec
      request inherited all thirteen renumbered descriptors — seven namespace
      descriptors among them — past `execve`, exactly opposite to what the
      comment above them promised. `F_DUPFD_CLOEXEC` + `dup3(O_CLOEXEC)`, and
      the failure report now goes to the *parked* error pipe rather than to
      the descriptor being duplicated. Escape suite case 16 attempts it
- [x] **B-04** The unpack path was hardened against hostile tars and the
      *flatten* path was not: `create_dir_all` follows a lower layer's
      symlink, and `Path::exists` is false for a dangling one, so both
      branches wrote through. Every decision now uses `symlink_metadata` and
      removes what is there first; the `hard_link` fallback narrowed to the
      errors that mean "links unsupported". Two tests, both confirmed against
      the old code: one plants `etc` as a link outside the store, one leaves a
      dangling link in the destination
- [x] **B-05** The registry client never compared what it was served with what
      it asked for. The body is hashed every time and checked against the
      header, `reference.digest` and the index descriptor; every digest in a
      manifest is parsed before it reaches a URL. Tested against
      `tests/registry_client.rs` — about a hundred lines of `std::net` that is
      enough to be a registry — which also covers the 401 → token → retry
      path for the first time, and that a token is reused rather than fetched
      per blob
- [x] **B-06** `make verify-login-linux` could not fail: the `-` prefix
      discarded the suite's exit status and CI ran the target verbatim. The
      status is captured before the cleanup now, and `sleep 3` became
      `poc/wait_for_registry.sh`, which polls the endpoint the suite is about
      to use
- [x] **B-07** `is_fn_name` at the top of `resolve_layer`, so every entry
      point gets it: `cgroup::sanitise` covered the cgroup and `tenant_data`,
      `agent_sock` and the pasta pid file joined the raw name, while
      `[fn."../x"]` is legal TOML
- [x] **B-08** `Drop` cannot restore a terminal under `panic = "abort"`,
      which is the release profile — the `catch_unwind` test passed only
      because the test profile unwinds. A panic hook restores from a registry
      now, and the new test reaches the same path through `mem::forget`,
      which *is* "no destructor ran". `prompt_password` grew a Drop guard and
      a SIGINT handler that turns echo back on and re-raises; it reads the
      current settings rather than a saved copy, so it needs nothing shared
      with the handler
- [x] **B-09** The Python spawn fallback took the next frame and demanded it
      be `GO`; anything else killed the worker and answered **nothing**, so
      the supervisor waited out the whole deadline. `zygo agent test` sends
      two `EXEC`s before any `GO`, so every handler that fell back failed
      conformance for this. `_await_go` now answers `overloaded`, `PONG` and
      `bad_message` and loops to this request's `GO`. Confirmed against the
      old code, where the new test times out
- [x] **S-01** Four types derived `Debug` over a password. Hand-written impls
      with a `<redacted>` marker, and a test that formats each and looks for
      the secret in the output — a field added later gets a derived `Debug`
      for free, and nothing else would notice
- [x] **S-02** `connect_timeout` and `read_timeout` on the registry client: a
      registry that accepted a connection and then said nothing hung `pull`,
      and so `run` and `serve`, for ever. Per-read rather than per-request, so
      a large layer on a slow link still completes
- [x] **S-03 / S-04** The ready pipe is `pipe_cloexec` (it stayed open across
      the spawns of `newuidmap`, `nft` and the long-lived `pasta`), and
      `recvmsg` takes `MSG_CMSG_CLOEXEC` so the `/run/secrets` descriptor is
      close-on-exec from the moment it exists rather than after a window in
      which another thread can fork
- [x] **C-01** `rust-version = "1.88"`, which is what let-chains need. Raising
      it enabled three clippy lints that the false floor had been suppressing
      (`collapsible_if` ×2, `manual_is_multiple_of`); all three applied
- [x] **T-04** `cargo test -p zygo-core --no-default-features` compiles: the
      `CredentialStore` sweep is gated on `feature = "registry"`. Added the
      DNS wire-parser sweep the report asked for — the only parser that reads
      raw bytes a *tenant* sent, in a process that serves every other function
      on the host — which promptly caught **B-16** below
- [x] **E-11 / C-03** `parse_digest` rejects upper-case hex, which `write_blob`
      could never verify; `tempfile` is no longer listed as both a dependency
      and a dev-dependency of `zygo-cli`

### Then (P1 — robustness, ceilings, drifted policy)

- [ ] **B-10** N concurrent requests on a crashed function queue N warm-ups;
      re-check the registry under the lock and hold a per-name "warming" flag.
- [ ] **B-11** `tier_idle` can freeze a function between `resume()` and the
      permit; resume after the permit, refuse `pause` while in flight.
- [ ] **B-12** When the agent's pid cannot be translated no request cgroup is
      created and a timed-out request is never killed; do what the comment
      says or refuse before `GO`. (`pool.rs:1188`)
- [ ] **B-13** `read_status` blocks with no deadline after `collect_request`
      gave up; poll with the remaining grace.
- [ ] **B-14 / B-15** `wait()` on a zero-timeout sandbox SIGKILLs it after
      5 s; two `launch` error paths `reap` a live child instead of `kill`.
      Then split `launch` (R-01) so the next error path cannot miss it.
- [x] **B-16** ANCOUNT was written as `answers.len()` while the loop stopped
      at the packet limit, so a name with ~28 addresses produced a header
      promising records that were not there — a resolver reports that as a
      failed lookup, not a short one. The list is truncated before the header
      is written. The truncation bit is deliberately *not* set: it tells a
      client to retry over TCP and this resolver is UDP-only, so obeying it
      would mean no answer instead of a usable subset. Found by the new DNS
      sweep, which fails on the old code with "the header promises 80 records
      and the body holds 464 bytes"
- [ ] **B-17** Bare IPv6 literals in `allow` are mis-split on the last colon;
      accept `[v6]:port`. (`spec/types.rs:634`)
- [ ] **B-18** One `is_private(IpAddr)` for `types.rs`, `dns.rs` and the
      nftables sets; today `100.64/10` passes resolve and is dead in the
      ruleset.
- [ ] **B-19** Normalise mount targets before the reserved-path check and
      compare against the launcher's own `MANAGED_TARGETS` (`/tmp`, `/run`
      are missing today).
- [ ] **B-20 / B-21 / B-22** `write_blob` temp file left behind on error and
      collides across threads; a corrupt `index.json` is silently emptied
      then overwritten; prune can delete a layer mid-unpack.
- [ ] **B-23 / B-24 / B-25** Python agent: `GO` barrier accepts EOF; an
      oversize result kills the agent instead of `bad_result`; the forked
      child can unwind into the parent loop (`finally: os._exit`).
- [ ] **B-26** Node agent finalises on `'exit'`; use `'close'`, and
      `128 + signals[signal]`.
- [ ] **B-27 / B-28** macOS shim: guest-binary stamp survives
      `limactl delete`; the environment (`--secret`, `ZYGO_API_TOKEN`, …) is
      not forwarded and signals do not reach `limactl shell`.
- [x] **B-30** `zygo_api_errors_total` counted in the `Err` arm *and* again on
      the status it produced, so the error rate on a dashboard was twice the
      truth. Counted once, from the status actually sent
- [ ] **B-29** Exit-code mapping walks only the outer error, so any
      `.context()` wrapper turns the library's 2/125 into 1.
- [x] **S-06..S-09** All four ceilings, each a number that was absent and each
      absent in the same shape — a request that costs the *server* more the
      larger the caller makes it. `MAX_BATCH` (1024) with a
      `BATCH_IN_FLIGHT` semaphore, so one caller cannot take every supervisor
      connection; `MAX_IDLE_CLIENTS`, because each idle one pins a supervisor
      thread and the pool only ever grew; `MAX_TIMEOUT_MS`, refused rather
      than clamped so a caller is never told it waited for what it asked for;
      and a `TokioTimer` on the hyper builder, without which
      `header_read_timeout` is accepted and silently ignored. A panicking
      batch element is now that element's failure rather than the whole
      batch's — which is the one thing a batch exists to prevent
- [ ] **E-01** A panicking warm-up job kills the launcher thread for the
      supervisor's lifetime; `catch_unwind` around `job()`.
- [ ] **E-02** `apt`/`pip` build failures are reported as
      `BackendUnavailable` (exit 125); an `Error::Build` (exit 1) — decide on
      the exit code first, it is visible to scripts.
- [ ] **E-03** Drop `#[from]` on `Error::Bare(io::Error)` so a bare `?` no
      longer compiles without a path; give `ImageError::Unpack`/`Registry` a
      `#[source]`.
- [ ] **E-14** `--json` output of `up` and `image rm` is several documents on
      one stdout, and some failures print nothing in JSON mode.
- [ ] **S-13** CI: pin actions by SHA, top-level `permissions: contents:
      read`, `timeout-minutes`, a `concurrency` group.

### When the file is next open (P2 — structure, duplication, dead code)

- [ ] **R-01** Split the seven functions over 175 lines: `resolve_layer`,
      `child::apply`, `serve_with_logs`, `cmd::supervisor::up` (move the lock
      policy into `zygo-core::lock`), `ns::launch`, `cmd::run::run`,
      `call_timed`.
- [ ] **R-02** `WarmFn`/`WarmExec` share nine methods verbatim; one `Common`.
- [ ] **R-03** `Message::Result`/`Done` share seven fields; one payload
      struct, fixtures pin the wire.
- [ ] **R-04** venv/derive build scaffolding → one `oneshot::Build`.
- [ ] **R-05** Stop running the full `doctor` to learn "overlayfs in userns"
      (four call sites, matched by display name); memoised
      `doctor::overlay_in_userns()`, `pub const` check names, one egress name.
- [ ] **R-06** Collapse the helpers written two to seven times (`which`,
      `is_timeout`, request-id generator — and make `zygo logs` ids equal the
      protocol ids as the comment claims — `pipe_cloexec`, `Platform`
      display, `digest_of`, NaN-safe sort, "no supervisor" fallback, …).
- [ ] **R-07 / R-08** One `block_on` helper instead of six runtimes in the
      CLI; `image_config` sync on `Store`; `spawn_blocking` around
      `unpack_layer` in `pull`.
- [ ] **R-09 / R-10** `Store::skeleton` gets a per-key lock, a `Paths`
      accessor and a prune entry; split `store.rs` into store / unpack /
      flatten.
- [ ] **R-11** Duplicate the namespace descriptors at serve time so
      `WarmExec::call_timed` no longer holds the sandbox mutex across `enter`.
- [ ] **R-16** `poc/lib.sh` for the nine copied preludes; `mktemp -d` +
      `trap` per suite instead of shared `/tmp` paths; one `POC_RUN` in the
      Makefile; one prepare step in CI.
- [ ] **D-01..D-03** Delete the unused items (`agent_failure`,
      `WarmupTiming`, `Supervisor::load`, `locate_field`, `_read_result`,
      `absolute()`, …), the never-sent `Step` variants, the unreachable
      seccomp instructions.
- [ ] **D-04** Rewrite the twenty-odd comments that describe the previous
      design (list in the report).
- [ ] **D-07** Hide `exec --batch` until it exists.
- [ ] **T-02 / T-03 / T-08** Tests for the reply demultiplexer, `enter.rs`
      and `wait_within`, and the HTTP router; a `Function::Fake` arm for the
      tiering transitions.
- [ ] **T-05 / T-06 / T-07** Inject the resolver in `net` tests; assert
      `defaults()` fills every field `resolve_layer` expects; one
      `Limits::for_tests()`.
- [ ] **C-02** A `lint` CI job: `cargo deny`, `shellcheck`, `ruff`.
- [ ] **C-03 / C-04 / C-05** `tempfile` listed twice in `zygo-cli`; Makefile
      `.PHONY`/`help` gaps and the unconditional `-t`; one build profile in
      the `unit` job.
- [ ] **X-01** The counts in README, todo.md, Makefile and SECURITY.md
      disagree (151/157 supervisor, 14/15 shim, 27/37 Python, 5.3/5.15
      kernel floor). Have the suites print their totals; quote those.
- [ ] **X-02..X-07** `PING` liveness is promised by the spec and not sent;
      SUMMARY.md links outside the book root; ported allow rules are
      TCP-only and undocumented; kernel series table ages silently; the
      Python floor is stated nowhere; `fn.run.mem` in error paths.

## From the use-case pass

Three things the use-case walk-through found, all of them now closed.

- [x] **`zygo run --requirements` did not exist**, and `examples/ci-job`
      documented it. Marked as a documentation defect; it was a missing
      feature the documentation had already promised, and the workaround it
      forced — installing packages into a directory and bind-mounting it —
      rebuilt them per job. Implemented against the venv cache `serve`
      already uses, so the two share: same key (image digest + the file's
      bytes), same `/venv` mount, same read-only guarantee. The image's own
      `PATH` is kept, with `/venv/bin` in front of it rather than instead of
      it — `Venv::env_over`, because the warm path has no image config to
      hand and uses the built-in default
- [x] **A time-limit kill and a memory-limit kill were both exit 137** at the
      CLI, so a judge could not tell them apart. They still are, because both
      are `SIGKILL` and the wait status has nothing else — so the *reason*
      now travels out of band. `zygo run --outcome PATH` writes
      `{"exit_code","timed_out","oom_killed","peak_rss_kb","wall_ms"}`:
      `timed_out` from the launcher, which enforced the deadline, and
      `oom_killed` from `memory.events` in the sandbox's own cgroup, read
      after it exits and before the directory is removed. Out of band because
      standard output belongs to the program
  - [x] `POST /run` reports all three, which is what UC6 asked for; both SDKs
        parse them; the MCP server says "ran out of memory" and "exceeded its
        time limit" in different sentences, because a model told "exit 137"
        cannot know which of its two problems to fix
  - [x] The exit code is still taken from the wait status and never from the
        file — a child killed after writing one would otherwise report the
        success it had written
- [x] **A reproduction for the one open supervisor question.**
      `poc/repro_blue_green.sh` does only the queued-request-during-a-replacement
      scenario, five times by default, and prints the timings of each round:
      how long `up` took, how much of the queue window was left, how long the
      queued request waited. A round where `up` outlasted the window is
      reported as *inconclusive* rather than counted either way, which is the
      distinction the single check inside `verify-supervisor-linux` has to
      make in one shot. `make repro-blue-green-linux`

## Open decisions (design document, section 6)

| # | Question | Status |
|---|---|---|
| A1 | Should the supervisor be a systemd unit or a user process? | Decided: a user process on first run; a unit via `zygo install-service` |
| A2 | A cgroup per request, or one per tenant? | Support both; per request by default. **Reopened**: the field report measured the per-request cgroup at +645 µs p50 and +13.6 ms p99 under sustained load, enough to fail the p99 criterion on its own — see "After test" |
| A3 | JSON or MessagePack for the protocol? | Start with JSON, measure at 100 KB+ payloads |

## Open, with evidence

**A request queued behind a function being replaced ran on the old one.**
Seen once, on the Raspberry Pi, by the check written to tell that apart from
a slow host:

```
FAIL  the queued request ran on the old function although the replacement
      was ready 402 ms in, with 7400 ms of queue left
```

`Supervisor::serve` documents the opposite — "requests the old one accepted
finish on it, and requests that were queued behind it are admitted to the new
one" — and the code reads as though it holds. `retire` closes the old gate
immediately after the registry swap; `Gate::close` sets `closed` and
`notify_all`; a waiter's predicate is `!closed && in_flight >= limit`, so it
wakes, sees `closed`, and the caller's loop looks the name up again and gets
the replacement. Every ordering I can trace ends on the new function.

The premises check out: `concurrency = 1`, the handler really does
`time.sleep(event["sleep"])`, and the default timeout is 30 s, so the 8 s
in-flight request was not killed early and did hold the only slot. The
container passes this check every time.

What is missing is a reproduction tight enough to instrument, and each attempt
on that machine is a thirty-five minute run. Left open rather than guessed at.
The check stays as it is: it asserts the documented behaviour, it distinguishes
that from `up` being slow, and it will say so again.

## Risks (tracked)

`R1` fork+thread deadlock · `R2` no cgroup delegation · `R3` old-kernel
overlayfs · `R4` kernel CVEs · `R5` layering guesswork · `R6` virtiofs import
cost · `R7` name collision · `R8` languages other than Python · `R9` the Docker
ecosystem · `R10` the "namespaces are not enough" perception · `R11` the
maintenance burden of agents — see
[ahmed.md](ahmed.md#6-riskler-ve-açık-sorular) for the detail.
