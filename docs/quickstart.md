# Quickstart

Ten minutes from a checkout to three warm functions — Python, Go and one
with apt packages — called from a webhook. Linux only; on macOS the code
builds and the platform-independent layers are tested, but sandboxes need a
Linux kernel (a VM or a container is fine, and phase 5 brings a hidden one).

## 1. Build and check the host

```bash
cargo build --release && sudo install -m 0755 target/release/zygo /usr/local/bin/zygo
zygo doctor
```

`doctor` prints one line per requirement — user namespaces, cgroup v2
delegation, seccomp, Landlock, `pasta`/`nft` for egress — with the fix for
each one that is missing. Nothing needs root.

**cgroup delegation is the usual gap**, and on a systemd machine it has two
halves. The user manager has to pass the controllers down:

```bash
mkdir -p ~/.config/systemd/user/user@.service.d
printf '[Service]\nDelegate=cpu cpuset io memory pids\n' > ~/.config/systemd/user/user@.service.d/delegate.conf
systemctl --user daemon-reexec
```

Whatever runs Zygo also has to *be* somewhere delegated, and an ssh login is
not: it sits in a `session-N.scope` that systemd owns, where a sandbox cannot
create the cgroup that would hold its limits. **Zygo handles that half
itself** — a command that builds a sandbox re-executes inside a transient
scope of its own, the way `podman` does, so there is nothing to type.

`zygo doctor` reports on the cgroup it is standing in, and it *attempts* the
thing rather than reading `cgroup.controllers`: from a plain ssh session it
says `cgroup v2 … FAIL`, which is the truth about that cgroup even though
`zygo run` from the same shell will work. To see what a sandbox sees, ask it
from a scope:

```bash
systemd-run --user --scope -p Delegate=yes -- zygo doctor
```

## 2. One function

```bash
mkdir demo && cd demo
cat > handler.py <<'EOF'
def handler(event):
    return {"doubled": event.get("n", 0) * 2}
EOF

zygo serve handler.py --name double      # pulls python:3.12-slim, warms ~300 ms
zygo exec double '{"n": 21}'             # {"doubled": 42}, ~2 ms
zygo ps
```

`serve` starts a supervisor in your session if there is not one, warms a
zygote — the interpreter with `handler.py` imported — and every `exec` is a
`fork()` of it: no import cost, no state carried over between requests.

## 3. A project

```toml
# sandbox.toml
[defaults]
image = "python:3.12-slim"
mem   = "256M"

[fn.resize]
entry        = "./resize.py"
requirements = "./requirements.txt"    # a venv, built once inside the image
system       = ["libwebp7"]            # apt packages, once, as a layer
mem          = "512M"

[fn.parse]
image = "alpine:3"                     # no runtime → warm-exec
mounts = ["./bin/parse:/app/parse:ro"] # a static Go binary
cmd   = ["/app/parse"]

[fn.fetch]
entry   = "./fetch.py"
network = "egress"
allow   = ["api.example.com:443", "*.cdn.example.com:443"]
secrets = ["API_KEY"]
```

```bash
export API_KEY=…                       # read from this shell, delivered as a file
zygo up                                # every [fn.*] warm; re-run it and only what changed restarts
zygo exec fetch '{"path": "/v1/ping"}'
zygo logs fetch -f
zygo down
```

## 4. Behind HTTP

```bash
export ZYGO_API_TOKEN=$(openssl rand -hex 16)
zygo api                               # 127.0.0.1:7700

curl -H "Authorization: Bearer $ZYGO_API_TOKEN" \
     -d '{"n": 4}' http://127.0.0.1:7700/fn/double
```

`POST /fn/<name>` runs one request; `408` is the deadline, `429` is the
tenant's queue being full (with `Retry-After`); `/fn/<name>/batch` takes a list;
`/metrics` is Prometheus text. The API refuses to start without a token unless
it is on loopback or a unix socket.

The same numbers can be pushed to an OpenTelemetry collector instead of
scraped: `zygo api --otlp-endpoint http://localhost:4318` (or
`OTEL_EXPORTER_OTLP_ENDPOINT`) sends them as OTLP/HTTP JSON every minute,
with `OTEL_EXPORTER_OTLP_HEADERS` for an auth header. Metrics only; there are
no request spans yet.

## 5. When something is wrong

```bash
zygo logs fetch --failed              # the requests that failed, with stderr
zygo shell fetch                      # a shell inside the warm sandbox
zygo spec explain fetch               # every limit and mount, resolved
zygo run --dry-run --json python:3.12-slim   # the mount plan without running it
```

## Where next

* [Concepts](concepts.md) — the eight principles, and what each one costs.
* [Spec reference](spec-reference.md) — every field of `sandbox.toml`.
* [Security](../SECURITY.md) and the [threat model](threat-model.md).
* [Comparison](comparison.md) — against Docker, gVisor, Firecracker and
  the function platforms.
* [`examples/`](../examples) — a webhook, a CI job, an LLM tool, a Go
  program, and agents in Node and POSIX sh.
