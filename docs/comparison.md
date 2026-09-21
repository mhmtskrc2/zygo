# Comparison

Where Zygo sits, and where it does not. Numbers for Zygo are the project's
own measurements (kernel 5.10, aarch64, Docker Desktop's VM — see the
[what Zygo costs](performance.md)); numbers for the others are their documented
or commonly measured figures and are marked as such.

## The request path

| | Docker `run` | Docker `exec` | Firecracker | gVisor `runsc` | AWS Lambda (warm) | **Zygo, warm** |
|---|---|---|---|---|---|---|
| Per-request overhead | 300–1000 ms | 50–100 ms | ~125 ms boot; 10–20 ms from a snapshot | 50–150 ms | ~1–5 ms + platform | **1.7 ms p50, 2.8 ms p99** |
| Clean state per request | yes | no | yes | yes | no | **yes** (a fresh process) |
| Daemon | yes | yes | yes (a VMM per VM) | yes (`runsc` + shim) | n/a | **no** |
| Root | daemon runs as root | same | needs `/dev/kvm` | no | n/a | **no** |
| Boundary | kernel | kernel | hardware | userspace kernel | hardware | kernel (`ns`); hardware (`vm`, not built) |

The first row is the whole argument. A container's cost is orchestration and
cold interpreter start; Zygo takes the orchestration off the request path
entirely and amortises the interpreter with a warm zygote.

## What each is for

* **Docker** is a general-purpose packaging and orchestration system. Zygo
  uses its images and none of its runtime; if you need `docker compose`,
  long-running services, or port publishing, you need Docker.
* **Firecracker / Cloud Hypervisor** are microVMs: the strongest boundary,
  at a boot cost per VM. Zygo's `vm` backend is meant to sit on libkrun and,
  later, Firecracker snapshots — for anonymous code, not for your own.
* **gVisor** is a userspace kernel: a smaller attack surface than the host
  kernel without KVM, at a syscall cost. Zygo's `gvisor` backend is built for
  one-shot runs (`zygo backend install gvisor`, then
  `zygo run --isolation gvisor`); warm functions still need `ns`.
* **Lambda and its relatives** are managed platforms. Zygo is a local runtime
  with a similar shape — a function, a warm instance, a request — and no
  platform: no billing, no scaling across machines, no ingress.
* **Windmill, Temporal, and other workflow platforms** run user scripts and
  are exactly the embedder Zygo is designed for: a worker calls `zygo serve`
  or the library, and each script run is a `fork()` instead of a container.

## What Zygo does not do

* Run on macOS or Windows natively. Sandboxes are Linux; on a Mac, Zygo
  manages a Linux VM for you.
* Provide ingress. No mode accepts connections; a function is called through
  the CLI, the library or Zygo's own HTTP API.
* Scale past one machine. Capacity is a per-host budget and `429` past it.
* Hide the kernel. The `ns` backend is one kernel, and everything here says so.

## Comparing security models honestly

| | Docker default | **Zygo `ns` default** |
|---|---|---|
| Capabilities | 14 kept | **none** |
| seccomp | denylist-shaped allowlist, ~350 syscalls | **allowlist, ~190**; `clone` flag-checked; `bpf`, `io_uring`, `userfaultfd`, `keyctl`, `perf_event_open`, `ptrace` absent |
| Root filesystem | writable | **read-only**, `pivot_root`, `/proc` masked |
| Landlock | no | **yes** where the kernel has it (5.13+; network rules 6.7+) |
| Network | bridge, everything reachable | **none** by default; `egress` is an allowlist enforced inside the namespace |
| Runs as | root daemon, root in the container | **your uid**, mapped |
| Limits | none unless set | **all mandatory** |
| Escape suite | — | 16 vectors attempted on every change, 0 escaping |

The comparison is not "Zygo is safer than Docker" — Docker's defaults are for
running software you chose, Zygo's are for running code you did not. It is
that the defaults differ in the direction the use case demands, and that
every row in the Zygo column is something a test attempts rather than a
setting a test reads.
