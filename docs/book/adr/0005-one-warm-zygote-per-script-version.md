# ADR 0005 — One warm zygote per script version, and what evicts it

*A record of the decision as it was taken; the numbers in it are as of its date. Today's numbers are in [chapter 25](../25-performance.md).*

**Status:** accepted, 2026-09-23. Answers the open question the first adoption
report — a consumer's own write-up, not in this repository — ends on, with numbers from
`tests/linux/bench_density.py` on the 2-core, 4 GB Lima VM.

## Context

The shape that forced this decision is a multi-tenant application: arbitrary tenant
code that changes whenever somebody presses Save, hundreds of projects, each
with its own mounts and egress allowlist, and per-run secrets. Such a product
reaches for `zygo run` first — a sandbox per event, 171 ms from a Mac shell of
which ~23 ms is the sandbox — because the mapping is obvious, and then asks
whether `zygo serve` is meant for its shape at all. Three things had to be decided:

1. Is **one warm zygote per script version** the intended shape?
2. What is the **eviction policy** when a server has four hundred projects?
3. Can per-run secrets and per-project mounts vary *under* one warm zygote,
   or do they force one zygote per (script, project)?

## What was measured

`make bench-density` on the Lima VM (Ubuntu 24.04, 6.8, aarch64, 2 vCPU,
3.8 GiB), `python:3.12-slim`, a hundred *distinct* Python handlers, each
served as its own function:

| | per warm script | 100 scripts |
|---|---|---|
| resident (RSS) | 21.4 MB | 2 143 MB |
| proportional (PSS: shared pages divided among sharers) | **11.3 MB** | **1 141 MB** |
| time to warm (`zygo serve`, round trip) | **109 ms** | 10.9 s |

PSS is the honest per-script figure: a hundred zygotes of one image share
nearly all of the interpreter's pages, and the slope is measured between
checkpoints rather than from the total. So on this VM about **300 warm
Python scripts fit in 4 GB** with nothing else running, and a *thousand*
would need 11 GB. The cost of evicting one and warming it again is the
109 ms above plus the handler's own imports; a paused script (past
`idle_timeout`) keeps its pages resident and costs one write to wake.

The same hundred scripts through **one runtime pool** (`--pool`), which is
the shape Phase 1 of [the roadmap](../../../ROADMAP.md) built for exactly this question:

| | one pool |
|---|---|
| resident, whatever the count | 29.5 MB, one zygote |
| what one more script adds | **0 kB** |
| second call, over the API, p50 / p99 | 1.95 ms / 2.38 ms |
| a warmed *function* on the same host, p50 / p99 | 1.36 ms / 1.57 ms |
| the pool's cost over a warmed handler | +0.6 ms p50, +0.8 ms p99 |

## Decision

**Yes, one warm zygote per script version is the intended shape for a
consumer whose scripts have imports worth amortising — and a runtime pool is
the intended shape for one whose scripts do not.** The two are not in
competition; the numbers say which applies:

* A version whose handler imports something expensive (an ML model, a
  large client library) pays that once per zygote and 1.4 ms per call. Its
  cost is 11 MB PSS resident while warm, and it is warm only while called.
* A version that is a few lines over the standard library — most such
  scripts — pays 0.6 ms more per call in a pool and *nothing* to be
  resident, because it is not: the pool holds the interpreter and the
  dependency set, and the script arrives with the request.

A consumer with four hundred projects does not have four hundred warm
zygotes; it has as many as were called in the last `idle_timeout`, and
`zygo ps` shows how many that is.

**The eviction policy is `idle_timeout` and `cold_after`, per function, and
nothing else in the product.** A version nobody has called for
`idle_timeout` (default ten minutes) is *paused*: frozen, resident, one
write to wake. Past `cold_after` (default an hour) it is *cold*: the
sandbox is dropped and only the spec kept, and the next call pays the
109 ms plus imports. An LRU over warm scripts, as `examples/workflow-engine`
keeps, is the *consumer's* policy layered on top — it decides which
versions to `serve` at all, and `if_changed=True` makes asking free — and
the product does not second-guess it with a global cap. `max_warm` is a
pool's ceiling on its own zygotes under load, a different knob. What the
product owes the consumer is the number to set the timeouts by, which is
the table above, and a `429` rather than a swap storm when the host is
full, which it has (capacity is a per-host budget).

**Per-run secrets vary under one zygote; per-project mounts and allowlists
do not.** Secrets are already per request by construction: the names are
declared on the function, the values arrive with the request and exist as
`/run/secrets/<name>` only while it runs, never in the zygote. Mounts and
the network allowlist are the sandbox's namespaces, built once at warm-up,
so a project's mounts and allowlist are the *function's*, and one script
version shared by two projects with different mounts is two functions:
`{project}-{digest}`. That is the naming the guide's worked example uses,
and it is what the adopter's model already implies — a script belongs to a
project.

## Consequences

* The guide documents the warm path as the production shape for a
  multi-tenant consumer, with the worked example, next to the one-shot
  section that consumers find first.
* `bench-density` is the number to re-measure when the interpreter, the
  image or the kernel changes; the ADR carries this VM's, and a Linux host
  with more memory scales it linearly.
* Not decided here, and worth an ADR of its own if a consumer needs it: a
  *global* warm budget (`max_warm_total`) that evicts the least-recently
  called function when the host is near its memory limit. Nothing measured
  above says it is needed before `idle_timeout` and a `429` do their work.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [ADR 0004 — A supervisor upgrade re-warms](0004-no-supervisor-reexec.md) · [Contents](../README.md) · **Next: [ADR 0006 — The memory limit is each request's](0006-memory-limit-per-request.md) →**
