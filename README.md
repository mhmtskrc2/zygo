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

Design document: [ahmed.md](ahmed.md). Plan and status: [todo.md](todo.md).

> **Status: phase 1 complete, phase 2 underway.** `zygo run` works: a real
> sandbox with namespaces, cgroup limits, `pivot_root`, capability dropping, a
> seccomp allowlist and Landlock. The warm pool works too: `zygo serve`,
> `exec`, `ps` and `stop` run against a supervisor in the user's own session,
> with per-tenant concurrency limits and backpressure.
>
> What is verified rather than asserted: the warm path measures **p50 1.32 ms,
> p99 1.97 ms** through the shipping code at 250 requests/s, against budgets of
> 2 ms and 10 ms ([docs/poc-report.md](docs/poc-report.md)). Drive the same
> tenant past its own CPU quota and the p99 becomes 47 ms — that is the quota
> being enforced, and `zygo bench warm` says so rather than reporting it as
> Zygo's cost. The launcher is checked by 52 scenarios against a real kernel, 16
> of them actual escape attempts, and the supervisor by 22 end-to-end ones.
>
> Still missing from phase 2: request timeouts with `cgroup.kill`, idle tiering,
> crash recovery, the `vm` backend, and the HTTP API. See [todo.md](todo.md) for
> exactly what is built, what is measured, and what is still unverified.

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

Three ideas, in full in [ahmed.md](ahmed.md):

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

Sandboxes need Linux. On any other host everything still builds and the
platform-independent layers are fully tested; `zygo doctor` says plainly what is
missing rather than failing obscurely.

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
mem          = "512M"
mounts       = ["./cache:/cache:rw"]

[fn.parse]
image = "golang:1.23"              # no runtime → warm-exec
cmd   = ["/app/parser"]            # stdin: JSON event, stdout: JSON result

[fn.fetch]
entry   = "./fetch.py"
network = "egress"
allow   = ["api.stripe.com:443", "*.example.com:443"]
secrets = ["STRIPE_KEY"]           # delivered as a file, only to the child
```

Precedence, highest first: **CLI flag → `[fn.<name>]` → `[defaults]` → built-in
default**. `zygo spec explain <fn>` prints the result.

Every limit has a default, and there is no way to disable one without
`--allow-unlimited`. Anything that widens the boundary — host networking,
private-range egress, a writable mount — must be spelled out.

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

Other languages either use a built-in agent (Node, Go) or warm-exec with JSON on
stdin and stdout. The wire protocol is language independent and documented in
[spec/protocol.md](spec/protocol.md); `examples/agents/` will hold reference
implementations.

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
```

## Development

```bash
make test           # Rust + Python suites
make check-linux    # type-check the Linux-only code from a non-Linux host
make test-linux     # the full suite inside a Linux container
make verify-linux   # 36 isolation and limit checks against a real kernel
make verify-supervisor-linux  # 22 end-to-end supervisor lifecycle checks
make escape-linux   # 16 escape attempts against a real kernel
make dist-linux     # the static musl binary, checked against N6
make lint
```

Three rules the test suite is built on, all learned the hard way here:

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

Several checks here passed — or failed — for the wrong reason before those rules
were applied; [docs/poc-report.md](docs/poc-report.md) lists all seven.

Zygo is Linux-first. The `ns` backend needs Linux **5.3+** — the floor is
`clone3`, which has no fallback — plus user namespaces and delegated cgroup v2
controllers. Two later kernels unlock things rather than gate them: 5.11 adds
unprivileged overlayfs (below it the store flattens image layers, which costs
disk and first-run time) and 6.1 has everything in the design document's
appendix C. `zygo doctor` reports each one and prints the fix.

On other platforms the code still builds and the platform-independent layers are
fully tested — macOS gets a hidden Linux VM in phase 5.

## Licence

Apache-2.0.
