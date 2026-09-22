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

Thirteen checks, run against your agent over a real socket. The agent is started
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
