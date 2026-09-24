# 26. Why it is built this way

Some choices in Zygo look strange until you know the reason: why the `vm`
backend has no warm functions, why there is no Deno agent, why an upgrade
throws the warm sandboxes away. Each of these was written down as an
*Architecture Decision Record* (ADR): a short, dated note of a question, the
answer, and what would change the answer. This chapter explains the five ADRs
in plain words; each section links to the full record in [adr/](adr/).

## The five decisions at a glance

| ADR | The question | The answer, in one line |
|---|---|---|
| [0001](adr/0001-embedded-runtime.md) | Who is Zygo for? | Programs that embed it to run many scripts — not a person typing commands. |
| [0002](adr/0002-warm-paths-stay-on-ns.md) | Should `vm` and `gvisor` have warm functions? | No. Warm functions are an `ns` feature. |
| [0003](adr/0003-no-deno-or-bun-agent.md) | Should there be Deno and Bun agents? | No, until an embedder asks with a measurement. |
| [0004](adr/0004-no-supervisor-reexec.md) | Can an upgrade keep the warm sandboxes? | No. An upgrade drains, restarts and re-warms. |
| [0005](adr/0005-one-warm-zygote-per-script-version.md) | One warm zygote per script version? What evicts it? | Yes for heavy scripts, a runtime pool for light ones; timeouts evict. |

```text
  how the five decisions depend on each other
  ─────────────────────────────────────────────────────────────────
   0001  the product is the warm path, for an embedder
     │
     ├──► 0002  keep one warm path (ns), do not build three
     ├──► 0003  keep the agent list small enough to test fully
     ├──► 0004  restarts re-warm; keep one copy of the state
     └──► 0005  how a real embedder should use warm paths
  ─────────────────────────────────────────────────────────────────
```

Each ADR ends with the facts that would reopen it. None of them is "forever".
They say "not until this is true".

## How to read an ADR

An ADR is a historical record. It is written once, at the time of the decision,
and not rewritten later. If a decision changes, a new ADR replaces the old one.
So an ADR can mention files or plans that have since moved. The sections below
are today's plain-English summary; the ADR itself is the exact wording.

## ADR 0001: Zygo is the embedded script runtime

### The question

Zygo can be described in two ways. One is "a faster Docker": one static binary,
no daemon, no root, OCI images, a sandbox in 18 ms instead of 300–1000 ms. That
is true, but the field is crowded — [kern](10-similar-projects.md#kern),
[nono](10-similar-projects.md#nono), [microsandbox](10-similar-projects.md#microsandbox)
and Docker's own sandboxes all compete there. The other is "the runtime a
workflow engine embeds": a program with ten thousand scripts in a database,
each run a few times a minute. The question was which of the two is the
product.

### The decision

The product is the second one. The target user is an *embedder*: a workflow
engine (Windmill, n8n, Temporal, Kestra), a SaaS running plugins its customers
wrote, or an agent platform running generated code. Four things follow from
that, in order of how often they come up:

1. **The warm path is the product.** A change that costs the warm path
   milliseconds needs a much better reason than one that costs the one-shot
   path the same.
2. **The API is the surface, not the CLI.** The CLI stays for debugging and
   demos.
3. **Tenants are strangers to each other**, so isolation between tenants gets
   the effort.
4. **One process per worker, one host.** No scheduler, no control plane, no
   Helm chart — the embedder has those already.

### Why

Nobody else serves the "ten thousand scripts, each run often" shape. A warm
fork is the tool built for it: each request is a `fork()` of a process that
never served a request, so it is as clean as a fresh container and as cheap as
a fork. That is the one part another project would have to rebuild rather than
just make faster. The one-shot sandbox still matters, but mostly because the
warm sandbox is built out of it.

### What it costs

Some things are put aside on purpose, so nobody reopens them by accident:
services, ports, compose files and restart policies; warm functions on `vm`
and `gvisor` (see [ADR 0002](#adr-0002-warm-functions-stay-on-ns)); the macOS
shim's latency; Windows; and a hosted service, which would compete with the
very users Zygo is for. Work that is good for a developer at a terminal but bad
for an embedder now loses. The macOS hop is the standing example: a developer
notices it, a production embedder never sees it.

### The gates, and how they were met

The ADR sets a gate for each phase of the roadmap, so that a phase cannot be
declared done by whoever did the work.

```text
  phase                        done when …                                    status
  ─────────────────────────────────────────────────────────────────────────────────
  0 prove the wedge            warm fork ≥ 10× under the best one-shot runner  met
  1 runtime zygotes            1 000 scripts, p99 < 5 ms, memory flat          met
  2 embedder API               a plugin host needs only the HTTP API
  3 runtimes                   Python and JavaScript through one API
  4 deployability              kubectl apply → green probe + agent test passes
  5 hardening                  no "untested" rows for multi-tenant claims
  6 integrations               one outside project runs it in production
  ─────────────────────────────────────────────────────────────────────────────────
```

Phase 0 was a real gate: without the 10× ratio, the ADR would be wrong, not
early. It passed at 60× and 100×. Phase 1 passed with a p99 of 2.92 ms and
29.4 MB for a thousand scripts in one zygote.
[Chapter 25](25-performance.md#the-embedders-benchmark) has both measurements.

### What would reopen it

The ADR names no single trigger; it stands as long as its Phase 0 gate holds.
If the warm fork stopped being many times cheaper than the best one-shot runner
on an import-heavy script, the idea behind the product would be wrong.
[Full ADR 0001](adr/0001-embedded-runtime.md).

## ADR 0002: Warm functions stay on `ns`

### The question

Zygo has three isolation backends behind one flag: `ns` (namespaces, cgroups,
seccomp and Landlock), `gvisor` (a kernel written in user space) and `vm`
(libkrun, a real hardware boundary). All three run one-shot sandboxes from the
same spec. Only `ns` runs warm functions. The question was whether the other
two should get warm functions too, or whether the gap was just unfinished work.

### The decision

Warm functions are an `ns` feature. `vm` and `gvisor` run one-shot sandboxes
and refuse warm modes with a reason. That refusal is **the design**, not a gap.
Networking on `vm` is refused for the same reason. The error messages point at
this ADR instead of saying "not yet".

### Why

Both gaps come from how the backends are built, not from missing time:

- **The agent gets its control socket as an inherited file descriptor.** An OCI
  runtime such as `runsc` closes everything except stdin, stdout and stderr,
  and a guest VM inherits nothing from the host at all. `gvisor` would need
  `runsc exec` instead of `setns`; `vm` would need the protocol carried over
  vsock and a supervisor inside the guest.
- **Networking on `vm`** would need the VM monitor inside Zygo's own network
  namespace, and the allowlist applied to a guest interface. That is a second
  copy of the network code with none of the first copy's tests.

Each is weeks of work. Worse, each makes a second warm path that must be kept
correct, measured and defended — next to the one the whole product rests on.

```text
                      one-shot     warm function    network
  ─────────────────────────────────────────────────────────────
  ns                  yes          yes              yes
  gvisor              yes          refused          —
  vm                  yes          refused          refused
  ─────────────────────────────────────────────────────────────
```

### What it costs

`--isolation vm` is a hardware boundary for work that fits a one-shot sandbox:
an untrusted build, a single tool call, a job with an input and an output. It
is not for a warm function serving many requests. An embedder who needs a
hardware boundary *per tenant* is not served by Zygo today;
[chapter 10](10-similar-projects.md#microsandbox) points at an alternative.
The effort goes instead into hardening `ns`, which stays one kernel away from
the host ([chapter 23](23-security.md)).

### What would reopen it

An embedder who asks for a hardware boundary per tenant, with a workload that
can afford about 100 ms of boot per request. That is a different product shape
from the warm fork. It should be thought through as its own thing, not bolted
onto this backend just because the flag already exists.
[Full ADR 0002](adr/0002-warm-paths-stay-on-ns.md).

## ADR 0003: No Deno or Bun agent until an embedder asks

### The question

Zygo ships two runtime agents, Python and Node. A third-party agent can be added
with `agent = { agent = "/path/in/sandbox" }`. Deno and Bun are the obvious
next two: both are popular, both start fast, and both would look good in a
table. The question was whether to write agents for them.

### The decision

No Deno or Bun agent. Anyone who wants either has two supported paths. One is
a **warm-exec pool**, which works today with no agent at all, because both
start in a few milliseconds:

```toml
[runtime.deno]
image = "denoland/deno:alpine"
cmd   = ["deno", "run", "--allow-none"]
```

The other is to write their own agent against the protocol and check it with
`zygo agent test` ([chapter 18](18-writing-an-agent.md)).

### Why

An agent is cheap to write and expensive to *keep*. Each one is a fork boundary
with four rules that must be exactly right: the child never returns to the
parent's loop, nothing runs before `GO`, a broken frame is reported and not
fatal, and every `EXEC` gets exactly one answer. Each rule has been broken at
least once in this repository, by someone who knew the language well. Each
agent also needs its own answer to the per-request seccomp filter; Node's
fallback was once *stricter* than the filter it replaced, which the seccomp
matrix caught. And each agent must sit in `make conformance`, the seccomp
matrix and the image matrix — a test nobody runs is only a claim.

Meanwhile Deno and Bun both run JavaScript, which the Node agent already
serves. Someone who asks for Deno usually wants its permission model, its
standard library or `deno.json`, not "something other than V8".

### What it costs

The comparison table says two agents, not four — the honest number. The test
matrices stay small enough to run on every change. A Deno user starts with a
three-line `cmd` and no protocol. Each request is slower than a fork from a
warm heap by the cost of an `execve`, not by the cost of an interpreter
start-up. The warm-exec pool gives up streaming, `progress()`, workspaces and
per-request tenant limits.

### What would reopen it

Any one of three facts — none of them a guess about the future:

- **An embedder asks**, with a workload where the warm-exec pool's per-request
  `execve` is measurably too slow. The measurement is the argument, not the
  runtime's popularity.
- **A dependency set needs it**: supporting `deno.json` or `bun.lockb` in
  `POST /deps`, which is smaller work than an agent.
- **Someone writes an agent and it passes** `zygo agent test`, including the
  child-filter checks. The project would rather link to it than rewrite it.

[Full ADR 0003](adr/0003-no-deno-or-bun-agent.md).

## ADR 0004: A supervisor upgrade re-warms; there is no `--reexec`

### The question

Upgrading Zygo replaces the supervisor process, and its warm sandboxes go with
it. Could the new binary `exec` over the old one and keep them, so an upgrade
costs nothing? `exec` is the right tool to ask about: it keeps the process id,
so the sandboxes' init processes stay children of the same process, and nothing
dies just because the binary changed.

### The decision

No `zygo api --reexec`. A supervisor upgrade is a restart: drain, exit, start,
re-warm. `min_warm` and a rolling update with `maxUnavailable: 0` make it
invisible to callers, and both already exist
([chapter 16](16-production.md#rolling-out-without-dropping-a-request)).

```text
  an upgrade, with two replicas and maxUnavailable: 0
  ──────────────────────────────────────────────────────────────────────
  old supervisor   serving ████████████ POST /drain ▓▓▓ finish ▓▓ exit
  new supervisor              start ░░ re-warm ░░ ready ████████ serving
                                        ~500 ms per zygote
  callers see:     no dropped request; at worst, slower ones while warm-up runs
  ──────────────────────────────────────────────────────────────────────
```

### Why

What would have to cross the `exec` is much more than process ids:

- **Every file descriptor, on purpose.** Each warm function holds an agent
  socket; each sandbox holds seven namespace descriptors, a `/run/secrets`
  descriptor and a cgroup descriptor. All are close-on-exec (`CLOEXEC`) by
  design — once after a real bug, where thirteen descriptors leaked into every
  tenant program. A hand-over turns "everything closes unless something says
  otherwise" into "everything closes unless this list says otherwise", and the
  list changes per function, per sandbox and per release.
- **All the state that is not a descriptor**: every function's spec, counters,
  logs, secrets, script leases, queue counts and idle clocks. That means a
  second, versioned copy of the whole supervisor state, which could go wrong
  silently.
- **The requests in flight.** Their answers are owed to client connections
  tracked by a thread that `exec` destroys. Keeping them means handing over the
  client sockets, each request's cgroup, child and deadline, and rebuilding the
  router. Anything less drops requests — the one thing the feature was for.

### What it costs

A pool re-warms in about **500 ms per zygote** for `python:3.12-slim` (502 ms
measured for a pool, 508 ms for a function); a Node zygote is ready in
15–43 ms. So an upgrade costs `min_warm × warm-up` of cold pool per replica,
once. A single-replica deployment has a short window where requests are slow,
not failed; two replicas remove it, and the Kubernetes example uses two. In
return, descriptors stay `CLOEXEC` with no exceptions, and Zygo keeps only one
copy of its state: the running one.

### What would reopen it

- **A warm-up of tens of seconds**, not half of one. Even then, the better fix
  may be to make that warm-up faster.
- **A deployment that cannot have two replicas**, with a latency budget a cold
  start breaks. `--reexec` is one answer; a second supervisor on the same host
  behind a load balancer is another, and needs nothing new.

What does **not** reopen it: wanting upgrades to be free. They are already
free of dropped requests, which is the part that matters.
[Full ADR 0004](adr/0004-no-supervisor-reexec.md).

## ADR 0005: One warm zygote per script version, and what evicts it

### The question

The first product to use Zygo as its default sandbox was a multi-tenant application: tenant code
that changes whenever someone presses Save, hundreds of projects, each with its
own mounts, network allowlist and per-run secrets. It started with `zygo run`,
a sandbox per event, and asked three things. Is **one warm zygote per script
version** the intended shape? What **evicts** warm zygotes when a server has
four hundred projects? And can secrets and mounts vary under one warm zygote?

### What was measured

A hundred distinct Python handlers on the Lima VM (Ubuntu 24.04, kernel 6.8,
aarch64, 2 vCPU, 3.8 GiB), `python:3.12-slim`:

| | per warm script | 100 scripts |
|---|---|---|
| memory (RSS) | 21.4 MB | 2 143 MB |
| memory, shared pages split fairly (PSS) | **11.3 MB** | **1 141 MB** |
| time to warm | **109 ms** | 10.9 s |

| The same hundred scripts in one runtime pool | |
|---|---|
| memory, whatever the count | 29.5 MB, one zygote |
| what one more script adds | **0 kB** |
| second call over the API, p50 / p99 | 1.95 ms / 2.38 ms |
| a warmed function on the same host, p50 / p99 | 1.36 ms / 1.57 ms |
| the pool's extra cost | +0.6 ms p50, +0.8 ms p99 |

So about **300 warm Python scripts fit in 4 GB** on that VM, and a thousand
would need 11 GB. [Chapter 25](25-performance.md#memory-per-warm-script-on-a-smaller-vm)
explains RSS and PSS.

### The decision, part 1: which shape

**One warm zygote per script version is right when the script's imports are
worth paying once** — an ML model, a large client library. It pays those once
and about 1.4 ms per call, and costs 11 MB while warm. **A runtime pool is right
when the script is a few lines over the standard library**, like most such
scripts. It pays 0.6 ms more per call and *nothing* to stay resident, because
the script is not kept; it arrives with each request.

```text
  does the script import something expensive?
  ─────────────────────────────────────────────────────────────
     yes ──► one warm zygote per script version
             pays the imports once · ~1.4 ms a call · ~11 MB warm
     no  ──► a runtime pool
             +0.6 ms a call · 0 kB per extra script
  ─────────────────────────────────────────────────────────────
```

### The decision, part 2: what evicts

Eviction is `idle_timeout` and `cold_after`, per function, and nothing else in
the product. A version nobody called for `idle_timeout` (default ten minutes)
is *paused*: frozen, still in memory, one write to wake. Past `cold_after`
(default an hour) it is *cold*: the sandbox is dropped, only the spec is kept,
and the next call pays the 109 ms plus imports. An LRU list of warm scripts is
the *consumer's* own policy on top, choosing which versions to `serve` at all.
`max_warm` is a different knob: a pool's ceiling on its own zygotes under
load. When the host is full, the answer is a `429`, not a swap storm.

```text
  a script version's life
  ─────────────────────────────────────────────────────────────────────
  warm ──(no call for idle_timeout, 10 min)──► paused ──(cold_after, 1 h)──► cold
   ▲                                             │                          │
   └──────────── one write to wake ◄─────────────┘                          │
   └──────────── 109 ms + imports to warm again ◄───────────────────────────┘
  ─────────────────────────────────────────────────────────────────────
```

### The decision, part 3: secrets and mounts

**Per-run secrets can vary under one zygote.** Their names are declared on the
function; their values arrive with the request and exist as
`/run/secrets/<name>` only while it runs, never in the zygote. **Mounts and the
network allowlist cannot.** They are the sandbox's namespaces, built once at
warm-up. So one script version used by two projects with different mounts is
two functions, named `{project}-{digest}` — which matches the adopter's own model,
where a script belongs to a project.

### What it costs, and what would reopen it

A busy consumer keeps as many warm zygotes as were called in the last
`idle_timeout`, and `zygo ps` shows how many. The density benchmark must be run
again when the interpreter, the image or the kernel changes. Left open for a
later ADR: a *global* warm budget (`max_warm_total`) that evicts the least
recently called function when the host nears its memory limit. Nothing
measured says it is needed before `idle_timeout` and a `429` do their job; a
consumer who shows that need would reopen it.
[Full ADR 0005](adr/0005-one-warm-zygote-per-script-version.md).

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [25. What Zygo costs](25-performance.md) · [Contents](README.md) · **Next: [ADR 0001 — The embedded runtime](adr/0001-embedded-runtime.md) →**
