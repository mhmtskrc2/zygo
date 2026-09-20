# `sandbox.toml` reference

Three sections: `[defaults]`, one `[fn.<name>]` per function, and an optional
`[api]`. Every function field may also appear in `[defaults]`, and every one
has a CLI flag on `zygo run` and `zygo serve`. Precedence, highest first:
**CLI flag → `[fn.<name>]` → `[defaults]` → built-in default**.
`zygo spec explain <name>` prints the result of that merge, resolved; `zygo
spec validate` checks the file without running anything. An unknown field is
an error, not a warning.

## What to run

| Field | Type | Default | Meaning |
|---|---|---|---|
| `image` | reference | — (required) | An OCI image: `python:3.12-slim`, `ghcr.io/org/app:1.2`, or a digest. Pulled on first use by `run`; `serve`/`up` need it pulled already. |
| `entry` | path | — | The handler file. Sets *agent* mode: the runtime loads it once and forks per request. Relative to the spec file. |
| `cmd` | list | image's `CMD` | The program to run. With no `runtime`, sets *warm-exec* mode: a fresh process per request, event on stdin, JSON on stdout. |
| `mode` | `function` \| `stdin` | `function` | How an agent calls the handler: `handler(event)` returning the result, or the event on stdin and the result from stdout. |
| `runtime` | `python` | inferred from `entry` | Which agent. Only Python ships built in; anything else runs as warm-exec. |
| `requirements` | path | — | A `requirements.txt`, installed once into a venv *inside the image* and mounted read-only at `/venv`. Keyed on the image digest and the file's bytes. |
| `system` | list | `[]` | apt packages, installed once into a derived layer of the image. Debian package names, optionally `=version`. |
| `nix` | list | `[]` | Declared, not implemented. |
| `workdir` | path | `/app` | Working directory inside the sandbox. Falls back to `/` if the image lacks it. |
| `user` | uid | `1000` | Uid inside the sandbox, mapped to your own on the host (or into your subordinate range where `newuidmap` exists). |

## Isolation

| Field | Type | Default | Meaning |
|---|---|---|---|
| `isolation` | `ns` \| `vm` \| `gvisor` | `ns` | Where the boundary is. `ns` is built; `gvisor` runs one-shot sandboxes after `zygo backend install gvisor`, and refuses warm and networked ones; `vm` needs KVM and is not built. |
| `seccomp` | `default` \| `strict` \| `permissive` | `default` | The syscall allowlist. See [seccomp profiles](seccomp-profiles.md). |

## Limits

All mandatory; none can be disabled without `--allow-unlimited`, and even
then only `timeout`.

| Field | Type | Default | Maps to |
|---|---|---|---|
| `mem` | bytes (`256M`, `2G`) | `256M` | `memory.max`; `memory.high` at 90%; swap off; the whole request tree is killed together. Minimum 8M. |
| `cpu` | cores (`0.5`, `2`) | `1.0` | `cpu.max`. A tenant that spins is throttled, not the host. |
| `pids` | integer | `64` | `pids.max` and `RLIMIT_NPROC`: the fork-bomb limit. |
| `timeout` | duration (`30s`, `5m`) | `30s` | Wall clock per request, enforced by the supervisor with `cgroup.kill`. Exit 137. `0` needs `--allow-unlimited`. |
| `scratch` | bytes | `64M` | `/tmp`, a tmpfs. Counts against `mem`, so it must be smaller. |
| `io_read`, `io_write` | bytes/s | unlimited (warned) | `io.max` on the device behind the root. |
| `nofile` | integer | `1024` | `RLIMIT_NOFILE`. |
| `connections` | integer | `256` | Concurrent TCP connections a networked function may hold; the next is refused with a reset. |
| `bandwidth` | bytes/s | unlimited (warned) | What the function may *send*; what it receives too where the host has an `ifb` device. |

## Network

| Field | Type | Default | Meaning |
|---|---|---|---|
| `network` | `none` \| `egress` \| `full` \| `host` | `none` | `none`: loopback only. `egress`: the `allow` list, enforced by nftables inside the namespace, with DNS through a resolver of Zygo's own. `full`: the public internet. `host`: no namespace; needs `--allow-host-net`. |
| `allow` | list | `[]` | `host:port`, `*.domain:port`, `CIDR:port`; a host with no port means every port. Only with `egress`. Private and link-local ranges are refused unless `--allow-private-net`, even when listed. |

## Files, environment, secrets

| Field | Type | Default | Meaning |
|---|---|---|---|
| `mounts` | list of `host:guest[:ro\|rw]` | `[]` | Bind mounts, read-only unless `:rw`. Cannot target `/proc`, `/sys`, `/dev`, `/run`. |
| `env` | table | `{}` | Environment for the sandbox. Visible to the zygote — do not put secrets here. |
| `secrets` | list | `[]` | Names of environment variables in the shell that runs `serve`/`up`. Each is delivered as `/run/secrets/<NAME>` (mode 0400) for exactly the duration of a request, and the zygote never sees the value. A name in both `env` and `secrets` is an error. |

## Warm pool

| Field | Type | Default | Meaning |
|---|---|---|---|
| `concurrency` | integer | `4` | Requests in flight at once; four times that may queue, and the rest get `BUSY` (`429` over HTTP, exit 75 from the CLI). |
| `idle_timeout` | duration | `10m` | After this long without a request the function is *paused*: frozen, still resident, thawed on the next request in single-digit milliseconds. |
| `cold_after` | duration | `1h` | After this long it is *cold*: the sandbox is dropped and only the spec kept. The next request pays the warm-up. Must not be shorter than `idle_timeout`. |

## `[api]`

| Field | Type | Default | Meaning |
|---|---|---|---|
| `listen` | address | `127.0.0.1:7700` | A TCP address or `unix:///path`. |
| `auth` | `bearer` \| `none` | `bearer` | `bearer` reads the token from `ZYGO_API_TOKEN` — never a flag, because flags show in `ps`. `none` is refused unless the address is loopback or a unix socket. |

## `zygo.lock`

Written beside the spec by `zygo up`, and meant to be committed. It records
what the spec's pointers resolved to on the machine that ran `up`:

```toml
version = 1

[fn.api]
image = "python:3.12-slim"
digest = "sha256:1f2e…"          # the index digest: the same image everywhere
system = ["libpq5=16.4-1"]       # the versions apt actually installed

[fn.api.requirements]
path = "requirements.txt"
sha256 = "9ab3…"
```

`digest` is the multi-platform index's when the registry served one, so the
line means the same image on every architecture. For an image that exists for
one platform only it is that manifest's digest, and a `platform` field says
whose.

What `up` does with it:

| Situation | What happens |
|---|---|
| No lock file | Written. |
| The spec changed — another reference, another package list, an edited requirements file | That entry is rewritten, silently. You asked for the change. |
| The spec is the same and the image is not the one recorded | **Refused**, with both digests. `zygo up --relock` accepts it. |
| The same packages resolved to different versions | Recorded, with a warning. `apt` does not keep old versions, so refusing would strand every fresh host. |
| A function is no longer in the spec | Its entry is dropped. |

It is a record, not a source of truth: nothing here ever changes what runs, it
only refuses to let it change silently. Pinning pip's transitive resolution
(only the requirements file's hash is recorded), pulling the locked digest and
a `--frozen` for CI are not built.

## An example that uses most of it

```toml
[defaults]
image     = "python:3.12-slim"
isolation = "ns"
seccomp   = "default"
mem       = "256M"
cpu       = 0.5
pids      = 64
timeout   = "30s"
network   = "none"

[fn.resize]
entry        = "./resize.py"
requirements = "./requirements.txt"
system       = ["libwebp7"]
mem          = "512M"
mounts       = ["./cache:/cache:rw"]
concurrency  = 8

[fn.parse]
image  = "alpine:3"
mounts = ["./bin/parse:/app/parse:ro"]
cmd    = ["/app/parse"]
mem    = "64M"
scratch = "16M"

[fn.fetch]
entry       = "./fetch.py"
network     = "egress"
allow       = ["api.stripe.com:443", "*.example.com:443", "203.0.113.0/24:5432"]
connections = 32
bandwidth   = "2M"
secrets     = ["STRIPE_KEY"]
timeout     = "10s"

[api]
listen = "127.0.0.1:7700"
auth   = "bearer"
```
