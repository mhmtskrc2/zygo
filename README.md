# Zygo

**Warm sandboxes for function-shaped code — daemonless, rootless, OCI-compatible.**

Zygo runs webhook handlers, agent tools, cron jobs and data transforms with
Docker's ergonomics, but without the container create/destroy cycle. The sandbox
waits warm; a request costs a `fork()`.

```bash
zygo run --mount ./hello.py:/hello.py:ro python:3.12 python3 /hello.py   # first run pulls the image
zygo run --mount ./hello.py:/hello.py:ro python:3.12 python3 /hello.py   # second: ~30 ms, mostly Python itself
zygo serve ./handler.py --name resize                                     # a warm zygote comes up
zygo exec resize '{"url": "..."}'                                         # ~2 ms of overhead
```

## Why

Running a 30-line Python function in a container costs 300–1000 ms, of which the
code itself is 5–20 ms. That overhead is not isolation — a namespace set costs
about 1 ms, a cgroup 0.1 ms, a seccomp filter microseconds. It is orchestration
(daemon → containerd → shim → runc) and cold interpreter start.

Zygo removes both: the orchestration leaves the request path, and the
interpreter start is amortised by a warm zygote that forks per request.

| | `docker run` | `docker exec` | `zygo run` | `zygo exec` (warm) |
|---|---|---|---|---|
| Overhead per request | 300–1000 ms | 50–100 ms | **18 ms** | **1.7 ms** |
| What that pays for | daemon, shim, `runc`, a container object | the daemon round trip | namespaces, cgroup, mounts — in one process | a `fork()` |
| Paid once, up front | — | a `docker run -d`: 300–1000 ms | — | a `zygo serve`: ~270 ms for a Python handler, plus your imports |
| Daemon | yes | yes | **no** | **no** |
| Root | yes | yes | **no** | **no** |
| Clean state per request | yes | no | **yes** | **yes** |
| Boundary | kernel | kernel | kernel, or gVisor | kernel |

Zygo's two numbers are medians with the image cached, on the machines named in
[what Zygo costs](docs/performance.md#the-machines) — a Raspberry Pi 5 and two
VMs on an Apple-silicon Mac, all aarch64; Docker's are its commonly measured
range. The program's own start-up is on top of every column.

## Try it

```bash
cargo build --release && sudo install -m 0755 target/release/zygo /usr/local/bin/zygo

zygo doctor                                   # can this host run sandboxes?
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
[troubleshooting](docs/troubleshooting.md) has both, and `doctor` names them.

**macOS** gets a Linux VM. Every sandbox command is forwarded into one that
Zygo starts and manages, with the same arguments, working directory and
streams, and your home directory mounted at the same path. Two things to have:

```bash
brew install lima            # what starts the VM
make poc/zygo-linux-musl     # the Linux build that runs inside it
```

Crossing into the VM costs about 100 ms per command, which hides the warm path
from anything typed at a Mac shell; it is still there through the API and the
SDKs. [The guide](docs/guide.md#macos) has the details.

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
import zygo
client = zygo.connect()                          # a unix socket, or 127.0.0.1:7700
out = client.fn("resize")({"url": "..."})        # ~2 ms, a fresh process
```

```js
import { connect } from 'zygo';
const out = await connect().fn('resize')({ url: '...' });
```

For an agent host, `zygo mcp` speaks the Model Context Protocol over a pipe.
The tools expose a *program* and nothing else — no image, no mounts, no network,
no limits — because a model reads untrusted text and that text can ask it for
things. Those are set once, by whoever installed the server:

```json
{ "mcpServers": { "zygo": { "command": "zygo", "args": ["mcp"] } } }
```

## Status

**Measured, not asserted.** The warm path is a median of **1.70 ms** and a 99th
percentile of **2.81 ms** through the shipping code at 250 requests a second,
sustaining **981 requests a second** at a concurrency of four; a cold `zygo run`
with the image cached is a median of **18.4 ms**. The suites run in three
places, which turned out to matter: a privileged container, a Raspberry Pi as
an ordinary user under a systemd session, and a Mac. The launcher is checked
against a real kernel, including actual escape attempts; every syscall number
the architecture has is swept against all three seccomp profiles; and fifty
scenarios shaped by use case rather than by mechanism run on two of the three.
[What Zygo costs](docs/performance.md) has the numbers, the hosts, and what is
*not* measured.

**Not ready.** The `vm` backend builds and links libkrun, and no host available
to this project has booted a guest on it, so nothing about it is claimed.
`gvisor` runs one-shot sandboxes only; warm functions and networked sandboxes
on it are refused with a reason rather than weakened. Zygo scales to one
machine, and answers `429` past its capacity. No external audit has been done.

## Documentation

| | |
|---|---|
| [Quickstart](docs/quickstart.md) | From a checkout to three warm functions behind HTTP. |
| [The guide](docs/guide.md) | Everything, in the order you meet it: installing, sandboxes, warm functions, limits, networking, secrets, dependencies, images, deploying, production, backends. |
| [Concepts](docs/concepts.md) | The eight principles, and what each one costs. |
| [`sandbox.toml` reference](docs/spec-reference.md) | Every field, its default, and what it maps to. |
| [What Zygo costs](docs/performance.md) | The measured numbers and the hosts they came from. |
| [Troubleshooting](docs/troubleshooting.md) | The errors people actually hit, and the fix for each. |
| [Threat model](docs/threat-model.md) | Every vector, the control against it, and whether the suite attempts it. |
| [Seccomp profiles](docs/seccomp-profiles.md) | The three syscall profiles and the compatibility matrix. |
| [The SDKs](docs/sdk.md) | The Python and Node clients, the HTTP API, and the deploy gate. |
| [The MCP server](docs/mcp.md) | Giving an agent host a code interpreter with a reviewed boundary. |
| [Writing an agent](docs/agents.md) | Warm functions in a language of your own. |
| [Comparison](docs/comparison.md) | Against Docker, gVisor, Firecracker and the function platforms — including `docker run` and `zygo run` side by side, flag by flag. |
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
agents/python        the reference runtime agent and its conformance suite
spec/protocol.md     the wire protocol
sdk/python           the Python client, and the async one beside it
sdk/node             the Node client, with types and no build step
```

## Development

```bash
make test           # Rust, agent and SDK suites
make check-linux    # type-check the Linux-only code from a non-Linux host
make test-linux     # the full suite inside a Linux container
make verify-linux   # 36 isolation and limit checks against a real kernel
make verify-supervisor-linux  # 139 end-to-end supervisor lifecycle checks
make escape-linux   # 16 escape attempts against a real kernel
make fuzz-linux     # every syscall number, against all three seccomp profiles
make gvisor-linux   # the gvisor backend against a real runsc, compared with ns
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
[docs/threat-model.md](docs/threat-model.md) lists every vector, the control
against it, and whether the escape suite actually attempts it. It also has a
section on where the boundary is weaker than it looks, which is the part worth
reading before you trust this with anything.

No external audit has been done.

## Licence

Apache-2.0.
