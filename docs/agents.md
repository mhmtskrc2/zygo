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
image = "golang:1.23"
cmd   = ["/app/parser"]     # the event on stdin, JSON on stdout
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

Nine checks, run against your agent over a real socket. The agent is started
with the control socket at descriptor 3, which is where a sandboxed agent finds
it too, and everything after the binary is passed through as its arguments.

## What there is to read

- [`examples/agents/`](../examples/agents) — the contract in short form, what
  `zygo agent test` checks, and the five things that cost an agent its
  milliseconds. Read it before writing one.
- [`examples/agents/sh/`](../examples/agents/sh) — a complete agent in POSIX
  sh, about 130 lines, passing the same nine checks the Python one does. It is
  the shortest proof that the protocol is language independent.
- [`examples/agents/node/`](../examples/agents/node) — a Node agent with a
  worker pool.
- [`agents/python/`](../agents/python) — the reference agent, and its
  conformance suite.
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
