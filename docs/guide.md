# The Zygo guide

Everything you need to run other people's code safely, in order. The
[quickstart](quickstart.md) is the five-minute version; this is the rest.

1. [Installing](#installing)
2. [Your first sandbox](#your-first-sandbox)
3. [Warm functions](#warm-functions)
4. [`sandbox.toml`](#sandboxtoml)
5. [Limits](#limits)
6. [Networking](#networking)
7. [Secrets](#secrets)
8. [Dependencies](#dependencies)
9. [Deploying](#deploying)
10. [Running it in production](#running-it-in-production)
11. [Choosing an isolation backend](#choosing-an-isolation-backend)
12. [What Zygo will not do](#what-zygo-will-not-do)

---

## Installing

Zygo is one static binary with no runtime dependencies.

```bash
cargo build --release
./target/release/zygo doctor
```

`zygo doctor` is the first thing to run anywhere. It probes this host for
everything a sandbox needs, reports each one, and prints the fix for anything
missing. It attempts what it reports rather than reading settings, so a kernel
that has a flag and ignores it is caught here rather than in a sandbox.

### Linux

Zygo needs kernel **5.3 or newer**, unprivileged user namespaces, and cgroup v2
controllers delegated to your user. Two later kernels unlock things rather than
gate them: 5.11 adds unprivileged overlayfs, and below it the image store
flattens layers instead, which costs disk and first-run time; 5.13 adds
Landlock.

**On Ubuntu and Debian**, two AppArmor policies get in the way, and both are
the distribution's rather than Zygo's.

`kernel.apparmor_restrict_unprivileged_userns=1` lets an unprivileged process
create a user namespace and then refuses the first mount inside it, which is
the first thing every sandbox does. `zygo doctor` detects it by attempting that
mount and prints the one-line fix. Read the [threat model](threat-model.md)
first: the fix turns off a protection for every process on the machine, not
only Zygo's.

An AppArmor profile also confines `pasta`, the program Zygo uses to give a
sandbox a network, and denies it access to the sandbox's user namespace. Where
that profile is enforcing, `network = "egress"` and `"full"` cannot start even
though `/dev/net/tun` is present and working. The error says so and names
`aa-complain`. The default, `network = "none"`, needs no `pasta` at all.

### macOS

Sandboxes are Linux, and on a Mac they get one. Every command except `doctor`,
`completion` and `agent test` is run by a Linux `zygo` inside a virtual machine
Zygo manages, with the same arguments, the same working directory and the same
streams; the exit status comes back out.

```bash
brew install lima            # what starts the VM
make poc/zygo-linux-musl     # the Linux build that runs inside it
```

The VM is built on the first command that needs it and takes about a minute.
After that a command is milliseconds, and `zygo stop --all` puts it away again.

Your home directory is mounted at *the same path* inside the VM, writable, so
`./handler.py` is one file seen from two sides. That is also the limit and it is
enforced: a command run from outside `$HOME` is refused, and the message names
both directories rather than quietly running somewhere else.

Crossing into the VM costs about 100 ms per command. See
[what Zygo costs](performance.md#on-a-mac) for what that does and does not mean.

---

## Your first sandbox

```bash
zygo run python:3.12-slim python3 -c 'print("hello")'
```

The first run pulls the image, exactly as `docker run` does. The second is
about 18 ms.

What that sandbox has: a read-only root filesystem built from the image's
layers, a writable `/tmp` sized by `scratch`, no network at all, no
capabilities, a seccomp allowlist, and memory, CPU and process limits it cannot
exceed. What it does not have: your filesystem, your network, your processes,
or any way to reach the image store it was built from.

```bash
zygo run --mem 128M --pids 16 --timeout 10s alpine:3 /bin/sh
zygo run --mount ./data:/data:ro alpine:3 /bin/ls /data
zygo run --tty alpine:3 /bin/sh          # a terminal of its own
```

**Before you trust it**, look at what it will actually do:

```bash
zygo run --dry-run --json python:3.12-slim
```

That prints the resolved configuration, the mount plan and the cgroup values
the launcher will apply, and runs nothing. It is how a sandbox's boundaries get
reviewed.

### Knowing why a sandbox ended

The exit code is the program's, with one exception: a sandbox that Zygo or the
kernel stopped exits **137**. That covers both running out of time and running
out of memory, because both are a `SIGKILL` and the wait status carries nothing
else.

When the difference matters — an online judge, a CI step — ask for it:

```bash
zygo run --outcome /tmp/why.json --mem 64M --timeout 5s python:3.12-slim python3 big.py
cat /tmp/why.json
# {"exit_code":137,"timed_out":false,"oom_killed":true,"peak_rss_kb":65780,"wall_ms":412.7}
```

`timed_out` comes from the launcher, which enforced the deadline. `oom_killed`
comes from the kernel's own counter in the sandbox's cgroup. Neither is a
guess. It goes to a file because standard output belongs to the program.

---

## Warm functions

A one-shot sandbox costs about 18 ms, most of it setup. A warm function pays
that once and then costs about **1.7 ms** a request.

```bash
zygo serve ./handler.py --name resize
zygo exec resize '{"url": "https://example.com/a.png"}'
zygo ps
zygo stop resize
```

The handler is an ordinary Python file:

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

### What "warm" means, exactly

The sandbox is built once and a small agent inside it imports your handler.
Each request is a `fork()` of that agent. The fork shares the parent's memory
copy-on-write, so nothing is copied and nothing is re-imported — and because
it is a separate process, **nothing a request writes is visible to the next
one**. You get the speed of a shared interpreter and the isolation of a fresh
one.

That is the trade Zygo exists to make. A long-running worker is fast and leaks
state between requests. A container per request is clean and costs hundreds of
milliseconds. A fork is both.

### Other languages

Two ways, and you pick by whether starting your runtime is worth amortising.

**Warm-exec** is the simple one: give a function a `cmd` and no runtime, and
each request is a fresh process in the held sandbox with the event on standard
input and JSON expected on standard output. That costs about 2.2 ms and needs
no code from you beyond the program.

```toml
[fn.parse]
image = "golang:1.23"
cmd   = ["/app/parser"]
```

**An agent** is for a runtime that is expensive to start. It warms once and
forks per request, the way the Python one does. The wire protocol is language
independent, and `zygo agent test` checks an implementation against it:

```bash
zygo agent test /bin/sh -- examples/agents/sh/agent.sh examples/agents/sh/handler.sh
```

[`examples/agents/`](../examples/agents) has the guide, a Node agent with a
worker pool, and a complete agent in POSIX sh of about 130 lines that passes
the same nine checks the Python one does.

---

## `sandbox.toml`

One file describes a project's functions.

```toml
[defaults]
image     = "python:3.12-slim"
isolation = "ns"          # ns | gvisor | vm
mem       = "256M"
cpu       = 0.5
pids      = 64
timeout   = "30s"
network   = "none"

[fn.resize]
entry        = "./resize.py"       # defines handler(event)
requirements = "./requirements.txt"
system       = ["libwebp7"]        # apt packages, installed once as a layer
mem          = "512M"
mounts       = ["./cache:/cache:rw"]

[fn.parse]
image = "golang:1.23"              # no runtime → warm-exec
cmd   = ["/app/parser"]

[fn.fetch]
entry       = "./fetch.py"
network     = "egress"             # nothing else is reachable
allow       = ["api.stripe.com:443", "*.example.com:443", "203.0.113.0/24:5432"]
connections = 32
bandwidth   = "2M"
secrets     = ["STRIPE_KEY"]
```

Precedence, highest first: **a CLI flag → `[fn.<name>]` → `[defaults]` → the
built-in default**. To see what a function actually resolved to:

```bash
zygo spec explain resize
zygo spec validate
```

Every field is in the [`sandbox.toml` reference](spec-reference.md).

---

## Limits

Every limit has a default and there is **no way to disable one** without
`--allow-unlimited`. That is deliberate: a sandbox with no memory limit is not
a sandbox, and the common way to end up with one is forgetting rather than
deciding.

| | what it bounds | default |
|---|---|---|
| `mem` | memory, enforced by the kernel | 256M |
| `cpu` | CPU, in cores | 0.5 |
| `pids` | processes — the fork-bomb limit | 64 |
| `timeout` | wall clock, against the whole process tree | 30s |
| `scratch` | the size of the writable `/tmp` | 64M |
| `nofile` | open files | 1024 |
| `connections` | concurrent TCP connections | 256 |
| `bandwidth` | bytes a second the sandbox may send | unlimited, with a warning |

A request that overruns `timeout` is killed with everything it started, not
just the process Zygo can see. The kill goes through the cgroup, so a handler
that forked helpers takes them with it.

---

## Networking

```toml
network = "none"     # the default
```

| | reachable |
|---|---|
| `none` | nothing at all — an empty network namespace with loopback |
| `egress` | exactly what `allow` names, plus DNS |
| `full` | the public internet |
| `host` | everything, no namespace — needs `--allow-host-net` |

`egress` and `full` hand the sandbox's network namespace to
[`pasta`](https://passt.top), which moves packets in userspace as your own
user, and install an nftables allowlist **inside** that namespace. No
privilege is needed anywhere. If `pasta` or `nft` is missing, a networked
sandbox **does not start** rather than starting unconfined.

```toml
allow = ["api.stripe.com:443", "*.example.com:443", "203.0.113.0/24:5432"]
```

Three forms: `host:port`, `*.domain:port` and `CIDR:port`.

Under `egress` the sandbox's resolver is Zygo's own, running inside the
sandbox's namespace. A name the list does not cover does not resolve, and one
it covers has its addresses admitted to the filter before the answer goes back.
So a wildcard is enforced on the name that was asked for, and a service that
changes address keeps working. A handler cannot reach a resolver of its own to
work around the list, and the host's search domains never enter the sandbox.

**Private and link-local ranges stay refused** in every namespaced mode unless
you pass `--allow-private-net`. That includes `169.254.169.254`, the cloud
metadata address, which is the first thing a compromised handler tries.

---

## Secrets

```toml
[fn.fetch]
secrets = ["STRIPE_KEY"]
```

```bash
export STRIPE_KEY=sk_live_...
zygo up
```

The value is read from **your** shell, not the supervisor's, and written into
the sandbox as `/run/secrets/STRIPE_KEY` at mode 0400 — created with that mode
rather than chmodded afterwards, so there is no moment at which it is readable
more widely.

The file exists only while a request is running, and only the request's own
process can read it. It is never in the environment, never in the warm agent's
memory, and never on the control socket. A handler reads it as a file:

```python
def handler(event):
    key = open("/run/secrets/STRIPE_KEY").read().strip()
```

That shape matters for agent tools in particular: the model writing the code
cannot print a secret it never had.

---

## Dependencies

Two kinds, and neither touches the image.

```toml
requirements = "./requirements.txt"   # Python packages, into a venv
system       = ["libwebp7"]           # apt packages, into a layer
```

`requirements` is built inside a sandbox with the image's own `pip` — the only
way the environment matches the interpreter that will run under it — and
mounted read-only at `/venv`, with `/venv/bin` first on `PATH`.

`system` installs the packages inside a writable copy of the image and diffs
the result into an OCI layer of its own. No Dockerfile, no rebuild of anything
else.

Both are keyed on the image's digest and the list, built once, and shared by
every function that names the same thing. An edit invalidates the key; a
different image is a different key, because a wheel built for another Python
fails at import time with a useless message.

One-shot runs share the same cache:

```bash
zygo run --requirements ./requirements.txt python:3.12-slim python3 -m pytest
```

The first job with a given image and file builds it; every job after that, and
every warm function with the same pair, reuses it.

---

## Deploying

```bash
zygo up        # every [fn.*] warm
zygo down      # stops what this spec declares, and nothing else
```

Run `up` again after an edit and **only what changed restarts**. It compares
each function's resolved spec, its secret values and the bytes of its handler
and requirements files against what is already running. An unchanged function
is left alone, warm pages and request counters and all.

A changed one is replaced **blue/green**: the new sandbox is warm before the
old one stops taking requests, requests the old one had accepted finish on it,
and requests queued behind it are admitted to the new one.

`up` also writes **`zygo.lock`** beside the spec: the digest each `image`
resolved to, and the versions `apt` chose for each `system` package. **Commit
it.** A later `up` whose spec nobody edited, on an image that has moved, stops
and prints both digests rather than quietly running something else. `zygo up
--relock` accepts the move. Editing the spec re-locks that function without
asking, because you just asked for the change.

---

## Running it in production

### Watching it

```bash
zygo ps                      # what is warm, and its counters
zygo top                     # ps on a timer, plus rates
zygo stats resize            # latencies over the log window
zygo logs resize -f          # the zygote's output and every request
zygo logs resize --failed -n 20
```

`zygo stats` keeps counters since warm-up and latencies over the log window
separate and labelled, and refuses to report a 99th percentile under a hundred
samples rather than inventing one.

### Debugging a live function

```bash
zygo shell resize
zygo shell resize -- cat /proc/1/cgroup
```

That is a fresh process entered into the function's namespaces. The warm agent
is untouched, keeps its memory and keeps serving. The shell sees the sandbox's
filesystem, processes, network and hostname, holds no capabilities, and is
deliberately **not** under the seccomp filter, the Landlock ruleset or the
tenant's cgroup — a debug shell that the memory limit kills is not one.

### Calling it from a program

```bash
zygo api                     # 127.0.0.1:7700, bearer auth
```

```python
import zygo
client = zygo.connect()
out = client.fn("resize")({"url": "..."})
```

The API starts **call-only**: a token reaches the functions somebody declared
in a spec file and nothing else. `--allow-deploy` adds serving, stopping and
one-shot runs, which together are a shell rather than an API. See
[the SDKs](sdk.md).

### Giving it to an agent

```json
{ "mcpServers": { "zygo": { "command": "zygo", "args": ["mcp"] } } }
```

That is the whole installation. See [the MCP server](mcp.md).

### Capacity

Zygo's capacity is a per-host budget. A function at its `concurrency` limit
queues briefly and then answers `429` with the numbers, which is backpressure
and not a failure: the request never ran, and retrying is the right response.

---

## Choosing an isolation backend

```toml
isolation = "ns"     # ns | gvisor | vm
```

**`ns`** is namespaces, cgroups, seccomp and Landlock: one kernel, shared with
the host, with every control the kernel offers turned on. It is what everything
here is measured on, and it is the only backend that runs warm functions.

**`gvisor`** puts a userspace kernel between the sandbox and yours, which is a
smaller attack surface at a syscall cost. One-shot runs only; warm functions
and networked sandboxes on it are refused with a reason rather than weakened.

```bash
zygo backend install gvisor
zygo run --isolation gvisor python:3.12-slim python3 -c 'import platform; print(platform.release())'
```

**`vm`** is a hardware boundary. It builds and links, and no host available to
this project can boot a guest on it, so nothing about it is claimed yet.

```bash
zygo backend list        # what this host can actually use
```

Which to pick: `ns` for code you chose or code you half trust, and the strongest
boundary you can get for code you did not choose. Read the
[threat model](threat-model.md) before trusting any of them with something that
matters — in particular the section on where the boundary is weaker than it
looks.

---

## What Zygo will not do

Stated plainly, because a tool that is vague about its limits is worse than one
that lacks a feature.

- **Run on macOS or Windows natively.** Sandboxes are Linux. On a Mac, Zygo
  manages a Linux VM for you.
- **Accept connections.** No mode of Zygo is a server for your traffic. A
  function is called through the CLI, the SDKs or Zygo's own HTTP API, and you
  put your own ingress in front of that.
- **Scale past one machine.** Capacity is a per-host budget and a `429` past it.
- **Replace Docker.** Zygo uses OCI images and none of Docker's runtime. If you
  need `docker compose`, long-running services or port publishing, you need
  Docker.
- **Analyse the code it runs.** Zygo contains hostile code. It does not tell
  you the code was hostile: there is no audit mode, no network log and no
  verdict.
- **Hide the kernel.** The `ns` backend is one kernel, and everything here says
  so.
