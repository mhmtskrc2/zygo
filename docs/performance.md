# What Zygo costs

Every number here was measured by a command in this repository, against a real
kernel, and every one can be reproduced by running that command. The machines
are below; every millisecond in this documentation came from one of them.

## The machines

| | Raspberry Pi 5 | Docker Desktop's VM | Lima VM (the macOS shim) |
|---|---|---|---|
| Hardware | 4× Cortex-A76, 8 GiB, aarch64 | 5 vCPU, 8 GiB, of an Apple M1 Max | 2 vCPU, 4 GiB, of the same Mac |
| OS and kernel | Ubuntu 23.10, Linux 6.5 | LinuxKit, Linux 5.10 | Ubuntu 24.04, Linux 6.8 |
| How Zygo ran | an ordinary user, under a systemd session — the way a real host runs it | a privileged container, as root | forwarded from the Mac shell, as an ordinary user |
| What was measured here | the fifty use-case scenarios, the supervisor and MCP suites, warm-up, and the `vm` backend end to end | the warm path, the cold start, throughput, the escape suite, the seccomp sweep and matrix | the shim's own overhead, and the same suites through the hop |

Two things follow. **Unless a section says otherwise, a number below is from
Docker Desktop's VM**, which is the slowest of the three for this work: a
cgroup operation there costs several times what it does on bare metal, so the
warm-path figures are conservative rather than flattering. And **no number
here is from an x86_64 machine**; all three hosts are aarch64. The CI workflow
builds and tests on x86_64 runners and the syscall tables are generated for it,
but nothing was timed there.

Reproduce any of them:

```bash
zygo bench warm        # the warm path, with a phase breakdown
zygo bench cold        # a one-shot sandbox, start to finish
zygo bench load        # sustained throughput through one warm function
```

## The warm path

A warm function is a sandbox that is already up. A request is a `fork()` into
it, and what you pay for is the fork, the cgroup write that admits it, and the
reply.

| | |
|---|---|
| Median request overhead | 1.70 ms |
| 99th percentile | 2.81 ms |
| Measured at | 250 requests a second |
| Sustained throughput | 981 requests a second at a concurrency of 4 |

That is overhead: the time Zygo adds around your handler, with the handler's
own work subtracted. `zygo bench warm` reports the two separately, and it
reports the host's own `fork()` floor beside them, so you can see how much of
the number belongs to the machine.

**Warming up** is paid once, by `zygo serve` or the first `zygo up`: the cold
sandbox, the interpreter, and whatever the handler imports. On the Raspberry
Pi a Python handler that imports nothing is warm in about **270 ms**; one that
imports `json`, `re`, `ssl`, `decimal`, `datetime`, `hashlib` and
`urllib.request` in about **470 ms**. Both include starting the supervisor,
which the first `serve` does. After `cold_after` the same cost is paid again.

**Warm-exec**, where the sandbox is held and each request is a fresh process
rather than a fork, costs a median of 2.2 ms. That is the mode a compiled
program uses: no agent, no runtime, just `cmd`.

**A runtime pool**, where the zygote holds no code and the script arrives with
the request, costs a median of **2.07 ms** and a 99th percentile of
**2.92 ms** — measured with a *different script on every request*, a thousand
of them, each called once before anything was measured.

| | p50 | p99 |
|---|---|---|
| A warm function | 1.42 ms | 2.12 ms |
| A pooled script | 2.07 ms | 2.92 ms |
| What the pool costs | +0.65 ms | +0.80 ms |

Both rows from one `zygo bench all` on one host, so the difference is the
pool's and not the machine's. That two-thirds of a millisecond is the whole
of it: writing the script into the sandbox, and the child compiling and
loading it. `zygo bench warm --pool --scripts 1000` reproduces it, and the
memory side — a thousand scripts in one zygote, flat in the script count — is
in [the embedder's benchmark](bench-embed.md).

### When the number is about your limits, not about Zygo

Drive a function past its own `cpu` quota and the 99th percentile becomes about
47 ms. That is the quota working: a process out of quota waits out the rest of
the enforcement period, and half a period is about 50 ms.

`zygo bench warm` reads the tenant's own CPU accounting and **declines to judge
the 99th percentile** when the tenant was throttled, because that number would
be about the limit rather than about the code. A benchmark that cannot tell you
which one it measured is not telling you anything.

## A one-shot sandbox

`zygo run` builds a sandbox, runs a program and tears it down.

| | |
|---|---|
| Median, image already pulled | 18.4 ms |
| Budget it was measured against | 50 ms |

Most of that is the namespace set, the cgroup and the mount plan. The first run
of an image also pulls it, and the first run on a kernel without unprivileged
overlayfs also flattens the image's layers — `zygo bench cold` says which of
those happened, because a number that hides them is misleading.

On a systemd login it costs more, because the shell's own cgroup cannot hold
a sandbox and `zygo run` has to re-execute inside a transient scope first —
about 15 ms of scope, second process and throwaway cgroup tree. When a
supervisor is running, `zygo run` hands the sandbox to it instead and pays
none of that:

| | p50 |
|---|---|
| `zygo run python:3.12-slim python3 -c pass`, own scope | 43–46 ms |
| The same, through a running supervisor | 29–30 ms |
| Measured on | Ubuntu 24.04 VM, kernel 6.8 |

`zygo run -v` says which happened: its timing line ends in `(through the
supervisor)` when it did.

## A sandbox with a hardware boundary

The `vm` backend boots a guest kernel under KVM and runs the program inside it.

| | |
|---|---|
| One-shot run, image already in the store | ~400 ms |
| The same run on `ns`, same host | ~40 ms |
| Measured on | the Raspberry Pi 5 |

Ten times the setup cost of `ns`, for a kernel the tenant cannot share with
the host. The first run of an image is several seconds longer because the
store flattens it.

That is the whole of what is measured: the `vm` backend has no warm path, no
networking, so there is nothing else to time yet.

## Dependencies

A `requirements` file is built into a virtual environment once and shared by
every function that names the same file against the same image.

| | |
|---|---|
| First build | 3970 ms |
| Reused by a second function | 111 ms |

The cache is keyed on the image's digest and the file's bytes, so two projects
with identical requirements share one build, and an edit invalidates it. The
same cache serves `zygo run --requirements` and `zygo serve`.

## On a Mac

Sandboxes are Linux. On macOS every command runs inside a Linux virtual machine
Zygo manages, and crossing into it costs about **20 ms per command** once the
VM is up. The shim goes over the multiplexed SSH connection Lima already holds
(`ssh -F ~/.lima/zygo/ssh.config`), and asks `limactl` for nothing unless that
connection is down — which is when the VM needs booting anyway.

The millisecond warm path is still reachable on a Mac — through the HTTP API or
the SDKs, where the round trip happens inside the VM and the hop is paid once by
the connection rather than once per request.

Where a one-shot `run` from a Mac spends its time (median of nine, after a
warm-up, on the Mac this was measured on):

| | |
|---|---|
| one event end to end, from a Mac shell | **30 ms** |
| of which the sandbox (`zygo run … true` typed *inside* the VM) | ~10 ms |
| `zygo ps` from the Mac — the pure-hop baseline, no sandbox | 20 ms |
| `ssh -F … lima-zygo true` — the connection alone | under 10 ms |
| `zygo --version` — no VM at all | 0 ms |

Before the shim used the connection directly the same run was **171 ms**, of
which ~148 ms was `limactl shell` (40–50 ms) plus a `limactl list` to ask
whether the VM was running, per command. Both are gone from the hot path.

So "why is `run` 170 ms when `bench cold` says 22" has one answer: the hop.
A Linux host sees the 23. The same run through Docker Desktop on the same
Mac was 433 ms. The hop is not being optimised, by decision (`mhmt/todo.md`,
"Warm path on macOS"); the warm path through the API is the answer for
anything that has to be fast on a Mac — [the guide](guide.md#a-multi-tenant-consumer-on-the-warm-path)
has the worked example, and `bench warm` in the same VM says **0.91 ms** p50
and 845 requests a second against `bench cold`'s 22.6 ms.

## What is not measured

- The `vm` backend beyond the one-shot cost above. There is no warm path to
  measure on it, and no network.
- Anything across more than one machine. Zygo's capacity is a per-host budget
  and a `429` past it.
- Receive-side bandwidth shaping, which needs an `ifb` device the test hosts do
  not have.

## Reproducing them

```bash
zygo bench all          # or `make bench`, which does the container setup too
```

It runs all four measurements — the warm path, warm-exec, a cold start and
sustained throughput — prints the machine it ran on, and then compares what it
measured with the numbers on this page. A difference is not a failure: these
were taken on the machines above and yours is a different one, which is why
the machine is printed next to the numbers.

Two things it does that a benchmark usually does not:

- **It lifts the tenant's CPU quota for the throughput run, and only for that
  run.** With the spec's default `cpu = 1.0` a tenant is quota-bound long
  before the runtime is, so the number would be a measurement of the limit.
  The latency runs keep the default quota, because there the limit is part of
  what is being reported.
- **It refuses to give a verdict on a disturbed host**, and exits 2 rather
  than 0 or 1 to say which kind of non-zero it is. It samples the CPU's
  thermal throttle counters and the Raspberry Pi's firmware flag around the
  whole run, and the load average before it: a number taken on a machine that
  was overheating or busy is a number about the machine.

## How these are kept honest

Four rules the benchmarks and test suites are built on, each learned by getting
it wrong first:

- **A test attempts the thing, it does not inspect a setting.** Reading a flag
  passes on a kernel that ignores the flag.
- **A test does not disturb what it measures.** Checking whether standard
  output is a terminal, through a pipe, measures the pipe.
- **A latency measurement can say whether it hit a limit.** See the CPU quota
  above.
- **A negative check first proves the thing ran.** "The connection was refused"
  and "nothing happened at all" look identical from outside, and only one of
  them is a result.
