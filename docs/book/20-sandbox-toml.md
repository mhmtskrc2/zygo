# 20. `sandbox.toml`, field by field

One file describes a project: its functions, its runtime pools and its API.
This chapter lists every section and every field the file accepts, with the
default Zygo uses when you leave it out. It is written from the parser and
the resolver (`crates/zygo-core/src/spec/`), not from memory.

## The smallest project

Two files make a whole project: the spec, and one handler.

```text
  my-project/
  ├── sandbox.toml      what to run, and how
  └── to_lower.py       the code
```

```toml
# sandbox.toml
[fn.lower]
entry = "./to_lower.py"       # the image is python:3.12-slim, guessed from .py
```

```python
# to_lower.py
def handler(event):           # event: the JSON the caller sent, as a dict
    return {"text": event["text"].lower()}   # returned as JSON
```

```bash
zygo up                                   # warm it once
zygo exec lower '{"text": "Hello WORLD"}' # prints {"text": "hello world"}
```

Everything else in this chapter is optional: a field you leave out has a
safe default. [Chapter 13](13-warm-functions.md) explains handlers in full.

## The shape of the file

```toml
[defaults]            # applies under every [fn.*] and every [runtime.*]
mem = "256M"

[fn.resize]           # a function: one handler, warmed into one zygote
entry = "./resize.py"

[runtime.py312]       # a runtime pool: an interpreter with no code in it
agent = "python"

[api]                 # where `zygo api` listens, and how it checks callers
listen = "127.0.0.1:7700"
```

There are exactly four sections. An unknown section or an unknown field is an
**error**, not a warning, so a typo cannot silently do nothing. There is no
section for tenants: they are created through the API and stored by Zygo
([chapter 17](17-api-sdk-mcp.md#tenants)).

## How the layers merge

```text
  highest ┌───────────────────────────────────────┐
          │ a CLI flag, or the body of an API call│   --mem 512M
          ├───────────────────────────────────────┤
          │ [fn.<name>]  or  [runtime.<name>]     │   mem = "384M"
          ├───────────────────────────────────────┤
          │ [defaults]                            │   mem = "256M"
          ├───────────────────────────────────────┤
  lowest  │ Zygo's built-in defaults              │   mem = 256M
          └───────────────────────────────────────┘
          the highest layer that sets a field wins
```

A list or table — `allow`, `mounts`, `env`, `secrets`, `cmd`, `system` —
**replaces** the one below it; it is never added to it. So `allow` in
`[fn.fetch]` is the whole allowlist for that function, not an addition to one
in `[defaults]`. `zygo spec explain <name>` prints the result of the merge,
and `zygo spec validate` checks the file without running anything.

## Where the file is found, and relative paths

`-f PATH` (or `--file PATH`) names the file. Without it, Zygo looks for
`sandbox.toml` in the current folder, then in each parent folder in turn,
and uses the first one it finds. Relative paths in the file — `entry`,
`requirements`, and the host side of `mounts` — are relative to the folder
the file is in, not to where you ran the command. Without a spec file, they
are relative to the current folder.

## Names

Function and pool names (`[fn.<name>]`, `[runtime.<name>]`) are 1 to 64
characters: a letter or digit first, then letters, digits, `.`, `_` or `-`.
Names in `env` and `secrets` follow the shell's rule: a letter or `_` first,
then letters, digits or `_`. A `system` package is a Debian package name, at
least two characters of `a-z 0-9 + . -`, optionally followed by
`=version`.

## Value syntax

| Kind | Written as | Notes |
|---|---|---|
| **bytes** | `"256M"`, `"1.5G"`, `"512K"`, or a bare integer | A bare integer is bytes. Suffixes `B`, `K`/`KB`/`KiB`, `M`/`MB`/`MiB`, `G`/`GB`/`GiB`, `T`/`TB`/`TiB`, any case. **All are binary**: `1M` is 1 MiB. |
| **duration** | `"30s"`, `"250ms"`, `"5m"`, `"1h"`, `"2d"`, or a bare integer | A bare integer is seconds. Units are **case-sensitive**: `ms`, `s`, `m`, `h`, `d`. |
| **cpu** | `0.5`, `2`, or `"1.5"` | Cores. Must be above zero. |
| **enums** | `"ns"`, `"strict"`, `"function"` | `isolation`, `seccomp`, `mode` and `[api] auth` must be **lowercase** in the file. `network` and the built-in runtime names accept any case. |

## What to run

| Field | Type | Default | What it does |
|---|---|---|---|
| `image` | image reference | python → `python:3.12-slim`, node → `node:22-slim`; otherwise **required** | The OCI image that becomes the root file system: `python:3.12-slim`, `ghcr.io/org/app:1.2`, or `name@sha256:…`. |
| `entry` | path | — | The handler file. Makes this an **agent** function: loaded once, forked per request. Cannot be used with `cmd`. Refused in a pool. |
| `cmd` | list of strings | — | The program to run. Without `entry` or a runtime, makes this a **warm-exec** function: a fresh process per request, event on stdin, result on stdout. For `zygo run`, the image's own entrypoint is used when empty. In a pool, makes a warm-exec pool (the script path is added as the last argument). |
| `mode` | `function` \| `stdin` | `function` | How the agent calls your handler: `handler(event)` and its return value, or the event on stdin and the result from stdout. |
| `runtime` (alias `agent`) | `"python"`, `"node"`, `"go"`, or `{ agent = "/path" }` | guessed from `entry`: `.py` → python; `.js .mjs .cjs .ts` → node; `.go` → go | Which agent lives in the sandbox. A table names an agent of your own, by its path inside the sandbox. A built-in runtime needs an `entry`. |
| `requirements` | path | — | A `requirements.txt`, built once into a venv inside the image, mounted read-only at `/venv`, with `/venv/bin` first on `PATH`. Shared by everything with the same image and the same file. |
| `system` | list | `[]` | apt packages (`"libwebp7"`, `"libpq5=16.4-1"`), installed once into a derived layer of the image. |
| `nix` | list | `[]` | Accepted by the parser; **not built**: serving a function that sets it fails with a clear error. |
| `workdir` | path | `/app` | The working folder inside the sandbox; `/` if the image has no `/app`. |
| `user` | uid | `1000` | The uid inside the sandbox, mapped to your own uid on the host. |

## Isolation

| Field | Values | Default | What it does |
|---|---|---|---|
| `isolation` | `ns` \| `gvisor` \| `vm` | `ns` | Where the wall is ([chapter 6](06-how-zygo-works.md#three-backends-one-command)). `ns` is the only backend with warm functions and networking; `gvisor` (after `zygo backend install gvisor`) and `vm` run one-shot sandboxes and refuse the rest with a reason. |
| `seccomp` | `default` \| `strict` \| `permissive` | `default` for a function, **`strict` for a pool** | The syscall allowlist ([seccomp profiles](24-seccomp-profiles.md)). A pool with anything but `strict` gets a warning, because its zygotes are shared between tenants. |

## Limits

These are enforced for every request. None can be switched off, except
`timeout = 0`, which needs `--allow-unlimited`.

| Field | Type | Default | Enforced by | Rules |
|---|---|---|---|---|
| `mem` | bytes | `256M` | `memory.max`; `memory.high` at 90%; no swap | At least `8M`. The whole request's process tree is killed together. |
| `cpu` | cores | `1.0` | `cpu.max`, over a 100 ms period | Above zero. A request that spins is slowed, not the host. |
| `pids` | integer | `64` | `pids.max` | Not zero. The fork-bomb limit. |
| `timeout` | duration | `30s` | the supervisor, with `cgroup.kill` | Not zero unless `--allow-unlimited`. The request exits 137. |
| `scratch` | bytes | the smaller of `64M` and half of `mem` | the size of the `/tmp` tmpfs; also the largest file a process may write; 10 000 files at most | Must be smaller than `mem` (it counts against it); a warning above half. |
| `nofile` | integer | `1024` | `RLIMIT_NOFILE` | |
| `io_read`, `io_write` | bytes per second | unlimited | `io.max` on the device behind the root | A warning when neither is set, except for `zygo run`. |
| `connections` | integer | `256` | the sandbox's firewall | Not zero. TCP connections a networked function may hold at once. |
| `bandwidth` | bytes per second | unlimited | traffic shaping in the sandbox | Not zero. What the function may send (and receive, where the host has an `ifb` device). A warning when unset with `egress` or `full`, except for `zygo run`. |

## Network

| Field | Type | Default | What it does |
|---|---|---|---|
| `network` | `none` \| `egress` \| `full` \| `host` | `none` | `none`: loopback only. `egress`: only what `allow` names, plus DNS through Zygo's own resolver. `full`: the public internet (`bridge` is accepted as another spelling). `host`: no network namespace at all; needs `--allow-host-net`. |
| `allow` | list of rules | `[]` | The egress allowlist. Only valid with `network = "egress"`; `egress` with an empty list allows nothing, and warns. |

The forms an `allow` rule takes:

| Form | Example | Matches |
|---|---|---|
| `host:port` | `api.stripe.com:443` | that name, that port |
| `host` | `api.stripe.com` | that name, every port |
| `*.domain:port` | `*.example.com:443` | every subdomain of `example.com`, **not** `example.com` itself |
| `CIDR:port` | `203.0.113.0/24:5432` | that address range, that port |
| IPv6 | `[2001:db8::1]:443`, `2001:db8::/32` | brackets when a port follows |

Private and link-local ranges — `10/8`, `172.16/12`, `192.168/16`,
`127/8`, `169.254/16` (the cloud metadata address), `100.64/10`, `0/8`,
multicast and reserved (`224/4`, `240/4`), `::1`, `fe80::/10`, `fc00::/7`,
`ff00::/8` — are never reachable in a namespaced mode unless you pass
`--allow-private-net`, and a CIDR rule inside one of them is refused without
it.

## Files, environment and secrets

| Field | Type | Default | What it does |
|---|---|---|---|
| `mounts` | list of `host:guest[:ro\|rw]` | `[]` | Bind mounts, **read-only unless `:rw`**, and always `nosuid,nodev`. Both apply to every mount below the host path too. `guest` must be absolute. A host path cannot contain `:`. Two mounts cannot share a target. |
| `env` | table | `{}` | Environment variables for the sandbox. The zygote sees them, so **never put a secret here**. |
| `secrets` | list of names | `[]` | Each name is read from the environment of the shell that runs `serve`/`up`, and delivered as the file `/run/secrets/<NAME>` (mode 0400), only for the length of one request. A name cannot be in both `env` and `secrets`. |

A mount may not target `/`, `/proc`, `/sys`, `/dev`, `/dev/pts`, `/dev/shm`,
`/tmp`, `/run`, `/run/script` or `/work`. These are Zygo's own; a folder
*inside* one, such as `/tmp/cache`, is fine.

## Keeping it warm

| Field | Type | Default | What it does |
|---|---|---|---|
| `concurrency` | integer | `4` | Requests running at once in one zygote. Up to four times as many more may wait; past that, the answer is **busy**: HTTP `429`, CLI exit 75. |
| `idle_timeout` | duration | `10m` | After this long without a request, the zygote is **paused**: frozen, still in memory, woken in milliseconds by the next request. |
| `cold_after` | duration | `1h` | After this long, it is **cold**: the sandbox is dropped and the next request pays the warm-up again. A warning if shorter than `idle_timeout`. |

```text
  request ──▶ WARM ──(idle_timeout: 10m)──▶ PAUSED ──(cold_after: 1h)──▶ COLD
               ▲                              │                            │
               └────── next request: ~ms ─────┘                            │
               └────── next request: pays the warm-up again (~100s of ms) ─┘
```

## `[runtime.<name>]`: a runtime pool

A pool is an interpreter and its dependencies, warmed with **no code in it**.
Each request brings its own script, which is loaded in the forked child and
gone with it. It is how one warm zygote serves thousands of different
scripts ([chapter 13](13-warm-functions.md#runtime-pools)). Every field
above means the same thing here, except these:

| Field | Default | What it does |
|---|---|---|
| `agent` (or `runtime`) | — | Which agent the zygotes run: `"python"`, `"node"`, or `{ agent = "/path" }`. A pool needs **either** `agent` **or** `cmd`, not both. |
| `min_warm` | `1` | Zygotes kept warm whatever the load; `0` counts as `1`. |
| `max_warm` | the larger of `min_warm` and `4` | Zygotes the pool may grow to, one per second while every zygote is full. Cannot be below `min_warm`. |
| `entry` | refused | Anything warmed into a shared zygote would be forked into every tenant's request. |
| `seccomp` | `strict` | Unless a layer sets it. |

`min_warm` and `max_warm` are refused in `[fn.*]`. A pool admits
`concurrency × max_warm` requests before it answers busy. `zygo up` starts
only `[fn.*]`; a pool is started by `zygo serve --runtime <name>` or
`POST /runtimes`.

## `[api]`

| Field | Values | Default | What it does |
|---|---|---|---|
| `listen` | `IP:PORT` or `unix:///path` | `127.0.0.1:7700` | Where `zygo api` listens. An IP address, not a host name: `localhost:7700` is refused. `--listen` overrides it. |
| `auth` | `bearer` \| `none` | `bearer` | `bearer` needs `ZYGO_API_TOKEN` set, or tokens minted with `zygo token`. `none` is only accepted on a unix socket or a loopback address. `--no-auth` overrides it. |

## Flags that loosen, and `zygo up`

Three things are refused unless a flag says so: `network = "host"`
(`--allow-host-net`), a private range in `allow` (`--allow-private-net`), and
`timeout = 0` (`--allow-unlimited`). The flags exist on `zygo run`,
`zygo serve` and `zygo mcp`. **`zygo up` has none of them, on purpose**: a
spec that needs one has to be served deliberately, one function at a time,
with the flag typed by a person.

## Which fields have a flag

| On `run` and `serve` | Only on `run` | Only on `serve` | **No flag: file or API only** |
|---|---|---|---|
| `--mem --cpu --pids --timeout --scratch --nofile --isolation --seccomp --net --allow --mount --env --user --workdir` | image (positional), command → `cmd`, `--requirements` | handler → `entry`, `--image`, `--requirements`, `--concurrency`, `--idle-timeout`, `--mode`, `--agent`, `--secret`, `--min-warm`, `--max-warm` | `system`, `nix`, `io_read`, `io_write`, `connections`, `bandwidth`, `cold_after`, `cmd` for a served function |

## `zygo.lock`

`zygo up` writes `zygo.lock` beside the spec, and it is meant to be
committed. It records what the spec's names pointed to on the machine that
ran `up`: the image digest each function resolved to, the versions apt chose
for its `system` packages, and the hash of its requirements file. It never
changes what runs; it only refuses to let it change **silently**. It is
saved only when something in it changed.

```toml
version = 1

[fn.api]
image  = "python:3.12-slim"
digest = "sha256:1f2e…"          # the multi-platform index: the same image everywhere
system = ["libpq5=16.4-1"]       # what apt really installed

[fn.api.requirements]
path   = "requirements.txt"
sha256 = "9ab3…"
```

`digest` is the multi-platform index's digest when the registry has one, so
it means the same image on every CPU type. For an image built for one
platform only, it is that manifest's digest, and a `platform` field says
which.

| Situation | What `zygo up` does |
|---|---|
| No lock file | Writes one. |
| The spec changed: another image, another package list, an edited requirements file | Rewrites that entry, silently — you asked for the change. |
| The spec is the same, but the image behind the tag moved | **Refuses**, printing both digests. `zygo up --relock` accepts it. |
| The same packages resolved to other versions | Records them, with a warning. apt does not keep old versions, so refusing would break every new host. |
| A function is gone from the spec | Drops its entry. |
| The file has a wrong version or cannot be read | An error that tells you to delete it. |

Not built yet: pinning pip's full dependency tree (only the requirements
file's hash is kept), pulling the locked digest, and a `--frozen` mode for CI.

```text
  sandbox.toml ──▶ zygo up ──▶ resolves tags and packages ──▶ zygo.lock (commit it)
                                        │
             next `zygo up` ────────────┴──▶ same spec, different image? ──▶ refuse
                                                                 └─ --relock ─▶ accept
```

## A complete example

```toml
[defaults]
image    = "python:3.12-slim"
mem      = "256M"
cpu      = 0.5
timeout  = "30s"

[fn.resize]                         # agent function, with dependencies
entry        = "./resize.py"
requirements = "./requirements.txt"
system       = ["libwebp7"]
mem          = "512M"
mounts       = ["./cache:/cache:rw"]
concurrency  = 8

[fn.parse]                          # warm-exec: a static binary, any language
image   = "alpine:3"
mounts  = ["./bin/parse:/app/parse:ro"]
cmd     = ["/app/parse"]
mem     = "64M"

[fn.fetch]                          # networked, with a secret
entry       = "./fetch.py"
network     = "egress"
allow       = ["api.stripe.com:443", "*.example.com:443"]
connections = 32
bandwidth   = "2M"
secrets     = ["STRIPE_KEY"]
timeout     = "10s"

[runtime.py312]                     # a pool for scripts that arrive per request
agent        = "python"
requirements = "./pool-requirements.txt"
min_warm     = 2
max_warm     = 8

[api]
listen = "unix:///run/user/1000/zygo-api.sock"
auth   = "bearer"
```

## The files beside it

The example above names some files. Here they are, so you can see what a
real project looks like on disk.

```text
  my-project/
  ├── sandbox.toml
  ├── resize.py                 [fn.resize]  makes a small picture
  ├── requirements.txt          [fn.resize]  its Python packages
  ├── cache/                    [fn.resize]  a folder it may write to
  ├── fetch.py                  [fn.fetch]   calls an API with a secret
  ├── bin/parse                 [fn.parse]   a program you compiled (Go, C, …)
  └── pool-requirements.txt     [runtime.py312]  packages for the pool
```

**resize.py** gets a picture as base64 text, makes it at most 200 pixels
wide, and sends it back as WebP. `PIL` comes from `requirements.txt`; the
`libwebp7` system package lets it write WebP.

```python
# resize.py
import base64, io
from PIL import Image                     # installed from requirements.txt

def handler(event):
    picture = Image.open(io.BytesIO(base64.b64decode(event["image"])))
    picture.thumbnail((200, 200))         # at most 200 × 200, same shape
    out = io.BytesIO()
    picture.save(out, format="WEBP")
    return {"image": base64.b64encode(out.getvalue()).decode()}
```

```text
# requirements.txt
Pillow
```

**fetch.py** reads its secret from a file — never from the environment — and
calls the one host its `allow` list opens.

```python
# fetch.py
import json, urllib.request

def handler(event):
    key = open("/run/secrets/STRIPE_KEY").read().strip()   # there for this request only
    request = urllib.request.Request(
        "https://api.stripe.com/v1/balance",
        headers={"Authorization": f"Bearer {key}"},
    )
    with urllib.request.urlopen(request, timeout=5) as answer:
        return json.load(answer)
```

**bin/parse** is any program that reads one JSON event on standard input and
writes one JSON answer on standard output ([chapter 6](06-how-zygo-works.md#what-you-do-instead-for-go-and-c)
shows how to build one in Go or C). **The pool** holds no file of yours: each
request brings its script with it.

```python
# word_count.py — sent with the request, not named in sandbox.toml
def handler(event):
    return {"words": len(event["text"].split())}
```

```bash
export STRIPE_KEY=sk_test_…      # secrets come from your shell
zygo up                           # warms resize, parse and fetch
zygo exec fetch '{}'
zygo serve --runtime py312        # pools are started by name
zygo exec --runtime py312 --script word_count.py '{"text": "one two three"}'
```

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [19. Every command](19-commands.md) · [Contents](README.md) · **Next: [21. Environment, files and exit codes](21-environment-files-exit-codes.md) →**
