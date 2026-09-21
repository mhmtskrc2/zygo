# Using Zygo instead of Docker — a field report

Source: an unscripted session spent replacing `docker` with `zygo` for ordinary
work. Plan: [todo.md](../todo.md) · Design: [ahmed.md](../ahmed.md)
Date: 20 September 2026

Nothing here was run from the test suite. Every command below is one a user
would type, run in the order a user would reach for it, and every number was
measured in the session rather than carried over from
[the PoC report](poc-report.md).

## Verdict

Zygo replaces Docker for the work it claims: one-shot runs, mounted tool
invocations, and warm function calls. Output is byte-identical to Docker's for
mounts, environment, stdin and exit codes. The limits bind, the egress
allowlist holds, and the full `sandbox.toml` → `up` → `exec` chain works
end to end including a venv build and a real HTTPS call.

Two findings are worth acting on before a launch. One is architectural: the
per-request cgroup is responsible for the entire latency tail, and with it the
project misses its own p99 acceptance criterion. The other is a one-line bug
that stops `zygo bench` from running at all on an ordinary Linux login, which
means the command that would prove the performance claim is the command that
fails first.

## Environment

```
host      macOS 24.1, Apple Silicon
VM        Lima `zygo` instance, kernel 6.8.0, 5 cores / 4 GB
zygo      release build, 5.5 MB, 45 s from clean
docker    20.10.17, in its own VM
image     python:3.12-slim (4 layers, 41.5 MB)
```

Both runtimes pay for a VM on this host, so the comparison is fair between
them and pessimistic in absolute terms. Where a number would be misleading on
its own it is given beside the one it should be read against.

## What was measured

Per-request cost, empty workload, warm image:

| Path | Per request |
|---|---|
| `docker run --rm` (cold) | 487 ms |
| `zygo run` (cold, through the macOS shim) | 180 ms |
| `docker exec` (container already up) | 137 ms |
| `zygo exec` (macOS CLI) | 100 ms |
| Zygo HTTP API, called from inside the VM | 2.08 ms p50 |
| Inside the sandbox, as `zygo stats` reports it | 0.5 ms p50 |

Cold one-shot is 2.7× faster than Docker. The warm path over the HTTP API is
66× faster than `docker exec`. Both promises hold.

## What worked

* **Docker-equivalent invocation.** `--mount`, `--workdir`, `--env` and stdin
  produced output byte-identical to `docker run -i -v -w -e`. Exit codes pass
  through, including 42. stdin works without an equivalent of Docker's `-i`.
* **Limits bind.** A memory hog against `--mem 128M` died with 137. A fork bomb
  against `--pids 16` was refused at exactly 15 children. `--timeout 3s` killed
  an infinite loop in 3.1 s with 137, which matches Docker and closes the open
  question in §2.9 of the roadmap. The host lost nothing: 2.8 GB still free.
* **Egress is real.** With `--net egress --allow example.com:443`, the allowed
  host answered 200 and `api.github.com` was blocked. With the default
  `network = "none"`, a raw socket to `1.1.1.1:443` got `EPERM`.
* **The whole chain.** A two-function `sandbox.toml` came up with `zygo up` in
  5.5 s, of which the venv build was most; `exec fetch` then did a real HTTPS
  request with `requests==2.32.3` through the allowlist. `down` stopped exactly
  the two functions the spec declared and left ad-hoc ones alone, as documented.
  No zygote processes and no cgroups were left behind.
* **Failures read well.** A handler raising `KeyError` gave exit 1, the
  traceback on stderr, and one line per request in `zygo logs` with the
  traceback indented under it.
* **Concurrency.** Eight parallel `exec` calls finished in 253 ms wall clock
  against roughly 800 ms if they had been serialised.

## The per-request cgroup is the whole tail

Sustained load showed a spike of 30–40 ms every ~30 requests, regular enough to
be systematic. The first hypothesis — generation-2 garbage collection in the
zygote — was wrong, and measuring said so: a probe handler reporting
`gc.get_stats()` showed gen-2 never ran across 210 requests, and the handler's
own `wall_ms` stayed at 0.42–0.54 ms through every spike. The time was being
spent outside the handler.

`zygo bench warm --no-cgroup` settles it. Both runs are 1500 requests at
200 req/s, back to back on the same host:

| | with per-request cgroup | without |
|---|---|---|
| p50 | 1868 µs | 1223 µs |
| p90 | 7860 µs | 1556 µs |
| p99 | 15608 µs | 1980 µs |
| p99.9 | 38010 µs | 2519 µs |
| max | 51639 µs | 2819 µs |
| `admit` phase p99 | 11310 µs | 85 µs |
| acceptance | **p99 FAIL** | **p99 PASS** |

The per-request cgroup costs 645 µs at p50 and 13.6 ms at p99. Open question
**A2** was closed in favour of per-request on the strength of PoC 3's 97 µs,
which was measured sequentially at low load; under sustained load the tail cost
is two orders of magnitude higher than that figure suggests. The absolute
numbers are inflated by nested virtualisation, but both rows come from the same
host minutes apart, so the ratio stands.

A2 should be reopened. The options are a per-tenant cgroup as the default, a
pool of request cgroups that are reused rather than created and destroyed, or
at minimum documenting `--no-cgroup` as a supported production choice with the
isolation it gives up spelled out.

## Bugs and rough edges

**`zygo bench` never enters a delegated scope.** On an ordinary systemd login
it dies with `cannot create the cgroup …/session-4.scope/zygo.slice`.
`needs_a_cgroup` in `crates/zygo-cli/src/scope.rs:36` lists `Run`, `Serve`,
`Up` and `Supervisor`, but not `Bench` — and `bench` warms a sandbox of its
own. Wrapping the call in `systemd-run --user --scope -p Delegate=yes` makes it
work. A one-line fix, but the command it breaks is the one that demonstrates
the product's central claim.

**`zygo run <image>` with no command refuses to run.** `zygo run --help` says
the command "Defaults to the image's entrypoint and cmd". In practice it fails
with `fn.run: nothing to run`, and `--dry-run` stops at the same place. The
image config is already parsed: `PATH` and `LANG` come out of the image and
merge correctly. Only the entrypoint and cmd go unused. `docker run --rm myimage`
is one of the most common things a Docker user types.

**`-v` means verbose here and volume in Docker.** Typing
`zygo run -v $PWD:/src image` yields `invalid image reference: repository has an
empty path component`, which is accurate and sends the reader to the wrong
place. An image reference shaped like `host:guest` should be recognised and
answered with a pointer to `--mount`.

**`stop --all` prints raw `limactl` output on macOS.** Stopping the VM when the
user says they are finished is deliberate and documented at `shim.rs:329`. What
reaches the terminal is about thirty lines of Lima's internal logging, one of
them `level=error`, on a teardown that succeeded. The subprocess output should
be swallowed and replaced with one line.

**`--mem 64M` alone is refused.** Scratch defaults to 64M and must be smaller
than memory, so the flag cannot be used by itself. The error explains why but
not what to do; either shrink scratch automatically or name the flag to pass.

**The first traceback frame belongs to Zygo.** A raising handler reports
`/zygo/agent.py line 250, in run_request` above the user's own frame. Zygo's
protocol does not promise otherwise, so this is not a violation — but showing
the developer their own line first is the argument a code-first runtime makes
against every alternative, and trimming the harness frame in the child is small.

**Disk.** Three small images totalling 47 MB of layers left a 230 MB data
directory once flatten caches and venvs were counted. Worth confirming that
`image prune` reaches both.

## macOS: the headline number is not reachable from the CLI

Inside the sandbox a request costs 0.5 ms. From the macOS CLI the same call
costs 100 ms, and the missing 99.5 ms is the cost of executing a command
through Lima. That is the shim working as designed, not a defect — but the
consequence is that on macOS `zygo exec` and `docker exec` feel the same, and
the only way to see the advantage is the HTTP API or the library. The README
should say so plainly; otherwise the first person to try the flagship feature
on a Mac measures the shim and concludes the benchmark was optimistic.

## Scope, honestly stated

Port publishing, detach, `build` and `cp` have no equivalent, all of them
declared out of scope in §1.4 and §1.6 of the design document. The code is
consistent with the document, and so is the README, whose first line says
"function-shaped code". The unqualified promise is in the design document's
own one-sentence summary (§1), which calls Zygo a drop-in equivalent of
`docker run` without saying for what — the only place a reader meets the claim
bare.

## Suggested order

1. Add `Bench` to `needs_a_cgroup`. One line, and it unblocks measurement.
2. Reopen A2 with the numbers above. The default configuration misses the
   project's own p99 target, and this is the first thing a reader will test.
3. Default `zygo run` to the image's entrypoint and cmd, which are already in
   hand, and add the `-v` hint. Both are first-five-minutes friction.
4. Quiet the `limactl` output, trim the harness traceback frame, and qualify
   the design document's drop-in claim.

## Since this was written

The session above is a dated record and the findings are left as they were
found. What has happened to them since lives in
[todo.md](../todo.md) under "After test", which was written by checking every
claim here against the code. Two of those claims did not survive the check:

* **`zygo top` was reported as a placeholder.** It landed the next morning,
  the rate columns and all. The finding was true when it was written and was
  stale by the time it was read, which is the ordinary fate of a report
  against a moving tree.
* **The README was reported as needing a qualifier on "drop-in".** It does not
  contain the phrase. The design document does, and that is where the
  qualifier belongs; the paragraph above is corrected.
