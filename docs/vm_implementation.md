# The `vm` backend: implementation roadmap

Date: 21 September 2026. Written against the working tree at `945e41a` plus
the uncommitted API work, after reading the backend, pool, network and
protocol code rather than the design alone. The test host is the Raspberry
Pi 5 at `192.168.1.32`, whose KVM was exercised the same day: an ordinary
user opened `/dev/kvm` and `KVM_CREATE_VM` returned a VM descriptor.

This is a plan with milestones that each end in a measurement, in the style
of [todo.md](../todo.md). Where a decision rests on something not yet
measured, the milestone that measures it is named and the decision is marked
*provisional*.

## 1. Where things stand

What already exists for `vm`, and is worth not rewriting:

| Thing | Where | State |
|---|---|---|
| `Isolation::Vm` | `spec/types.rs` | Parses, resolves, has no vm-specific validation |
| Backend placeholder | `backend/mod.rs::for_isolation` | Returns `Unimplemented` with a reason and a remedy |
| `zygo backend install vm` | `cmd/backend.rs` | Refuses with "linked into the binary, not downloaded" |
| `doctor` check | `doctor.rs::kvm()` | Attempts to open `/dev/kvm` read-write; absent when it cannot |
| Data directory | `paths.rs` | Reserves `krun/` for backend artefacts |
| Protocol transport | `spec/protocol.md` §1 | Names vsock for `vm`; frames carry no descriptors |
| Agent start-up | `agents/python/zygo_agent.py` | `socket.socket(fileno=3)` adopts whatever family fd 3 is |
| The template | `backend/gvisor.rs` | A non-`ns` backend on the same trait, with the refusals spelled out |
| The comparison harness | `poc/verify_gvisor.sh` | Same command on `ns` and on the other backend, compared (requirement N8) |
| Prior research | `docs/firecracker-snapshots.md` | Snapshots as a v2 candidate, behind this backend |

What does not exist: any libkrun code, PoC 8 (libkrun boot and virtiofs
import cost, "could not be run: no `/dev/kvm`"), and the acceptance row
"warm request < 3 ms on `vm`", blocked on the same. CI's `pending-hardware`
job prints `KVM … ABSENT` on every run and says "KVM is the one with no home
yet". It now has one.

## 2. The test host

Verified on 21 September 2026 over ssh as user `m`:

| | Value |
|---|---|
| Machine | Raspberry Pi 5 Model B, Cortex-A76 (part 0xd0b), 4 cores, 8 GB, no swap |
| OS / kernel | Ubuntu 23.10 (EOL), `6.5.0-1007-raspi`, aarch64, 4 KiB pages |
| KVM | `/dev/kvm` group `kvm`, user is a member; API 12, up to 8 vCPUs, 40-bit IPA; `KVM_CREATE_VM` succeeds unprivileged |
| Also present | `/dev/vhost-vsock` (group `kvm`), `/dev/net/tun`, `pasta`, `nft` |
| cgroup v2 | `cpu memory pids` delegated to `user-1000.slice` |
| `zygo doctor` | everything ok except `kernel age` (degraded, 3 years) and `landlock` (unavailable); `kvm /dev/kvm ok` — the first host in this project where that line is green |
| Toolchain | none: no gcc, cargo, make, cmake, patchelf. Docker 24.0.7 and git are there |
| Disk | 3.2 GB free of 14 GB on the SD card |
| Existing Zygo | `~/zygo/` with `poc/zygo-linux-musl`, `data/`, the examples; the earlier test rounds ran here |

Two consequences shape the whole plan:

* **The Pi is the test target, not the build host.** Nothing is compiled on
  it. The binary is built on the Mac in a container, the way
  `make poc/zygo-linux-musl` already does, and copied over with `scp`. That
  is what removes the disk and toolchain problems rather than solving them.
* **The Pi's own kernel is exactly the case `vm` is for.** `doctor` says the
  6.5 series is out of support and that Landlock is missing, and its remedy
  is "upgrade, or run untrusted code on `vm`". Under `vm` the sandbox runs
  on libkrunfw's guest kernel, which is a recent mainline: whether Landlock
  and `cgroup.kill` appear *inside* the guest on a host that lacks them is
  one of the first things M1 checks, because if they do, the backend
  improves this host twice.

Housekeeping before the first run: a `zygo-linux-musl supervisor run` and a
`zygo-linux-musl up` from an earlier round were still alive at the time of
writing (21 and 13 minutes old), so another session may be using the
machine; check `pgrep -a zygo` before starting and do not kill what is not
yours. The disk should still be widened: a USB SSD or an NVMe HAT is the
proper fix, and the two-year-old `nextcloud` and `postgres` Docker images
hold about 2 GB if they are unused, which is a question for the owner.

## 3. What `vm` is, in one paragraph and one diagram

libkrun is linked into the `zygo` binary as a library. A sandbox is a child
process that calls `krun_start_enter` and *becomes* the VMM: it never
returns, so the fork before it is what `clone3` is to the `ns` backend. The
guest boots libkrunfw's kernel, gets its root filesystem from the host over
virtiofs, and runs a guest init that is Zygo itself. That init sets up
inside the guest what `ns` sets up on the host, then execs the agent or holds
for warm-exec. The supervisor talks to the agent over a vsock port that
libkrun bridges to a host unix socket, so the host side of the protocol does
not change.

```text
host                                        guest (libkrunfw kernel)
────────────────────────────────────────    ──────────────────────────────────
supervisor
  │ fork
  ├─► child: enters tenant cgroup,          zygo guest-init  (pid 1 after libkrun's init)
  │          enters Zygo's netns,             ├─ mounts: / (virtiofs, ro), /tmp (tmpfs = scratch),
  │          krun_* setup,                    │          /run/secrets (virtiofs tag, per sandbox),
  │          krun_start_enter ──── KVM ────►  │          /proc /sys /dev, the spec's bind mounts
  │                                           ├─ guest cgroup v2: zygote/, request/<id>/
  │                                           ├─ seccomp + Landlock, from the same SandboxConfig
  │  unix socket ◄── libkrun vsock bridge ──► ├─ control port 3 → fd 3 → execs the agent
  │  (agent.sock, unchanged)                  └─ control port 4: admit / kill / exec / shell
  │
  ├─ pasta + nft + resolver on the netns  (unchanged; the VMM's sockets live there)
  └─ tenant + generation cgroup around the VMM (unchanged; freeze, kill, memory.peak)
```

## 4. Decisions

Each one names what in the current code it rests on. The ones marked
*provisional* are settled by a measurement in the milestone named.

**D1. Fork, then `krun_start_enter`; the VMM process is the sandbox.**
`NsSandbox` is a pid, a state and a cgroup; the trait says the kernel owns
the process. A `VmSandbox` is the same three things where the pid is the
VMM's. `kill()` is `SIGKILL` to that pid, which ends the guest with it;
`wait()` reaps it; the exit code comes back over the control port because
libkrun's own exit status is the VMM's, not the workload's.

**D2. The VMM lives inside the `ns` backend's cgroups and network
namespace.** Nothing in `cgroup.rs` cares what kind of process it holds:
`create_tenant`, `create_generation`, `attach`, `freeze`, `kill`,
`peak_memory` all apply to the VMM unchanged, and `tier_idle` pausing a
tenant freezes the whole guest, which is exactly right. For networking, the
launcher today builds a user+net namespace and hands its *paths* to
`pasta`, runs `nft` inside it with `setns`, and binds the resolver's socket
inside it through a forked helper. A VMM forked inside that same namespace
sends every packet through it: with libkrun's default TSI networking the
guest's `connect()` becomes a host `connect()` issued by the VMM process,
so the allowlist, the private-range refusal and Zygo's own resolver apply
without a line of guest-side network configuration. *Provisional*: M4
measures TSI against a `passt` descriptor (`krun_set_passt_fd`) on the same
namespace and keeps whichever passes the N8 network checks; the namespace
placement is the decision, the guest transport is the detail.

**D3. The root filesystem is a flat directory over virtiofs, first.**
`krun_set_root` takes one directory. `gvisor.rs::flat_rootfs` already asks
the store for `RootfsView::Flat` and the store already keys a flattened
rootfs on layers plus mount points, so M1 uses that path and pays what the
overlayfs fallback pays: disk and first-run time. The design's R6 risk is
that virtiofs makes imports slow; PoC 8 in M0 puts a number on it. If the
number is bad, M3 exports each layer as its own virtiofs tag and lets the
guest kernel build the overlay, which restores layer sharing and is the
shape the design's "overlayfs view over virtiofs" line describes. That is
an optimisation with a measurement in front of it, not the first cut.

**D4. The guest init is `zygo` itself, running the `ns` code inside the
guest.** The static musl binary runs in any guest rootfs; it is shared in
on a read-only virtiofs tag and named to `krun_set_exec`. A hidden
subcommand `zygo guest-init` receives the `SandboxConfig` (serialised on the
kernel command line or a small virtiofs file) and applies what
`ns/child.rs` applies: mounts from the same `MountPlan`, `/tmp` as a tmpfs
sized to `scratch`, the seccomp filter, the Landlock ruleset, a cgroup v2
tree for the zygote and per request, rlimits, uid. This is the §3.9 line
"the agent applies the same restrictions inside the VM", made structural:
the escape suite's checks run inside the guest with the same code, and a
control that `ns` has is not one `vm` forgot. The guest needs no `newuidmap`
and no user namespace: it is root in its own kernel, and that is the point.

**D5. The host pool keeps its protocol socket; request admission moves into
the guest behind one small trait.** `krun_add_vsock_port(port, path)` makes
a guest vsock port appear as a host unix socket, so `WarmFn` keeps speaking
`READY`/`EXEC`/`FORKED`/`GO`/`DONE` on `agent.sock` exactly as now. What
cannot cross the boundary is what the pool does *between* `FORKED` and
`GO`: `host_pid_of` reads `/proc/<host>/status`, `admit` writes
`cgroup.procs`, `enforce_deadline` writes `cgroup.kill`. The pool gets a
`RequestControl` trait with `admit(id, pid)`, `kill(id)` and
`release(id)`; the `ns` implementation is the code that exists today,
moved; the `vm` implementation sends the three verbs on the second vsock
port and `guest-init` performs them against the guest's cgroup tree, where
the agent's pid is native and needs no translation. `Sandbox` gains
`fn request_control(&self) -> Option<&dyn RequestControl>`, `None` meaning
the host path. The timeout keeps two layers: the guest-side `cgroup.kill`
is the precise one, and the host-side deadline on the VMM is the backstop
that a wedged guest cannot defeat.

**D6. Secrets are written by the host into a per-sandbox directory that is
the guest's `/run/secrets` over virtiofs.** `write_secrets` already ends in
one directory descriptor and creates files relative to it; the descriptor
today comes from `/proc/<pid>/root` or from a socket the held init handed
out. For `vm` it comes from a directory the backend created under the
runtime dir at mode 0700 and exported as a virtiofs tag mounted at
`/run/secrets`. `Sandbox::secrets_dir()` returns it; nothing in the pool
changes, the value still never touches the control socket or the agent's
memory, and `unlink_all` removes the files through the same descriptor.
*Provisional*: M3 tests that a file written on the host is visible in the
guest before `GO` is sent, because virtiofs attribute caching can delay
that; libkrun's virtiofs cache mode is the knob, and the test is
"write, `GO`, read, assert", not a setting.

**D7. Memory: the guest's RAM is `mem` plus a kernel allowance, and the host
cgroup holds the VMM.** `krun_set_vm_config(vcpus, ram_mib)` sizes the
guest; the tenant cgroup's `memory.max` is set to that plus the VMM's own
overhead so a guest cannot exceed what the spec said by way of the VMM's
page cache. `cpu` maps to `cpu.max` on the host, which throttles the VMM
and therefore the guest. `pids` is enforced inside the guest. What the
allowance and the overhead are is measured in M2, not guessed;
`docs/poc-report.md` gets the numbers.

**D8. The kernel is a downloaded artefact; libkrun is linked statically;
libkrunfw is not linked at all.** This is the todo 2.5 item "static linking
of libkrun + libkrunfw (licence/size)", and the answer to both halves is the
same: `krun_set_kernel(path, …)` lets libkrun boot a kernel image from a
file, so the GPL kernel stays out of the Apache-2.0 binary, and the 15 MB
static budget in `dist-linux` is not blown by a 10–20 MB `Image`.
`zygo backend install vm` downloads the kernel into `krun/` and verifies a
pinned SHA-256, the way `install gvisor` verifies `runsc`, and `doctor`
reports "kvm ok, guest kernel not installed" as a distinct line. The refusal
in `cmd/backend.rs` becomes the install. *Provisional*: M0 confirms that
`krun_set_kernel` is in the libkrun version pinned and that libkrun links
against musl; if the second fails, the vm-capable build is a separate
`dist-linux-vm` target on glibc and the plan says so rather than pretending.

**D9. Not in v1, refused by name like `gvisor` does.** `network = "host"`
(no namespace to remove); `--tty` until M5 wires virtio-console;
`system`/`requirements` builds stay on `ns` because the builder sandboxes
run Zygo's own code, not a tenant's; `isolation = "vm"` on macOS is the
shim's Lima VM, which has no nested KVM, so the shim refuses with the
reason. Each refusal is a test, as in
`gvisor.rs::what_the_backend_cannot_do_is_refused_by_name`.

## 5. Milestones

Each milestone ends with something measured on the Pi and written into
`docs/poc-report.md` or the milestone's own section of `todo.md`. Estimates
are for one person who knows the tree.

### M0 — Groundwork and PoC 8 (2–3 days)

The milestone that turns "blocked on KVM" into numbers.

1. **Build environment on the Mac.** A `Dockerfile.vm` (or a target in the
   Makefile) that builds libkrun for `aarch64-unknown-linux-musl` as a
   static library inside `rust:1-alpine`, then builds `zygo` with a `vm`
   cargo feature that links it. Decide here whether musl works for
   libkrun; record the answer and the libkrun commit pinned.
2. **Kernel artefact.** Fetch the libkrunfw release for aarch64, extract the
   `Image`, dump its config: confirm `CONFIG_CGROUPS`, `CGROUP_PIDS`,
   `SECCOMP_FILTER`, `SECURITY_LANDLOCK`, `OVERLAY_FS`, `NF_TABLES`,
   `VIRTIO_FS`, `VIRTIO_VSOCKETS`. Anything missing is a custom kernel
   build, which is a week and needs the disk, so find out now.
3. **PoC 8, finally.** A throwaway `examples/poc8_libkrun.rs` (or a C file)
   that boots the kernel with a flattened `python:3.12-slim` over virtiofs
   and runs `python3 -c pass`, then `python3 -c "import json, re, ssl"`,
   twenty times each. Report boot-to-exec, first import, second import.
   Acceptance is not a threshold; it is a row in `poc-report.md` and a
   decision on D3 (flat first, or layered tags first).
4. **Pi checklist script** `poc/pi_env.sh`: what §2 verified, as a script
   that prints it, so the next person does not ssh around by hand.

Deliverable: the numbers, the Dockerfile, the pinned versions, and D3/D8
settled or re-marked.

### M1 — `zygo run --isolation vm` (1 week)

One-shot only, `network = "none"`, no agent. The shape is `gvisor.rs`.

* `backend/vm.rs`: `VmBackend` with `availability()` (KVM openable, kernel
  installed, libkrun feature compiled in) and `start()` (fork; in the child:
  cgroup attach, `krun_*` calls, `krun_start_enter`). `VmSandbox` with
  `pid`, `state`, `wait`, `kill`, `cgroup`.
* `zygo guest-init` (hidden subcommand, `cmd/guest_init.rs`): mounts from
  the `MountPlan`, `/tmp` tmpfs at `scratch`, `/proc`, `/sys`, `/dev`
  nodes, then `execve` of `argv` with `env` and `workdir`. Stdout/stderr
  over a vsock port bridged to the caller's pipes, or virtio-console;
  choose the one whose exit-code path is simpler and say why.
* `zygo backend install vm` and the `doctor` line (D8).
* `poc/verify_vm.sh`, copied from `verify_gvisor.sh`: the `ns` baseline
  first, then the same three probes on `vm`, then `uname -r` differing,
  then `/tmp` writable and `/` not, then what is refused (hold, agent,
  network, tty) refused by name.
* `make vm-pi`: `scp` the binary and the suite, run it over ssh, bring the
  log back. Same as the earlier Pi rounds, written down.

Acceptance: `verify_vm.sh` green on the Pi; `zygo bench cold --isolation
vm` reported beside `ns` (the design table says 100–300 ms boot); inside the
guest, `landlock` and `cgroup.kill` availability printed, because that is
the "improves this host twice" claim from §2.

### M2 — Limits, hardening and the escape suite inside the guest (1 week)

* Guest-side cgroup v2 in `guest-init`: `zygote/` and `request/<id>/`,
  `pids.max` from the spec, `memory.max` as a second fence below the
  guest's RAM.
* seccomp filter and Landlock ruleset applied by `guest-init` from the
  same `SandboxConfig` the `ns` child uses; the syscall tables are the
  architecture's, unchanged.
* D7's numbers: guest RAM allowance, VMM overhead, host `memory.max`
  derived from them; OOM inside the guest produces exit 137 the way `ns`
  does, and the timeout path is host `SIGKILL` on the VMM with guest-side
  `cgroup.kill` in front of it.
* `escape_suite.sh` and `fuzz_syscalls.sh` gain an `ISOLATION=vm` mode.
  Some vectors are about user namespaces and do not apply; each one is
  either run or listed as not applicable with the reason, never skipped
  silently (the fourth rule in the README).

Acceptance: escape suite `N blocked, 0 escaped` on `vm` with the
not-applicable list printed; fuzz `0 failed`; `--mem 64M` with a 200 MB
allocation exits 137; a fork bomb stops at `pids`; a 10 s sleep under
`--timeout 2s` ends in about 2 s with the VMM gone and the tenant cgroup
empty.

### M3 — Warm: the agent over vsock (1–2 weeks)

The milestone the acceptance row is about.

* `guest-init` connects guest vsock port 3 and hands it as fd 3 to
  `zygo_agent.py --fd 3 …`; on the host, `krun_add_vsock_port(3,
  <agent.sock path>)` and the pool opens `agent.sock` as it does now.
* `RequestControl` (D5): trait, the `ns` move, the `vm` implementation over
  port 4, `guest-init`'s server for `admit`/`kill`/`release`.
* Secrets over a virtiofs tag (D6), with the write-`GO`-read test.
* `WarmFn` on `vm`: `serve`, `exec`, `ps`, `stop`, rewarm after crash,
  `tier_idle` freezing the VMM's cgroup and thawing it, `cold_after`
  dropping the VM.
* `verify_supervisor.sh` gains `ISOLATION=vm`; the checks that inspect
  `/proc/<pid>` of the agent are rewritten to ask the guest.
* If D3's PoC 8 number was bad: layered virtiofs tags and a guest overlay,
  here.

Acceptance: `zygo bench warm --isolation vm` on the Pi, p50 and p99
reported next to `ns` on the same machine, against the design's "1–3 ms" and
the todo's "< 3 ms"; `bench warm` still declining to judge when the tenant
hit its CPU quota; the supervisor suite green on `vm`; a secret readable in
the request and absent from `/proc/1/environ`, the agent's memory, and the
control socket, asserted the same way §1.5 of the use-case instructions
does.

### M4 — Networking (1 week)

* The VMM forked inside Zygo's user+net namespace (D2), after id maps and
  before `pasta`, at the point `launch()` today parks the child.
* TSI versus `krun_set_passt_fd` measured on the same namespace: DNS through
  Zygo's resolver, allowlist, the private and link-local refusals, the
  connection cap, `bandwidth` via `tc`. Keep the one that passes; record
  the other's failure mode.
* `verify_vm.sh` network section: the positive control first, then the
  refusals, on `ns` and `vm`, compared.

Acceptance: scenarios 1.4 and 7.2 of
[use_case_test_instructions.md](use_case_test_instructions.md) give the
same answers on `vm` as on `ns`; `metadata.google.internal` and
`169.254.169.254` refused inside the guest; the resolver's admit-on-resolve
behaviour observed from the guest.

### M5 — Warm-exec, `shell`, `logs`, `--tty` (1 week)

* Warm-exec on `vm`: port 4 gains `exec argv stdin` and `guest-init` forks
  and execs inside the held guest, returning stdout, stderr and the exit
  code; the host `WarmExec` uses it where `namespaces()` is `None`.
* `zygo shell` into a guest: the same verb with a pty, over vsock.
* `--tty` for one-shot runs via virtio-console.
* `zygo logs` unchanged, since the request path already returns streams.

Acceptance: the Go warm-exec example and `sh -c cat` under `bench warm --
sh -c cat` on `vm`; `zygo shell` shows the guest's `/proc/1/cgroup` and
`uname -r`; `examples/` validated on `vm` by `verify_examples.sh`.

### M6 — Distribution, docs, CI (3–4 days)

* `dist-linux` builds the vm-capable binary and checks the budget; if D8's
  musl question went the other way, a `dist-linux-vm` target and a sentence
  in the README.
* `zygo backend install vm` documented; `doctor` prints the guest kernel
  line; `backend list` shows `vm` available on the Pi.
* README status paragraph, `docs/threat-model.md` T3 row, `spec-reference.md`
  isolation row, `docs/comparison.md`, `ahmed.md` 3.9 table with measured
  numbers replacing designed ones. The "not built" sentences in
  `examples/agent-tool/README.md` and `sandbox.toml` are removed.
* CI: `pending-hardware` stops saying KVM has no home. The honest options
  are a self-hosted runner on the Pi for a manual-dispatch `vm` job (the
  usual caveat: never on pull requests from forks), or the `make vm-pi`
  target run by hand and its log committed under `docs/`. Pick one and
  write it down.
* `todo.md` 2.5 rewritten as the checklist below, each line with the
  measurement that closes it.

### Later, and deliberately not here

Firecracker snapshot restore (`docs/firecracker-snapshots.md`: entropy, time,
portability, all three before it is a backend); guest memory ballooning for
the 1000-warm-tenants row; `gvisor` warm mode, which M3's `RequestControl`
makes possible by the same route; a `vm` build for the macOS shim's VM,
which would need nested virtualisation Lima's `vz` does not offer.

## 6. Risks and the experiment that settles each

| # | Risk | Settled by |
|---|---|---|
| V1 | libkrun does not link against musl, or its static build is unsupported | M0 step 1; fallback is a glibc `dist-linux-vm` |
| V2 | libkrunfw's kernel lacks a config Zygo needs (Landlock, nftables, cgroup pids) | M0 step 2; fallback is a custom kernel build, which needs the disk |
| V3 | virtiofs makes Python imports slow (design R6) | PoC 8; fallback is layered tags plus guest overlay in M3 |
| V4 | TSI does not honour the VMM's network namespace, or breaks UDP to the resolver | M4; fallback is `passt` on a descriptor |
| V5 | Per-tenant memory overhead is at the top of the 5–30 MB range and the design's density row is off | M2 measurement; the docs quote the number either way |
| V6 | virtiofs caching delays secret visibility past `GO` | D6 test in M3; the cache mode is the knob |
| V7 | `krun_start_enter` swallows the child's tracing and panics | Stderr pipe from the child kept open through the fork; a panic hook that writes before `_exit` |
| V8 | The Pi's kernel is EOL and the host side of the boundary is what it is | Stated in the report; the guest kernel is the newer one, and that is an argument for the backend, not against the host |
| V9 | Another session is using the Pi when the suite runs | `make vm-pi` checks `pgrep zygo` and refuses; `ZYGO_DATA_HOME` per run under `~/zygo-vm/` |
| V10 | `doctor` says `kvm ok` on a host where `KVM_CREATE_VM` fails (a nested or restricted hypervisor) | Extend `doctor::kvm()` to attempt `KVM_CREATE_VM` and close it, as the probe in §2 did |

## 7. The todo 2.5 checklist, rewritten

To replace the six lines under "2.5 The `vm` backend" when M0 lands:

- [ ] PoC 8 run on the Pi: libkrun boot, first and second import over
      virtiofs, twenty samples each, in `poc-report.md`
- [ ] libkrun linked statically into the `vm` feature build; the guest
      kernel a verified download into `krun/` via `zygo backend install vm`
- [ ] `zygo run --isolation vm` passes `verify_vm.sh` on the Pi, including
      the `ns` comparison and the refusals-by-name
- [ ] `guest-init` applies the `ns` child's mounts, cgroups, seccomp and
      Landlock inside the guest; the escape and fuzz suites run there
- [ ] The agent protocol over vsock with `RequestControl` in the guest;
      `bench warm --isolation vm` p50/p99 reported beside `ns`
- [ ] Secrets over a virtiofs tag, with the write-`GO`-read test
- [ ] `egress` on `vm` through the VMM's network namespace; the network
      checks compared with `ns`
- [ ] Warm-exec, `shell` and `--tty` on `vm`
- [ ] `doctor`, `backend list`, README, threat model and spec reference
      say what is measured; `pending-hardware` no longer says KVM has no home

## 8. First week, concretely

```bash
# Mac: the build container and the kernel artefact (M0.1, M0.2)
make vm-build            # to be written: builds libkrun (musl, aarch64) + zygo --features vm
make vm-kernel           # to be written: fetches libkrunfw aarch64, extracts Image, dumps config

# Pi: environment, once
ssh m@192.168.1.32 'pgrep -a zygo; df -h /; mkdir -p ~/zygo-vm'
scp poc/zygo-linux-musl-vm poc/verify_vm.sh poc/pi_env.sh m@192.168.1.32:~/zygo-vm/

# Pi: PoC 8 (M0.3), then M1
ssh m@192.168.1.32 'cd ~/zygo-vm && ./pi_env.sh && ./poc8_libkrun'
ssh m@192.168.1.32 'cd ~/zygo-vm && ZYGO_DATA_HOME=$PWD/data ZYGO=$PWD/zygo-linux-musl-vm sh verify_vm.sh'
```

The report each step writes is the deliverable, in the style of the rounds
recorded in `todo.md`: what was attempted, what it printed, what changed
because of it.
