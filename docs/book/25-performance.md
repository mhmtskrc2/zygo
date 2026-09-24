# 25. What Zygo costs: performance

This chapter lists every number Zygo publishes about itself: how long a request
takes, how much memory a warm script uses, and where the time goes. Each number
was measured by a command in this repository, on a real kernel, and each one
names the machine it came from. You can run the same commands and check them.

## The short version

```text
  what one request costs, median, on Docker Desktop's VM unless marked
  ──────────────────────────────────────────────────────────────────────────
  warm function (a fork)          1.70 ms  ▌
  runtime pool (a fork)           2.07 ms  ▌
  warm-exec (a new process)        2.2 ms  ▌
  one-shot sandbox                18.4 ms  ██
  vm backend, one-shot (Pi 5)     ~400 ms  ████████████████████████████████████
  ──────────────────────────────────────────────────────────────────────────
  one █ is about 11 ms
```

| | Median | Where it was measured |
|---|---|---|
| A warm request | 1.70 ms | Docker Desktop's VM |
| A warm request from a pool, a different script each time | 2.07 ms | Docker Desktop's VM |
| Sustained throughput through one warm function | 981 requests a second | Docker Desktop's VM |
| A one-shot sandbox, image already pulled | 18.4 ms | Docker Desktop's VM |
| A one-shot sandbox under a hardware boundary (`vm`) | ~400 ms | Raspberry Pi 5 |
| One more warm script, one zygote each | 9.98 MB | Docker Desktop's VM |
| One more warm script, in a runtime pool | 0.0 kB | Docker Desktop's VM |

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

| | Raspberry Pi 5 | Docker Desktop's VM | Lima VM (the macOS shim) |
|---|---|---|---|
| Hardware | 4× Cortex-A76, 8 GiB, aarch64 | 5 vCPU, 8 GiB, of an Apple M1 Max | 2 vCPU, 4 GiB, of the same Mac |
| OS and kernel | Ubuntu 23.10, Linux 6.5 | LinuxKit, Linux 5.10 | Ubuntu 24.04, Linux 6.8 |
| How Zygo ran | an ordinary user, under a systemd session — the way a real host runs it | a privileged container, as root | forwarded from the Mac shell, as an ordinary user |
| What was measured here | the fifty use-case scenarios, the supervisor and MCP suites, warm-up, and the `vm` backend end to end | the warm path, the cold start, throughput, the escape suite, the seccomp sweep and matrix | the shim's own overhead, and the same suites through the hop |

## Two things to know about these machines

**Unless a section says otherwise, a number is from Docker Desktop's VM.** That
is the slowest of the three for this work. A cgroup operation there costs
several times what it does on bare metal ("bare metal" means a real machine,
not a virtual one). So the warm-path figures are careful, not flattering.

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

| | |
|---|---|
| Median request overhead | 1.70 ms |
| 99th percentile | 2.81 ms |
| Measured at | 250 requests a second |
| Sustained throughput | 981 requests a second at a concurrency of 4 |

These are overhead: the time Zygo adds around your handler, with the handler's
own work taken away. `zygo bench warm` reports the two separately. It also
reports the host's own `fork()` floor next to them, which is the time the
machine needs for a bare fork. So you can see how much of the number belongs
to the machine and how much to Zygo.

## Warming up

Warming up is paid once, by `zygo serve` or the first `zygo up`. It is the cold
sandbox, the interpreter, and whatever the handler imports. After `cold_after`
the sandbox is dropped and the same cost is paid again.

```text
  warm-up of a Python handler, Raspberry Pi 5 (includes starting the supervisor)
  ──────────────────────────────────────────────────────────────────────────
  imports nothing        ~270 ms  ███████████████████████
  imports seven modules  ~470 ms  ████████████████████████████████████████
  ──────────────────────────────────────────────────────────────────────────
  the seven: json, re, ssl, decimal, datetime, hashlib, urllib.request
```

Both numbers include starting the supervisor, which the first `serve` does.

## Warm-exec

In warm-exec, the sandbox is held open but each request is a fresh process, not
a fork. This is the mode for a compiled program: no agent, no runtime, just
`cmd`. It costs a median of **2.2 ms**. [Chapter 13](13-warm-functions.md#warm-exec-functions)
shows how to set it up.

## A runtime pool

In a runtime pool the zygote holds no code at all. The script arrives with the
request, and the forked child loads it. It costs a median of **2.07 ms** and a
99th percentile of **2.92 ms**. That was measured with a *different script on
every request*: a thousand scripts, each called once before anything was
measured.

| | p50 | p99 |
|---|---|---|
| A warm function | 1.42 ms | 2.12 ms |
| A pooled script | 2.07 ms | 2.92 ms |
| What the pool costs | +0.65 ms | +0.80 ms |

Both rows come from one `zygo bench all` on one host. So the difference belongs
to the pool, not to the machine. (The 1.42 ms here and the 1.70 ms above are
two different runs.)

```text
  warm function vs pooled script, one run, Docker Desktop's VM
  ──────────────────────────────────────────────────────────────────
  p50   warm function   1.42 ms  ██████████████
        pooled script   2.07 ms  █████████████████████
  p99   warm function   2.12 ms  █████████████████████
        pooled script   2.92 ms  █████████████████████████████
  ──────────────────────────────────────────────────────────────────
  one █ is 0.1 ms
```

That two-thirds of a millisecond is the whole cost. It is writing the script
into the sandbox, and the child compiling and loading it.
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

| | |
|---|---|
| Median, image already pulled | 18.4 ms |
| Budget it was measured against | 50 ms |

Most of that is the namespace set, the cgroup and the mount plan
([chapter 6](06-how-zygo-works.md#the-one-shot-sandbox-zygo-run)).
Two first-time costs are not in it. The first run of an image also pulls it.
The first run on a kernel without unprivileged overlayfs also flattens the
image's layers into one directory. `zygo bench cold` says which of those
happened, because a number that hides them is misleading.

## A one-shot sandbox on a systemd login

On a normal systemd login, `zygo run` costs more. The shell's own cgroup cannot
hold a sandbox, so `zygo run` first re-executes itself inside a transient
systemd *scope* (a small cgroup that systemd makes on request). That is about
15 ms of scope, a second process and a cgroup tree that is thrown away.

When a supervisor is running, `zygo run` hands the sandbox to it instead and
pays none of that:

| | p50 |
|---|---|
| `zygo run python:3.12-slim python3 -c pass`, own scope | 43–46 ms |
| The same, through a running supervisor | 29–30 ms |
| Measured on | Ubuntu 24.04 VM, kernel 6.8 |

```text
  zygo run python:3.12-slim python3 -c pass, p50, Ubuntu 24.04 VM
  ────────────────────────────────────────────────────────────────
  own scope             ████████████████████████████████████████  43–46 ms
  through supervisor    ██████████████████████████               29–30 ms
  ────────────────────────────────────────────────────────────────
```

`zygo run -v` says which one happened: its timing line ends in
`(through the supervisor)` when it did. The full hunt for this cost is in
[the other finding](#the-other-finding-zygo-run-pays-for-a-cgroup-it-throws-away).

## A sandbox with a hardware boundary

The `vm` backend boots a guest kernel under KVM (the Linux feature that runs
virtual machines) and runs the program inside it.

| | |
|---|---|
| One-shot run, image already in the store | ~400 ms |
| The same run on `ns`, same host | ~40 ms |
| Measured on | the Raspberry Pi 5 |

```text
  one-shot run, Raspberry Pi 5
  ──────────────────────────────────────────────────────────
  ns    ████                                        ~40 ms
  vm    ████████████████████████████████████████   ~400 ms
  ──────────────────────────────────────────────────────────
```

That is ten times the setup cost of `ns`, in exchange for a kernel the tenant
does not share with the host. The first run of an image is several seconds
longer, because the store flattens it. That is all that is measured: the `vm`
backend has no warm path and no networking, so there is nothing else to time
yet ([ADR 0002](adr/0002-warm-paths-stay-on-ns.md) says why).

## Dependencies

A `requirements` file is built into a virtual environment (a *venv*: a folder
with its own Python packages) once. Every function that names the same file
against the same image then shares it.

| | |
|---|---|
| First build | 3970 ms |
| Reused by a second function | 111 ms |

```text
  first build  ████████████████████████████████████████  3970 ms
  reused       █                                          111 ms
```

The build installs with the image's own `pip` (`pip --python <venv>`) and skips
`ensurepip`. `ensurepip` put a second pip into every venv, and it cost 2.0 s of
every build before a single package was installed. Building a venv with
`requests` in the same sandbox took 3.0 s the old way and 1.8 s this way.

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
again: `import re` alone was 34 ms.

So the first `zygo pull` or `run` of such an image compiles its standard
library once, inside a sandbox, into a layer of its own. That takes ≈2.4 s and
18.5 MB for `python:3.12-slim`. The result is served as
`<image>+bytecode.<key>`. The `.pyc` files sit next to the sources and are
`unchecked-hash`: a layer never changes, so there is nothing to check them
against.

| run phase, median of seven, Lima VM | without | with |
|---|---|---|
| `python -c pass` | 13.7 ms | 13.7 ms |
| a harness importing `re`, `json`, `hmac`, `urllib.request` and a few more | 190 ms | 47 ms |
| `import ssl` | — | 26.8 ms |

```text
  a harness importing re, json, hmac, urllib.request and a few more, Lima VM
  ─────────────────────────────────────────────────────────────────────
  without bytecode layer   ████████████████████████████████████████  190 ms
  with bytecode layer      ██████████                                 47 ms
  ─────────────────────────────────────────────────────────────────────
```

An image that already has bytecode, or has no Python, is served as it is.
`ZYGO_BYTECODE=0` turns the layer off. A build that fails is a warning, and you
get the original image — never a failed run.

## On a Mac

Sandboxes are Linux. On macOS every command runs inside a Linux virtual machine
that Zygo manages. Crossing into it costs about **20 ms per command** once the
VM is up. The shim goes over the SSH connection Lima already holds open
(`ssh -F ~/.lima/zygo/ssh.config`). It asks `limactl` for nothing unless that
connection is down — which is when the VM needs booting anyway.

The millisecond warm path is still reachable on a Mac: through the HTTP API or
the SDKs. There the round trip happens inside the VM, and the hop is paid once
by the connection, not once per request.

## Where a one-shot run from a Mac spends its time

Median of nine, after a warm-up, on the Mac this was measured on:

| | |
|---|---|
| one event end to end, from a Mac shell | **30 ms** |
| of which the sandbox (`zygo run … true` typed *inside* the VM) | ~10 ms |
| `zygo ps` from the Mac — the pure-hop baseline, no sandbox | 20 ms |
| `ssh -F … lima-zygo true` — the connection alone | under 10 ms |
| `zygo --version` — no VM at all | 0 ms |

```text
  one zygo run from a Mac shell, 30 ms end to end
  ──────────────────────────────────────────────────────────
  ◄───── the hop into the VM: 20 ms ─────►◄─ sandbox ~10 ms ─►
  ████████████████████████████████████████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓
  ──────────────────────────────────────────────────────────
```

Before the shim used the connection directly, the same run was **171 ms**. About
148 ms of that was `limactl shell` (40–50 ms) plus a `limactl list` per command
to ask whether the VM was running. Both are gone from the hot path.

## Why `run` looks slow on a Mac

So "why is `run` 170 ms when `bench cold` says 22?" has one answer: the hop. A
Linux host sees the 23. The same run through Docker Desktop on the same Mac was
433 ms. The hop is not being made faster, by decision
([ADR 0001](adr/0001-embedded-runtime.md) puts macOS latency on its "not now"
list). For anything that must be fast on a Mac, use the warm path through the
API; [chapter 13](13-warm-functions.md#a-multi-tenant-consumer-on-the-warm-path)
has the worked example of a multi-tenant consumer on the warm path.

| Inside the same Lima VM | |
|---|---|
| `bench warm`, p50 | **0.91 ms** |
| `bench warm`, throughput | 845 requests a second |
| `bench cold`, p50 | 22.6 ms |

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
make bench-embed            # or: sh poc/bench_embed.sh --runs 100
```

## What the embedder's benchmark ran

| | |
|---|---|
| Host | Docker Desktop's Linux VM on an Apple M1 Max — 5 vCPU, 8 GiB, **Linux 5.10**, aarch64 |
| Image | `python:3.12-slim`, already pulled, **the same one for every runner** |
| Script | sixteen standard-library modules imported at module level, then a little XML, a hash and a UUID |
| Runs | 60 per runner, after 3 warm-up calls |
| Measured | the **whole per-request command**: process start, request, answer |

A *runner* here is one way of running the script: a warm fork, a fresh sandbox
per call, or a fresh container per call.

## The result on the M1 VM

| runner | p50 | p90 | p99 | min |
|---|---|---|---|---|
| **`zygo exec`** (warm fork) | **6.4 ms** | 7.1 ms | 7.8 ms | 5.1 ms |
| `zygo run` (a fresh sandbox per call) | 384.7 ms | 403.7 ms | 419.0 ms | 371.0 ms |
| `docker run --rm` | 761.9 ms | 790.6 ms | 827.4 ms | 716.3 ms |
| `kern box` | not measured on this host — see the Pi table below | | | |

```text
  p50 per call, same script, same image, Docker Desktop's VM on an M1 Max
  ────────────────────────────────────────────────────────────────────────
  docker run --rm   ████████████████████████████████████████   761.9 ms
  zygo run          ████████████████████                       384.7 ms
  zygo exec         ▌                                            6.4 ms
  ────────────────────────────────────────────────────────────────────────
```

**60× faster than the best one-shot runner.** The one-time cost of getting
there — warming the function — was 598 ms. Two calls of `zygo run` would have
paid for it.

## A second host, with `kern` in it

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
it. The M1 figure above (761.9 ms at p50) is the one to read.

## What the two hosts agree on

This is the only claim being made. The gap is the interpreter. Every one-shot
runner pays the interpreter on every call, including the fastest one. A warm
fork does not pay it.

## Reading it honestly: the gap is the interpreter

`zygo run` is 385 ms in the M1 table, while
[the one-shot number](#a-one-shot-sandbox) is 18 ms. Both are right. The 18 ms
is the sandbox; the other 367 ms is CPython starting and importing sixteen
modules. A one-shot runner that was *infinitely* fast would still take 367 ms
on this script, because the interpreter is the cost. Warming it is the only
thing that removes it. A fork is the only way to warm it without also keeping
its state.

```text
  zygo run, 384.7 ms, M1 VM
  ──────────────────────────────────────────────────────────────────
  ██▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓
  █ the sandbox, ~18 ms    ▓ CPython start + sixteen imports, ~367 ms
  ──────────────────────────────────────────────────────────────────
```

So the ratio is a property of the *script*, not of the runner. A script that
imports nothing would show the three runners much closer together. A script
that imports pandas and Pillow would show them further apart. Sixteen
standard-library modules is the careful, low end of what a real script does.

## 6.4 ms is the CLI, not the API

The 6.4 ms includes starting `zygo` itself, which is about 3 ms of a static
binary over virtiofs (the file sharing between the Mac and the VM). An embedder
does not pay that. It calls `zygo api` over a unix socket, or links `zygo-core`,
and gets the ~1.7 ms that [`zygo bench warm`](#the-warm-path) measures. The CLI
number is used here because it is the only thing `docker run` can be compared
with.

## This host is the slow end

The M1 host is Linux 5.10 in a nested VM, with no unprivileged overlayfs, so the
image store flattens layers. A Raspberry Pi 5 on bare metal is faster at
everything in the M1 table.

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
can be shared as a duplicate), one image, one runtime, on the M1 VM above:

| | |
|---|---|
| 32 warm | 524.4 MB resident, 325.7 MB proportional |
| Time to warm each | 107 ms |
| **One more script, marginal RSS** | **16.39 MB** |
| **One more script, marginal PSS** | **9.98 MB** |

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
      │                     ●              slope = 9.98 MB
      │            ●                       per extra script
      │   ●  ◄── the first pays for the interpreter
      └──────────────────────────────────────── scripts
  ─────────────────────────────────────────────────────────
```

## Extrapolated: one zygote per script does not scale

This is the number a SaaS company asks first:

| scripts | PSS |
|---|---|
| 1 000 | ~9.7 GiB |
| 10 000 | ~97 GiB |

**This does not work, and that is the finding.** Ten thousand scripts is a small
platform, and ninety-seven gigabytes is not one machine. Idle tiering does not
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
| 1 000 scripts, proportional memory | ~9.7 GiB (extrapolated from the slope) | **29.4 MB, measured** |
| One more script | 9.98 MB | **0.0 kB** |
| Zygotes | 1 000 | **1** |

```text
  memory for 1 000 distinct scripts, M1 VM
  ────────────────────────────────────────────────────────────────────────
  one zygote per script   ████████████████████████████████████████  ~9.7 GiB
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
zygo bench warm --pool --scripts 1000     # or `make bench`, which runs it
```

A thousand distinct scripts, a **different one on every request**, each called
once before anything is measured, at 250 requests a second. That is the same
tool, rate and host as the published warm-path figures:

| Docker Desktop's VM | p50 | p99 |
|---|---|---|
| A warm function | 1.42 ms | 2.12 ms |
| A pooled script | 2.07 ms | 2.92 ms |
| **What the pool costs** | **+0.65 ms** | **+0.80 ms** |

**Phase 1's exit criterion is met**: p99 2.92 ms against a budget of 5 ms, with
a slope of zero. The two-thirds of a millisecond the pool adds is writing the
script into the sandbox and the child compiling it. The phase breakdown puts it
in `run` (`GO`→`DONE`), where the load happens, not in `fork` or `admit`.

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
here (+2.08 ms, against the +0.65 ms the direct measurement finds), because HTTP
variance lands in both columns.

It is kept as a warning about tools, and as the number an embedder calling over
HTTP from Python will really see on a Raspberry Pi. It is not the number the
phase gate is written in. (The same run inside Docker Desktop's VM: p50
3.38 ms, p99 14.74 ms, slope 0.0 kB.)

## Memory per warm script on a smaller VM

[ADR 0005](adr/0005-one-warm-zygote-per-script-version.md) repeated the density
benchmark on the Lima VM (2 vCPU, 3.8 GiB, Ubuntu 24.04, kernel 6.8), with a
hundred distinct Python handlers, each its own function.

| | per warm script | 100 scripts |
|---|---|---|
| resident (RSS) | 21.4 MB | 2 143 MB |
| proportional (PSS) | **11.3 MB** | **1 141 MB** |
| time to warm (`zygo serve`, round trip) | **109 ms** | 10.9 s |

On that VM about **300 warm Python scripts fit in 4 GB** with nothing else
running, and a thousand would need 11 GB. The same hundred scripts in one pool
used 29.5 MB, and one more script added 0 kB. The ADR has the latency side too.

## The other finding: `zygo run` pays for a cgroup it throws away

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
passes its gate on both hosts. It is **60×** on an M1 VM against `zygo run`, and
**100×** on a Raspberry Pi against `kern`, the fastest one-shot runner in the
field. The bar was 10×.

```text
  how much faster the warm fork is than the best one-shot runner
  ──────────────────────────────────────────────────────────────
  the gate            ████                                   10×
  M1 VM               ████████████████████████               60×
  Raspberry Pi 5      ████████████████████████████████████████  100×
  ──────────────────────────────────────────────────────────────
```

It also showed why Phase 1 had to come first. The warm fork is worth sixty to a
hundred one-shot runs, yet with one zygote per script you could have only about
five hundred warm scripts per host. The thing that makes Zygo worth embedding
was the thing it could not yet do at an embedder's scale. The runtime pool
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
