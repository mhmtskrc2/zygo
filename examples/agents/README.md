# Writing a Zygo agent

An agent is the piece that lives *inside* a warm sandbox, loads a handler once,
and gives each request a process of its own. Zygo ships two — Python and Node,
both in [`agents/`](../../agents) — and the protocol is language independent on
purpose: anything that speaks it gets resource limits, request deadlines, idle
tiering, metrics, secrets and the `vm` transport without knowing they exist.

**You may not need one.** Without an agent Zygo uses *warm-exec*: the sandbox is
held open and each request is a fresh process running your `cmd`, with the event
on stdin and JSON expected on stdout. That works for every image and every
language, and for a compiled binary it costs about 2 ms. An agent is worth
writing only when starting your runtime is expensive — a Python interpreter with
its imports, a JVM, a Node process with a large dependency tree. If your
language starts in a millisecond, use `cmd` and stop reading.

## The contract

[`spec/protocol.md`](../../spec/protocol.md) is normative. The short version:

* One frame is a 4-byte big-endian length and that many bytes of UTF-8 JSON.
* You get a connected socket at **descriptor 3**.
* Send `READY` when your warm-up is done. Answer `PING` with `PONG`.
* On `EXEC`: create a process, send `FORKED` with its pid, and have it do
  **nothing** until `GO` arrives — before `GO` it is still in the agent's
  cgroup, so anything it allocates is billed to the wrong place and escapes the
  request's limits.
* Answer every `EXEC` with exactly one `DONE` or one `ERROR` carrying the same
  `id`. A child that dies without a result is a `DONE` with a non-zero
  `exit_code` and an `error`, never silence.
* Concurrency is optional. An agent that serves one request at a time answers
  the second with `ERROR` / `overloaded`, which is conforming.
* Secrets need nothing from you: they are files the supervisor writes from
  outside the sandbox, and they are deliberately not in `EXEC`.
* Under `seccomp = "strict"` the supervisor sets `ZYGO_CHILD_SECCOMP`: a
  base64 seccomp program the child installs with one `prctl` before the
  handler runs, removing `execve` and process creation. There are two
  conforming answers and no third — install it, or fail the request. Running
  the request with the sandbox filter alone is what `zygo agent test` now
  catches, and both examples here used to do exactly that.

## Checking it

```bash
zygo agent test <binary> -- [args…]
```

The agent is started with the control socket at descriptor 3 and everything
after `--` as its arguments, then put through the checks `spec/protocol.md` §3
lists. It runs on the host rather than in a sandbox: what is under test is the
conversation, and a sandbox would add failure modes that are Zygo's rather than
yours.

The handler you start it with has to satisfy a five-line contract, or there is
nothing to assert about the answers:

* return the event it was given, unchanged;
* if `event.stdout` is a string, write it to stdout;
* if `event.stderr` is a string, write it to stderr;
* if `event.spawn` is a string, start a **program** that prints it — which is
  what the `strict` child filter takes away, and so what the suite has to be
  able to attempt;
* if `event.sleep_ms` is a number, sleep for that long — so that the cancel
  check has a request to arrive *during*, rather than racing one that is over
  in a millisecond.

```bash
# the reference Python agent
zygo agent test python3 -- agents/python/zygo_agent.py --fd 3 \
    examples/agents/conformance/handler.py

# the reference Node agent
zygo agent test node -- agents/node/zygo_agent.js \
    examples/agents/conformance/handler.js

# and the sh one
zygo agent test /bin/sh -- examples/agents/sh/agent.sh examples/agents/sh/handler.sh
```

This found a real bug in the reference agent the first time it was run: a frame
that was not valid JSON raised out of the read loop and killed the agent,
taking every request in flight with it. It is an `ERROR` now.

## What is here

| | |
|---|---|
| [`sh/`](sh) | A complete agent in POSIX sh + `jq`, about 130 lines including its comments. It exists to keep the "language independent" claim honest, and it passes the same suite the Python agent does. It cannot reach `prctl`, so under `strict` it refuses every request rather than running one unfiltered — the protocol's other conforming answer. |
| [`conformance/`](conformance) | The echo handlers, one per language, that `zygo agent test` expects. |

The two agents Zygo *ships* are not here — they are in
[`../../agents/`](../../agents), because they are shipped code rather than
examples. The Node one is the interesting read if your language has no
`fork()`: it keeps a pool of pre-loaded workers instead, each serving one
request and exiting, with a replacement started off the request path.

For a language whose runtime starts fast — Go, Rust, C — do not write an
agent: [`../warm-exec/go`](../warm-exec/go) is the whole integration, a
`cmd` and nothing else.

## The parts that cost you performance

These are recommendations in the spec rather than requirements, because none of
them can be checked from outside — but they are where the milliseconds are.

* **Import nothing lazily on the request path.** Every module the child touches
  must already be loaded in the agent, so the child inherits it copy-on-write.
  In the reference agent a deferred `import inspect` (~10 ms) and
  `import random` (~2 ms) put p50 at 11.8 ms against a 2 ms budget; hoisting
  them halved it.
* **Freeze the heap before forking.** In a refcounting runtime the first GC pass
  in a child touches every shared object and copies the pages it lives on.
  CPython's `gc.freeze()` cut per-request copying from 14.96 MB to 0.81 MB.
* **Reseed the RNG in the child**, or every request produces the same "random"
  token.
* **Exit hard** (`_exit`), so teardown cannot corrupt state the parent owns.
* **Bound captured output** — Zygo's default is 256 KiB — and say when you
  truncated.
