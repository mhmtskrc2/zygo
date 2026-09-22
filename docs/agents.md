# Writing an agent

A *warm function* keeps a sandbox up and pays for a request with a `fork()`
rather than a container. Zygo ships a Python agent that does this, and the
protocol it speaks is language independent: an agent in any language that can
open a socket and fork can serve warm functions at the same speed.

You need one only when starting your runtime is expensive enough to be worth
amortising. For a language that starts fast, **warm-exec** is simpler and
needs no code from you at all:

```toml
[fn.parse]
image  = "alpine:3"
mounts = ["./bin/parse:/app/parse:ro"]
cmd    = ["/app/parse"]     # the event on stdin, JSON on stdout
```

That costs about 2.2 ms a request. An agent is what you write when importing
your libraries costs more than that.

## The protocol

Nine messages, documented in full in [the wire protocol](../spec/protocol.md):
the agent says `READY`, the supervisor sends `EXEC`, the agent forks and
answers `FORKED` with the child's pid, the supervisor puts that pid in a cgroup
and sends `GO`, and the child answers `DONE` with the result and both streams.

The `FORKED`/`GO` handshake is the part worth understanding. It exists so the
supervisor can put a request's process into its own cgroup **before** the
request runs, which is what makes a per-request memory limit and a deadline
that kills the whole process tree possible.

## Checking one

```bash
zygo agent test /bin/sh -- examples/agents/sh/agent.sh examples/agents/sh/handler.sh
```

Fourteen checks, run against your agent over a real socket. The agent is started
with the control socket at descriptor 3, which is where a sandboxed agent finds
it too, and everything after the binary is passed through as its arguments.

`--script <file>` adds the protocol 1.1 check: a script that arrives with the
request rather than with the zygote. Pass a file in the agent's own language —
the suite cannot guess which one that is, and sending Python to a Node agent
fails in a way indistinguishable from "does not implement 1.1". Without the
flag the check is skipped and the agent is reported as serving functions only,
which is conforming. With it, the suite sends the script three ways: as
`source`, as a `path`, and with a `digest` that does not match. An agent that
runs the third has no defence against a tenant swapping the file it was about
to load, and fails; one that manages `source` but not `path` is reported as
**partial**, because `path` is what the supervisor actually sends.

The **cancel** check (protocol 1.2) needs no flag, and it plays the
supervisor's whole part: it sends `CANCEL`, then kills the child, then waits
for the answer. What is being checked is the answer — `DONE` with `cancelled`
— because the frame is not what stops the request. A real supervisor writes
`cgroup.kill` from outside the sandbox, which reaches everything the handler
spawned and does not need the handler to be somewhere a signal helps. An agent
that does not know the message answers `ERROR`/`bad_message`, carries on, and
is reported as not implementing 1.2 rather than failed; the `sh` agent is the
worked example of that.

The **stream** check (protocol 1.3) needs no flag either, and what it asserts
is *order* rather than content: the same text is in `DONE` whether an agent
streams or not, so an implementation that buffered every `CHUNK` and sent them
at the end would satisfy any check that looked only at what arrived. So the
handler prints, sleeps for a second and a half, and the first chunk has to be
in hand while it is still sleeping.

The **heartbeat** check (protocol 1.4) starts a long request and waits for a
`PING` carrying its id. That is how a supervisor tells a request that is
working from one that is wedged, which matters because Zygo's timeout ceiling
is a day: without it, a request stuck in the first minute of a six-hour budget
would hold its slot for the rest of it. An agent that sends none is bounded by
the deadline as before, and is skipped rather than failed.

Protocol 1.5 adds a `workspace` on `EXEC`: the directory this request's files
are in. An agent puts it in `ZYGO_WORKSPACE` and makes it the child's working
directory. It is not a fixed path and cannot be — a forked child has no
capability to create the mount namespace that would make one path mean
different directories to different requests, which is measured rather than
assumed. The parent holds other requests' directories, including other
tenants', so an agent must not look around it.

## What there is to read

- [`examples/agents/`](../examples/agents) — the contract in short form, what
  `zygo agent test` checks, and the five things that cost an agent its
  milliseconds. Read it before writing one.
- [`examples/agents/sh/`](../examples/agents/sh) — a complete agent in POSIX
  sh, about 130 lines, passing the same checks the Python one does. It is the
  shortest proof that the protocol is language independent.
- [`agents/python/`](../agents/python) — the reference Python agent, and its
  conformance suite.
- [`agents/node/`](../agents/node) — the reference Node agent. Node has no
  `fork()`, so it keeps a pool of pre-loaded workers instead; everything above
  that is identical, which is the point of having a protocol rather than an
  interface. It also carries the forty-line C helper that lets a Node worker
  install the `strict` child filter, because a Node process cannot reach
  `prctl` on its own.
- [`examples/warm-exec/go/`](../examples/warm-exec/go) — a Go program as a warm
  function, where the whole integration is a `cmd`.

## TypeScript, without a build step

A `.ts` handler and a `.ts` script both run as they are. There is no compile
step, no bundler and nothing cached between requests: the types are stripped
as the module loads, in the child, after the fork — which is the same place
and the same moment a `.js` script is compiled.

```toml
[fn.resize]
image = "node:22-slim"
entry = "./resize.ts"     # `runtime` is inferred from the extension
```

or, over the API, by uploading the TypeScript itself:

```python
script = client.put_script(open("resize.ts").read())
client.run_script("node-pool", script.sha256, {"url": "..."})
```

Three things are worth knowing:

* **The digest is over the source as uploaded.** Not over the JavaScript the
  agent makes of it, which would differ between Node versions and break every
  request. The check a child does before it loads a script is the check that
  makes `/run/script/<digest>` safe on a shared uid, and it is unchanged here.
* **`enum`, namespaces and parameter properties work.** They are code rather
  than annotation, so they need Node's `transform` mode rather than the
  `strip` mode it enables by default; the agent asks for `strip` first and
  falls back, because strip mode leaves every line and column where the
  tenant wrote it and transform mode does not.
* **There is no file extension on the wire.** A script arrives as
  `/run/script/<digest>` or as bytes, so the agent decides by what the source
  *is*: valid JavaScript is valid TypeScript, so it compiles as JavaScript
  first and only a `SyntaxError` — raised before a line of the module body has
  run — sends it back to strip types and try again. A file with no types in it
  comes back from the stripper unchanged, which is how a plain JavaScript
  syntax error stays a JavaScript one.

It needs Node 22.13 or later, which is where `module.stripTypeScriptTypes`
arrives. Below that the agent uses `amaro` if the image's dependency set has
it, and otherwise says so rather than running the file as JavaScript. Amaro is
not vendored: it is a megabyte of WebAssembly, and this agent is loaded into
every Node sandbox Zygo runs.

## A handler that computes without yielding

A request that spends three seconds in a tight loop is the case where an
agent's own housekeeping either survives or does not, and what survives is
worth measuring rather than assuming.
[`poc/agent_stall.py`](../poc/agent_stall.py) is that measurement: it drives an
agent over a socket pair, sends one streaming request whose script writes,
reports progress, then spins without yielding, and prints when each frame came
back.

Against both reference agents, with the handler spinning for three seconds:

| | Python | Node |
|---|---|---|
| `PONG` to a `PING` sent a second into the spin | 0.5 ms | 0.3 ms |
| the request's heartbeat (proto 1.4) | on time | on time |
| output written before the spin | forwarded at once | forwarded at once |

Neither agent goes quiet, and the reason is structural: the request runs in a
*process* of its own — a fork in Python, a pooled worker in Node — so the loop
that answers `PING` and `CANCEL` holds no tenant code and has nothing to be
blocked by.

What the measurement did find was output: a Node handler that wrote a megabyte
and then spun had 70 KB of it forwarded and **978 KB silently lost**, because
Node queues a write to a pipe in memory and `process.exit` discards what is
still queued. The worker now makes stdout and stderr blocking, as Python's
are; every byte is forwarded as it is written, and the `DONE` carries the
ring's 256 KiB with its truncation note. If you write an agent in a runtime
with asynchronous stdio, this is the part to get right.

A **thread pool inside the worker** — running tenant code off the worker's own
loop — buys nothing further and is not there. Measured on `node:22`, one
`worker_threads` isolate costs 13 MB resident and 15–23 ms to start, against
the 43 MB and ~2 ms the whole Node agent costs now; a tenant whose handler
really must report progress while it computes can start one itself, since the
`strict` filter refuses `clone` only without `CLONE_THREAD` and the permission
fallback passes `--allow-worker` for exactly that reason. What would reopen it
is a measurement showing an agent going quiet with a thread pool absent — the
one above shows the opposite.

## The runtimes that have no agent

Deno and Bun do not have one, and are not waiting for one:
[ADR 0003](adr/0003-no-deno-or-bun-agent.md) records why and what would change
it. Both start in a few milliseconds, so a **warm-exec pool** serves them with
no protocol at all — `cmd = ["deno", "run", "--allow-none"]` and each
request's script as the last argument. What that gives up is streaming,
`progress()`, workspaces and per-request tenant limits; see
[`examples/warm-exec/`](../examples/warm-exec).

The same is true of anything that starts fast: Go, Rust, C, `bash`. Write an
agent when there is something expensive to keep warm, not because the runtime
is popular.

## Four rules an agent has to keep

Each one is in the protocol document with the reason. They are here because
each has been got wrong at least once, in this repository:

1. **The child never returns to the parent's loop.** A forked child that
   unwinds into the agent's `serve()` runs the parent's interpreter teardown,
   and then there are two agents on one socket.
2. **Nothing runs before `GO`.** The supervisor has not put the child in a
   cgroup yet, so anything that runs early is unbounded.
3. **A malformed frame is reported, not fatal.** The stream is still aligned;
   killing the agent takes every in-flight request with it.
4. **Every `EXEC` gets exactly one answer.** A request that is refused,
   overloaded or unparseable still gets a reply, because the supervisor's only
   alternative is to wait out the deadline.
