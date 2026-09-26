# ADR 0001 — Zygo is the embedded script runtime

*A record of the decision as it was taken; the numbers in it are as of its date. Today's numbers are in [chapter 25](../25-performance.md).*

**Status:** accepted, 2026-09-21. This is the decision
[the roadmap](../../../ROADMAP.md) is the plan for; the roadmap
says *what* and *when*, and this says *who for* and *what that costs*.

## Context

Zygo can be described two ways, and only one of them is a product.

**"A faster Docker."** One static binary, no daemon, no root, OCI images, a
sandbox in 18 ms instead of 300–1000. This is true, it is what the README led
with, and it is a crowded field: [kern](https://github.com/getkern/kern) does
the same thing and starts a box in single-digit milliseconds, nono confines a
process without any of the machinery, microsandbox gives a hardware boundary
per call, and Docker itself now ships sandboxes. Zygo is not the fastest of
these and has no reason to become it.

**"The runtime a workflow engine embeds."** A process that has ten thousand
scripts in a database, runs each a few times a minute, and can afford neither
a container nor a cold interpreter per run. Nobody is serving that shape. The
warm fork — a request costs a `fork()` of a process that has never served one,
so it is as clean as a fresh container and as cheap as a fork — is the only
primitive built for it, and it is the only thing here that another project
would have to rebuild rather than out-optimise.

The second is the product. Everything about the first is a means to it: the
one-shot sandbox exists because the warm one is built out of it.

## Decision

**The target user is an embedder**: a workflow engine (Windmill, n8n,
Temporal, Kestra), a SaaS running customer-written plugins, an agent platform
running generated code. Not a developer running a command.

What that decides, in order of how often it comes up:

1. **The warm path is the product.** The README leads with it, the benchmarks
   compare against what an embedder would otherwise do rather than against a
   budget, and a change that costs the warm path milliseconds needs a reason
   that a change costing the one-shot path milliseconds does not.
2. **The API is the surface, not the CLI.** An embedder's worker talks to
   `zygo api` or links `zygo-core`. The CLI stays, because it is how the thing
   is debugged and demonstrated, but it stops being what the design is shaped
   around.
3. **Multi-tenancy is a first-class concern**, because an embedder's customers
   are not each other's. Tenant-versus-tenant isolation gets the effort that a
   per-request hardware boundary does not.
4. **One process per worker, one host.** No scheduler, no control plane, no
   Helm chart. The embedder already has all three.

## What is deprioritised, and why

Written down so nobody re-opens them by accident. This is the same list as
[the roadmap](../../../ROADMAP.md)'s, with the reasoning kept here.

- **Services, ports, compose, restart policies.** A different product. kern and
  Docker both do it; Zygo runs functions, not servers.
- **Warm functions on `vm` and `gvisor`.** A second warm path to keep correct,
  benchmark and defend, against the one everything rests on. See
  [ADR 0002](0002-warm-paths-stay-on-ns.md).
- **macOS shim latency.** A Mac is where Zygo is developed and tested; Linux
  is where it runs. The hop costs about 22 ms per *command*, which a developer
  notices and a production embedder never sees. Keep it working.
- **Windows.** Embedders deploy on Linux.
- **A hosted service.** It would compete with the people this is for.

## Exit criteria

Each phase of [the roadmap](../../../ROADMAP.md) has one; they are the same criteria, here,
so that a phase cannot be declared done by whoever is doing it.

| Phase | Done when |
|---|---|
| 0 Prove the wedge | the warm fork's p50 is at least 10× under the best one-shot runner on an import-heavy script, measured on one host, published with the commands |
| 1 Runtime zygotes | 1 000 distinct scripts on one runtime, p99 under 5 ms after each script's first call, resident memory flat in the number of scripts |
| | **Met.** p99 **2.92 ms** with a different script on every request (`zygo bench warm --pool --scripts 1000`), and **29.4 MB in one zygote** for a thousand scripts — a slope of **0.0 kB** per script, against 9.98 MB for a zygote each. Both in [`bench-embed.md`](../25-performance.md#the-embedders-benchmark), both reproducible by `make bench` and `make bench-density ARGS="--pool --scripts 1000"`. *Re-measured 25 September 2026: 3.20 ms on Docker Desktop's Linux 5.10 VM, 11.4 ms on the Lima VM's stock Linux 6.8 and 3.3 ms there with `favordynmods`; [chapter 25](../25-performance.md#why-1-in-100-is-slow-on-newer-kernels) explains why the tail depends on the kernel, not the pool.* |
| 2 Embedder API | a plugin host can be written against the HTTP API alone — no `sandbox.toml`, no files on the Zygo host |
| 3 Runtimes | the same plugin host runs a Python and a JavaScript plugin through one API with one set of limits |
| 4 Deployability | `kubectl apply` of the example on a stock cluster gives a green readiness probe and a passing `zygo agent test` inside the pod |
| 5 Hardening | the threat model has no "untested" rows for any multi-tenant claim |
| 6 Integrations | one external project runs Zygo in production for user scripts, publicly |

**Phase 0 is a real gate.** If the ratio is not there, the wedge is not there,
and this ADR is wrong rather than early.

## Consequences

- The README, the comparison doc and the benchmarks were all written for the
  first framing and have been rewritten for the second. That work is done.
- `docs/bench-embed.md` is the number this decision rests on, and it has to be
  reproducible by a reader on their own host — hence `make bench-embed`.
- The current zygote-per-function shape does not serve ten thousand scripts.
  `make bench-density` is how much it does not, and Phase 1 is the answer.
- Work that is good for a developer at a terminal and bad for an embedder now
  loses. The macOS hop is the standing example.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [26. Why it is built this way](../26-decisions.md) · [Contents](../README.md) · **Next: [ADR 0002 — Warm paths stay on `ns`](0002-warm-paths-stay-on-ns.md) →**
