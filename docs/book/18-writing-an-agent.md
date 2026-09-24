# 18. Writing an agent

An *agent* is the small program inside a warm sandbox that loads a handler
once and gives each request a process of its own. Zygo ships one for Python
and one for Node. This chapter is for people who want to write one for
another language: it explains the wire protocol in plain words, how to test an
agent with `zygo agent test`, and the mistakes that cost agents their speed.

## What an agent does

A *warm function* keeps a sandbox up and pays for each request with a
`fork()` — a system call that makes a copy of a running process — rather than
with a new container ([chapter 13](13-warm-functions.md)). The agent is the
part that does the forking. It talks to the *supervisor*, Zygo's process on
the host that owns every warm sandbox, over a *socket* (a two-way channel
between two processes). The protocol is language independent: an agent in
any language that can open a socket and fork can serve warm functions at the
same speed. Anything that speaks it gets resource limits, request deadlines,
idle pausing, metrics, secrets and the `vm` transport without knowing they
exist.

```text
  host                                 │  inside the sandbox
                                       │
  ┌──────────────┐   socket (fd 3)     │  ┌──────────────────────────────┐
  │  supervisor  │◀────────────────────┼─▶│ agent (the zygote)           │
  │              │   frames: READY,    │  │ runtime started, handler     │
  │ cgroups,     │   EXEC, FORKED,     │  │ loaded, never runs a request │
  │ deadlines,   │   GO, DONE, ...     │  └──────┬───────────────────────┘
  │ secrets      │                     │         │ fork, per request
  └──────────────┘                     │  ┌──────▼───────────────────────┐
                                       │  │ child: runs handler(event)   │
                                       │  │ once, sends RESULT, exits    │
                                       │  └──────────────────────────────┘
```

## Do you need one?

You need an agent only when starting your runtime is slow enough to be worth
doing once. Without an agent Zygo uses *warm-exec*: the sandbox is held open
and each request is a fresh process running your `cmd`, with the event on
standard input and JSON expected on standard output. That works for every
image and every language, and needs no code from you at all. It costs a median
of about 2.2 ms a request, measured in Docker Desktop's VM on an Apple M1 Max
([chapter 25](25-performance.md)). An agent is what you write when starting
your runtime and importing your libraries costs more than that: a Python
interpreter with its imports, a JVM, a Node process with a large dependency
tree.

```toml
[fn.parse]
image  = "alpine:3"
mounts = ["./bin/parse:/app/parse:ro"]
cmd    = ["/app/parse"]     # the event on stdin, JSON on stdout
```

If your language starts in a millisecond, use `cmd` and stop reading.

## How messages travel

Every message is one *frame*: a 4-byte length, then that many bytes of JSON.
The length is an unsigned 32-bit number in *big-endian* order (most
significant byte first) — `struct.pack(">I", n)` in Python,
`Buffer.writeUInt32BE` in Node. The body is UTF-8 JSON: an object with a
`type` field that names the message.

```text
  ┌────────────────────┬──────────────────────────────────────────────┐
  │ length: 4 bytes    │ body: `length` bytes of UTF-8 JSON           │
  │ uint32, big-endian │ {"type":"EXEC","id":"01f3","event":{...}}    │
  └────────────────────┴──────────────────────────────────────────────┘
```

| Backend | Transport |
|---|---|
| `ns`, `gvisor` | an `AF_UNIX` stream socket (a local socket that is a file), mode 0600 |
| `vm` | *vsock*, a socket between a virtual machine and its host |

A frame may be at most **32 MiB**. A larger payload belongs in a file, not on
the control socket. This cap protects the supervisor: the agent runs untrusted
code, and a frame that claims to be 4 GiB must not make anyone allocate 4 GiB.
So check the announced length **before** allocating. A connection closed
between frames is a clean shutdown; one closed in the middle of a frame is an
error, so an agent that dies mid-write does not look like an orderly exit.

## The messages

There are a handful of messages. The core set is enough for a working agent;
the others are optional additions that an agent may ignore.

| Message | Direction | What it is for | Since |
|---|---|---|---|
| `READY` | agent → supervisor | "Warm-up is done." Sent once, with `proto`, the agent's `pid`, `imports_ms`, `rss_kb` and a `runtime` name. | 1 |
| `EXEC` | supervisor → agent | One request: an `id`, the `event`, `timeout_ms`, and optional `env_overrides`. | 1 |
| `FORKED` | agent → supervisor | "The child for this request exists, with this pid, and is waiting." | 1 |
| `GO` | supervisor → agent | "The child is in its cgroup now; it may start." | 1 |
| `RESULT` | child → agent | The handler's answer: `exit_code`, `result` or `error`, `stdout`, `stderr`, and metrics. | 1 |
| `DONE` | agent → supervisor | The `RESULT`, passed on without dropping a field. | 1 |
| `PING` / `PONG` | both ways | "Are you alive?" "Yes." An agent that stops answering is restarted. | 1 |
| `SHUTDOWN` | supervisor → agent | Finish the requests in flight within `grace_ms`, then exit. | 1 |
| `ERROR` | either way | A protocol failure, with a `code` (see [Errors](#errors)). | 1 |
| `EXEC.script` | supervisor → agent | The code to run arrives with the request (runtime pools). | 1.1 |
| `CANCEL` | supervisor → agent | "Somebody asked to stop this request." | 1.2 |
| `CHUNK` | child → agent → supervisor | A piece of output, sent while the request runs. | 1.3 |
| `PING` with `id` | agent → supervisor | A heartbeat: "this request is still running." | 1.4 |
| `EXEC.workspace` | supervisor → agent | The folder with this request's files. | 1.5 |

`READY` may also carry `child_filter`, which says how the agent honours the
`strict` child filter: `seccomp`, a name for an equivalent, or `none`. It is
for diagnostics only. [`spec/protocol.md`](../../spec/protocol.md) is the
full, normative text, with every field.

## One request, step by step

The agent says `READY`. The supervisor sends `EXEC`. The agent forks and
answers `FORKED` with the child's pid. The supervisor moves that pid into a
new cgroup for this request and sends `GO`. The child runs the handler, sends
`RESULT` to the agent and exits, and the agent passes it on as `DONE`.

```text
  supervisor                  agent                       child
      │                         │                           │
      │◀──────── READY ─────────│                           │
      │                         │                           │
      │───────── EXEC ─────────▶│                           │
      │                         │───────── fork() ─────────▶│
      │◀──────── FORKED ────────│                           │ (waiting)
      │  [move pid to cgroup]   │                           │
      │────────── GO ──────────▶│──────────────────────────▶│
      │                         │                           │ handler(event)
      │                         │◀──────── RESULT ──────────│
      │                         │                           │ _exit(0)
      │◀──────── DONE ──────────│
      │  [remove cgroup]        │
```

## Why `FORKED` and `GO` exist

The `FORKED`/`GO` handshake is the part worth understanding. A *cgroup* is
the kernel's way to limit and count the memory and CPU of a group of
processes ([chapter 3](03-cgroups.md)). A new child starts in the agent's
cgroup. The handshake lets the supervisor move it into the request's own
cgroup **before** the request runs. That is what makes a per-request memory
limit possible, and a deadline that kills the whole process tree. So the child
must do **nothing** before `GO`: anything it allocated would be billed to the
agent and escape the request's limits.

## Secrets need nothing from you

Secrets are deliberately **not** in `EXEC`. The supervisor writes each one as a
file, `/run/secrets/<NAME>`, from *outside* the sandbox, between `FORKED` and
`GO`, and removes it when the last request in flight finishes. The agent never
sees a value, so it cannot leak one: it is not in `EXEC`, not in the zygote's
memory, and not on this connection. An agent must not try to handle secrets.
The files are there before `GO` and are the child's to read.

## Results, errors and a child that dies

`RESULT` needs only `type`, `id` and `exit_code`; everything else is optional.
`error`, with a human-readable traceback, is present when the handler raised,
and `result` is then meaningless. Metric fields such as `peak_rss_kb`,
`wall_ms` and `cpu_ms` sit at the top level, not nested. If the child dies
without a `RESULT` — killed for memory, killed at its deadline, or crashed in
a C extension — the agent makes up a `DONE` with a non-zero `exit_code` and an
`error` that describes the death. Silence is never an answer: the supervisor
has a request waiting on it.

```json
{"type":"RESULT","id":"01f3","exit_code":0,"result":{"status":200},
 "stdout":"…","stderr":"","peak_rss_kb":41200,"wall_ms":12.3,"cpu_ms":9.1}
```

## Errors

`ERROR` is a failure of the protocol, not of the handler. A handler that raised
is a `DONE` with a non-zero `exit_code`. A child that could not run the
request *at all* may answer `ERROR` instead of `RESULT`, and the agent passes
that on with the request's `id` instead of a `DONE`. Either way there is
exactly one answer per `EXEC`.

| Code | Meaning |
|---|---|
| `bad_message` | The frame could not be parsed, or is not valid right now. |
| `unsupported_version` | The `proto` is not supported. |
| `handler_load` | The handler could not be imported (the agent is unusable), or a script's bytes do not match its digest. |
| `spawn_failed` | `fork()`, or the spawn fallback, failed. |
| `timeout` | The request ran past `timeout_ms`. |
| `overloaded` | Too many requests in flight. |
| `bad_result` | The result could not be turned into JSON. |
| `internal` | Anything else; see `message`. |

## Streaming output: `CHUNK` (1.3)

An `EXEC` with `"stream": true` asks to watch the request. The child then sends
`CHUNK` frames as it writes, each with a `stream` (`stdout`, `stderr` or
`progress`) and the `data`. Streaming is asked for per **request**, not per
function: a chunk per `print()` is a system call per `print()`, so only a
caller that wants to watch pays for it. An `EXEC` that did not ask carries no
`stream` field at all, and must produce no `CHUNK`.

```text
  supervisor                  agent                       child
      │──── EXEC{stream} ──────▶│                           │
      │◀──────── FORKED ────────│                           │
      │────────── GO ──────────▶│──────────────────────────▶│ handler(event)
      │                         │◀──────── CHUNK ───────────│   print(…)
      │◀──────── CHUNK ─────────│                           │
      │                         │◀──────── RESULT ──────────│
      │◀──────── DONE ──────────│
```

Three rules make streaming honest. The agent passes chunks on **without
buffering**; one that collected them would deliver the bytes at the same moment
as `RESULT`. A chunk is not cut at line ends: half a line written before the
handler blocks should arrive. And `RESULT` still carries the whole of `stdout`
and `stderr`, so a caller that streamed and one that did not see the same
text. `progress` is not a line of stdout: the reference agents give the
handler a `progress()` call on the event, present whether or not anyone is
listening. There is no sequence number; frames of one request travel in order
on one connection.

## Cancelling a request: `CANCEL` (1.2)

`CANCEL` says someone asked for a request to stop. **It is not what stops
it.** The supervisor kills the request by writing `cgroup.kill` on the
request's cgroup from outside the sandbox. That takes the child and everything
it started, and does not need the handler to be somewhere a signal helps. The
frame is for the **answer**: a cancel, a deadline and an out-of-memory kill
all look like signal 9 and exit 137. An agent that gets `CANCEL` sets
`"cancelled": true` on that request's `DONE`, so the caller can tell its own
cancel from a limit it needs to raise.

```text
  supervisor                  agent                       child
      │───────── EXEC ─────────▶│───────── fork() ─────────▶│
      │◀──────── FORKED ────────│                           │ (waiting)
      │────────── GO ──────────▶│──────────────────────────▶│ handler(event)
      │                         │                           │
      │──────── CANCEL ────────▶│ (marks the request)       │
      │  [write cgroup.kill]    │                           X
      │◀──── DONE{cancelled} ───│
```

An agent that implements `CANCEL` must not treat an unknown id as an error:
the request may have finished just before the frame arrived. An agent that
does not know the message answers `ERROR` / `bad_message` and carries on; the
kill still lands, and the supervisor fills in `cancelled` itself. A `CANCEL`
that arrives **before** `GO` is the best case: Zygo kills the child and never
sends `GO`, so not one line of the handler ran. That is why
`DELETE /requests/<id>` can report whether the work had started.

## Heartbeats: `PING` with an `id` (1.4)

While a request runs, the agent may send `PING` with that request's `id`,
without waiting for an answer. Zygo's timeout can be as long as a day. Without
a heartbeat, a request stuck in the first minute of a six-hour budget would
hold its slot for the rest of it. A supervisor that hears nothing about a
request for its grace period (a minute, in Zygo) kills it as **stuck**, which
is a different answer from "too slow". The reference agents send one every two
seconds. A `CHUNK` counts as a sign of life too. Do not skip the heartbeat
because the child *looks* idle: an agent cannot tell a child that is computing
from one that is blocked.

```json
{"type":"PING","seq":0,"id":"01f3"}
```

## Scripts that arrive with the request (1.1)

For a *runtime pool* ([chapter 13](13-warm-functions.md#runtime-pools)), the
agent starts with no handler, and each `EXEC` carries a `script`. This lets one
warm zygote serve thousands of scripts; a warm zygote costs about 10 MB of
memory, so ten thousand of them would need about 97 GiB.

| Field | Meaning |
|---|---|
| `path` | Where the supervisor put the file in the sandbox before `GO`: mode 0400, in a read-only folder that cannot be listed. |
| `source` | The script itself, on the wire. |
| `digest` | `sha256:…` of the contents. **The child checks it before loading.** |
| `entry_point` | What to call. Default `handler`. |

At least one of `path` and `source` is set, and the supervisor sends `path`
whenever it can. The reason is memory: a `source` passes through the agent,
and every later child — perhaps another tenant's — forks from the agent's
memory. With `path`, only the child ever holds the bytes. Paths end in the
digest, so two tenants with identical bytes share one file, and nobody can
swap in different bytes under a digest someone else is running.

```text
  EXEC{script: path + digest}
      │
      ▼
  child (after GO) ─▶ install the child filter ─▶ read the bytes ─▶ hash them
                                                                       │
                     digest matches? ── no ──▶ ERROR / handler_load  ◀─┘
                          │
                         yes ─▶ load the script ─▶ call entry_point(event)
```

## Rules for scripts

- **The child loads the script, after `GO`.** Never the agent: a zygote that
  imported a tenant's script would pass it on to the next request, which may
  be someone else's.
- **After the child filter too** (see [The `strict` child
  filter](#the-strict-child-filter)). A script's top-level code is request
  code. An agent that loads it first and filters afterwards gives a `strict`
  pool nothing.
- **The digest is not advisory.** Hash the bytes you are about to load — not
  the file read a second time, which gives a tenant a moment to change it —
  and refuse a mismatch with `ERROR` / `handler_load` before any of it runs.
  The same rule covers `source`.
- **What a script prints while it loads belongs to the request**, so it goes
  in that request's `stdout`, not the agent's.
- **The load is paid per request.** That is the trade, and it is why `entry`
  still exists: warm a hot function with its handler, and let the long tail
  arrive as scripts.

Implementing `script` is optional. An agent that ignores the field serves the
handler it was warmed with, and `zygo agent test` reports it as "functions
only".

## A workspace per request (1.5)

`EXEC` may carry a `workspace`: the folder with the files this request's
caller sent, and where the handler leaves what it wants back. An agent puts it
in `ZYGO_WORKSPACE` **and makes it the child's working folder** before any
handler code. It is not a fixed path, and cannot be. A forked child has no
right to create the mount namespace that would make one path mean a different
folder to each request: measured on Linux 6.12, `unshare(CLONE_NEWNS)` fails
with `EPERM` for the child, whatever the seccomp profile. So three weaker
things keep requests apart: the parent folder is mode 0311 and cannot be
listed, the name is 128 random bits, and the folder is removed when the
request ends. An agent must not look around the parent, which holds other
requests' folders, including other tenants'.

## Versions

`proto` goes up only for a breaking change. Adding an **optional** field or
message is not breaking, and an agent must ignore fields it does not know. So
every addition since the first version still announces `proto: 1`. A
supervisor that sees an unknown `proto` refuses the agent rather than
guessing.

| Version | Adds | An agent that does not know it |
|---|---|---|
| 1.1 | `EXEC.script` | ignores it and serves its own handler |
| 1.2 | `CANCEL`, `DONE.cancelled` | answers `bad_message`; the kill still lands |
| 1.3 | `EXEC.stream`, `CHUNK` | answers the same `DONE` as before |
| 1.4 | `PING` with `id` | is bounded by the request's deadline |
| 1.5 | `EXEC.workspace` | runs the handler where it was; the request fails to find its files |

## The contract in short

- One frame is a 4-byte big-endian length and that many bytes of UTF-8 JSON.
- You get a connected socket at **file descriptor 3**.
- Send `READY` when your warm-up is done. Answer `PING` with `PONG`.
- On `EXEC`: create a process, send `FORKED` with its pid, and let it do
  **nothing** until `GO` arrives.
- Answer every `EXEC` with exactly one `DONE` or one `ERROR` with the same
  `id`.
- Concurrency is optional. An agent that serves one request at a time answers
  the second with `ERROR` / `overloaded`, which is conforming.
- Never run a request in the agent's own process. Its memory stays as it was
  just after a clean import, so request *n* cannot see what request *n−1* did.
- Return stdout and stderr in separate fields, beside the exit code and the
  measurements.
- A frame that is whole but is not a valid message gets `ERROR` /
  `bad_message`, and the agent carries on. (A length above 32 MiB is
  different: the stream cannot be recovered, so close the connection.)

## The `strict` child filter

*Seccomp* is the kernel feature that limits which system calls a process may
make ([chapter 4](04-other-locks.md)). Under `seccomp = "strict"` the
supervisor sets `ZYGO_CHILD_SECCOMP`: base64 of a raw seccomp-bpf program, in
the host's byte order, that the *child* installs with
`prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog)` after
`PR_SET_NO_NEW_PRIVS`. The child does this after `GO` and before any handler
code. It removes `execve` and process creation from the child without taking
them from the agent. A value the agent cannot decode is a start-up `ERROR`.

There are exactly two conforming answers: install the filter, or fail the
request. Running the request with only the sandbox's filter is not allowed,
because the function's author asked for a tighter wall and would not get it.
A language that cannot reach `prctl` can still conform. The Node agent ships a
forty-line C shared object whose constructor installs the program; without
it, it falls back to Node's own permission model (no child processes, no
native addons, no WASI) and says which in `READY`. The `sh` agent refuses
every request under `strict` instead. `zygo agent test` checks this, and the
`sh` and Node example agents both failed it silently until the check existed.

## Checking an agent: `zygo agent test`

`zygo agent test` runs a conformance suite against your agent over a real
socket. It starts the agent with the control socket at descriptor 3 — where a
sandboxed agent finds it too — and passes everything after `--` to the agent
as its arguments. It runs on the host, not in a sandbox: what is under test is
the conversation, and a sandbox would add failures that are Zygo's rather than
yours. It exits with **1 if any check failed**, and 0 otherwise. With `--json`
it prints the report as JSON.

```bash
zygo agent test BINARY [--script FILE] [--script-spawn FILE] [--pool-script FILE] -- [ARGS…]

# the reference Python agent
zygo agent test python3 -- agents/python/zygo_agent.py --fd 3 \
    examples/agents/conformance/handler.py

# the reference Node agent
zygo agent test node -- agents/node/zygo_agent.js \
    examples/agents/conformance/handler.js

# the sh one
zygo agent test /bin/sh -- examples/agents/sh/agent.sh examples/agents/sh/handler.sh
```

| Option | What it adds |
|---|---|
| `--script FILE` | The protocol 1.1 check: a script, in the agent's own language, sent inside `EXEC`. |
| `--script-spawn FILE` | A script whose top-level code starts a program. Checks that the child filter is on before the script's first line. |
| `--pool-script FILE` | The agent holds **no handler**: send this file as every request's script (the runtime-pool shape). |
| `-- ARGS…` | Arguments for the agent itself. |

## The test handler

The handler you start the agent with must follow a small contract, or there is
nothing to check about the answers.
[`examples/agents/conformance/`](../../examples/agents/conformance) has one for
each language.

- Return the event it was given, unchanged.
- If `event.stdout` is a string, write it to stdout.
- If `event.stderr` is a string, write it to stderr.
- If `event.spawn` is a string, start a **program** that prints it. That is
  what the `strict` child filter takes away, so the suite must be able to try.
- If `event.sleep_ms` is a number, sleep that long, so that the cancel check
  has a request to arrive *during*.

## What it checks

The suite stops early if `READY` never comes, because nothing else means
anything without a warmed agent. It also stops at the first missed deadline,
and marks the remaining checks as skipped rather than failed many times over.

| # | Check | Required? |
|---|---|---|
| 1 | The agent announces itself with `READY` (`proto` 1, a pid, a runtime name). | yes |
| 2 | `PING` is answered by `PONG` with the same `seq`. | yes |
| 3 | `EXEC` is answered by `FORKED`, naming a process that is not the agent. | yes |
| 4 | The child does nothing until `GO`. | yes |
| 5 | The event reaches the handler and its result comes back. | yes |
| 6 | stdout and stderr come back in separate fields. | yes |
| 7 | Two requests in flight are both answered, each with its own id. | yes |
| 8 | A frame that is not a message gets `ERROR`, not a crash. | yes |
| 9 | A script in `EXEC` is loaded by the child (1.1). | needs `--script` |
| 10 | `ZYGO_CHILD_SECCOMP` is installed in the child, or the request is refused. A second copy of the agent is started with the variable set, and the handler is asked to `spawn`. | yes, on Linux (skipped elsewhere, or if the handler cannot start a program even without a filter) |
| 11 | The child filter is installed before the script's first line (1.1). | needs `--script-spawn` |
| 12 | A cancelled request comes back as `DONE{cancelled}` (1.2). | optional |
| 13 | Output arrives in `CHUNK`s before the `DONE` (1.3). | optional |
| 14 | A long request is reported alive with `PING{id}` (1.4). | optional |
| 15 | `SHUTDOWN` makes the agent exit. | yes |

The first time this suite ran, it found a real bug in the reference Python
agent: a frame that was not valid JSON raised out of the read loop and killed
the agent, taking every request in flight with it. It is an `ERROR` now.

## How the optional checks are judged

An optional feature that is missing is **skipped**, not failed: an agent that
only serves functions is conforming. The last line says "conforms to protocol
1" when nothing failed. A feature that is only half there is reported as
**partial** — "conforms, with gaps named above" — and does not fail the run.

- **Scripts (`--script`).** Pass a file in the agent's own language: the suite
  cannot guess it, and Python sent to a Node agent fails in a way that looks
  like "does not implement 1.1". The suite sends the script three ways: as
  `source`, as a `path`, and with a `digest` that does not match. An agent
  that runs the third has no defence against a tenant swapping the file, and
  fails. One that handles `source` but not `path` is **partial**, because
  `path` is what the supervisor really sends.
- **Filter before script (`--script-spawn`).** The script is run once without
  the filter, to prove it can start a program, and once under it, to prove it
  cannot.
- **Cancel** needs no flag. The suite plays the supervisor's whole part: it
  sends `CANCEL`, kills the child, and checks the answer is `DONE` with
  `cancelled`. An agent that answers `ERROR` / `bad_message` is reported as
  not implementing 1.2; the `sh` agent is the worked example.
- **Stream** needs no flag, and it checks *order*, not content. The handler
  prints, then sleeps for a second and a half, and the first chunk must arrive
  while it is still asleep. Otherwise an agent that buffered everything would
  pass.
- **Heartbeat** starts a long request and waits for a `PING` with its id. An
  agent that sends none is skipped.

## Checking a runtime-pool agent

An agent may also start with **no handler at all**: one warm interpreter per
image and dependency set, with the code arriving in each `EXEC`. That is a
different path through the agent — the child loads tenant code after the fork
and under the child filter — so passing in one shape says little about the
other. Check both. `--pool-script` sends the same echo handler as every
request's script, with a digest, so every check above runs unchanged.

```bash
zygo agent test python3 \
    --pool-script examples/agents/conformance/handler.py \
    --script examples/agents/conformance/script.py -- \
    agents/python/zygo_agent.py --fd 3          # note: no handler

zygo agent test node \
    --pool-script examples/agents/conformance/handler.js \
    --script examples/agents/conformance/script.js -- \
    agents/node/zygo_agent.js
```

`make conformance-node` sends
[`conformance/script.ts`](../../examples/agents/conformance/script.ts) in the
pool run, because that is where TypeScript is checked end to end. The file
goes up as written, the digest is over those bytes, and it contains an `enum`
so that a runtime which only blanks out type annotations cannot pass.

## Four rules an agent has to keep

Each rule is in the protocol document with its reason. They are here because
each has been got wrong at least once, in this repository.

1. **The child never returns to the parent's loop.** A forked child that
   unwinds back into the agent's `serve()` runs the parent's interpreter
   teardown, and then there are two agents on one socket.
2. **Nothing runs before `GO`.** The supervisor has not put the child in a
   cgroup yet, so anything that runs early has no limits.
3. **A malformed frame is reported, not fatal.** The stream is still aligned;
   killing the agent takes every request in flight with it.
4. **Every `EXEC` gets exactly one answer.** A request that is refused,
   overloaded or unreadable still gets a reply, because the supervisor's only
   other choice is to wait out the deadline.

## Where the milliseconds go

These are recommendations, not requirements, because none can be checked from
outside. But they are where an agent's speed is won or lost.

- **Import nothing lazily on the request path.** Every module the child uses
  must already be loaded in the agent, so the child gets it copy-on-write. In
  the reference agent, a deferred `import inspect` (~10 ms) and
  `import random` (~2 ms) put the median at 11.8 ms against a 2 ms budget.
  Moving them into the agent halved it, and the median on Linux is now 1.9 ms.
- **Freeze the heap before forking.** In a runtime that counts references, the
  first garbage-collection pass in a child touches every shared object and
  copies the page it lives on. CPython's `gc.freeze()` cut per-request copying
  from 14.96 MB to 0.81 MB.
- **Reseed the random-number generator in the child**, or every request makes
  the same "random" tokens, temporary names and jitter.
- **Exit hard** with `_exit()`, so that exit handlers and teardown cannot
  damage state the parent also owns.
- **Bound captured output** — Zygo's default is 256 KiB per stream — and say
  when you cut it.

## TypeScript, without a build step

A `.ts` handler and a `.ts` script both run as they are. There is no compile
step, no bundler, and nothing cached between requests: the types are removed
as the module loads, in the child, after the fork — the same place and moment
a `.js` script is compiled.

```toml
[fn.resize]
image = "node:22-slim"
entry = "./resize.ts"     # `runtime` is inferred from the extension
```

Or, over the API, by uploading the TypeScript itself:

```python
script = client.put_script(open("resize.ts").read())
client.run_script("node-pool", script.sha256, {"url": "..."})
```

## Three things to know about TypeScript

- **The digest is over the source as uploaded**, not over the JavaScript the
  agent makes from it, which would differ between Node versions and break
  every request. The check a child makes before loading a script is what makes
  `/run/script/<digest>` safe on a shared user id, and it is unchanged.
- **`enum`, namespaces and parameter properties work.** They are code, not
  just annotations, so they need Node's `transform` mode rather than the
  default `strip` mode. The agent tries `strip` first and falls back, because
  `strip` keeps every line and column where the tenant wrote it and
  `transform` does not.
- **There is no file extension on the wire.** A script arrives as
  `/run/script/<digest>` or as bytes, so the agent decides by what the source
  *is*. Valid JavaScript is valid TypeScript, so it compiles the file as
  JavaScript first. Only a `SyntaxError` — raised before any line of the
  module body runs — sends it back to strip types and try again. A file with
  no types comes back from the stripper unchanged, so a plain JavaScript
  syntax error stays a JavaScript error.

This needs Node 22.13 or later, where `module.stripTypeScriptTypes` arrives.
On older Node the agent uses `amaro` if the image's dependency set has it, and
otherwise says so rather than running the file as JavaScript. Amaro is not
bundled: it is a megabyte of WebAssembly, and this agent is loaded into every
Node sandbox Zygo runs.

## A handler that computes without yielding

A request that spends three seconds in a tight loop is the test of whether an
agent's own housekeeping survives. [`poc/agent_stall.py`](../../poc/agent_stall.py)
measures it. It drives an agent over a socket pair and sends one streaming
request whose script writes, reports progress, then spins without yielding. It
prints when each frame came back. Against both reference agents, with the
handler spinning for three seconds:

| | Python | Node |
|---|---|---|
| `PONG` to a `PING` sent one second into the spin | 0.5 ms | 0.3 ms |
| the request's heartbeat (1.4) | on time | on time |
| output written before the spin | passed on at once | passed on at once |

Neither agent goes quiet, and the reason is in the design. The request runs in
a *process* of its own — a fork in Python, a pooled worker in Node — so the
loop that answers `PING` and `CANCEL` holds no tenant code and has nothing to
block it.

## Lost output, and why there is no thread pool

What the measurement did find was lost output. A Node handler that wrote a
megabyte and then spun had 70 KB of it passed on, and **978 KB silently
lost**: Node queues a write to a pipe in memory, and `process.exit` throws
away what is still queued. The worker now makes stdout and stderr blocking, as
Python's are. Every byte is passed on as it is written, and the `DONE` carries
the last 256 KiB with a note that it was cut. If you write an agent in a
runtime with asynchronous output, this is the part to get right.

A **thread pool inside the worker**, to run tenant code off the worker's own
loop, would buy nothing more, so there is none. Measured on `node:22`, one
`worker_threads` isolate costs 13 MB of memory and 15–23 ms to start, against
43 MB and about 2 ms for the whole Node agent now. A tenant whose handler must
report progress while it computes can start one itself: the `strict` filter
refuses `clone` only without `CLONE_THREAD`, and the permission fallback
passes `--allow-worker` for exactly that reason. What would reopen the
question is a measurement showing an agent going quiet *without* a thread
pool; the one above shows the opposite.

## The runtimes that have no agent

Deno and Bun do not have one, and are not waiting for one:
[ADR 0003](adr/0003-no-deno-or-bun-agent.md) records why, and what would
change it. Both start in a few milliseconds, so a **warm-exec pool** serves
them with no protocol at all — `cmd = ["deno", "run", "--allow-none"]`, with
each request's script as the last argument. That gives up streaming,
`progress()`, workspaces and per-request tenant limits; see
[`examples/warm-exec/`](../../examples/warm-exec). The same is true of anything
that starts fast: Go, Rust, C, `bash`. The runtime name `go` is accepted in
`sandbox.toml`, but no Go agent ships, so a Go function uses warm-exec. Write
an agent when there is something slow to keep warm, not because a runtime is
popular.

## What there is to read

- [`examples/agents/`](../../examples/agents) — the contract in short form,
  what `zygo agent test` checks, and the things that cost an agent its
  milliseconds. Read it before writing one.
- [`examples/agents/sh/`](../../examples/agents/sh) — a complete agent in POSIX
  sh and `jq`, about 130 lines, passing the same checks the Python one does.
  It is the shortest proof that the protocol is language independent. It
  cannot reach `prctl`, so under `strict` it refuses every request.
- [`agents/python/`](../../agents/python) — the reference Python agent, and
  its conformance suite.
- [`agents/node/`](../../agents/node) — the reference Node agent. Node has no
  `fork()`, so it keeps a pool of pre-loaded workers, each serving one request
  and exiting, with a replacement started off the request path. Everything
  else is identical, which is the point of a protocol rather than a library.
  It also carries the forty-line C helper that lets a Node worker install the
  `strict` child filter.
- [`examples/warm-exec/go/`](../../examples/warm-exec/go) — a Go program as a
  warm function, where the whole integration is a `cmd`.
- [`spec/protocol.md`](../../spec/protocol.md) — the full protocol, with every
  field. Its test fixtures live in `spec/fixtures/` and are used by both the
  Rust tests and `agents/python/test_zygo_agent.py`.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [17. The HTTP API, the SDKs and MCP](17-api-sdk-mcp.md) · [Contents](README.md) · **Next: [19. Every command](19-commands.md) →**
