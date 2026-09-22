# Zygo warm execution protocol — v1

Status: **draft**, implemented by `zygo-core` and by the reference Python agent.

This is the contract between the **supervisor** (on the host) and a **runtime
agent** (inside the sandbox). It is deliberately small and language independent:
anything that speaks it — the built-in Python agent, a Node agent,
thirty lines of bash — gets resource limits, timeouts, idle tiering, metrics and
`vm` transport for free.

An agent is optional. Without one, Zygo uses **warm-exec**: it spawns the
function's `cmd` inside the already-warm sandbox and passes the event on stdin.
Every image and every command works that way, which is how Docker's generality
is preserved.

---

## 1. Transport and framing

| Backend | Transport |
|---|---|
| `ns`, `gvisor` | `AF_UNIX` stream socket, mode 0600 |
| `vm` | vsock |

Each message is one frame:

```
+--------+------------------+
| uint32 | JSON body        |
| BE     | `length` bytes   |
+--------+------------------+
```

- The length prefix is **4 bytes, big-endian** — `struct.pack(">I", n)` in
  Python, `Buffer.writeUInt32BE` in Node.
- The body is UTF-8 JSON, an object with a `type` field.
- Maximum frame: **32 MiB**. A larger payload belongs in a scratch file, not on
  the control socket.

The size cap is a protection boundary, not a tuning knob. The agent runs
untrusted code; a frame claiming 4 GiB must not make the supervisor allocate
4 GiB. Implementations must check the announced length **before** allocating.

A connection closed at a frame boundary is a clean shutdown. A connection closed
partway through a frame is an error — an agent that dies mid-write must not look
like an orderly exit.

---

## 2. Messages

### `READY` — agent → supervisor, once

Sent after the handler has been loaded and the warm-up is complete.

```json
{"type":"READY","proto":1,"pid":42,"imports_ms":312.5,"rss_kb":41200,"runtime":"python/3.12.4"}
```

| Field | Type | Meaning |
|---|---|---|
| `proto` | int | Protocol version. This document is `1`. |
| `pid` | int | Agent pid *inside the sandbox's pid namespace*. |
| `imports_ms` | float | One-time warm-up cost. Reported by `zygo ps`. |
| `rss_kb` | int | Resident memory after warm-up — the per-tenant cost. |
| `runtime` | string | Free-form `name/version`, for diagnostics only. |
| `child_filter` | string | Optional. How the agent is honouring `ZYGO_CHILD_SECCOMP`: `seccomp`, an implementation-defined name for an equivalent, or `none`. Diagnostics only. |

A supervisor that sees an unknown `proto` refuses the agent rather than guessing.

### `EXEC` — supervisor → agent

```json
{"type":"EXEC","id":"01f3","event":{"url":"…"},"timeout_ms":30000,"env_overrides":{}}
```

`id` correlates every later message for this request. `event` is arbitrary JSON.
`env_overrides` is optional and may be omitted when empty.

**Secrets are deliberately not in this message.** A function's `secrets` are
delivered as files at `/run/secrets/<name>`, written by the supervisor from
*outside* the sandbox between `FORKED` and `GO` and removed when the last
request in flight finishes. The agent never receives a value, so an agent
cannot leak one: it is not in `EXEC`, not in the zygote's memory, and not on
this connection. An agent needs to do nothing for secrets to work, and must
not try to — the files appear before `GO` and are the child's to read.

#### `script` — the runtime pool shape (1.1)

`EXEC` may carry the code to run:

```json
{"type":"EXEC","id":"01f3","event":{…},"timeout_ms":30000,
 "script":{"source":"def handler(event): …","digest":"sha256:…"}}
```

Absent is the original shape and the fast path: the agent was started with a
handler, imported it once, and every request is a fork of that.

Present is the **runtime pool**: one zygote per image-and-dependency-set,
holding no tenant code at all, and the script arrives with the request. An
embedder has ten thousand scripts and cannot hold ten thousand zygotes — a
warm one costs about 10 MB of proportional memory, which is 97 GiB at that
count ([`docs/bench-embed.md`](../docs/bench-embed.md)). This field is how one
zygote serves all of them.

| Field | Type | Meaning |
|---|---|---|
| `path` | string | Where the supervisor put it inside the sandbox before `GO`, `0400`, in a directory that cannot be listed. |
| `source` | string | The script itself, on the wire. |
| `digest` | string | `sha256:…` of the contents. **Checked by the child before it loads them.** |
| `entry_point` | string | What to call. Default `handler`. |

At least one of `path` and `source` is set, and `path` is the shape the
supervisor sends whenever it can write into the sandbox. The difference is
which processes hold the bytes. A `source` has to be read into the *agent's*
address space to be forwarded, and the agent is the zygote every later request
forks from — so in a pool shared between tenants, the next tenant's child
inherits a copy-on-write view of a heap that held this one's code. With `path`
only the child ever has it. `source` remains correct where there is no writable
path into the sandbox, and for a one-off.

Scripts are content-addressed: `path` ends in the digest's hex, so two tenants
that register identical bytes name one file and neither can substitute a
different script under a digest somebody else is running.

**The digest is not advisory.** A sandbox has one uid: the child about to load
`/run/script/<hash>` can unlink that file and write its own in its place —
for itself, or for another request in flight on the same pool zygote. What it
cannot reach is this field, which arrives on the supervisor's connection. So
an agent that is given a `digest` **must** hash the bytes it is about to load
and refuse them with `ERROR` / `handler_load` if they do not match, before any
of the script runs. The same rule covers `source`, where it is only
self-consistency, so that there is one rule rather than two.

**The child loads it, after `GO`.** Not the agent, and not before: a zygote
that imported a tenant's script would hold that tenant's code, and a pool is
shared, so the next request could be somebody else's. The whole value of a
pool is that the warm process is anonymous — an interpreter and its
dependencies, and nothing of anybody's. Under `strict`, load it *after*
installing `ZYGO_CHILD_SECCOMP` too, so a script that tries to start a program
is refused by the kernel while it is loading rather than after.

The cost is that the load is paid per request instead of once. That is the
trade, and it is why `entry` still exists: warm a hot function with its
handler and fork it, and let the long tail arrive this way.

Implementing this is **optional**. An agent that does not know the field
ignores it, as §5 requires, and serves the handler it was warmed with;
`zygo agent test` reports that as "functions only" rather than as a failure.

### `FORKED` — agent → supervisor

```json
{"type":"FORKED","id":"01f3","pid":1234}
```

The child exists but **has not started work**.

### `GO` — supervisor → agent

```json
{"type":"GO","id":"01f3"}
```

Sent once the supervisor has moved `pid` into the request's cgroup. Until then
the child is still accounted to the *agent's* cgroup, so an allocation before
`GO` would be billed to the wrong place and escape the request's limits.

### `RESULT` — child → agent

```json
{"type":"RESULT","id":"01f3","exit_code":0,"result":{"status":200},
 "stdout":"…","stderr":"","peak_rss_kb":41200,"wall_ms":12.3,"cpu_ms":9.1}
```

`error` is present, with a human-readable traceback, when the handler raised;
`result` is then meaningless. Metric fields are flattened at the top level, not
nested.

Every field except `type`, `id` and `exit_code` is optional. A minimal
conforming result is:

```json
{"type":"RESULT","id":"01f3","exit_code":0}
```

A child that could not run the request *at all* may answer with an `ERROR`
frame in place of its `RESULT`, and the agent forwards that upwards with the
request's `id` instead of a `DONE`. The distinction is who was wrong: a handler
that raised is a `DONE` with a non-zero `exit_code`, and a script whose bytes
do not hash to the digest the supervisor sent is an `ERROR` / `handler_load`,
because the supervisor and the child disagree about what this request *is*.
Either way it is exactly one answer per `EXEC` (§3.5).

### `DONE` — agent → supervisor

The same payload as `RESULT`, forwarded upwards. The agent must not drop fields
on the way.

If the child dies without sending a `RESULT` — an OOM kill, a deadline kill, a
segfault in a C extension — the agent synthesises a `DONE` with a non-zero
`exit_code` and an `error` describing the death. Silence is not an acceptable
outcome: the supervisor has a request waiting on it.

### `PING` / `PONG`

```json
{"type":"PING","seq":7}   →   {"type":"PONG","seq":7}
```

Liveness. An agent that stops answering is restarted.

### `SHUTDOWN` — supervisor → agent

```json
{"type":"SHUTDOWN","grace_ms":5000}
```

Finish in-flight requests, then exit.

### `ERROR` — either direction

```json
{"type":"ERROR","id":"01f3","code":"handler_load","message":"…"}
```

A protocol-level failure, distinct from a handler failure (which is a `DONE`
with a non-zero `exit_code`). `code` is one of:

| Code | Meaning |
|---|---|
| `bad_message` | Unparseable, or not valid in this state |
| `unsupported_version` | `proto` is not supported |
| `handler_load` | The handler could not be imported; the agent is unusable |
| `spawn_failed` | `fork()` or the spawn fallback failed |
| `timeout` | The request exceeded `timeout_ms` |
| `overloaded` | Too many in-flight requests |
| `bad_result` | The result was not JSON-serialisable |
| `internal` | Anything else; see `message` |

---

## 3. Required agent behaviour

An implementation is conforming if and only if all of the following hold. These
are what `zygo agent test <binary> -- [args…]` checks: it starts the agent with
the control socket at descriptor 3 and runs the conversation against it.

The handler the agent is started with has to satisfy a small contract, or there
is nothing the suite can assert about the answers:

* return the event unchanged;
* write `event.stdout` to stdout and `event.stderr` to stderr when they are
  strings;
* when `event.spawn` is a string, start a **program** that prints it — which
  is what the `strict` child filter takes away, and so what the suite has to
  be able to attempt.

[`examples/agents/conformance/`](../examples/agents/conformance) has one per
language, and [`examples/agents/sh/`](../examples/agents/sh) is a complete
agent in POSIX sh, to check the suite against something that is not Python.

1. **One process per request.** Every request runs in its own process, or at
   minimum a pid that can be moved into its own cgroup.
2. **Announce, then wait.** The child pid is reported with `FORKED`, and the
   child performs no user-visible work until `GO` arrives.
3. **Structured results.** stdout and stderr are returned in separate fields,
   alongside the exit code and resource measurements.
4. **A clean zygote.** The agent never handles a request in its own process. Its
   memory stays in the "just after a clean import" state, so request *n* cannot
   observe anything request *n-1* did.
5. **No silent loss.** Every `EXEC` is answered by exactly one `DONE` or one
   `ERROR` with the same `id`. Concurrency is *not* required: an agent that
   serves one request at a time answers the second with `overloaded`, which is
   conforming. Losing it is not.
6. **A protocol error is reported, not fatal.** A frame that arrives whole but
   whose body is not a message leaves the stream aligned at a frame boundary,
   so the agent answers `ERROR` / `bad_message` and carries on. An agent that
   dies there takes every request in flight with it. (An announced length past
   the 32 MiB cap is different: nothing was consumed, the stream cannot be
   resynchronised, and closing the connection is the only correct answer.)
7. **`ZYGO_CHILD_SECCOMP` is installed, or the request is failed.** When the
   supervisor sets it, the value is base64 of a raw seccomp-bpf program
   (`struct sock_filter[]`, the host's byte order) that the *child* is to
   install — `prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog)` after
   `PR_SET_NO_NEW_PRIVS` — after `GO` and before any handler code. It is how
   the `strict` profile removes `execve` and process creation from the child
   without removing them from the agent. A value the agent cannot decode is a
   start-up `ERROR`.

   There are exactly two conforming answers: install it, or fail the request.
   Running the request anyway, with the sandbox's filter alone, is the third
   and it is not allowed — a `strict` function whose agent quietly ignored
   this variable is a function whose author asked for a tightening and did not
   get it. `zygo agent test` checks this by starting a second copy of the
   agent with the variable set and asking the handler to `spawn`; the `sh`
   and Node example agents both failed it silently until it existed.

   A language that cannot reach `prctl` can still conform. The Node agent
   ships a forty-line shared object whose constructor installs the program,
   and falls back to Node's own permission model — no child processes, no
   native addons, no WASI — when the image has no such object; it says which
   in `READY`. The `sh` agent refuses every request instead, which is the
   other conforming answer.
8. **A script's digest is checked, if there is one.** An agent that implements
   the 1.1 `script` field and is given a `digest` hashes the bytes it is about
   to load and refuses them with `ERROR` / `handler_load` unless they match.
   Hash *what was read*, not the file again: reading twice is a window for the
   tenant to change it in between. An agent that does not implement `script`
   at all is unaffected — it never loads anything a digest describes.

### Strongly recommended
- **Import nothing lazily on the request path.** Every module the child touches
  must already be loaded in the agent, so the child inherits it through
  copy-on-write. This is the easiest mistake to make and the most expensive: in
  the reference agent, a deferred `import inspect` (~10 ms) and `import random`
  (~2 ms) in the child put p50 at 11.8 ms against a 2 ms budget. Hoisting them
  to the agent halved it, and the measured p50 on Linux is now 1.9 ms. See
  [what Zygo costs](../docs/performance.md).
- **Reseed the RNG in the child.** A forked child inherits the parent's seeded
  random state; without a reseed, every request produces identical "random"
  values — tokens, temporary names, jitter.
- **Freeze the heap before forking.** In a refcounting runtime, the first GC
  pass in a child touches every shared object and copies the pages it lives on,
  which defeats copy-on-write. CPython's `gc.freeze()` is the reference case.
- **Exit hard.** `os._exit()` rather than a normal return, so atexit handlers
  and interpreter teardown cannot corrupt state the parent also owns.
- **Bound captured output.** Truncate stdout and stderr (Zygo's default is
  256 KiB) and mark the truncation.

---

## 4. Sequence

```
supervisor                agent                     child
    |                       |                         |
    |<------- READY --------|                         |
    |                       |                         |
    |------- EXEC --------->|                         |
    |                       |------ fork() ---------->|
    |<------ FORKED --------|                         | (waiting)
    |  [move pid to cgroup] |                         |
    |-------- GO ---------->|------------------------>|
    |                       |                         | handler(event)
    |                       |<------ RESULT ----------|
    |                       |                         | _exit(0)
    |<------- DONE ---------|
    |  [remove cgroup]      |
```

On timeout the supervisor writes `cgroup.kill`, which takes down the whole
subtree; the agent observes the child's death and reports it as a `DONE` with
`error`.

---

## 5. Versioning

`proto` is incremented only for a breaking change. Adding an **optional** field
is not breaking — implementations must ignore fields they do not recognise.

That is why the script-carrying `EXEC` above is called 1.1 and still announces
`proto: 1`: an agent written before it existed ignores the field and keeps
working, and a supervisor that sends one to such an agent gets the agent's own
handler back rather than an error. The version number is for the day something
*removes* or *changes the meaning of* a field, and nothing has.

Conformance fixtures live in `spec/fixtures/` and are exercised by both the Rust
tests and `agents/python/test_zygo_agent.py`.
