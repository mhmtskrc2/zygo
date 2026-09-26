# 13. Warm functions

A warm function is a sandbox that Zygo builds once and keeps ready, so that
each request costs a `fork()` instead of a new sandbox. This chapter shows how
to serve one, call it, watch it and stop it, and what your code must look like
in Python, Node, TypeScript or any other language.
[Chapter 6](06-how-zygo-works.md#the-warm-path) explains the idea behind it;
this chapter is about using it.

## Warm functions in one picture

A *handler* is the function you write: it gets one request and returns one
answer. `zygo serve` builds a sandbox, starts a small helper program inside it
called the *agent*, and the agent loads your handler. That loaded, waiting
process is the *zygote*. `zygo exec` sends a request; the zygote makes a copy
of itself with `fork()`, and the copy runs your handler once and exits.

```text
  zygo serve ./handler.py --name resize        (once, about 150 ms)
        │
        ▼
  ┌────────────────────── warm sandbox "resize" ───────────────────────┐
  │                                                                    │
  │   zygote: python started, imports done, handler loaded             │
  │      │                                                             │
  │      ├── fork ─▶ child: handler(event 1) ─▶ answer ─▶ exit         │
  │      ├── fork ─▶ child: handler(event 2) ─▶ answer ─▶ exit         │
  │      └── fork ─▶ child: handler(event 3) ─▶ answer ─▶ exit         │
  │                                                                    │
  └────────────────────────────────────────────────────────────────────┘
        ▲
        │
  zygo exec resize '{"url": "…"}'             (each time, about 1.4 ms)
```

## Why this is the production shape

A one-shot sandbox (`zygo run`) costs about 12 ms, and most of that is setup.
A warm function pays the setup once and then costs about **1.4 ms** a
request: that is the median overhead measured on a 2-vCPU Lima VM on an Apple
M1 Max ([chapter 25](25-performance.md)). The gap is easy to miss. `zygo run`
looks like the natural way to say "run this code once", so a program that
uses Zygo that way gets one sandbox per event. On that VM, `zygo bench cold`
says 12.3 ms for that, while `zygo bench warm` says **1.44 ms** and 1,108
requests a second — more than eight times faster, and the gap grows with
every module the handler imports.

A multi-tenant application that starts with `run` for exactly that reason
should expect to move to warm functions once it counts the events. [The
multi-tenant example below](#a-multi-tenant-consumer-on-the-warm-path) shows
what that looks like.

```text
  one sandbox per event (zygo run)          one warm function (zygo serve + exec)
  ────────────────────────────────          ──────────────────────────────────────
  event ─▶ build sandbox ─▶ run ─▶ clean    serve: build sandbox + load code, once
  event ─▶ build sandbox ─▶ run ─▶ clean    event ─▶ fork ─▶ run ─▶ exit
  event ─▶ build sandbox ─▶ run ─▶ clean    event ─▶ fork ─▶ run ─▶ exit
  12.3 ms each (2-vCPU Lima VM)             1.44 ms each (same VM)
```

## Serving a function

`zygo serve` takes a handler file and a name. It starts the *supervisor* (the
background process that owns every warm sandbox) if one is not running yet,
warms the sandbox, and returns. From then on the function is ready.

```bash
zygo serve ./handler.py --name resize
zygo serve ./handler.py --name resize --requirements requirements.txt --mem 512M
zygo serve ./report.py  --name report --secret STRIPE_KEY --idle-timeout 5m
```

| Flag | What it does |
|---|---|
| `--name N` | The name you call the function by. |
| `--image I` | The image to warm from. The default comes from the runtime: `python:3.12-slim` or `node:22-slim`. |
| `--requirements FILE` | A dependency file, installed once into a shared, cached `/venv`. |
| `--concurrency N` | How many requests may run at the same time in one zygote. Default 4. |
| `--idle-timeout D` | Pause the zygote after this long with no requests. Default `10m`. |
| `--mode function\|stdin` | How the handler is called; see [`mode = "stdin"`](#mode--stdin). |
| `--secret NAME` | Deliver the secret `NAME`, taken from this shell's environment, as `/run/secrets/NAME`. Repeat for more. |
| `-f PATH` | Read this `sandbox.toml` instead of searching upwards for one. |

Every limit and sandbox flag of `zygo run` works here too: `--mem`, `--cpu`,
`--pids`, `--timeout`, `--network`, `--allow`, `--mount` and the rest. They
are described in [chapter 14](14-limits-network-secrets.md). The same
settings can live in a `[fn.<name>]` table of `sandbox.toml`
([chapter 20](20-sandbox-toml.md)).

## Calling a function

`zygo exec NAME EVENT` sends one request. The *event* is any JSON value; if
you leave it out, `zygo exec` reads it from standard input. The handler's
answer goes to standard output. Whatever the handler itself printed goes to
standard error, so you can pipe the answer into another program and still see
the logs.

```bash
zygo exec resize '{"url": "https://example.com/a.png"}'
echo '{"url": "https://example.com/b.png"}' | zygo exec resize
zygo exec resize --timeout 5s '{"url": "https://example.com/c.png"}' > out.json
```

`--timeout` gives up on this request after that long; without it, the
function's own `timeout` applies. The exit code of `zygo exec` tells a script
what happened:

| Exit code | Meaning |
|---|---|
| the request's own | The handler finished; `0` is success. |
| `137` | The request's deadline killed it. |
| `75` | The function is busy: every slot and the whole queue are full. Try again later. |
| `4` | There is no function with that name. |
| `125` | There is no supervisor running. |

## Many events at once: `--batch`

`--batch` reads *NDJSON* from standard input: one JSON event per line. It
sends them to the function in parallel and prints one JSON answer per line,
in the same order as the input. The exit code is 0 only if every request
succeeded.

```bash
zygo exec resize --batch < events.ndjson > answers.ndjson
```

```text
  events.ndjson           the function (concurrency 4)          answers.ndjson
  ─────────────           ────────────────────────────          ──────────────
  line 1 ──────────────▶  fork ─▶ answer 1 ─────────────────▶  line 1
  line 2 ──────────────▶  fork ─▶ answer 2 ─────────────────▶  line 2
  line 3 ──────────────▶  fork ─▶ answer 3 ─────────────────▶  line 3
  ...                     (they run side by side,               (always in
                           and may finish in any order)          input order)
```

## When it is full: concurrency and the queue

Each zygote runs up to `concurrency` requests at once (default 4). More
requests wait in a queue that holds up to four times `concurrency`. When the
queue is full too, a new request is turned away at once as **busy**: HTTP 429
from the API, exit code 75 from `zygo exec`. Turning work away quickly is
better than letting every caller wait for a timeout.

```text
                          concurrency = 4
  new request ─▶ ┌───────────────────────────────┐
                 │ running: [1] [2] [3] [4]      │  full? ─▶ wait in the queue
                 └───────────────────────────────┘
                 ┌───────────────────────────────┐
                 │ queue: up to 16 (4 × 4)       │  full? ─▶ busy: HTTP 429, exit 75
                 └───────────────────────────────┘
```

## Looking after warm functions

These commands work on functions that are already served. [Chapter
19](19-commands.md) has every flag.

| Command | What it does |
|---|---|
| `zygo ps` | Lists warm sandboxes: name, state (warm, paused, cold), memory, requests. |
| `zygo stop NAME` | Stops one function. `zygo stop --all` stops every one. |
| `zygo logs NAME` | Shows the last 50 log entries: the zygote's own output and one line per request with its stdout and stderr. |
| `zygo logs NAME -f` | Keeps printing new entries as they arrive. `-n 200` starts with more. |
| `zygo logs NAME --failed` | Only requests that failed: a non-zero exit or an error. |
| `zygo shell NAME` | Opens a shell inside the function's sandbox, for debugging. |
| `zygo shell NAME -- ls /app` | Runs one command there instead of a shell. |
| `zygo top` | A live table of every function's resources, updated every 2 seconds. |
| `zygo stats [NAME]` | A summary of the metrics, for all functions or one. |

`zygo shell` needs one warning. It starts a new process and enters the
sandbox's *namespaces* (the kernel's walls around files, processes, network
and host name), so it sees what the handler sees. It holds no capabilities.
But it is **not** under the seccomp filter, the Landlock rules or the
function's cgroup, so that a debugging shell is not killed by the memory
limit. The warm zygote is not touched and keeps serving.

## Warm, paused, cold

A warm function does not stay in memory forever. After `idle_timeout`
(default 10 minutes) with no requests, Zygo *pauses* it: the cgroup is frozen,
so its processes stop using the CPU but stay in memory. The next request wakes
it with one write, in far less time than a warm-up. After `cold_after`
(default 1 hour) the sandbox is dropped. The function still exists by name,
and the next request pays a full warm-up again: about 150 ms for a Python
handler with no imports, measured on a Raspberry Pi 5.

```text
                zygo serve               no request for idle_timeout (10m)
  (nothing) ─────────────▶ ┌────────┐ ──────────────────────────────▶ ┌──────────┐
                           │  warm  │                                 │  paused  │
                           │        │ ◀────────────────────────────── │          │
                           └────────┘   a request wakes it (1 write)  └──────────┘
                               ▲                                           │
                               │ a request warms it again                  │ no request for
                               │ (about 150 ms for Python)                 │ cold_after (1h)
                           ┌───┴────┐                                      │
                           │  cold  │ ◀────────────────────────────────────┘
                           └────────┘   sandbox dropped, name kept
```

| State | In memory? | Uses CPU? | Cost of the next request |
|---|---|---|---|
| warm | yes | only while serving | a fork, about 1.4 ms |
| paused | yes | no | one write to wake it, then a fork |
| cold | no | no | a full warm-up, then a fork |

## Which shape to use

There are three ways to give Zygo code to keep warm. Pick by two questions:
is the code the same on every request, and does its runtime start slowly?

```text
                         is your code the same on every request?
                              │
               yes ───────────┴──────────── no: each request brings a script
                │                                        │
      does its runtime start slowly?              RUNTIME POOL
      (Python, Node with many modules)            [runtime.<name>], --runtime
                │                                 one warm interpreter,
        yes ────┴──── no (Go, Rust, C, sh)        thousands of scripts
         │                    │
   AGENT FUNCTION        WARM-EXEC FUNCTION
   entry = "h.py"        cmd = ["/app/bin"]
   fork per request      new process per request
   ~1.4 ms               ~1.4 ms + your program's start
```

## A Python handler

Write a module with a function called `handler` at the top level. It gets the
event — whatever JSON the caller sent, already parsed — and returns something
that can be turned into JSON. It may be a normal function or an `async` one;
Zygo runs an awaitable to the end. There is no second "context" argument.
Everything at module level runs **once**, in the zygote, before any request:
put your imports, your model loading and your compiled regular expressions
there.

```python
import json, re                       # runs once, when the zygote warms
PATTERN = re.compile(r"\d+")          # also once

def handler(event):                   # runs in a fresh fork, per request
    numbers = PATTERN.findall(event["text"])
    return {"count": len(numbers)}
```

A larger one, which makes image thumbnails:

```python
from PIL import Image          # imported once, in the zygote
import io, base64

MAX = (800, 800)               # module-level state is shared, copy-on-write

def handler(event: dict) -> dict:
    """Runs in a fresh fork per request. Writing to globals is safe, but the
    next request will not see it."""
    img = Image.open(io.BytesIO(base64.b64decode(event["image"])))
    img.thumbnail(MAX)
    out = io.BytesIO(); img.save(out, "WEBP")
    return {"image": base64.b64encode(out.getvalue()).decode(), "size": img.size}
```

## What "warm" means, exactly

The sandbox is built once, and the agent inside it imports your handler. Each
request is a `fork()` of that agent. A fork shares the parent's memory
*copy-on-write*: the pages are shared until one side writes to one, and only
that page is copied. So nothing is copied up front and nothing is imported
again. And because the child is a separate process, **nothing a request writes
is visible to the next one**. You get the speed of a shared interpreter and
the isolation of a fresh one.

That is the trade Zygo exists to make. A long-running worker is fast but leaks
state from one request to the next. A container per request is clean but costs
hundreds of milliseconds. A fork is both fast and clean.

```text
  long-running worker          container per request         fork per request (Zygo)
  ───────────────────          ─────────────────────         ───────────────────────
  fast                         clean                         fast AND clean
  request 2 sees what          hundreds of ms each           each child starts as a copy
  request 1 left behind                                      of the zygote, then is gone
```

A fork carries three things that a fresh process would not. `make
fork-sweep-linux` measured them across 44 popular PyPI packages. The next
three sections say what Zygo does about each.

## When a handler is not safe to fork

A fork copies only the thread that called it. If another thread held a lock at
that moment, the lock stays locked in every child, and the child can hang. So
a process that started threads cannot be forked safely. At warm-up the Python
agent checks for threads that would survive a fork — Python threads *and*
native ones, such as the four that `import duckdb` starts. If it finds any, it
falls back to **spawning** a fresh interpreter per request instead of forking.
That is correct, but much slower, and `zygo logs` says when it happened.
Thread pools that stop themselves before a fork, as OpenBLAS's does, do not
trigger it. The fix is to start threads inside the handler, or lazily on first
use.

## Random numbers in a fork

A child starts with a copy of its parent's random-number state, so without
care every request would draw the same "random" numbers. The agent reseeds
Python's `random`, numpy's global generator and torch's generator in every
child. It cannot reseed a generator that *you* created at import time, such as
`RNG = random.Random()` or `np.random.default_rng()`: every request draws the
same numbers from it. The agent names such a generator in `zygo logs` at
warm-up. Create it inside the handler instead.

## Work done lazily on first use

Every request is the first call in its own process. So a library that sets
itself up on first use pays that cost on every request, and none of it is
kept. Do that work at import time instead, where the zygote pays it once:
build the `boto3` client, compile the template, and create the pydantic model
at module level.

## A Node handler

Export a function: `module.exports = function handler(event) {…}` or
`module.exports.handler = …`. It may return a value or a promise; `undefined`
becomes `null`. One difference to know: Node is not safe to fork, so the Node
agent keeps **two pre-loaded worker processes** ready. Each worker runs one
request and exits, and a new one is started off the request path. The effect
— a clean process per request — is the same, but the cost per request is a
little higher than Python's fork.

```javascript
const crypto = require("crypto");          // loaded once, in each pre-loaded worker

module.exports = async function handler(event) {
  return { sha256: crypto.createHash("sha256").update(event.text).digest("hex") };
};
```

```text
  Node agent (no fork)
  ┌────────────────────────────────────────────────────────────────┐
  │  agent ── keeps 2 workers loaded and parked                    │
  │    worker A: handler loaded ─▶ request 1 ─▶ exit               │
  │    worker B: handler loaded ─▶ request 2 ─▶ exit               │
  │    worker C: started in the background, ready for request 3    │
  └────────────────────────────────────────────────────────────────┘
```

## TypeScript handlers

A `.ts` file is loaded as TypeScript with its types removed, with no build
step and no bundler. The types are stripped as the module loads, in the
worker, which is the same place a `.js` file is compiled. `enum`, namespaces
and parameter properties work too. This needs Node 22.13 or later, or the
`amaro` package in the function's dependencies.
[Chapter 18](18-writing-an-agent.md#typescript-without-a-build-step) has the
details.

```toml
[fn.resize]
image = "node:22-slim"
entry = "./resize.ts"     # runtime = "node", inferred from the extension
```

## What a request sees

| | |
|---|---|
| **The event** | The JSON the caller sent (`null` for an empty body). In Python a JSON object arrives as a `dict` with one extra method, `event.progress(msg)`. |
| **Environment** | `ZYGO_REQUEST_ID`; `ZYGO_DEADLINE_MS`, the request's time budget in milliseconds; `ZYGO_WORKSPACE` if a workspace was sent; `ZYGO_FUNCTION`, the function's name; plus the function's `env`. `ZYGO_TENANT` holds the same name as `ZYGO_FUNCTION`; it is an older, misleading name kept so that old handlers do not break. |
| **Secrets** | Files at `/run/secrets/<NAME>`, mode 0400, readable only inside this function's sandbox, and gone once no request of it is running. Never in the environment. In a runtime pool: the *calling* tenant's values, and the request has its zygote to itself while they exist. |
| **Files** | The image, read-only; your `mounts`; a temporary folder of its own, which `TMPDIR` names and which is removed afterwards; `/venv` if there are `requirements`. `/tmp` itself is shared by every request in the sandbox: write through `tempfile`, `os.tmpdir()` or `$TMPDIR`, not to a literal `/tmp/...` path. |
| **Working folder** | `workdir` (`/app`), or the workspace if one was sent: the agent changes into it before your code runs. |
| **Memory** | A copy of the zygote's. What you change is yours alone and gone at the end. |

## What a request returns

A return value must turn into JSON; in Python, `bytes` become base64. Output
printed to stdout and stderr is **not** the result. Each is collected
separately (the last 256 KiB of each) and returned beside it, and sent live,
in pieces, when the caller asks for a stream. If the handler raises an
exception, the request fails with exit code 1, and its error is the traceback
without the agent's own lines. `event.progress("step 2 of 5")` sends a
progress line to a caller that is streaming, and does nothing otherwise.

```text
                   ┌──────────── what comes back ────────────┐
  handler(event) ─▶│ result     the return value, as JSON    │
                   │ stdout     what it printed (≤ 256 KiB)  │
  print(...)    ──▶│ stderr     what it logged  (≤ 256 KiB)  │
                   │ exit_code  0, or 1 if it raised         │
                   │ error      the traceback, if it raised  │
                   └─────────────────────────────────────────┘
```

## `mode = "stdin"`

This is for a Python handler written as a script rather than as a function.
The file is run once per request with the event as JSON on standard input, and
whatever it prints to standard output is parsed as the result. A non-zero exit
is an error. Use it for existing scripts you do not want to change. It is
slower than a handler: each request starts a new Python process for the
script, inside the warm sandbox, so the script's imports are paid every time.
Because it starts a program, it cannot run under `seccomp = "strict"`, which
forbids that.

```toml
[fn.legacy]
entry = "./old_script.py"
mode  = "stdin"
```

## Warm-exec functions

For a program that starts fast, set `cmd` and no `entry`. The sandbox is kept
warm, and each request starts `cmd` as a new process inside it. The contract
is the simplest one possible: read one JSON event from **stdin**, write one
JSON result to **stdout**, exit 0. Stderr is kept as the log, and a non-zero
exit is a failure. `sh -c cat` is the smallest program that obeys it. It works
with any language and any image, and needs no agent. It costs a median of
1.4 ms per request, measured on a Lima VM, plus your program's own
start.

```toml
[fn.parse]
image  = "alpine:3"
mounts = ["./bin/parse:/app/parse:ro"]   # a static binary you built
cmd    = ["/app/parse"]
```

## Runtime pools

A pool is a warm interpreter with **no code of anyone's in it**. Each request
carries a script — as source, or as the `sha256:` *digest* (a fingerprint of
the file's bytes) of a script stored earlier with `PUT /scripts`. The forked
child loads it, runs its `handler` (or the function named by
`entry_point`), and exits. The script is loaded *after* the child's seccomp
filter is on, so even its import-time code is filtered, and a pool's seccomp
profile defaults to `strict`. This is how a platform with ten thousand user
scripts keeps a handful of zygotes warm instead of ten thousand.

```bash
zygo serve --runtime py312 --image python:3.12-slim --agent python
zygo serve --runtime node22 --image node:22-slim --agent node --min-warm 2 --max-warm 8
zygo exec --runtime py312 --script report.py '{"month": "2026-09"}'
zygo exec --runtime py312 --script sha256:9f2c… --entry-point monthly '{}'
```

```text
  one pool, many tenants' scripts
  ┌────────── pool py312 (min_warm 2, max_warm 8) ──────────────┐
  │  zygote  python + requirements, NO user code                │
  │    ├── fork ─▶ child loads script A (tenant acme) ─▶ exit   │
  │    ├── fork ─▶ child loads script B (tenant beta) ─▶ exit   │
  │    └── fork ─▶ child loads script A again         ─▶ exit   │
  └─────────────────────────────────────────────────────────────┘
      +0.5 ms per request, against a function with its code warmed in
```

A pool keeps `min_warm` zygotes ready whatever the load (default 1; 0 counts
as 1) and may grow to `max_warm` under load (default the larger of `min_warm`
and 4). `--agent` is `python`, `node`, or the path of your own agent inside
the sandbox. The +0.5 ms was measured with a different script on every
request, a thousand of them: +0.47 ms on the Lima VM and +0.46 ms on Docker
Desktop's ([chapter 25](25-performance.md#the-embedders-benchmark)). A pool
with a `cmd` instead of an `agent` is a *warm-exec pool*: the script's path
is passed as the last argument of `cmd` on each request.

**Secrets in a pool.** A pool can name secrets (`secrets = ["STRIPE_KEY"]`
in `[runtime.<name>]`, or `serve_runtime(..., secrets=[...])`), but it holds
no values: the zygotes are shared. On each request the *calling* tenant's
values are read from the tenant secret store and written as
`/run/secrets/<NAME>` for that one request, exactly as for a function — and
while they exist, the request has its zygote **to itself**, so no other
tenant's child is forked beside the files. A tenant that lacks one of the
names is refused before anything runs. [Chapter
14](14-limits-network-secrets.md#secrets-in-a-runtime-pool) has the rules;
`zygo stop <name>` stops a pool as it does a function.

## A multi-tenant consumer on the warm path

Think of a low-code platform, a workflow engine or a plugin host. It has hundreds of scripts
written by its users, the *tenants*, and a script changes whenever somebody
presses Save. Each project has its own mounts and its own egress allowlist
(the list of hosts it may reach), and each run has its own secrets. The
one-shot mapping is one sandbox per event. The warm mapping is **one warm
zygote per script version**, forked per run, and it looks like this:

```python
import hashlib, zygo

client = zygo.connect()                      # zygo api --allow-deploy, in the VM on a Mac

def run(project, script_source, event, secrets):
    # A version is a function. The name carries the digest, so an edit is a
    # new function and the old one is reaped by --idle-timeout, not by you.
    digest = hashlib.sha256(script_source.encode()).hexdigest()[:16]
    name = f"{project.id}-{digest}"
    client.serve(
        name,
        {
            "entry": project.script_path(digest),          # the version, on disk
            "mounts": [f"{project.data_dir}:/data:rw"],    # per project
            "network": "egress",
            "allow": project.allowlist,                    # per project
            "secrets": list(secrets),                      # names; values per run
            "idle_timeout": "10m",                         # reaped when idle
            "cold_after": "1h",
            "mem": "256M", "timeout": "30s",
        },
        if_changed=True,                                   # a no-op when it is warm
    )
    return client.fn(name)(event)                          # one fork
```

```text
  project acme, script v1 ─▶ function "acme-3f9a…" ─▶ warm zygote ─▶ fork per run
  project acme, script v2 ─▶ function "acme-c21e…" ─▶ warm zygote ─▶ fork per run
                             (v1 is no longer called: paused after 10m, dropped after 1h)
  project beta, script v1 ─▶ function "beta-77d0…" ─▶ warm zygote ─▶ fork per run
```

## What each line buys

- **`if_changed=True`** makes the `serve` free when the version is already
  warm. The caller does not have to track state: it always asks, and the
  supervisor does nothing when nothing changed.
- **Mounts and the allowlist are per function**, so two projects' versions
  are two zygotes that share an interpreter's memory pages and nothing else.
- **Secrets are per run.** The names are declared once; the values arrive
  with the request and exist as files only while the request runs.
- **`idle_timeout` and `cold_after`** are the eviction policy. A version
  nobody has called for ten minutes is paused (still in memory, one write to
  wake). After an hour it is dropped, and the next call pays a warm-up —
  about 150 ms for a Python handler, plus its imports.

Four hundred projects do not mean four hundred warm zygotes. They mean as many
as were called in the last ten minutes, which is the number that matters, and
`zygo ps` shows it. [ADR 0005](adr/0005-one-warm-zygote-per-script-version.md)
has the memory per warm script, measured on a 4 GB VM, and the eviction policy
written down. [`examples/workflow-engine/`](../../examples/workflow-engine) is
this shape end to end — a worker draining a job queue, one warm function per
script version, and an LRU (least recently used) list over warm scripts — in
Python and Node.

## Other languages

There are two ways, and you pick by whether starting your runtime is slow
enough to be worth doing only once.

**Warm-exec** is the simple one: give a function a `cmd` and no runtime. Each
request is a fresh process in the held sandbox, with the event on standard
input and JSON expected on standard output. That costs about 1.4 ms and needs
no code from you beyond the program. Go, Rust, C and `bash` all belong here;
[`examples/warm-exec/`](../../examples/warm-exec) has Go and shell examples.

**An agent** is for a runtime that is slow to start. It warms once and gives
each request a process of its own. Two agents ship with Zygo, and the
runtime is chosen from the `entry` file's extension:

| `entry` ends in | `runtime` | How each request runs |
|---|---|---|
| `.py` | `python` | a fork of the warmed interpreter |
| `.js`, `.mjs`, `.cjs`, `.ts` | `node` | one of the pre-loaded, parked workers |
| `.go` | `go` | **not built yet**: the name is accepted, but no Go agent ships, so `serve` fails with "no warm agent for go". Use warm-exec for Go. |
| anything | `{ agent = "/path" }` | your own agent, at that path in the sandbox |

```toml
[fn.resize]
entry = "./resize.py"      # runtime = "python": a fork of the warmed interpreter
[fn.summarise]
entry = "./summarise.js"   # runtime = "node": a pool of pre-loaded workers
[fn.custom]
entry   = "./handler.rb"
runtime = { agent = "/app/zygo-agent" }
```

Node has no `fork()` in the Unix sense, so its agent keeps workers loaded and
parked instead — one request each, replaced off the request path. Everything
above that is the same: the same wire protocol, the same per-request cgroup,
the same deadline, the same secrets. Deno and Bun have no agent and are not
waiting for one ([ADR 0003](adr/0003-no-deno-or-bun-agent.md)); they start in
a few milliseconds, so a warm-exec pool serves them.

## Your own agent

Any language can have a real warm path by shipping an agent: a program that
talks to the supervisor over a socket, using a small documented protocol
([`spec/protocol.md`](../../spec/protocol.md)). Messages are length-prefixed
JSON: `READY` when warm, `EXEC` per request, `FORKED` and `GO` around the
fork, `CHUNK` for streamed output, `RESULT`, `DONE`, `CANCEL`, `PING`/`PONG`
and `SHUTDOWN`. Name it with `runtime = { agent = "/path/in/sandbox" }`.
`zygo agent test` runs the conformance suite against it before you trust it:

```bash
zygo agent test /bin/sh -- examples/agents/sh/agent.sh examples/agents/sh/handler.sh
```

[`examples/agents/`](../../examples/agents) has a complete agent in POSIX sh
of about 130 lines that passes the same checks the shipped ones do.
[Chapter 18](18-writing-an-agent.md) is the full guide.

## Handler rules, in short

1. Do slow things at module level; they run once.
2. Do not start threads at import.
3. Create random generators inside the handler, not at import.
4. Return JSON; print only for logs.
5. Read secrets from `/run/secrets/`, never from the environment.
6. Write only to `/tmp`, the workspace or a `:rw` mount; the rest is read-only.
7. Assume nothing survives the request — because nothing does.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [12. One-shot sandboxes](12-one-shot-sandboxes.md) · [Contents](README.md) · **Next: [14. Limits, networking and secrets](14-limits-network-secrets.md) →**
