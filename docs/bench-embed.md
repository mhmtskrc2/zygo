# The embedder's benchmark

[What Zygo costs](performance.md) measures Zygo against its own budgets. This
measures it against what an embedder would otherwise do, because the number
that decides whether a workflow engine can use a warm fork is not Zygo's
overhead — it is the *ratio* to running a container per call.

This is the gate in [ADR 0001](adr/0001-embedded-runtime.md): the warm fork
has to be at least **10× under** the best one-shot runner on an import-heavy
script, or the product thesis is wrong.

```bash
make bench-embed            # or: sh poc/bench_embed.sh --runs 100
```

## What was measured

| | |
|---|---|
| Host | Docker Desktop's Linux VM on an Apple M1 Max — 5 vCPU, 8 GiB, **Linux 5.10**, aarch64 |
| Image | `python:3.12-slim`, already pulled, **the same one for every runner** |
| Script | sixteen standard-library modules imported at module level, then a little XML, a hash and a UUID |
| Runs | 60 per runner, after 3 warm-up calls |
| Measured | the **whole per-request command**: process start, request, answer |

## The result

| runner | p50 | p90 | p99 | min |
|---|---|---|---|---|
| **`zygo exec`** (warm fork) | **6.4 ms** | 7.1 ms | 7.8 ms | 5.1 ms |
| `zygo run` (a fresh sandbox per call) | 384.7 ms | 403.7 ms | 419.0 ms | 371.0 ms |
| `docker run --rm` | 761.9 ms | 790.6 ms | 827.4 ms | 716.3 ms |
| `kern box` | not measured on this host — see the Pi table below | | | |

**60× faster than the best one-shot runner.** The one-time cost of getting
there was 598 ms, which two calls of `zygo run` would have paid for.

## A second host, with `kern` in it

The same benchmark on a **Raspberry Pi 5** — 4× Cortex-A76, 8 GiB, Ubuntu
24.04, kernel 6.5, bare metal, nothing else running — with
[kern](https://github.com/getkern/kern) 0.10.0 fetched from its releases and
checksum-verified against the published `.sha256`. 40 runs each.

| runner | p50 | p90 | p99 | min |
|---|---|---|---|---|
| **`zygo exec`** (warm fork) | **9.2 ms** | 9.5 ms | 9.8 ms | 8.6 ms |
| `kern box` | 924.1 ms | 943.8 ms | 954.6 ms | 915.6 ms |
| `zygo run` | 940.1 ms | 5439.9 ms | 20582.6 ms | 923.5 ms |
| `docker run --rm` | 40006.5 ms | 46684.4 ms | 51533.1 ms | 27922.8 ms |

**100× faster than the best one-shot runner**, which here is kern. Two things
in that table are about Zygo and are not flattering, and they are the reason
it is printed rather than summarised.

**kern and `zygo run` are the same speed at p50 — and `zygo run`'s tail is
the host's storage, not Zygo's.** 924 ms against 940 ms is a tie. The p99 of
20 583 ms was chased down, and it is not a code path:

* Per-phase timing was added to `zygo run` (`zygo -v run …` prints
  `timing: plan … start … run …`; the same fields land in `--outcome`). In a
  slow run the whole stall is in **`plan`** — 1324 ms against 7.9 ms for a
  typical run — while `start` (29 vs 10 ms) and `run` (46.6 ms, identical)
  are untouched. `plan` is everything before the sandbox exists: the image
  index, the layer and whiteout reads, the root directory's `mkdir`. It is the
  first disk I/O the process does, which is where a stalled filesystem
  journal is felt.
* The same 50 runs with the data home on tmpfs (`/dev/shm`) instead of the
  SD card: p99 152 ms, no slow run at all.
* `zygo run` itself writes **about 2 KB per run** to the card (measured from
  `/proc/diskstats` across ten runs). It is not causing the pressure; it is
  stalled behind it.
* `/proc/pressure/io` on this Pi read `full avg10=75%` at the end of the
  benchmark — every task on the machine blocked on I/O three-quarters of the
  time — and decayed to ~1% within a minute of idleness. The root filesystem
  is an SD card at 89% full that lost 9,699 sectors three days earlier; `/tmp`
  and `/var/log/journal` are on it.

So the honest row is: on a healthy disk, `kern box` and `zygo run` are a tie;
on this one, anything that reads the disk first pays the tail, and `zygo run`
reads first. kern's flatter p99 here is most likely its prepared rootfs
cache touching the card later or less — which is a real property and a
plausible one, and not one this host can measure fairly. The warm path's
spread on the same card is 9.2 ms to 9.8 ms, because a fork touches no disk.

**The docker column is not a fair number and is kept only because deleting it
would be worse.** Forty seconds for `docker run --rm` on a Pi is not a
plausible measurement of Docker; it is a measurement of this Pi's storage
under a container create/destroy cycle, and possibly of the four runners
sharing the machine. Do not quote it. The M1 figure of 782 ms above is the one
to read.

What the two hosts agree on is the only claim being made: the gap is the
interpreter, the interpreter is paid per call by every one-shot runner
including the fastest one, and a warm fork does not pay it.

## Reading it honestly

**The gap is the interpreter, not the sandbox.** `zygo run` is 385 ms here
while [the cold-start number](performance.md#a-one-shot-sandbox) is 18 ms,
and both are right: 18 ms is the sandbox, and the other 367 ms is CPython
starting and importing sixteen modules. That is the whole argument. A one-shot
runner that were *infinitely* fast would still be 367 ms on this script,
because the interpreter is the cost. Warming it is the only thing that
removes it, and a fork is the only way to warm it without also keeping its
state.

So the ratio is a property of the *script*, not of the runner. A script that
imports nothing would show these three columns much closer together, and a
script that imports pandas and Pillow would show them further apart. Sixteen
stdlib modules is the conservative end of what a real script does.

**6.4 ms is the CLI, not the API.** It includes `zygo` itself starting, which
is about 3 ms of a static binary over virtiofs. An embedder does not pay that:
it calls `zygo api` over a unix socket, or links `zygo-core`, and gets the
~1.7 ms that [`zygo bench warm`](performance.md#the-warm-path) measures. The
CLI number is used here because it is the only thing `docker run` can be
compared with.

**This host is the slow end.** Linux 5.10 in a nested VM, with no
unprivileged overlayfs, so the image store flattens layers. A Raspberry Pi 5
on bare metal is faster at everything in this table.

## Adding the `kern` column yourself

```bash
# fetch kern from its releases, check it against the published .sha256
ZYGO_BENCH_KERN=/path/to/kern make bench-embed
```

It is not downloaded by `make bench-embed`: a benchmark that fetches and
executes a binary from the internet is not one to run unattended. Adding the
column takes one deliberate act.

`kern box` is what the harness invokes, not `kern run` — `run` caps a process
on the host with no image and no namespaces, which is a different thing
entirely, and comparing it with `zygo run` would flatter nobody honestly.

The prediction made before it was measured was that kern would beat `zygo run`
on the sandbox and land in the same band as everything else on this script,
because it also starts a fresh interpreter per call. Half right: it lands in
the same band (924 ms against 940 ms on the Pi) and does *not* beat `zygo run`
at p50. Its p99 is far better on that host, and the section above says why
that number belongs to the host's SD card rather than to either runner.

## The density benchmark: what script number 501 costs

The other half of Phase 0, and the one that decides whether today's shape can
serve an embedder at all. An embedder does not have one function; it has ten
thousand scripts, most of them idle. A Zygo zygote is currently *one function*
— `entry` imported, forked per request — so the question is what one more warm
script adds.

```bash
make bench-density ARGS="--scripts 32"
```

Thirty-two distinct scripts (a different constant in each source, so nothing
can be deduped), one image, one runtime, on the host above:

| | |
|---|---|
| 32 warm | 524.4 MB resident, 325.7 MB proportional |
| Time to warm each | 107 ms |
| **One more script, marginal RSS** | **16.39 MB** |
| **One more script, marginal PSS** | **9.98 MB** |

PSS is the honest figure — it divides each shared page among its sharers, so
thirty-two interpreters of one image are not counted thirty-two times. The
marginal cost is taken from the *slope* between the first and last checkpoint
rather than the total divided by the count, because the first zygote pays for
the interpreter's pages and the thirty-second does not.

Extrapolated, which is the number a SaaS asks first:

| scripts | PSS |
|---|---|
| 1 000 | ~9.7 GiB |
| 10 000 | ~97 GiB |

**This does not work, and that is the finding.** Ten thousand scripts is a
small platform and ninety-seven gigabytes is not a host. Idle tiering does not
rescue it either: a paused zygote is still a process holding its address
space. (In this run nothing was tiered down inside the window at all, and the
harness says so rather than reporting the warm figure as a paused one.)

Phase 1 of the embedded-runtime roadmap ([ADR 0001](adr/0001-embedded-runtime.md)) is the answer — a
zygote per **runtime** rather than per script, with the script arriving in the
`EXEC` and loaded by the forked child. Its exit criterion is this number going
flat: 1 000 distinct scripts, one runtime, resident memory that does not grow
with the script count.

## The other finding: `zygo run` pays for a cgroup it throws away

Not part of the gate, but it came out of the same work and it is the largest
avoidable cost on the one-shot path — and avoiding it took a different route
than the obvious one. The whole investigation is in
[`crates/zygo-cli/src/scope.rs`](../crates/zygo-cli/src/scope.rs); the short
version:

On an ordinary systemd user session, `zygo run` re-executes itself inside a
transient scope, because the `session-N.scope` a login lands in cannot take a
child cgroup. Decomposed on an idle Ubuntu 24.04 VM, kernel 6.8:

| | p50 |
|---|---|
| `/bin/true` | 0.30 ms |
| `systemd-run --user --scope … -- true` | 5.14 ms |
| `zygo --version` | 3.23 ms |
| `systemd-run --user --scope … -- zygo --version` | 13.72 ms |
| `systemd-run --user --scope … -- zygo run …` | 41.21 ms |
| **`zygo run …`** | **50.31 ms** |

A fresh cgroup is ~10 ms of overhead before Zygo does anything, and the cgroup
*operations* are not where it goes: every `mkdir` and `subtree_control` write
is under 0.15 ms, while migrating a process into a freshly created cgroup is
5.6 ms on its own.

**The obvious fix was built, and it cannot work.** Putting `zygo.slice` under
`user@$UID.service` — which systemd already delegates — makes the layout
persist, and the layout does work there: tenants get `memory.max`, `pids.max`
and `cpu.max`, and the limits bite (exit 137 on OOM, threads refused at the
pids cap, both verified). But *getting into it* is forbidden by cgroup v2's
delegation containment rule, which requires write access to the common
ancestor of source and destination. That ancestor is `user-$UID.slice`, which
is `root:root 644` on both hosts checked — an aarch64 VM on 6.8 and a
Raspberry Pi 5 on 6.5. The code was removed rather than left as something that
never fires.

Two other candidates were measured and rejected: `systemd-run
--slice=zygo.slice` still pays for the scope, and caching the `zygo doctor`
host probe made no difference at all (44.92 ms against 45.33 ms over 90 runs
each, interleaved). The probe cache was kept anyway, because the supervisor
probes once per function warmed and an embedder warming five hundred scripts
was paying five hundred times.

What does work is reusing a cgroup that is already delegated and already
built, which is exactly what the supervisor holds — so now `zygo run` hands a
one-shot sandbox to a running supervisor. The client sends the spec and its
flags, passes its own three streams over `SCM_RIGHTS`, forwards its terminal's
signals and waits; the supervisor starts the sandbox in the cgroup it already
has. Exit code, stdin, both output streams, `--outcome`, the deadline and OOM
all come back as they would have. Same VM, same command, forty runs a round,
two rounds:

| | p50 | p90 |
|---|---|---|
| `zygo run …`, making its own scope | 45.9 / 43.0 ms | 50.1 / 49.5 ms |
| `zygo run …`, a supervisor takes it | 30.4 / 29.0 ms | 32.9 / 33.8 ms |

A third of the command, gone. None of this ever touched an embedder: a
supervisor pays for its cgroup once at start-up. It was `zygo run` at a
terminal that paid every time, and now only on a machine where nothing else
is running.

## Verdict

Phase 0 of the embedded-runtime roadmap ([ADR 0001](adr/0001-embedded-runtime.md)) passes its gate on both
hosts: **60×** on an M1 VM against `zygo run`, and **100×** on a Raspberry Pi
against `kern`, the fastest one-shot runner in the field. The bar was 10×.

It also produced the reason Phase 1 is first: the warm fork is worth sixty to a
hundred one-shot runs, and today you can only have about five hundred warm
scripts per host. The thing that makes Zygo worth embedding is the thing it
cannot yet do at an embedder's scale.

And it produced two defects, neither of them in the warm path:

* `zygo run` on an ordinary systemd session pays ~34 ms for a cgroup it throws
  away, and the obvious fix is forbidden by cgroup delegation containment.
* `zygo run` had no per-phase timing, so a 20 s p99 on the Pi looked like a
  Zygo defect for a day. It has one now, and the p99 was the SD card.

Both are in the one-shot path, which an embedder does not use and a developer
at a terminal does.
