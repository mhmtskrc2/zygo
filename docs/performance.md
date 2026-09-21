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
| What was measured here | the fifty use-case scenarios, the supervisor and MCP suites, warm-up, and the `vm` backend's attempt to boot | the warm path, the cold start, throughput, the escape suite, the seccomp sweep and matrix | the shim's own overhead, and the same suites through the hop |

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
Zygo manages, and crossing into it costs about **100 ms per command**. That is
the floor for anything typed at a Mac shell, and `zygo exec` and `docker exec`
feel the same there.

The millisecond warm path is still reachable on a Mac — through the HTTP API or
the SDKs, where the round trip happens inside the VM and the 100 ms is paid once
by the connection rather than once per request. A warm `exec` from a Mac shell
round-trips in 96 ms, nearly all of it the hop.

## What is not measured

- The `vm` backend. It builds and links, and no host available to this project
  can boot a guest on it, so there are no numbers and none are claimed.
- Anything across more than one machine. Zygo's capacity is a per-host budget
  and a `429` past it.
- Receive-side bandwidth shaping, which needs an `ifb` device the test hosts do
  not have.

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
