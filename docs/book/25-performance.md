# 25. What Zygo costs: performance

This chapter lists every number Zygo publishes about itself: how long a request
takes, how much memory a warm script uses, and where the time goes. Each number
was measured by a command in this repository, on a real kernel, and each one
names the machine it came from. You can run the same commands and check them.

## The short version

All numbers were measured on 25 September 2026, on the code at commit
`9607289`, on the **Lima VM** unless marked. [The machines](#the-machines)
says why that machine.

```text
  what one request costs, usually (the median), on the Lima VM unless marked
  ──────────────────────────────────────────────────────────────────────────
  warm function (a fork)          1.44 ms  ▌
  runtime pool (a fork)           1.91 ms  ▌
  warm-exec (a new process)       1.43 ms  ▌
  one-shot sandbox                12.3 ms  █
  vm backend, one-shot (Pi 5)      422 ms  ████████████████████████████████████████
  ──────────────────────────────────────────────────────────────────────────
  one █ is about 10.5 ms
```

| | Usually | 1 in 100 | Where it was measured |
|---|---|---|---|
| A warm request | 1.44 ms | 10.5 ms | Lima VM |
| A warm request from a pool, a different script each time | 1.91 ms | 11.4 ms | Lima VM |
| A warm-exec request | 1.43 ms | 4.3 ms | Lima VM |
| Sustained throughput through one warm function | 1,108 requests a second | | Lima VM |
| A one-shot sandbox, image already pulled | 12.3 ms | 15.3 ms | Lima VM |
| A one-shot sandbox under a hardware boundary (`vm`) | 422 ms | | Raspberry Pi 5 |
| One more warm script, one zygote each | 11.1 MB | | Lima VM |
| One more warm script, in a runtime pool | 0.0 kB | | Lima VM |

The "1 in 100" column is much higher than the usual one for the warm function
and the pool. That is a known kernel effect on Linux 6.x, not noise: with the
`favordynmods` setting `zygo doctor --fix` offers, both fall to about 3.4 ms
([why](#why-1-in-100-is-slow-on-newer-kernels)).

The rest of the chapter explains each line, and the
[embedder's benchmark](#the-embedders-benchmark) compares them with what you
would do without Zygo.

## Words used in this chapter

A few words come up again and again. Each is simple once named.

| Word | What it means |
|---|---|
| **p50** (the median) | Sort all the request times. The one in the middle is p50: half the requests were faster, half slower. |
| **p90, p99** | The time that 90 (or 99) requests out of 100 beat. p99 shows the slow "tail" that a few unlucky requests see. |
| **min** | The fastest single request. |
| **overhead** | The time Zygo adds around your code, with your code's own time taken out. |
| **throughput** | How many requests are finished per second, when many are sent. |
| **RSS** (resident set size) | The memory pages a process holds in RAM, counting shared pages in full for every process that shares them. |
| **PSS** (proportional set size) | The same, but each shared page is split between the processes that share it. Two processes sharing one page count half each. |
| **throttling** | When a cgroup's CPU limit is reached, the kernel stops the process until the next time slice. The CPU of a hot machine can also slow itself down; that is thermal throttling. |
| **zygote** | A warm process that already started the interpreter and loaded the code. Each request is a `fork()` of it ([chapter 6](06-how-zygo-works.md#the-idea-of-a-zygote)). |

Why p99 and not just the average? Because the average hides the slow requests,
and a user who waits for the slow one does not care about the average.

```text
  100 requests, sorted from fastest to slowest (each bar is one request's time)

  request   1  ██████████
  request  25  ███████████
  request  50  ████████████          ◄── p50: half of the requests were faster
  request  75  █████████████
  request  90  ███████████████       ◄── p90: 9 in 10 were faster
  request  99  ███████████████████   ◄── p99: 99 in 100 were faster
  request 100  ████████████████████████████████   the slowest; p99 ignores it
```

## The machines

Every millisecond in this book came from one of these three machines.

| | Lima VM (the main one) | Raspberry Pi 5 | Docker Desktop's VM |
|---|---|---|---|
| Hardware | 2 vCPU, 4 GiB, of an Apple M1 Max | 4× Cortex-A76, 8 GiB, aarch64 | 5 vCPU, 8 GiB, of the same Mac |
| OS and kernel | Ubuntu 24.04, Linux 6.8 | Ubuntu, Linux 6.5 | LinuxKit, Linux 5.10 |
| How Zygo ran | an ordinary user under a systemd login, the binary on the VM's own disk | an ordinary user, under a systemd session, everything in RAM (`/dev/shm`) | a privileged container, as root |
| What is measured here | almost everything: warm path, pool, warm-exec, one-shot, throughput, density, the embedder's benchmark, bytecode, dependencies | the `vm` backend, warm-up times, and the older embedder's table with kern | a check of the warm path on an older kernel |

**Why Lima.** Until 25 September most numbers came from Docker Desktop's VM.
Re-measured that day, it had become twice as slow at one-shot sandboxes
(40.7 ms against the 18.4 ms once published) — and a build of Zygo from 20
September was just as slow there, so the machine had changed, not the code.
Lima is closer to a real host: a normal Linux, a normal user, overlayfs. On
Docker Desktop's VM, the same day, the warm path was 1.55 ms usually and
2.60 ms for 1 in 100; the pool 2.01 / 3.20 ms; throughput 1,043 requests a
second.

## Two things to know about these machines

**Unless a section says otherwise, a number is from the Lima VM.** It is a
virtual machine on a laptop with two virtual CPUs, not a server. A real
server is usually faster; the numbers here are careful, not flattering.

**No number here is from an x86_64 machine.** All three hosts are aarch64
(64-bit ARM). The CI workflow builds and tests on x86_64 runners, and the
syscall tables are generated for x86_64, but nothing was timed there.

Reproduce any of the numbers:

```bash
zygo bench warm        # the warm path, with a phase breakdown
zygo bench cold        # a one-shot sandbox, start to finish
zygo bench load        # sustained throughput through one warm function
```

[Reproducing them](#reproducing-them) below lists every flag and budget.

## The warm path

A warm function is a sandbox that is already up. A request is a `fork()` into
it. You pay for three things: the fork, the cgroup write that admits the new
child, and the reply.

```text
  one warm request, from the supervisor's side
  ─────────────────────────────────────────────────────────────────
   fork ───────► admit ──────► run (GO … DONE) ──► reply
   copy the      put the       your handler        the answer goes
   zygote        child in its  runs; its time is   back to the caller
                 own cgroup    taken out of the
                               overhead
  ─────────────────────────────────────────────────────────────────
```

| Lima VM, 10,000 requests at 250 a second | |
|---|---|
| Usually (median) | 1.44 ms |
| 1 in 100 (99th percentile) | 10.5 ms |
| Sustained throughput | 1,108 requests a second, 4 clients |

These are overhead: the time Zygo adds around your handler, with the handler's
own work taken away. `zygo bench warm` reports the two separately. It also
reports the host's own `fork()` floor next to them, which is the time the
machine needs for a bare fork. So you can see how much of the number belongs
to the machine and how much to Zygo.

## What the 1.4 ms is made of

`zygo bench warm` times each phase of every request. One run of 3,000
requests at 250 a second, on the Lima VM (Linux 6.8) inside a privileged
container, on 25 September 2026 — so a little noisier than the table above:

| phase | what happens | usually | 1 in 100 |
|---|---|---:|---:|
| fork | the agent copies the zygote, and says `FORKED` | 507 µs | 4.9 ms |
| admit | the supervisor makes the request's cgroup and moves the child in | 156 µs | 9.7 ms |
| run | `GO` to `DONE`: the child wakes, reseeds, makes its temp folder, runs the handler, writes the answer | 496 µs | 4.1 ms |
| — of which the handler | an empty one | 17 µs | 85 µs |
| release | the request's cgroup is removed, after the answer has gone | 26 µs | 217 µs |
| **the whole request** | | **1.19 ms** | 15.6 ms |

The same host's bare `fork()` and `wait()` take 95 µs, so most of `fork` is
Python and the protocol, not the kernel. Without a cgroup per request
(`--no-cgroup`), `admit` drops to 27 µs and the median to 1.0 ms: per-request
containment costs about a fifth of the median, and most of the slow tail.

What is **not** in the number, because it happens once, when the zygote
starts (`zygo serve`, about 150 ms):

- creating the namespaces and mounting the root, `/proc` and `/tmp`;
- loading the seccomp filter and the Landlock rules — a child inherits both,
  and only `strict` adds a small filter of its own per request;
- starting the interpreter and running your imports.

## Why 1 in 100 is slow on newer kernels

On a stock Linux 6.x, 99 requests in 100 take about 1.5 ms and the last one
takes about 10. The phase breakdown shows where: 80–87% of that slow request
is `admit`, the step that puts the new process in its own cgroup. Moving a
process between cgroups takes one of the kernel's locks for writing, and since
Linux 6.0 the first writer after a quiet spell waits for the whole kernel to
pass a quiet point — several milliseconds. Before 6.0 the kernel kept that lock
ready for writers all the time, which is why Docker Desktop's 5.10 kernel does
not show it (1 in 100 there: 2.60 ms).

There are two ways to not pay it, and Zygo uses both:

- **Do not move the process at all.** A process *created* inside its cgroup
  (`clone3` with `CLONE_INTO_CGROUP`, Linux 5.7) never takes the lock for
  writing. One-shot sandboxes and zygotes have always been started this way,
  and since 25 September 2026 so is every **warm-exec** request.
- **Keep the lock ready for writers.** A warm request on the *agent* path is
  forked by Python inside the sandbox, where `clone3` is refused by the
  seccomp filter on purpose, so it has to be moved. Mounting the cgroup file
  system with the `favordynmods` option makes the move cheap. `zygo doctor`
  reports it as `cgroup moves`, and `zygo doctor --fix` turns it on, now and
  at every boot.

| Lima VM, Linux 6.8, 10,000 requests at 250 a second | usually | 1 in 100 |
|---|---|---|
| agent warm function, as the kernel comes | 1.61 ms | 10.3 ms |
| agent warm function, with `favordynmods` | 1.61 ms | **3.4 ms** |
| pooled script, as the kernel comes | 2.06 ms | 11.1 ms |
| pooled script, with `favordynmods` | 1.99 ms | **3.3 ms** |
| warm-exec, created in its cgroup, as the kernel comes | 1.43 ms | **4.3 ms** (was 10.5) |

```text
  1 in 100 warm requests, Lima VM, Linux 6.8
  ─────────────────────────────────────────────────────────────────
  agent, as the kernel comes     ████████████████████████  10.3 ms
  agent, with favordynmods       ████████                   3.4 ms
  warm-exec, born in its cgroup  ██████████                 4.3 ms
  ─────────────────────────────────────────────────────────────────
```

**What `favordynmods` costs.** It is a setting of the whole machine, not only
Zygo's: every fork and every exit takes a slightly slower path through the
same lock. Measured on the same VM, a bare fork-and-wait went from 106 to
108 µs usually and from 206 to 230 µs for 1 in 100. That is why Zygo asks
before turning it on rather than doing it for you. Inside a container it is
the host's setting, and `doctor` says so instead of offering a fix.

## Warming up

Warming up is paid once, by `zygo serve` or the first `zygo up`. It is the cold
sandbox, the interpreter, and whatever the handler imports. After `cold_after`
the sandbox is dropped and the same cost is paid again.

```text
  warm-up of a Python handler, Raspberry Pi 5 (includes starting the supervisor)
  ────────────────────────────────────────────────────────────────
  imports nothing        154 ms  █████████████████████████████████
  imports seven modules  185 ms  ████████████████████████████████████████
  ────────────────────────────────────────────────────────────────
  the seven: json, re, ssl, decimal, datetime, hashlib, urllib.request
```

Both numbers are the median of five, and both include starting the
supervisor, which the first `serve` does. With a supervisor already running
it is less: on the Lima VM, `zygo bench warm` warms a function in 34 ms.
(Before the [bytecode layer](#python-bytecode), the same two took ~270 and
~470 ms: most of the seven imports' cost was compiling them.)

## Warm-exec

In warm-exec, the sandbox is held open but each request is a fresh process, not
a fork. This is the mode for a compiled program: no agent, no runtime, just
`cmd`. It costs **1.43 ms** usually, and 4.3 ms for 1 in 100. Zygo creates
each request directly inside its cgroup, so the kernel tail
[above](#why-1-in-100-is-slow-on-newer-kernels) does not reach it. [Chapter 13](13-warm-functions.md#warm-exec-functions)
shows how to set it up.

## A runtime pool

In a runtime pool the zygote holds no code at all. The script arrives with the
request, and the forked child loads it. It costs **1.91 ms** usually and
**11.4 ms** for 1 in 100. That was measured with a *different script on
every request*: a thousand scripts, each called once before anything was
measured.

| One `zygo bench all`, Lima VM | usually | 1 in 100 |
|---|---|---|
| A warm function | 1.44 ms | 10.5 ms |
| A pooled script | 1.91 ms | 11.4 ms |
| What the pool costs | +0.47 ms | +0.83 ms |

Both rows come from one run on one host, so the difference belongs to the
pool, not to the machine. Docker Desktop's VM, the same day, agrees on the
cost: +0.46 ms usually, +0.60 ms for 1 in 100.

```text
  warm function vs pooled script, usually, one run, Lima VM
  ────────────────────────────────────────────────────
  warm function   1.44 ms  ██████████████
  pooled script   1.91 ms  ███████████████████
  ────────────────────────────────────────────────────
  one █ is 0.1 ms
```

That half a millisecond is the whole cost. It is writing the script into the
sandbox, and the child compiling and loading it.
`zygo bench warm --pool --scripts 1000` reproduces it. The memory side — a
thousand scripts in one zygote, flat in the script count — is in
[density with a runtime pool](#density-with-a-runtime-pool).

## When the number is about your limits, not about Zygo

Every function has a `cpu` limit, its CPU quota. Drive a function past its own
quota and the 99th percentile becomes about 47 ms. That is the quota working,
not Zygo being slow.

The kernel counts CPU time in periods. A process that has used up its quota
waits for the rest of the period before it may run again, and half a period is
about 50 ms. A request that lands in that wait pays it.

```text
  cpu quota reached: the tenant waits for the next period
  ──────────────────────────────────────────────────────────────────
  period 1                              period 2
  ████████████████████░░░░░░░░░░░░░░░░░░█████████████████...
  runs, uses its quota  waits (throttled)  runs again
                        ▲
                        a request arriving here waits too: p99 ≈ 47 ms
  ──────────────────────────────────────────────────────────────────
```

`zygo bench warm` reads the tenant's own CPU accounting. When the tenant was
throttled, it **declines to judge the 99th percentile** and prints
`NOT MEASURED` instead. That number would be about the limit, not about the
code. A benchmark that cannot tell you which one it measured is not telling you
anything.

## A one-shot sandbox

`zygo run` builds a sandbox, runs a program and tears it down.

| Lima VM, `python3 -c pass`, 50 runs | |
|---|---|
| Usually, image already pulled | 12.3 ms |
| 1 in 100 | 15.3 ms |
| Budget it was measured against | 50 ms |
| The same with `/bin/true` instead of Python | 3.6 ms |

The `/bin/true` line is the sandbox alone: namespaces, cgroup, mounts
([chapter 6](06-how-zygo-works.md#the-one-shot-sandbox-zygo-run)). The rest of
the 12.3 ms is Python starting.

Two first-time costs are not in it. The first run of an image also pulls it.
The first run on a kernel without unprivileged overlayfs also flattens the
image's layers into one directory. `zygo bench cold` says which of those
happened, because a number that hides them is misleading. (Docker Desktop's
VM is such a kernel; there the same run took 40.7 ms on 25 September —
see [the machines](#the-machines).)

## A one-shot sandbox on a systemd login

On a normal systemd login, `zygo run` costs more. The shell's own cgroup cannot
hold a sandbox, so `zygo run` first re-executes itself inside a transient
systemd *scope* (a small cgroup that systemd makes on request). That costs
about 12 ms: a scope, a second process, and a cgroup tree that is thrown
away.

When a supervisor is running, `zygo run` hands the sandbox to it instead and
pays none of that:

| `zygo run python:3.12-slim python3 -c pass`, usually, median of 15 | |
|---|---|
| in its own scope, no supervisor | 25.7 ms |
| the same, through a running supervisor | 13.5 ms |
| Measured on | Lima VM, Ubuntu 24.04, kernel 6.8 |

```text
  zygo run python:3.12-slim python3 -c pass, usually, Lima VM
  ────────────────────────────────────────────────────────────────
  own scope             ████████████████████████████████████████  25.7 ms
  through supervisor    █████████████████████                     13.5 ms
  ────────────────────────────────────────────────────────────────
```

`zygo run -v` says which one happened: its timing line ends in
`(through the supervisor)` when it did. The full hunt for this cost is in
[the other finding](#the-other-finding-zygo-run-pays-for-a-cgroup-it-throws-away).

## A sandbox with a hardware boundary

The `vm` backend boots a guest kernel under KVM (the Linux feature that runs
virtual machines) and runs the program inside it.

| Raspberry Pi 5, `zygo run … true`, median of 7 | |
|---|---|
| `--isolation vm`, image already in the store | 422 ms |
| `--isolation ns`, same host, own systemd scope | 73 ms |

```text
  one-shot run, Raspberry Pi 5
  ────────────────────────────────────────────────────────
  ns    ███████                                     73 ms
  vm    ████████████████████████████████████████   422 ms
  ────────────────────────────────────────────────────────
```

That is about six times the cost of `ns`, in exchange for a kernel the tenant
does not share with the host. The first run of an image is several seconds
longer, because the store flattens it. That is all that is measured: the `vm`
backend has no warm path and no networking, so there is nothing else to time
yet ([ADR 0002](adr/0002-warm-paths-stay-on-ns.md) says why).

## Dependencies

A `requirements` file is built into a virtual environment (a *venv*: a folder
with its own Python packages) once. Every run and every function that names
the same file against the same image then shares it.

| Lima VM, `requirements.txt` = `requests` | |
|---|---|
| First build (the `plan` phase of the first `zygo run --requirements`) | 2.5 s |
| Every later run: finding the built venv | 1–2 ms |

```text
  first build  ████████████████████████████████████████  2516 ms
  reused       ▏                                            2 ms
```

The build installs with the image's own `pip` (`pip --python <venv>`) and skips
`ensurepip`. `ensurepip` put a second pip into every venv, and when this was
changed it cost 2.0 s of every build before a single package was installed.

The cache is keyed on the image's digest and the file's bytes. So two projects
with the same requirements share one build, and any edit makes a new one. The
same cache serves `zygo run --requirements` and `zygo serve`.
[Chapter 15](15-images-and-dependencies.md) shows how to use it.

## Python bytecode

Python compiles each `.py` file to *bytecode* (a `.pyc` file) before it runs
it, and normally saves that file for next time. The official `python:*-slim`
images ship no `.pyc` files — 1097 `.py` files in `python:3.12-slim`'s standard
library and not one compiled. A sandbox's root is read-only, so Python cannot
save what it compiles either. So every run compiled every module it imported
again.

So the first `zygo pull` or `run` of such an image compiles its standard
library once, inside a sandbox, into a layer of its own. On the Lima VM that
takes 3.2 s and makes an 18.6 MB layer for `python:3.12-slim`. The result is
served as `<image>+bytecode.<key>`. The `.pyc` files sit next to the sources
and are `unchecked-hash`: a layer never changes, so there is nothing to check
them against.

| Lima VM, the run phase, median of 7 | without the layer | with it |
|---|---|---|
| `python3 -c pass` | 9.4 ms | 6.9 ms |
| `import re, json, hmac, hashlib, base64, datetime, urllib.request, urllib.parse, uuid, decimal` | 164.8 ms | 34.9 ms |
| `import ssl` | 65.7 ms | 17.1 ms |

```text
  importing ten common modules, Lima VM
  ────────────────────────────────────────────────────────────────
  without bytecode layer   ████████████████████████████████████████  164.8 ms
  with bytecode layer      ████████                                   34.9 ms
  ────────────────────────────────────────────────────────────────
```

An image that already has bytecode, or has no Python, is served as it is.
`ZYGO_BYTECODE=0` turns the layer off. A build that fails is a warning, and you
get the original image — never a failed run.

## On a Mac

Sandboxes are Linux. On macOS every command runs inside a Linux virtual machine
that Zygo manages. Crossing into it costs about **22 ms per command** once the
VM is up. The shim goes over the SSH connection Lima already holds open
(`ssh -F ~/.lima/zygo/ssh.config`). It asks `limactl` for nothing unless that
connection is down — which is when the VM needs booting anyway.

The millisecond warm path is still reachable on a Mac: through the HTTP API or
the SDKs. There the round trip happens inside the VM, and the hop is paid once
by the connection, not once per request.

## Where a one-shot run from a Mac spends its time

Median of nine, after a warm-up, on an M1 Max, 25 September 2026:

| | supervisor running in the VM | no supervisor |
|---|---|---|
| `zygo run python:3.12-slim true`, from a Mac shell | **28.7 ms** | 41.8 ms |
| the same, typed *inside* the VM — the sandbox alone | 6.2 ms | 16.4 ms |
| `zygo ps` from the Mac — the pure hop, no sandbox | 23.6 ms | |
| `ssh -F … lima-zygo true` — the connection alone | 10.9 ms | |
| `zygo --version` — no VM at all | 5.4 ms | |

```text
  one zygo run from a Mac shell, supervisor running, 28.7 ms end to end
  ──────────────────────────────────────────────────────────────────
  ◄──────── the hop into the VM: ~22 ms ─────────►◄ sandbox 6 ms ►
  ████████████████████████████████████████████████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓
  ──────────────────────────────────────────────────────────────────
```

Before the shim used the SSH connection directly, the same run was
**171 ms**. About 148 ms of that was `limactl shell` (40–50 ms) plus a
`limactl list` per command to ask whether the VM was running. Both are gone
from the hot path.

## Why `run` looks slow on a Mac

So "why is `run` 29 ms from my Mac when `bench cold` says 12?" has one
answer: the hop. A Linux host sees the 12. The same `true` through Docker
Desktop on the same Mac (`docker run --rm python:3.12-slim true`) took
397 ms. The hop is not being made faster, by decision
([ADR 0001](adr/0001-embedded-runtime.md) puts macOS latency on its "not now"
list). For anything that must be fast on a Mac, use the warm path through the
API; [chapter 13](13-warm-functions.md#a-multi-tenant-consumer-on-the-warm-path)
has the worked example of a multi-tenant consumer on the warm path.

## What is not measured

- The `vm` backend beyond the one-shot cost above. There is no warm path to
  measure on it, and no network.
- Anything across more than one machine. Zygo's capacity is a per-host budget,
  and a `429` answer past it.
- Receive-side bandwidth shaping (limiting how fast data comes *in*). It needs
  an `ifb` device the test hosts do not have.
- Any timing on x86_64, as said [above](#two-things-to-know-about-these-machines).

## Reproducing them

One command runs everything:

```bash
zygo bench all          # or `make bench`, which does the container setup too
zygo bench all --quick  # fewer runs: checks the harness works, not a measurement
```

It runs five steps, prints the machine it ran on, and then compares what it
measured with the numbers published in this chapter. It allows a factor of two
either way. A difference is not a failure: these numbers were taken on the
machines above and yours is a different one. That is why the machine is
printed next to the numbers.

```text
  zygo bench all
  ──────────────────────────────────────────────────────────────
   1. warm        a warm function at 250 requests a second
   2. warm-exec   the same, with `sh -c cat` as the command
   3. pool        a runtime pool, a different script each time
   4. cold        a one-shot sandbox, start to finish
   5. load        sustained throughput through one function
  ──────────────────────────────────────────────────────────────
   then: print the host, compare with the published numbers
```

## The single commands and their budgets

Each command has a *budget*: the number it must beat to print PASS.

| Command | What it measures | PASS when |
|---|---|---|
| `zygo bench warm` | a warm function | p50 < 2000 µs and p99 < 10000 µs |
| `zygo bench warm -- CMD` | warm-exec, running `CMD` per request | p50 < 3000 µs |
| `zygo bench warm --pool --scripts 1000` | a runtime pool | p50 and p99 < 5000 µs |
| `zygo bench cold` | a one-shot sandbox | p50 < 50 ms |
| `zygo bench load` | sustained throughput | ≥ 600 requests a second |

(1000 µs, microseconds, is 1 ms.) If the tenant hit its CPU quota during
`bench warm`, the p99 line reads `NOT MEASURED` rather than PASS or FAIL.

| Flag | Default | What it does |
|---|---|---|
| `warm --n` | 10000 | how many requests |
| `warm --no-cgroup` | off | serve without a per-request cgroup, to see what the cgroup costs |
| `warm --rate R` | as fast as possible | offered load in requests a second; unset drives the tenant into its own quota |
| `warm --cpu C` | the spec's `1.0` | the tenant's CPU quota, in cores |
| `warm --pool --scripts N` | 1000 | measure a runtime pool cycling through N distinct scripts |
| `warm -- CMD` | none | measure warm-exec with this command, e.g. `-- sh -c cat` |
| `cold --n` | 50 | how many runs |
| `cold --image` | `python:3.12-slim` | the image; it must already be pulled |
| `cold --command` | start the interpreter and exit | the program to run |
| `load --seconds` | 10 | how long to run |
| `load --concurrency` | 4 | how many clients call at once |
| `load --cpu` | none | the tenant's CPU quota, in cores |

## The raw records

`make bench-record` runs `bench all` and the warm path with and without a
per-request cgroup, and keeps what they printed as JSON in
[`bench/results/`](../../bench), one folder per run, named after the date, the
kernel and the architecture. A run that missed a budget is kept as well.
Records from Linux 6.8 and 5.10, both aarch64, are there now. The `bench`
workflow makes the same record on GitHub's x86_64 and arm64 runners every
week; those are shared VMs, so their numbers are noisier than a quiet host's,
and a run that saw the machine busy says so.

## Two things `bench all` does that most benchmarks do not

**It lifts the tenant's CPU quota for the throughput run, and only for that
run.** With the spec's default `cpu = 1.0`, a tenant hits its quota long before
the runtime is the limit, so the number would measure the quota. The latency
runs keep the default quota, because there the limit is part of what is being
reported.

**It refuses to give a verdict on a disturbed host.** Around the whole run it
reads the CPU's thermal throttle counters and the Raspberry Pi's firmware flag.
Before it, it reads the load average (how many processes wanted the CPU over
the last minute). A number taken on a machine that was overheating or busy is a
number about the machine. It exits 2, not 0 or 1, to say which kind of
non-zero it is.

| Exit code | Meaning |
|---|---|
| 0 | every budget was met |
| 1 | a budget was missed |
| 2 | no verdict: the throttle counters rose, the Pi firmware flagged throttling, or the 1-minute load was above half the number of cores |

## How these are kept honest

Four rules the benchmarks and test suites are built on. Each one was learned by
getting it wrong first.

- **A test attempts the thing, it does not inspect a setting.** Reading a flag
  passes on a kernel that ignores the flag.
- **A test does not disturb what it measures.** Checking whether standard
  output is a terminal, through a pipe, measures the pipe.
- **A latency measurement can say whether it hit a limit.** See the CPU quota
  [above](#when-the-number-is-about-your-limits-not-about-zygo).
- **A negative check first proves the thing ran.** "The connection was refused"
  and "nothing happened at all" look the same from outside, and only one of
  them is a result.

## The embedder's benchmark

The sections above measure Zygo against its own budgets. This one measures it
against what an *embedder* would otherwise do. An embedder is a program, such as
a workflow engine, that runs other people's scripts and would use Zygo inside
it. For an embedder, the number that decides is not Zygo's overhead. It is the
*ratio* to running a container per call.

This is the gate in [ADR 0001](adr/0001-embedded-runtime.md): the warm fork
has to be at least **10× under** the best one-shot runner on an import-heavy
script, or the product idea is wrong.

```bash
make bench-embed            # or: sh tests/linux/bench_embed.sh --runs 100
```

## What the embedder's benchmark ran

| | |
|---|---|
| Host | the Lima VM: 2 vCPU, 4 GiB, Ubuntu 24.04, **Linux 6.8**, aarch64 |
| Image | `python:3.12-slim`, already pulled, **the same one for every runner** |
| Script | sixteen standard-library modules imported at module level, then a little XML, a hash and a UUID |
| Runs | 60 per runner, after 3 warm-up calls |
| Measured | the **whole per-request command**: process start, request, answer |

A *runner* here is one way of running the script: a warm fork, a fresh sandbox
per call, or a fresh container per call.

## The result on the Lima VM

| runner | usually (p50) | p90 | 1 in 100 (p99) | fastest |
|---|---|---|---|---|
| **`zygo exec`** (warm fork) | **2.8 ms** | 3.4 ms | 11.2 ms | 2.1 ms |
| `zygo run` (a fresh sandbox per call) | 70.8 ms | 72.1 ms | 76.1 ms | 67.4 ms |
| `docker run --rm` | 542.4 ms | 553.2 ms | 559.1 ms | 528.5 ms |
| `kern box` | not measured on this host — see the Pi table below | | | |

```text
  usually, per call, same script, same image, Lima VM
  ────────────────────────────────────────────────────────────────────────
  docker run --rm   ████████████████████████████████████████   542.4 ms
  zygo run          █████▏                                      70.8 ms
  zygo exec         ▏                                            2.8 ms
  ────────────────────────────────────────────────────────────────────────
```

**25× faster than the best one-shot runner.** The one-time cost of getting
there — warming the function — was 114 ms. Two calls of `zygo run` would have
paid for it.

The ratio was 60× when this was first measured, on Docker Desktop's VM
(6.4 ms against 384.7 ms, with `docker run` at 761.9 ms). It fell because the
one-shot path got five times faster — mostly the [bytecode
layer](#python-bytecode), which stopped Python from compiling its standard
library on every run — not because the warm path got slower. The gate in
ADR 0001 is 10×, so it still holds with room.

## A second host, with `kern` in it

This table is older: it was measured before the bytecode layer existed, and
was not repeated on 25 September, to keep load off that machine (it is also a
production server). Read its ratios, not its absolute numbers.

The same benchmark on a **Raspberry Pi 5**: 4× Cortex-A76, 8 GiB, Ubuntu 24.04,
kernel 6.5, bare metal, nothing else running. It includes
[kern](https://github.com/getkern/kern) 0.10.0, fetched from its releases and
checked against the published `.sha256`. 40 runs each.

| runner | p50 | p90 | p99 | min |
|---|---|---|---|---|
| **`zygo exec`** (warm fork) | **9.2 ms** | 9.5 ms | 9.8 ms | 8.6 ms |
| `kern box` | 924.1 ms | 943.8 ms | 954.6 ms | 915.6 ms |
| `zygo run` | 940.1 ms | 5439.9 ms | 20582.6 ms | 923.5 ms |
| `docker run --rm` | 40006.5 ms | 46684.4 ms | 51533.1 ms | 27922.8 ms |

```text
  p50 per call, Raspberry Pi 5 (docker left out: see below)
  ────────────────────────────────────────────────────────────────────────
  zygo run          ████████████████████████████████████████   940.1 ms
  kern box          ███████████████████████████████████████    924.1 ms
  zygo exec         ▌                                            9.2 ms
  ────────────────────────────────────────────────────────────────────────
```

**100× faster than the best one-shot runner**, which here is kern. Two things
in that table are about Zygo and are not flattering. They are the reason the
table is printed in full rather than summed up.

## On the Pi, `zygo run`'s slow tail is the SD card

kern and `zygo run` are the same speed at p50: 924 ms against 940 ms is a tie.
But `zygo run`'s p99 was 20 583 ms. That was chased down, and it is not a code
path in Zygo:

- Per-phase timing was added to `zygo run`. `zygo -v run …` prints
  `timing: plan … start … run …`, and the same fields land in `--outcome`. In a
  slow run the whole stall is in **`plan`**: 1324 ms against 7.9 ms for a
  typical run. `start` (29 vs 10 ms) and `run` (46.6 ms, the same) are
  untouched. `plan` is everything before the sandbox exists: the image index,
  the layer and whiteout reads, the root directory's `mkdir`. It is the first
  disk I/O the process does, which is where a stalled filesystem journal is
  felt.
- The same 50 runs with the data home on tmpfs (`/dev/shm`, a filesystem in
  RAM) instead of the SD card: p99 152 ms, no slow run at all.
- `zygo run` itself writes **about 2 KB per run** to the card (measured from
  `/proc/diskstats` across ten runs). It is not causing the pressure; it is
  stuck behind it.
- `/proc/pressure/io` on this Pi read `full avg10=75%` at the end of the
  benchmark: every task on the machine was blocked on disk I/O three-quarters
  of the time. It fell to ~1% within a minute of rest. The root filesystem is an
  SD card at 89% full that lost 9,699 sectors three days earlier, and `/tmp`
  and `/var/log/journal` are on it.

```text
  zygo run on the Pi: where a slow run's time went, compared with a normal run
  ──────────────────────────────────────────────────────────────────────
             normal run         slow run
  plan         7.9 ms   ▏       1324 ms   ████████████████████████████████
  start         10 ms   ▏         29 ms   ▌
  run         46.6 ms   █       46.6 ms   █
  ──────────────────────────────────────────────────────────────────────
  plan is the first disk read; the card was stalled, so plan waited
```

So the honest reading is this. On a healthy disk, `kern box` and `zygo run` are
a tie. On this disk, anything that reads the disk first pays the tail, and
`zygo run` reads first. kern's flatter p99 here is most likely because its
prepared rootfs cache touches the card later or less. That is a real property
and a likely one, but not one this host can measure fairly. The warm path's
spread on the same card is 9.2 ms to 9.8 ms, because a fork touches no disk.

## On the Pi, the Docker column is not a fair number

It is kept only because deleting it would be worse. Forty seconds for
`docker run --rm` on a Pi is not a believable measurement of Docker. It is a
measurement of this Pi's storage under a cycle of creating and destroying
containers, and possibly of the four runners sharing the machine. Do not quote
it. The Lima figure above (542.4 ms usually) is the one to read.

## What the two hosts agree on

This is the only claim being made. The gap is the interpreter. Every one-shot
runner pays the interpreter on every call, including the fastest one. A warm
fork does not pay it.

## Reading it honestly: the gap is the interpreter

`zygo run` is 70.8 ms in the Lima table, while
[the one-shot number](#a-one-shot-sandbox) for `/bin/true` is 3.6 ms. Both are
right. The ~4 ms is the sandbox; the other ~67 ms is CPython starting and
importing sixteen modules — even with the bytecode layer. A one-shot runner
that was *infinitely* fast would still take those ~67 ms on this script,
because the interpreter is the cost. Warming it is the only thing that removes
it. A fork is the only way to warm it without also keeping its state.

```text
  zygo run, 70.8 ms, Lima VM
  ──────────────────────────────────────────────────────────────────
  ██▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓
  █ the sandbox, ~4 ms    ▓ CPython start + sixteen imports, ~67 ms
  ──────────────────────────────────────────────────────────────────
```

So the ratio is a property of the *script*, not of the runner. A script that
imports nothing would show the three runners much closer together. A script
that imports pandas and Pillow would show them further apart. Sixteen
standard-library modules is the careful, low end of what a real script does.

## 2.8 ms is the CLI, not the API

The 2.8 ms includes starting the `zygo` program itself for every call, and
that program talking to the supervisor. An embedder does not pay that. It
calls `zygo api` over a unix socket, or links `zygo-core`, and gets the
~1.4 ms that [`zygo bench warm`](#the-warm-path) measures. The CLI number is
used here because it is the only thing `docker run` can be compared with.

## This host is a small one

The Lima VM is a virtual machine with two CPUs on a laptop. A real server
with more cores is faster at everything in the table, and a bare-metal
kernel avoids some of the virtual machine's costs. The ratio between the
runners is what carries over; the absolute numbers are this machine's.

## Adding the `kern` column yourself

```bash
# fetch kern from its releases, check it against the published .sha256
ZYGO_BENCH_KERN=/path/to/kern make bench-embed
```

`make bench-embed` does not download kern. A benchmark that fetches and runs a
binary from the internet is not one to run unattended, so adding the column
takes one deliberate act.

The harness calls `kern box`, not `kern run`. `kern run` limits a process on
the host with no image and no namespaces. That is a different thing entirely,
and comparing it with `zygo run` would be an honest comparison of nothing.

## What was predicted about kern, and what happened

Before it was measured, the prediction was: kern would beat `zygo run` on the
sandbox, and land in the same band as everything else on this script, because
it also starts a fresh interpreter per call. That was half right. It lands in
the same band (924 ms against 940 ms on the Pi). It does *not* beat `zygo run`
at p50. Its p99 is far better on that host, and
[the SD-card section](#on-the-pi-zygo-runs-slow-tail-is-the-sd-card) says why
that number belongs to the card rather than to either runner.

## Density: what script number 501 costs

*Density* is how many warm scripts fit on one machine. This was the other half
of Phase 0 of the embedded-runtime roadmap, and the one that decides whether
the old shape could serve an embedder at all. An embedder does not have one
function. It has ten thousand scripts, most of them idle. A Zygo zygote was
then *one function*: `entry` imported, forked per request. So the question is
what one more warm script adds.

```bash
make bench-density ARGS="--scripts 32"
```

Thirty-two distinct scripts (a different constant in each source, so nothing
can be shared as a duplicate), one image, one runtime, on the Lima VM:

| | |
|---|---|
| 32 warm | 672.5 MB resident, 366.5 MB proportional |
| Time to warm each | 114 ms |
| **One more script, marginal RSS** | **21.01 MB** |
| **One more script, marginal PSS** | **11.14 MB** |

(On Docker Desktop's VM, when it was the reference, the same benchmark gave
16.39 MB and 9.98 MB.)

## Why PSS, and why the slope

PSS is the honest figure. It divides each shared page among the processes that
share it, so thirty-two interpreters of one image are not counted thirty-two
times. RSS counts every shared page in full for every process, so it
overstates.

The cost of one more script is taken from the *slope* between the first and
the last checkpoint, not from the total divided by the count. The first zygote
pays for the interpreter's pages, and the thirty-second does not.

```text
  total PSS as scripts are added (shape, not to scale)
  ─────────────────────────────────────────────────────────
  PSS │                                       ●
      │                              ●
      │                     ●              slope = 11.14 MB
      │            ●                       per extra script
      │   ●  ◄── the first pays for the interpreter
      └──────────────────────────────────────── scripts
  ─────────────────────────────────────────────────────────
```

## Extrapolated: one zygote per script does not scale

This is the number a SaaS company asks first:

| scripts | PSS |
|---|---|
| 1 000 | ~10.9 GiB |
| 10 000 | ~109 GiB |

**This does not work, and that is the finding.** Ten thousand scripts is a small
platform, and a hundred and nine gigabytes is not one machine. Idle tiering does not
rescue it either: a paused zygote is still a process holding its memory. (In
this run nothing was tiered down inside the window at all, and the harness says
so rather than reporting the warm figure as a paused one.)

Phase 1 of the roadmap ([ADR 0001](adr/0001-embedded-runtime.md)) is the answer:
a zygote per **runtime** instead of per script. The script arrives in the
`EXEC` message and the forked child loads it. Its exit criterion was this number
going flat: 1 000 distinct scripts, one runtime, and resident memory that does
not grow with the script count.

## Density with a runtime pool

Phase 1 is built, so the same benchmark can be asked of it:

```bash
make bench-density ARGS="--pool --scripts 1000"
```

A thousand distinct scripts — the same distinct-constant sources — are each
registered once with `PUT /scripts`, then called by digest, through **one**
runtime pool. Every call goes over the HTTP API. That is the path an embedder
uses, and a `zygo exec` per call would measure process start-up instead.

| | one zygote per script | one runtime pool |
|---|---|---|
| 1 000 scripts, proportional memory | ~10.9 GiB (extrapolated from the slope) | **29.4 MB, measured** |
| One more script | 11.14 MB | **0.0 kB** |
| Zygotes | 1 000 | **1** |

```text
  memory for 1 000 distinct scripts, Lima VM
  ────────────────────────────────────────────────────────────────────────
  one zygote per script   ████████████████████████████████████████  ~10.9 GiB
  one runtime pool        ▏                                          29.4 MB
  ────────────────────────────────────────────────────────────────────────
```

The slope is not "small", it is **zero**. The checkpoint at 125 scripts and the
checkpoint at 1 000 read the same 29.4 MB. The pool's zygote holds an
interpreter and a dependency set, and the scripts are never in it. That is the
whole of what Phase 1 set out to change, and it is a measurement, not an
argument.

## The latency half, settled

```bash
zygo bench warm --pool --scripts 1000     # or `zygo bench all`, which runs it
```

A thousand distinct scripts, a **different one on every request**, each called
once before anything is measured, at 250 requests a second. Phase 1's exit
criterion is a 1-in-100 time under 5 ms, with memory flat in the script count.
Measured on both machines on the same day:

| 25 September 2026 | usually | 1 in 100 |
|---|---|---|
| Docker Desktop's VM (Linux 5.10): a warm function | 1.55 ms | 2.60 ms |
| Docker Desktop's VM: a pooled script | 2.01 ms | **3.20 ms** — inside 5 ms |
| Lima VM (Linux 6.8): a warm function | 1.44 ms | 10.5 ms |
| Lima VM: a pooled script | 1.91 ms | **11.4 ms** — outside 5 ms |

**The memory half is met everywhere: the slope is zero.** The latency half is
met on the older kernel and missed on a stock newer one — and there a warm
*function*, with no pool at all, misses it by the same amount. The slow 1 in
100 is the kernel's cgroup move ([why](#why-1-in-100-is-slow-on-newer-kernels)),
not the pool: with `favordynmods` on the same Lima VM, the pooled script's
1 in 100 is **3.3 ms**, inside the budget. What the pool itself adds is the same on both machines: about
half a millisecond usually, and under one millisecond for 1 in 100. It is
writing the script into the sandbox and the child compiling it; the phase
breakdown puts it in `run` (`GO`→`DONE`), where the load happens, not in
`fork` or `admit`.

## The same thing measured badly, and why it looked like a failure

The first attempt measured the pool over the HTTP API, with a Python client
making a thousand calls one after another. On a Raspberry Pi it reported p50
6.28 ms / p99 25.24 ms: outside the budget by a factor of five. What saved the
conclusion was the *control* in the same run — a warm *function*, no pool
involved. It measured 4.28 / 20.99 through the same client on the same host. If
the control cannot meet a budget either, the budget is not measuring the thing
under test.

The numbers are kept here because the pair is the point:

| Raspberry Pi 5, 1 000 scripts | p50 | p99 |
|---|---|---|
| Pool, by digest, over the API | 6.36 ms | 25.24 ms |
| **Control**: a warm function, same API, same host, same 1 000 calls | 4.28 ms | 20.99 ms |
| The pool's own cost | +2.08 ms | +4.25 ms |

This was measured with the data directory on `tmpfs`, and repeated with it on
the Pi's SD card: 6.28 / 26.10 against a 4.29 / 21.29 control. The two runs
agree to within a millisecond, which rules the storage out. That card has
stalled this machine on I/O before, and a tail measured on it is worth nothing
until something shows it was not the disk.

## What that bad measurement really measured

Neither column is inside the 5 ms budget, including the one with no pool in it
at all. So this tool measures a Python client making a thousand serial HTTP
calls on a four-core machine. It does not measure the request path the budget
was written for. The pool's own cost is the difference, and even that reads high
here (+2.08 ms, against the +0.47 ms the direct measurement finds), because HTTP
variance lands in both columns.

It is kept as a warning about tools, and as the number an embedder calling over
HTTP from Python will really see on a Raspberry Pi. It is not the number the
phase gate is written in. (The same run inside Docker Desktop's VM: p50
3.38 ms, p99 14.74 ms, slope 0.0 kB. On the Lima VM on 25 September: the pool
2.48 / 5.99 ms over the API, against a warm-function control of
1.54 / 2.38 ms, slope 0.0 kB.)

## Memory per warm script on a smaller VM

[ADR 0005](adr/0005-one-warm-zygote-per-script-version.md) ran the density
benchmark on the Lima VM earlier, with a hundred distinct Python handlers,
each its own function. It agrees with the 25 September run above (11.14 MB
proportional per script):

| | per warm script | 100 scripts |
|---|---|---|
| resident (RSS) | 21.4 MB | 2 143 MB |
| proportional (PSS) | **11.3 MB** | **1 141 MB** |
| time to warm (`zygo serve`, round trip) | **109 ms** | 10.9 s |

On that VM about **300 warm Python scripts fit in 4 GB** with nothing else
running, and a thousand would need 11 GB. The same hundred scripts in one pool
used 29.5 MB, and one more script added 0 kB. The ADR has the latency side too.

## The other finding: `zygo run` pays for a cgroup it throws away

*This section and the three after it are the story of a fix, with the
numbers measured while it was made. Today's numbers are in
[a one-shot sandbox on a systemd login](#a-one-shot-sandbox-on-a-systemd-login).*

This is not part of the gate, but it came out of the same work. It is the
largest avoidable cost on the one-shot path, and avoiding it took a different
route than the obvious one. The whole investigation is written up in
[`crates/zygo-cli/src/scope.rs`](../../crates/zygo-cli/src/scope.rs). Here is
the short version.

On an ordinary systemd user session, `zygo run` re-executes itself inside a
transient scope. It has to, because the `session-N.scope` a login lands in
cannot take a child cgroup. Broken down on an idle Ubuntu 24.04 VM, kernel 6.8:

| | p50 |
|---|---|
| `/bin/true` | 0.30 ms |
| `systemd-run --user --scope … -- true` | 5.14 ms |
| `zygo --version` | 3.23 ms |
| `systemd-run --user --scope … -- zygo --version` | 13.72 ms |
| `systemd-run --user --scope … -- zygo run …` | 41.21 ms |
| **`zygo run …`** | **50.31 ms** |

```text
  p50, idle Ubuntu 24.04 VM, kernel 6.8
  ──────────────────────────────────────────────────────────────────────────
  /bin/true                        0.30  ▏
  systemd-run --scope -- true      5.14  ████
  zygo --version                   3.23  ███
  scope -- zygo --version         13.72  ███████████
  scope -- zygo run …             41.21  █████████████████████████████████
  zygo run …                      50.31  ████████████████████████████████████████
  ──────────────────────────────────────────────────────────────────────────
  ms; one █ is about 1.25 ms; "scope --" is systemd-run --user --scope --
```

A fresh cgroup is ~10 ms of overhead before Zygo does anything. The cgroup
*operations* are not where it goes: every `mkdir` and `subtree_control` write
is under 0.15 ms. Moving a process into a freshly made cgroup is 5.6 ms on its
own.

## The obvious fix was built, and it cannot work

The obvious fix was to put `zygo.slice` under `user@$UID.service`, which
systemd already *delegates* (hands over to the user to manage). That makes the
layout last between runs, and the layout does work there. Tenants get
`memory.max`, `pids.max` and `cpu.max`, and the limits bite: exit 137 on OOM,
threads refused at the pids cap, both checked.

But *getting into it* is forbidden by cgroup v2's delegation containment rule.
To move a process, you need write access to the common ancestor of the source
and the destination cgroups. That ancestor is `user-$UID.slice`, which is
`root:root 644` on both hosts checked — an aarch64 VM on 6.8 and a Raspberry
Pi 5 on 6.5. The code was removed rather than left as something that never
runs.

```text
  moving a process from the login session into zygo.slice
  ─────────────────────────────────────────────────────────────────
  user-$UID.slice          (root:root 644 — you cannot write here)
    ├── session-N.scope    ◄── the zygo run process starts here
    └── user@$UID.service
          └── zygo.slice   ◄── where it wants to go
  the common ancestor is user-$UID.slice, so the move is refused
  ─────────────────────────────────────────────────────────────────
```

## Two more ideas, measured and rejected

`systemd-run --slice=zygo.slice` still pays for the scope. Caching the
`zygo doctor` host probe made no difference at all: 44.92 ms against 45.33 ms
over 90 runs each, taken in turns. The probe cache was kept anyway. The
supervisor probes once per function it warms, and an embedder warming five
hundred scripts was paying five hundred times.

## What works: hand the sandbox to the supervisor

What works is reusing a cgroup that is already delegated and already built —
exactly what the supervisor holds. So now `zygo run` hands a one-shot sandbox
to a running supervisor. The client sends the spec and its flags, passes its
own three streams (stdin, stdout, stderr) over `SCM_RIGHTS` (a way to send open
files over a unix socket), forwards its terminal's signals, and waits. The
supervisor starts the sandbox in the cgroup it already has. Exit code, stdin,
both output streams, `--outcome`, the deadline and OOM all come back as they
would have.

Same VM, same command, forty runs a round, two rounds:

| | p50 | p90 |
|---|---|---|
| `zygo run …`, making its own scope | 45.9 / 43.0 ms | 50.1 / 49.5 ms |
| `zygo run …`, a supervisor takes it | 30.4 / 29.0 ms | 32.9 / 33.8 ms |

A third of the command is gone. None of this ever touched an embedder: a
supervisor pays for its cgroup once, at start-up. It was `zygo run` at a
terminal that paid every time — and now it pays only on a machine where nothing
else is running.

## Verdict of the embedder's benchmark

Phase 0 of the embedded-runtime roadmap ([ADR 0001](adr/0001-embedded-runtime.md))
passes its gate on every host it was measured on. It is **25×** on the Lima
VM against `zygo run` today, was **60×** on Docker Desktop's VM before the
bytecode layer made one-shot runs faster, and **100×** on a Raspberry Pi
against `kern`, the fastest one-shot runner in the field. The bar was 10×.

```text
  how much faster the warm fork is than the best one-shot runner
  ──────────────────────────────────────────────────────────────────
  the gate                 ████                                       10×
  Lima VM, 25 Sep          ██████████                                 25×
  Docker Desktop, earlier  ████████████████████████                   60×
  Raspberry Pi 5, earlier  ████████████████████████████████████████  100×
  ──────────────────────────────────────────────────────────────────
```

It also showed why Phase 1 had to come first. The warm fork is worth tens of
one-shot runs, yet with one zygote per script you could have only a few
hundred warm scripts per host. The thing that makes Zygo worth embedding was
the thing it could not yet do at an embedder's scale. The runtime pool
([above](#density-with-a-runtime-pool)) is the fix.

## Two defects the benchmark found

Neither of them is in the warm path:

- `zygo run` on an ordinary systemd session pays ~34 ms for a cgroup it throws
  away, and the obvious fix is forbidden by cgroup delegation containment. The
  supervisor hand-off above now avoids it when a supervisor is running.
- `zygo run` had no per-phase timing, so a 20 s p99 on the Pi looked like a Zygo
  defect for a day. It has one now, and the p99 was the SD card.

Both are in the one-shot path. An embedder does not use that path; a developer
at a terminal does.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [24. Seccomp profiles](24-seccomp-profiles.md) · [Contents](README.md) · **Next: [26. Why it is built this way](26-decisions.md) →**
