# Zygo

**Warm sandboxes for function-shaped code — daemonless, rootless, OCI-compatible.**

*This page is the first page of [the Zygo book](docs/book/README.md). Read it
for the idea in five minutes; the book takes it from there.*

Zygo runs webhook handlers, agent tools, cron jobs and data transforms with
Docker's ergonomics, but without the container create/destroy cycle. The sandbox
waits warm; a request costs a `fork()`.

```bash
zygo serve ./handler.py --name resize        # a warm zygote: ~150 ms, once
zygo exec resize '{"url": "..."}'            # 1.4 ms of overhead, a fresh process
zygo exec resize '{"url": "..."}'            # and again, on a clean copy
```

## Why

A warm process that serves many requests is fast and dirty: request *n* sees
whatever request *n-1* left — a monkeypatch, a cached connection, a mutated
module, an `atexit` handler. A container per request is clean and slow:
300–1000 ms, of which your code is 5–20 ms.

Zygo is the third thing. `zygo serve` starts an interpreter, lets it do its
imports, and parks it. `zygo exec` forks it. The child is a copy of a process
that has **never served a request**, so it is as clean as a fresh container
and as cheap as a fork — a median of **1.4 ms** against 300–1000.

| | `docker exec` | a shared worker process | **`zygo exec`** |
|---|---|---|---|
| Overhead per request | 50–100 ms | ~0 | **1.4 ms** usually (1 in 100: 10.5 ms on Linux 6.x, 2.6 ms on 5.10) |
| What request *n* can see of *n-1* | everything | everything | **nothing** |
| Limits per request | the container's | none | **its own cgroup: memory, pids, CPU, a deadline** |
| A request that overruns | kills the container | kills the worker | killed through its own cgroup; the zygote keeps serving |
| Paid once, up front | a `docker run -d` | your worker's start | a `zygo serve`: ~150 ms for a Python handler, plus your imports |

```text
WARM ── pay once, then request after request
═══════════════════════════════════════════════════════════════════════════

  docker run -d ──▶ one container, shared state
                       │
                       ├─ docker exec ▶ dockerd ▶ containerd ▶ shim ▶ runc ▶ process
                       ├─ docker exec ▶ dockerd ▶ containerd ▶ shim ▶ runc ▶ process
                       │                                   same state, every time
                       ▼
                    docker rm                                50–100 ms per exec


  zygo serve ──▶ warm zygote: interpreter up, imports done, waiting
                       │
                       ├─ zygo exec ▶ fork() ▶ process
                       ├─ zygo exec ▶ fork() ▶ process
                       │               clean copy, every time
                       ▼
                    zygo down                                ~1.7 ms per exec
```

A handler is a function — `def handler(event)` — and everything around it is
the runtime's: the fork, the per-request cgroup, the deadline, the secrets
written outside the sandbox and removed afterwards, and the metrics. A
compiled program needs none of that and gets the same treatment through
*warm-exec*: the sandbox is held and each request is a fresh process running
your `cmd`, at about 2 ms. There is a protocol
([`spec/protocol.md`](spec/protocol.md)) rather than an interface, so an agent
in any language gets all of it — Zygo ships one for Python and one for Node,
and `zygo agent test` checks anything else against the same conversation.

## And the one-shot case, underneath

Everything above is built on an ordinary sandbox, and that sandbox is worth
having by itself:

```bash
zygo run --mount ./hello.py:/hello.py:ro python:3.12 python3 /hello.py   # first run pulls the image
zygo run --mount ./hello.py:/hello.py:ro python:3.12 python3 /hello.py   # second: ~12 ms on Linux, mostly Python itself
```

Running a 30-line Python function in a container costs 300–1000 ms, and that
overhead is not isolation — a namespace set costs about 1 ms, a cgroup 0.1 ms,
a seccomp filter microseconds. It is orchestration: daemon → containerd →
shim → runc, and a container object left behind to remove.

| | `docker run` | `docker exec` | `zygo run` | `zygo exec` (warm) |
|---|---|---|---|---|
| Overhead per request | 300–1000 ms | 50–100 ms | **12 ms** | **1.4 ms** |
| What that pays for | daemon, shim, `runc`, a container object | the daemon round trip | namespaces, cgroup, mounts — in one process | a `fork()` |
| Paid once, up front | — | a `docker run -d`: 300–1000 ms | — | a `zygo serve`: ~150 ms for a Python handler, plus your imports |
| Daemon | yes | yes | **no** | **no** |
| Root | yes | yes | **no** | **no** |
| Clean state per request | yes | no | **yes** | **yes** |
| Boundary | kernel | kernel | kernel, or gVisor | kernel |

Zygo's two numbers are medians with the image cached, on the machines named in
[what Zygo costs](docs/book/25-performance.md#the-machines) — a Raspberry Pi 5 and two
VMs on an Apple-silicon Mac, all aarch64; Docker's are its commonly measured
range. The program's own start-up is on top of every column. `zygo bench all`
reproduces every one of them on your host, prints the machine it ran on, and
refuses to give a verdict if that machine was throttled or busy.

```text
ONE-SHOT ── one request, one fresh sandbox
════════════════════════════════════════════════════════════════════

  docker run                          zygo run
  ──────────                          ────────
  docker CLI                          zygo
     │                                   │  clone3 · mounts · cgroup
     ▼                                   │  seccomp · Landlock · execve
  dockerd                                ▼
     │                                your program
     ▼                                   │
  containerd                             ▼ exit
     │                                nothing left behind
     ▼
  shim
     │
     ▼
  runc
     │
     ▼
  your program
     │
     ▼ exit
  container object stays → docker rm

  300–1000 ms                         ~12 ms
```

## Try it

```bash
# Linux, x86_64 or aarch64: one static binary, no runtime dependencies
curl -fsSL "https://github.com/mhmtskrc2/zygo/releases/latest/download/zygo-$(uname -m)-unknown-linux-musl.tar.gz" | tar xz
sudo install -m 0755 zygo-*/zygo /usr/local/bin/zygo

# macOS: the shim, the Linux build it forwards into, and Lima
brew install mhmtskrc2/zygo/zygo

# or from source, anywhere with Rust
cargo install zygo-cli

# or the container image, which needs no privileges, only a few specific flags
docker run --user 0:0 --security-opt seccomp=unconfined \
    --security-opt systempaths=unconfined --security-opt apparmor=unconfined \
    --cgroupns=host --cgroup-parent=/zygo -v /sys/fs/cgroup/zygo:/sys/fs/cgroup/zygo:rw \
    -p 7700:7700 -e ZYGO_API_TOKEN=... ghcr.io/mhmtskrc2/zygo
```

Every release lists the archives' checksums in `SHA256SUMS`, and the container
image is signed with cosign;
[chapter 11](docs/book/11-getting-started.md) shows how to check both.

[Chapter 16](docs/book/16-production.md#running-zygo-inside-a-container) says
what each flag is for and why; `zygo doctor` names any that are missing, in
the container or on a host.

```bash
zygo doctor                                   # can this host run sandboxes?
zygo doctor --fix                             # and apply what it names, after asking

# the warm path, which is the point
echo 'def handler(event): return {"got": event}' > handler.py
zygo serve ./handler.py --name echo
zygo exec echo '{"n": 1}'
zygo bench all                                # every published number, on this host

# and the sandbox underneath it
zygo run python:3.12-slim python3 -c 'print("hello")'
zygo run --mem 128M --pids 16 --timeout 10s alpine:3 /bin/sh
zygo run --tty alpine:3 /bin/sh               # with a terminal of its own
zygo run --dry-run --json python:3.12-slim    # the plan, without running it
```

**Linux** needs kernel 5.3 or newer, unprivileged user namespaces and cgroup v2
controllers delegated to your user; 6.1 or newer is recommended, because that
is where Landlock's network rules, `cgroup.kill` and `memory.peak` are all
present. `zygo doctor` attempts each requirement rather than reading a setting,
and prints the fix for anything missing. On Ubuntu and Debian two AppArmor
policies get in the way of sandboxes and of networked sandboxes respectively;
[troubleshooting](docs/book/22-troubleshooting.md) has both, and `doctor` names them.

**macOS** gets a Linux VM. Every sandbox command is forwarded into one that
Zygo starts and manages, with the same arguments, working directory and
streams, and your home directory mounted at the same path. It needs `limactl`,
which starts the VM, and a Linux build of Zygo to put inside it. The Homebrew
formula installs both; from a checkout they are:

```bash
brew install lima            # what starts the VM
make guest-build             # the Linux build that runs inside it, compiled in the VM
```

Crossing into the VM costs about 22 ms per command once it is up — the shim
uses the SSH connection Lima already holds — so a one-shot `run` from a Mac
shell is about 29 ms, of which ~6 ms is the sandbox. The millisecond warm path
is there through the API and the SDKs, and through `zygo api` running *inside*
the VM. [Getting started](docs/book/11-getting-started.md) has the details, and
[what Zygo costs](docs/book/25-performance.md#on-a-mac) has the numbers.

## How

Three ideas:

1. **A sandbox is a constrained process, not a container.** Namespaces, cgroups,
   seccomp and Landlock, set up in one process with no RPC.
2. **The sandbox waits warm; nothing is built on the request path.** Compiled
   binaries spawn into a ready sandbox in 1–3 ms (*warm-exec*). Where interpreter
   start is expensive, a small in-sandbox *agent* warms it once and forks per
   request — copy-on-write, so no copying, but no leaked state either.
3. **Isolation is one flag.** `ns` (namespaces), `gvisor` (userspace kernel),
   `vm` (libkrun). Same spec, same command, same protocol.

## What is in the box

One file describes a project:

```toml
[defaults]
image     = "python:3.12-slim"
mem       = "256M"
cpu       = 0.5
timeout   = "30s"
network   = "none"

[fn.resize]
entry        = "./resize.py"       # defines handler(event); warmed once, forked per request
requirements = "./requirements.txt"
system       = ["libwebp7"]        # apt packages, installed once as a layer
mem          = "512M"

[fn.parse]
image  = "alpine:3"                # no runtime → warm-exec: a fresh process per request
mounts = ["./bin/parse:/app/parse:ro"]  # a static binary you built, mounted in
cmd    = ["/app/parse"]            # stdin: JSON event, stdout: JSON result

[fn.fetch]
entry   = "./fetch.py"
network = "egress"                 # nothing else is reachable
allow   = ["api.stripe.com:443", "*.example.com:443"]
secrets = ["STRIPE_KEY"]           # delivered as a file, only to the request's process
```

```bash
zygo up                            # every function warm; run it again and only what changed restarts
zygo exec fetch '{"path": "/v1/ping"}'
zygo logs fetch --failed -n 20
zygo shell resize                  # a debug shell inside the warm sandbox
zygo down
```

- **Every limit is mandatory** — memory, CPU, pids, wall clock, scratch, open
  files — with a default, and no way to disable one without a flag you have to
  type. The deadline kills the request's whole process tree.
- **Networking is off by default.** `egress` is an allowlist by name, enforced
  by nftables inside the sandbox's own namespace with a resolver Zygo controls;
  private ranges and the cloud metadata address stay refused. No privilege
  anywhere: `pasta` moves the packets as your own user.
- **Dependencies never touch the image.** `requirements` becomes a venv built
  with the image's own `pip`; `system` becomes an OCI layer of its own. Both are
  built once and shared by everything that names the same thing.
- **`zygo up` is a deploy, not a restart.** Unchanged functions are left warm;
  changed ones are replaced blue/green. It writes `zygo.lock` with the digest
  each image resolved to, and refuses to run a moved image silently.
- **Secrets are files, for the duration of a request.** Read from your shell,
  written by the supervisor from outside the sandbox at mode 0400, never in the
  environment and never in the warm agent's memory.
- **Any language.** Python handlers get the fork path. Anything else is
  warm-exec with a `cmd`, or an agent of your own — the protocol is language
  independent, and `zygo agent test` checks an implementation against it.

From a program, the same functions are behind an HTTP API with bearer auth and
two dependency-free clients:

```python
import zygo_sdk as zygo
client = zygo.connect()                          # a unix socket, or 127.0.0.1:7700
out = client.fn("resize")({"url": "..."}).result  # ~2 ms, a fresh process
```

```js
import { connect } from 'zygo-sdk';
const out = (await connect().fn('resize')({ url: '...' })).result;
```

For an agent host, `zygo mcp` speaks the Model Context Protocol over a pipe.
The tools expose a *program* and nothing else — no image, no mounts, no network,
no limits — because a model reads untrusted text and that text can ask it for
things. Those are set once, by whoever installed the server:

```json
{ "mcpServers": { "zygo": { "command": "zygo", "args": ["mcp"] } } }
```

## Status

**Measured, not asserted.** On a Linux 6.8 VM, the warm path is a median of
**1.44 ms** through the shipping code at 250 requests a second — with a 99th
percentile of **10.5 ms**, a kernel cgroup cost the book explains — sustaining
**1,108 requests a second** at a concurrency of four; a cold `zygo run` with
the image cached is a median of **12.3 ms**. The warm path is the
production shape, and the gap is the argument: [the book](docs/book/13-warm-functions.md)
shows a multi-tenant consumer — one warm zygote per script version — on it. The suites run in three
places, which turned out to matter: a privileged container, a Raspberry Pi as
an ordinary user under a systemd session, and a Mac. The launcher is checked
against a real kernel, including actual escape attempts; every syscall number
the architecture has is swept against all three seccomp profiles; and fifty
scenarios shaped by use case rather than by mechanism run on two of the three.
[What Zygo costs](docs/book/25-performance.md) has the numbers, the hosts, and what is
*not* measured — and `zygo bench all` reproduces every one of them on your own
host, printing the machine it ran on and refusing to give a verdict when that
machine was throttled or busy.

**Scoped, not unfinished.** The `vm` backend boots a guest and runs one-shot
sandboxes — about 420 ms against `ns`'s 73 ms on the same host, for a kernel
of the guest's own. The guest can write, to a private layer bounded by
`scratch` and never to the shared image. It has no network and no warm
functions, and `gvisor` has neither either; both refuse them with a reason
rather than weakening something. That is a decision rather than a gap —
[ADR 0002](docs/book/adr/0002-warm-paths-stay-on-ns.md) says why, and what would
reopen it. Warm functions are an `ns` feature.

Zygo scales to one machine, and answers `429` past its capacity. No external
audit has been done.

## Documentation

Everything is in **[the Zygo book](docs/book/README.md)** — one book, in plain
English, with diagrams throughout. It starts from zero and ends with the full
reference.

| | |
|---|---|
| [Part I — Container 101](docs/book/README.md#part-i--container-101) | The kernel, namespaces, cgroups, seccomp and Landlock, and Docker: what a sandbox is made of. |
| [Part II — Zygo, explained](docs/book/README.md#part-ii--zygo-explained) | How Zygo works, where it saves, its principles, FreeBSD jails, and every similar project — `docker run` against `zygo run`, flag by flag. |
| [Part III — Using Zygo](docs/book/README.md#part-iii--using-zygo) | [Getting started](docs/book/11-getting-started.md), one-shot sandboxes, warm functions, limits and networking, images, production, [the API and SDKs](docs/book/17-api-sdk-mcp.md), writing an agent. |
| [Part IV — Reference](docs/book/README.md#part-iv--reference) | [Every command](docs/book/19-commands.md), [every `sandbox.toml` field](docs/book/20-sandbox-toml.md), environment, files and exit codes, [troubleshooting](docs/book/22-troubleshooting.md). |
| [Part V — Security and speed](docs/book/README.md#part-v--security-and-speed) | [The threat model](docs/book/23-security.md), seccomp profiles, and [what Zygo costs](docs/book/25-performance.md). |
| [Part VI — Decisions](docs/book/README.md#part-vi--decisions) | Why it is built this way, and the design records. |
| [`examples/`](examples/) | A webhook, a CI job, an LLM tool, a Go program, and agents in Node and POSIX sh. |
| [`spec/protocol.md`](spec/protocol.md) | The wire protocol between the supervisor and an agent. |

## Layout

```
crates/zygo-core     the library; the CLI and the bindings sit on top
  spec/              sandbox.toml surface, layering, validation
  image/             OCI references, content-addressed store, registry client
  sandbox/           mount plan and resource limits, backend independent
  cgroup.rs          the two-level cgroup v2 hierarchy
  backend/           ns | gvisor | vm
  protocol/          the warm execution wire protocol
  doctor.rs          environment probing
crates/zygo-cli      the `zygo` binary
agents/python        the reference Python agent and its conformance suite
agents/node          the reference Node agent: a worker pool, not a fork
spec/protocol.md     the wire protocol
sdk/python           the Python client, and the async one beside it
sdk/node             the Node client, with types and no build step
packaging/oci        the container image, and a worker image built on it
```

## Development

```bash
make test           # Rust, agent and SDK suites
make check-linux    # type-check the Linux-only code from a non-Linux host
make test-linux     # the full suite inside a Linux container
make verify-linux   # 36 isolation and limit checks against a real kernel
make verify-supervisor-linux  # 139 end-to-end supervisor lifecycle checks
make escape-linux   # 22 escape attempts against a real kernel
make fuzz-linux     # every syscall number, against all three seccomp profiles
make landlock-net-linux  # Landlock's bind/connect rules, on a 6.7+ kernel
make verify-deps-linux   # a dependency set built from a lockfile, over the API
make seccomp-matrix-linux  # real packages, and Node, under default and strict
make gvisor-linux   # the gvisor backend against a real runsc, compared with ns
make conformance    # the agent protocol suite, against all three reference agents
make bench          # every published number, reproduced on this host
make verify-mcp     # 26 checks driving `zygo mcp` over a pipe, against a real kernel
make test-sdk       # the Python and Node clients, against a stand-in API
make verify-shim    # 14 macOS checks, against the Linux VM the shim manages
make verify-login-linux  # 15 checks against a registry that really refuses people
make dist-linux     # the static musl binary, checked against its size budget
make lint
```

Four rules the test suite is built on, all learned the hard way here:

- **A test must attempt the thing, not inspect a setting.** Reading a flag
  passes on a kernel that ignores the flag. Every escape case runs the escape.
- **A test must not disturb what it measures.** Checking `isatty(1)` through a
  pipe, or comparing a sandbox's terminal while redirecting stdout, measures the
  pipe.
- **A latency measurement must be able to say whether it hit a limit.** A
  closed-loop benchmark under a hard CPU quota measures the quota: `zygo bench
  warm` reads the tenant's `cpu.stat` and declines to judge the p99 budget when
  the tenant was throttled, because that number would be about the limit rather
  than about this code.
- **A negative check must first prove the thing ran.** "The connection was
  refused", "no process leaked" and "the suite exited zero" are all satisfied by
  nothing having happened at all — the failure mode that fails *open*. Each one
  establishes the positive case first.

Several checks here passed — or failed — for the wrong reason before those rules
were applied.

## Security

Zygo runs other people's code on purpose, so an escape is the most serious kind
of bug it can have. [SECURITY.md](SECURITY.md) is how to report one — privately,
through GitHub, not as an issue — and what is in scope.
[docs/book/23-security.md](docs/book/23-security.md) lists every vector, the control
against it, and whether the escape suite actually attempts it. It also has a
section on where the boundary is weaker than it looks, which is the part worth
reading before you trust this with anything.

No external audit has been done.

## Licence

Apache-2.0.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

**Next: [The Zygo booklet](docs/book/README.md) →**
