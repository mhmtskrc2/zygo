# Zygo

**Warm sandboxes for function-shaped code — daemonless, rootless, OCI-compatible.**

Zygo runs webhook handlers, agent tools, cron jobs and data transforms with
Docker's ergonomics, but without the container create/destroy cycle. The sandbox
waits warm; a request costs a `fork()`.

```bash
zygo run python:3.12 hello.py           # first run: pulls the image
zygo run python:3.12 hello.py           # second: ~30 ms, mostly Python itself
zygo serve ./handler.py --name resize   # a warm zygote comes up
zygo exec resize '{"url": "..."}'       # ~1 ms of overhead
```

**Docs:** the [quickstart](docs/quickstart.md) gets you to three warm
functions; the [guide](docs/guide.md) is everything else. Also:
[concepts](docs/concepts.md) ·
[`sandbox.toml` reference](docs/spec-reference.md) ·
[what it costs](docs/performance.md) ·
[threat model](docs/threat-model.md) ·
[seccomp profiles](docs/seccomp-profiles.md) ·
[comparison](docs/comparison.md) ·
[the SDKs](docs/sdk.md) ·
[the MCP server](docs/mcp.md) ·
[troubleshooting](docs/troubleshooting.md) ·
[examples](examples/) ·
[writing an agent](docs/agents.md) ·
[the wire protocol](spec/protocol.md)

## What works

`zygo run` builds a real sandbox: namespaces, cgroup limits, `pivot_root`,
every capability dropped, a seccomp allowlist and Landlock where the kernel
has it. `zygo serve`, `exec`, `ps` and `stop` run a warm pool in your own
session, with per-function concurrency limits, backpressure, request deadlines
enforced against the whole process tree, automatic rewarming after a crash, and
idle functions that pause and wake in single-digit milliseconds.

`zygo up` brings a whole `sandbox.toml` up and down. It is a deploy rather than
a restart: it replaces only what changed, blue/green. `system = [...]` installs
apt packages once as a layer of their own, with no Dockerfile.
`network = "egress"` gives a sandbox an allowlist enforced by nftables inside
its own network namespace, with no privilege anywhere.

There is an [HTTP API](docs/sdk.md) with bearer auth, [Python and Node
clients](docs/sdk.md) with no dependencies, and an [MCP server](docs/mcp.md)
that gives an agent host a code interpreter whose limits live in a file
somebody reviewed.

**What is measured rather than asserted.** The warm path is a median of
**1.70 ms** and a 99th percentile of **2.81 ms** through the shipping code at
250 requests a second, and it sustains **981 requests a second** at a
concurrency of four. A cold `zygo run` with the image cached is a median of
**18.4 ms**. [What Zygo costs](docs/performance.md) has the rest, including
what is *not* measured and why.

The suites run in three places, which turned out to matter: a privileged
container, a Raspberry Pi as an ordinary user with a systemd session, and a
Mac. The launcher is checked against a real kernel, including actual escape
attempts; every syscall number the architecture has is swept against all three
seccomp profiles; and fifty scenarios shaped by use case rather than by
mechanism run on two of the three.

**What is not ready.** The `vm` backend builds and links libkrun, and no host
available to this project can boot a guest on it, so nothing about it is
claimed. `gvisor` runs one-shot sandboxes only; warm functions and networked
sandboxes on it are refused with a reason rather than weakened. Zygo scales to
one machine, and answers `429` past its capacity. No external audit has been
done.

---

## Why

Running a 30-line Python function in a container costs 300–1000 ms, of which the
code itself is 5–20 ms. That overhead is not isolation — a namespace set costs
about 1 ms, a cgroup 0.1 ms, a seccomp filter microseconds. It is orchestration
(daemon → containerd → shim → runc) and cold interpreter start.

Zygo removes both: the orchestration leaves the request path, and the
interpreter start is amortised by a warm zygote that forks per request.

| | Docker (per request) | Docker `exec` | Zygo, warm |
|---|---|---|---|
| Overhead | 300–1000 ms | 50–100 ms | **1–2 ms** |
| Daemon | yes | yes | **no** |
| Clean state per request | yes | no | **yes** |
| Boundary | kernel | kernel | kernel / gVisor / **KVM** |

## How

Three ideas:

1. **A sandbox is a constrained process, not a container.** Namespaces, cgroups,
   seccomp and Landlock, set up in one process with no RPC.
2. **The sandbox waits warm; nothing is built on the request path.** Compiled
   binaries spawn into a ready sandbox in 1–3 ms (*warm-exec*). Where interpreter
   start is expensive, a small in-sandbox *agent* warms it once and forks per
   request — copy-on-write, so no copying, but no leaked state either.
3. **Isolation is one flag.** `ns` (namespaces), `gvisor` (userspace kernel),
   `vm` (libkrun/Firecracker). Same spec, same command, same protocol.

## Try it

```bash
cargo build --release

zygo doctor                                   # can this host run sandboxes?
zygo run python:3.12-slim python3 -c 'print("hello")'
zygo run --mem 128M --pids 16 --timeout 10s alpine:3 /bin/sh
zygo run --tty alpine:3 /bin/sh               # with a terminal of its own
zygo run --dry-run --json python:3.12-slim    # the plan, without running it
```

Sandboxes need Linux, and on a Mac they get one. Every command except
`doctor`, `completion` and `agent test` is run by a Linux `zygo` inside a VM
Zygo starts for itself, with the same arguments, the same working directory
and the same streams; the exit status comes back out. Two things to have, and
then the lines above work unchanged:

```bash
brew install lima            # what starts the VM
make poc/zygo-linux-musl     # the Linux build that runs inside it
```

The second is what a release would ship beside the binary; from a checkout it
is one `make`, and `zygo` says so if it is missing. The VM is built on the
first command that needs it and takes about a minute; after that a command is
milliseconds, and `zygo stop --all` puts it away again.

One number to set expectations by, because the alternative is measuring the
wrong thing and concluding the benchmark was optimistic. Crossing into the VM
costs about 100 ms per command, and that is the floor for anything typed at a
Mac shell: `zygo exec` and `docker exec` feel the same there, and the ~1 ms
warm path is entirely hidden by the hop. It is reachable on a Mac — through
the HTTP API or the library, where the round trip happens inside the VM and
the 100 ms is paid once by the connection rather than once per request — and
it is what a Linux host gives you at the CLI. The shim is doing its job here
rather than failing at it; it is simply not the thing to benchmark.

The VM mounts your home directory at *the same path*, writable, so
`./handler.py` is one file seen from two sides. That is also the limit and it
is enforced: a command run from outside `$HOME` is refused, and the message
names both directories rather than quietly running somewhere else.

`--dry-run` prints the resolved configuration, the mount plan and the cgroup
values the launcher will apply — how to review a sandbox's boundaries without
running it.

## `sandbox.toml`

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
cmd   = ["/app/parser"]            # stdin: JSON event, stdout: JSON result

[fn.fetch]
entry       = "./fetch.py"
network     = "egress"             # nothing else is reachable
allow       = ["api.stripe.com:443", "*.example.com:443", "203.0.113.0/24:5432"]
connections = 32                   # concurrent TCP connections
bandwidth   = "2M"                 # bytes/s the function may send
secrets     = ["STRIPE_KEY"]       # delivered as a file, only to the child
```

Precedence, highest first: **CLI flag → `[fn.<name>]` → `[defaults]` → built-in
default**. `zygo spec explain <fn>` prints the result.

Every limit has a default, and there is no way to disable one without
`--allow-unlimited`. Anything that widens the boundary — host networking,
private-range egress, a writable mount — must be spelled out.

`requirements` and `system` never touch the image: the venv is built inside a
sandbox with the image's own `pip`, and the packages are installed inside a
writable copy of the image and diffed into a layer of their own. Both are
keyed on the image digest and the list, built once, and shared by every
function that names the same thing.

### Networking

`none` (the default) gives the sandbox an empty network namespace — loopback
and nothing else. `egress` and `full` hand that namespace to
[`pasta`](https://passt.top), which moves packets in userspace as your own
user, and install an nftables allowlist *inside* it:

| | reachable |
|---|---|
| `none` | nothing |
| `egress` | exactly what `allow` names, plus DNS |
| `full` | the public internet |
| `host` | everything, no namespace (needs `--allow-host-net`) |

Private and link-local ranges — the host and its neighbours — stay refused in
every namespaced mode unless you pass `--allow-private-net`. DNS is forced to a
resolver Zygo controls, so a handler cannot reach a resolver of its own to work
around the list, and the host's search domains never enter the sandbox. If
`pasta` or `nft` is missing, a networked sandbox **does not start** rather than
starting unconfined.

`allow` takes `host:port`, `*.domain:port` and `CIDR:port`. Under `egress` the
sandbox's resolver is Zygo's own, running inside the sandbox's namespace: a
name the list does not cover does not resolve, and one it covers has its
addresses admitted to the filter before the answer goes back — so a wildcard
is enforced on the name asked for, and a service that changes address keeps
working.

Two more limits apply to a networked function: `connections` (default 256
concurrent TCP connections, refused with a reset past that) and `bandwidth`
(bytes per second the sandbox may *send*; what it receives is shaped too where
the host has an `ifb` device, and `zygo doctor`'s advice applies where it does
not).

### Deploying a project

```bash
zygo up        # every [fn.*] warm; run it again and only what changed restarts
zygo down      # stops what this spec declares, nothing else
```

```bash
zygo shell resize                    # a debug shell inside the warm sandbox
zygo shell resize -- cat /proc/1/cgroup
zygo logs resize -f                  # the zygote's output and every request
zygo logs resize --failed -n 20      # only the ones that failed
zygo completion zsh > "${fpath[1]}/_zygo"
```

`up` compares each function's resolved spec, secret values and the bytes of its
handler and requirements files against what the supervisor already holds.
An unchanged function is left alone — warm pages, request counters and all — and
a changed one is replaced blue/green: the new sandbox is warm before the old one
stops taking requests, requests the old one had accepted finish on it, and
requests queued behind it are admitted to the new one. `zygo serve` on a name
always replaces, because you just said what you want it to be.

It also writes **`zygo.lock`** beside the spec: the digest each `image`
resolved to and the versions `apt` chose for each `system` package. Commit it.
A later `up` whose spec nobody edited, on an image that has moved, stops and
prints both digests rather than quietly running something else; `zygo up
--relock` accepts the move. Editing the spec re-locks that function without
asking, because you just asked for the change.

## Handler contract

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

Any other language runs as warm-exec: give the function a `cmd` and no runtime,
and each request is a fresh process in the held sandbox with the event on stdin
and JSON expected on stdout.

Where starting your runtime is expensive enough to be worth amortising, write an
*agent* instead. The wire protocol is language independent
([spec/protocol.md](spec/protocol.md)) and `zygo agent test` checks an
implementation against it:

```bash
zygo agent test /bin/sh -- examples/agents/sh/agent.sh examples/agents/sh/handler.sh
```

[`examples/agents/`](examples/agents) has the guide, a Node agent with a
worker pool, and a complete agent in POSIX sh — about 130 lines, passing the
same nine checks the Python one does. For a language that starts fast there is
nothing to amortise: [`examples/warm-exec/go`](examples/warm-exec/go) is a Go
program as a warm function, and the whole integration is a `cmd`.

## From a program, and from an agent

The CLI is for a person. A platform embeds the API, and an agent host speaks a
protocol of its own; both are shipped.

```python
import zygo

client = zygo.connect()                   # `zygo api`, on loopback or a unix socket
out = client.fn("resize")({"url": "..."})  # ~2 ms, a fresh process
r = client.run("python:3.12-slim", ["python3", "-c", "print(6*7)"], mem="128M")
```

```js
import { connect } from 'zygo';
const out = await connect().fn('resize')({ url: '...' });
```

Both clients have **no dependencies** and both speak to the same HTTP API,
over a unix socket at `0600` when it is on this machine — so there is no port
and no token in the usual case. Every kind of failure is its own type, because
each implies something different: a `Busy` means the request never ran and
retrying is correct, a `HandlerError` means it will fail again.
[docs/sdk.md](docs/sdk.md).

`zygo api` starts **call-only**: a token reaches the functions somebody
declared in a spec file and nothing else. `--allow-deploy` adds serving,
stopping and one-shot runs, which together are a shell rather than an API, so
it is a flag rather than a default.

For an agent, `zygo mcp` speaks the Model Context Protocol over a pipe:

```json
{ "mcpServers": { "zygo": { "command": "zygo", "args": ["mcp"] } } }
```

That is the whole installation, and the host gains `run_code`,
`list_functions`, `call_function` and `function_logs`. The tools expose a
*program* and nothing else — no image, no mounts, no network, no limits. Those
are set once, on the command line, by the person who installed the server,
because a model reads untrusted text and that text can ask it for things. A
model that needs more declares a function in `sandbox.toml` and calls it by
name, so the boundary lives in a file somebody reviewed.
[docs/mcp.md](docs/mcp.md).

## Layout

```
crates/zygo-core     the library; the CLI and the bindings sit on top (ADR-008)
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
make dist-linux     # the static musl binary, checked against N6
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

Zygo is Linux-first. The `ns` backend needs Linux **5.3+** — the floor is
`clone3`, which has no fallback — plus user namespaces and delegated cgroup v2
controllers. Two later kernels unlock things rather than gate them: 5.11 adds
unprivileged overlayfs (below it the store flattens image layers, which costs
disk and first-run time) and 6.1 has everything in the design document's
appendix C. `zygo doctor` reports each one and prints the fix.

On macOS the code builds, the platform-independent layers are fully tested, and
sandboxes run in a Linux VM the shim manages — `zygo doctor` prints what that
VM says about itself and exits with its answer. A warm `exec` from the Mac
round-trips in 96 ms, nearly all of it the hop into the VM rather than the
request; `make verify-shim` is 14 checks against a real VM.

Two things to know before running Zygo on Ubuntu or Debian. Both are the same
shape: a distribution's AppArmor policy, not a Zygo setting, and `zygo doctor`
or the error message names the fix.

A **networked** sandbox needs `pasta`, and Ubuntu ships an AppArmor profile
that confines it. Where that profile is enforcing, `pasta` is denied
`/proc/<pid>/ns/user` and `network = "egress"` or `"full"` cannot start — on a
host where `/dev/net/tun` is present and working. The error says so and names
`aa-complain`; `network = "none"`, the default, needs no `pasta` at all.

And:
`kernel.apparmor_restrict_unprivileged_userns=1` lets an unprivileged process
create a user namespace and then refuses the first mount inside it, which is
the first thing every sandbox does. `zygo doctor` detects it by attempting
that mount, and prints the one-line fix — read
[docs/threat-model.md](docs/threat-model.md) first, because the fix turns off
a protection for every process on the machine, not only Zygo's.

## Security

Zygo runs other people's code on purpose, so an escape is the most serious kind
of bug it can have. [SECURITY.md](SECURITY.md) is how to report one — privately,
through GitHub, not as an issue — and what is in scope.
[docs/threat-model.md](docs/threat-model.md) lists every vector, the control
against it, and whether the escape suite actually attempts it. It also has a
section on where the boundary is weaker than it looks, which is the part worth
reading before you trust this with anything.

[docs/seccomp-profiles.md](docs/seccomp-profiles.md) describes the three
syscall profiles and the compatibility matrix — five reference packages
exercised under `default` and `strict`, every cell an attempt — including the
two bugs the first run of that matrix found in the profile itself.

No external audit has been done.

## Licence

Apache-2.0.
