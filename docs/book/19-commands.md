# 19. Every command

This is the complete list of `zygo` commands, every flag, and every default.
It is written from the parser (`crates/zygo-cli/src/cli.rs`) and from the
code behind each command. `zygo <command> --help` prints the same flags;
this chapter adds what the help cannot: what each command really does, what
it needs, and how it ends.

## The commands on one page

```text
  ONE-SHOT                 WARM FUNCTIONS                 A PROJECT (sandbox.toml)
  zygo run                 zygo serve    zygo exec        zygo up      zygo down
                           zygo ps       zygo stop        zygo spec validate | explain
                           zygo logs     zygo shell
                           zygo top      zygo stats

  IMAGES                   ACCESS AND SECRETS             PROGRAMS AND AGENTS
  zygo pull                zygo login                     zygo api
  zygo images              zygo token  mint | ls | revoke zygo mcp
  zygo image rm | prune    zygo secrets keygen | set |    zygo agent test
                                        ls | rm

  THE HOST                 MEASURING                      SHELL HELPERS
  zygo doctor [--fix]      zygo bench warm | cold |       zygo completion
  zygo backend list |                 load | all
               install     (hidden) zygo supervisor run | status | stop
```

## How to read this chapter

Each command has a line showing how to call it, a few sentences on what it
does, a table of its flags, and notes on output and exit codes. `[x]` means
optional, `…` means it can repeat, `A | B` means one of them. Sizes are
written like `256M`, durations like `30s`; [chapter 20](20-sandbox-toml.md#value-syntax)
has the exact rules, which are the same for flags and for the file. Exit
codes are collected in [chapter 21](21-environment-files-exit-codes.md#exit-codes).

## Global flags

These work with every command, before or after the command name.

| Flag | Default | Meaning |
|---|---|---|
| `--json` | off | Print machine-readable JSON instead of tables and lines. Logs become JSON too. |
| `-v`, `--verbose` | warnings only | More log output; repeat for more (`-v` info, `-vv` debug, `-vvv` trace). `ZYGO_LOG` overrides it. |
| `--data-root DIR` | `$XDG_DATA_HOME/zygo` | Use another data folder: images, caches, tokens, secrets. Also read from `ZYGO_DATA_HOME`. |
| `-h`, `--help` · `-V`, `--version` | | Help for any command · the version. |

## Flags shared by `run`, `serve` and `mcp`

Two groups of flags describe a sandbox, and they mean the same thing
wherever they appear. Each maps to a `sandbox.toml` field and wins over it.

| Limit flag | Default | Meaning |
|---|---|---|
| `--mem SIZE` | `256M` | Memory limit (at least `8M`). |
| `--cpu CORES` | `1.0` | CPU quota in cores, e.g. `0.5`. |
| `--pids N` | `64` | Most processes and threads at once; the fork-bomb limit. |
| `--timeout DURATION` | `30s` | Wall-clock limit. `0` needs `--allow-unlimited`. |
| `--scratch SIZE` | smaller of `64M` and half of `--mem` | Size of the writable `/tmp`. |
| `--nofile N` | `1024` | Most open files. |

| Sandbox flag | Default | Meaning |
|---|---|---|
| `--isolation ns \| gvisor \| vm` | `ns` | Where the wall is. |
| `--seccomp default \| strict \| permissive` | `default` | The syscall profile ([chapter 24](24-seccomp-profiles.md)). |
| `--net none \| egress \| full \| host` | `none` | Network mode; `bridge` is accepted for `full`. |
| `--allow RULE` … | none | Egress allowlist entry: `host:port`, `*.domain:port`, `CIDR:port`. |
| `--mount HOST:GUEST[:ro\|rw]` … | none | Bind mount, **read-only unless `:rw`**. |
| `--env KEY=VALUE` … | none | Environment variable. |
| `--user UID` | `1000` | uid inside the sandbox. |
| `--workdir PATH` | `/app` | Working folder inside the sandbox. |
| `--allow-host-net` | off | Permit `--net host`, which removes the network boundary. |
| `--allow-private-net` | off | Permit private and link-local ranges. |
| `--allow-unlimited` | off | Permit `--timeout 0`. |

`-f`, `--file PATH` names the spec file on every command that reads one;
without it, `sandbox.toml` is searched for upwards from the current folder.

---

## `zygo run` — a one-shot sandbox

```text
zygo run [FLAGS] IMAGE [COMMAND [ARGS…]]
```

Builds a fresh sandbox from `IMAGE`, runs `COMMAND` in it (or the image's own
entrypoint), and removes everything when it exits. Standard input, output and
the exit code pass straight through, so it behaves like the program itself.
Everything after `IMAGE` belongs to the command: `zygo run alpine sh -c "echo
--mem"` does not set a memory limit. [Chapter 12](12-one-shot-sandboxes.md)
teaches it step by step.

| Flag | Default | Meaning |
|---|---|---|
| limit and sandbox flags | see above | |
| `-f`, `--file PATH` | searched | Use this spec's `[defaults]` under the flags; `[fn.*]` tables are ignored. |
| `-t`, `--tty` | off | Give the sandbox a terminal of its own. Without it, the sandbox shares yours — colours work, but the program can write to your terminal. |
| `--requirements FILE` | none | A `requirements.txt` built once into a shared venv at `/venv`, `/venv/bin` first on `PATH`. |
| `--pull missing \| never \| always` | `missing` | When to download the image. `never` refuses a missing image before anything starts (exit 1, outcome `phase: plan`). |
| `--outcome FILE` | none | Write why the sandbox ended, as JSON ([chapter 21](21-environment-files-exit-codes.md#the-outcome-file)). |
| `--dry-run` | off | Print the plan — command, root, mounts, network, seccomp, Landlock, cgroup values — and run nothing. With `--json`, the full plan including the allowed syscalls. |
| `-q`, `--quiet` | off | Hide Zygo's own progress lines on stderr. The program's output is never hidden. |

**Exit:** the program's own code; `137` if the deadline or the memory limit
killed it; `2` for a spec error; `125` if this host cannot run sandboxes; `1`
for other Zygo errors. **Signals:** the first Ctrl-C is passed to the program;
a second one kills it.

## `zygo serve` — warm a function or a pool

```text
zygo serve HANDLER --name NAME [FLAGS]
zygo serve --runtime NAME [--image IMAGE] [--agent AGENT] [FLAGS]
```

The first form builds a sandbox, loads `HANDLER` in it, and keeps it warm
under `NAME`. The second form warms a **runtime pool**: an interpreter with no
code, for scripts that arrive with each request. It starts the supervisor if
none is running. [Chapter 13](13-warm-functions.md) explains both.

| Flag | Default | Meaning |
|---|---|---|
| `HANDLER` | — | The handler file (`.py`, `.js`, `.ts`, …). Not with `--runtime`. |
| `--name NAME` | — | The function's name. Needs a `HANDLER`. |
| `--runtime NAME` | — | Serve a pool under this name instead. |
| `--image IMAGE` | the spec's, or `python:3.12-slim` / `node:22-slim` for the runtime | The image to warm from. |
| `--agent python \| node \| PATH` | — | With `--runtime`: which agent the pool runs. |
| `--min-warm N` · `--max-warm N` | `1` · larger of `min-warm` and `4` | With `--runtime`: zygotes kept warm, and the most it may grow to. |
| `--requirements FILE` | none | Shared, cached venv, as for `run`. |
| `--concurrency N` | `4` | Requests at once per zygote. |
| `--idle-timeout DURATION` | `10m` | Pause the zygote after this long idle. |
| `--mode function \| stdin` | `function` | How the handler is called. |
| `--secret NAME` … | none | Deliver `$NAME` from this shell as `/run/secrets/NAME`, per request. |
| `-f`, `--file PATH` | searched | Spec to read. |
| limit and sandbox flags | see above | |

**Output:** the function's runtime, resident memory, import time and warm-up
time. **Exit:** `0` when warm; `2` for a spec error; `125` if it cannot start.

## `zygo exec` — call a warm function

```text
zygo exec NAME [EVENT]
zygo exec --runtime NAME --script FILE|sha256:… [--entry-point FN] [EVENT]
```

Sends `EVENT` (JSON; read from standard input when left out; empty means
`null`) to a warm function and waits. The result goes to **stdout**; what the
handler printed goes to **stderr**. With `--runtime`, runs a script in a pool
instead; the positional argument is then the event.

| Flag | Default | Meaning |
|---|---|---|
| `--runtime NAME` | — | Run a script in this pool. Needs `--script`. |
| `--script FILE \| sha256:…` | — | A file on this host, or the digest of a script already stored (`PUT /scripts`). |
| `--entry-point FN` | `handler` | The function in the script to call. |
| `--batch` | off | Read one JSON event per line from stdin, run them in parallel, print one answer per line in the same order. |
| `--timeout DURATION` | the function's `timeout` | Give up after this long. |

**Exit:** the request's own code; `137` if its deadline killed it; `75` if
the function is busy; `4` if there is no such function; `125` if no
supervisor is running. With `--batch`: `0` only if every line succeeded.

## `zygo ps` — list warm sandboxes

```text
zygo ps
```

Columns: `NAME`, `STATE` (`starting`, `warm`, `paused`, `cold`, `failed`),
`RUNTIME`, `RSS` (resident memory), `REQUESTS`, `FAILURES`. It never starts a
supervisor: with none running it says so and exits `0`.

## `zygo logs` — a function's recent log

```text
zygo logs NAME [-f] [-n N] [--failed]
```

The zygote's own output, and one entry per request with its exit, duration,
stdout and stderr. The supervisor keeps the last 500 entries per function,
across replacements and cold spells.

| Flag | Default | Meaning |
|---|---|---|
| `-f`, `--follow` | off | Keep printing new entries (checks every 500 ms). |
| `-n`, `--tail N` | `50` | How many recent entries to start with. |
| `--failed` | off | Only requests that failed. |

With `--json`, one entry per line.

## `zygo stop` — stop sandboxes

```text
zygo stop NAME
zygo stop --all
```

Stops one function, or everything and then the supervisor itself. Give a
name or `--all`, not both. `--all` with nothing running exits `0`; a named
stop with no supervisor is an error (`125`). On a Mac, `stop --all` also
stops the Linux VM afterwards.

**Pools:** `NAME` must be a function; a runtime pool's name gives "no function
named …" (exit `4`). To end a pool, use `--all` — which ends it with the
supervisor, even though it prints "nothing to stop" when only pools are
running — or `DELETE /runtimes/{name}` through the API.

## `zygo top` — live resource table

```text
zygo top [-i SECONDS] [--once]
```

`ps` on a timer, plus rates. Functions: `NAME`, `STATE`, `RSS`, `REQ/S`,
`REQUESTS`, `FAILURES`. Pools: `RUNTIME`, `WARM`, `PAUSED`, `ROOM` (how many
more zygotes it may start), `IN/QUEUE`, `RSS`, `REQUESTS`, `FAILURES`. Rates
need two samples, so the first frame shows `—`.

| Flag | Default | Meaning |
|---|---|---|
| `-i`, `--interval SECONDS` | `2.0` | Time between frames. |
| `--once` | off | One frame, then exit — for scripts. `--json` implies it. |

## `zygo stats` — latency and outcome summary

```text
zygo stats [NAME]
```

For each function and pool: `STATE`, `REQUESTS`, `FAILURES`, `SAMPLES`,
`p50` (a usual request), `p99` (the slowest 1 in 100), `MAX`, `KILLED`. Counters run from when the function was
warmed; latencies come from the log ring. `p99` is shown only with 100
samples or more. `KILLED` counts timeouts and other `137` exits (usually
out of memory).

## `zygo up` — start a whole project

```text
zygo up [-f PATH] [--relock]
```

Serves every `[fn.*]` in the spec, in the order they are written. A function
whose spec, secret values, handler and requirements are all unchanged is
left warm; a changed one is replaced **blue/green**: the new one warms, then
takes new requests, while running ones finish on the old. One failure does
not stop the others. It writes `zygo.lock` ([chapter 20](20-sandbox-toml.md#zygolock)).
`[runtime.*]` pools are not started by `up`, and the `--allow-*` flags do not
exist here, on purpose.

| Flag | Default | Meaning |
|---|---|---|
| `-f`, `--file PATH` | searched | Spec to read. |
| `--relock` | off | Accept an image that moved under an unchanged tag, and rewrite `zygo.lock`. |

**Output:** one line per function — `✓` started or replaced, `·` unchanged,
`✗` failed with the reason. **Exit:** `1` if any function failed.

```text
  zygo up, for one changed function
  old ──────── serving ──────────────── finishing running requests ──▶ stopped
  new            └─ warming ─▶ ready ─▶ takes every new request ──────────────▶
```

## `zygo down` — stop a project

```text
zygo down [-f PATH]
```

Stops the functions the spec declares, and nothing else. With no supervisor
running, it says so and exits `0`.

## `zygo spec` — check and explain the spec

```text
zygo spec [-f PATH] validate
zygo spec [-f PATH] explain [NAME]
```

`validate` resolves every function, so limits, names and network rules are
really checked, and prints the warnings once each. `explain NAME` prints the
final settings for one function after every layer is merged — and says, for
each value, which layer it came from. `explain` with no name shows what `zygo
run` would use. Both need a `sandbox.toml`.

## `zygo pull` — download an image

```text
zygo pull IMAGE [--platform OS/ARCH[/VARIANT]]
```

Downloads an image into the local store, with progress lines. On Linux it
then builds the Python bytecode layer once, if the image needs one
([chapter 15](15-images-and-dependencies.md)).

| Flag | Default | Meaning |
|---|---|---|
| `--platform` | this host's | Pull for another platform, e.g. `linux/amd64`. |

## `zygo images` — list local images

```text
zygo images
```

Columns: reference, digest (first 12 characters), layers, size, and when it
was pulled.

## `zygo image rm` — remove images

```text
zygo image rm IMAGE…        (alias: zygo image remove)
```

Removes an image, the derived `+system` images built on it, and any layers
only they used. **Refused** while a warm function runs on it: stop the
function first.

## `zygo image prune` — free disk space

```text
zygo image prune [--dry-run] [--unused-for DURATION] [--blobs]
```

With no flags, deletes only what nothing can reach any more: layers of
removed images, and venvs, flattened roots and derived-layer records whose
image is gone, plus leftover temporary roots. It prints what a flag would
also collect.

| Flag | Default | Meaning |
|---|---|---|
| `--dry-run` | off | Say what would go, and how much it would free; delete nothing. |
| `--unused-for DURATION` | off | Also delete venvs and flattened roots not used for this long, e.g. `30d`. |
| `--blobs` | off | Also delete the compressed copy of every unpacked layer. Roughly halves the store; a lost layer then means a new download. |

## `zygo login` — a private registry

```text
zygo login REGISTRY [-u USER] [--password-stdin]
```

Asks for the password with echo off, checks it against the registry, and only
then stores it in Zygo's own `auth.json`. There is **no `--password` flag**:
an argument is visible to every user in `ps` and lands in shell history.
Zygo reads `~/.docker/config.json` too, but never writes to it; where both
have a credential, Zygo's wins. `docker.io`, `index.docker.io` and
`registry-1.docker.io` are the same registry.

| Flag | Default | Meaning |
|---|---|---|
| `-u`, `--username USER` | asked | The account name. |
| `--password-stdin` | off | Read the password from standard input, for CI. |

## `zygo secrets` — the per-tenant secret store

```text
zygo secrets keygen
zygo secrets set TENANT NAME [--stdin]
zygo secrets ls TENANT          (alias: list)
zygo secrets rm TENANT NAME
```

An encrypted store of secrets per tenant, used by functions served through
the API. `keygen` prints a new 32-byte key; put it in `ZYGO_SECRETS_KEY` (or a
file named by `ZYGO_SECRETS_KEY_FILE`) before the supervisor starts. A
passphrase is refused, and a lost key means every stored secret is lost.
`set` reads the value with echo off, or from stdin with `--stdin` (at most
64 KiB); there is no `--value` flag. `ls` prints names, never values; there
is no way to read a value back. [Chapter 14](14-limits-network-secrets.md)
explains how the store and `--secret` fit together.

## `zygo token` — API tokens

```text
zygo token mint [--tenant TENANT]
zygo token ls                   (alias: list)
zygo token revoke ID
```

`mint` creates a token and prints its secret — `zygo_` and 64 hex characters
— on stdout, **once**; only a hash is stored. Without `--tenant` it is an
**operator** token (the host's); with it, a **tenant** token (one customer's,
and the tenant is created if new). Because the secret goes alone to stdout,
`ZYGO_API_TOKEN=$(zygo token mint)` works. `ls` lists id, scope and state;
`revoke` works from the next request, and keeps the record so old logs still
make sense.

## `zygo api` — the HTTP API

```text
zygo api [-f PATH] [--listen ADDR] [--no-auth] [--allow-deploy] [--openapi]
         [--otlp-endpoint URL] [--otlp-interval D] [--usage-webhook URL]
         [--usage-interval D]
```

Runs the HTTP API in the foreground, starting the supervisor if needed. By
default every caller needs a bearer token — `ZYGO_API_TOKEN`, or one from
`zygo token mint`. [Chapter 17](17-api-sdk-mcp.md) has every route.

| Flag | Default | Meaning |
|---|---|---|
| `--listen ADDR` | `[api] listen`, else `127.0.0.1:7700` | `IP:PORT`, or `unix:///path`. |
| `--no-auth` | off | No tokens. Refused except on a unix socket or a loopback address. |
| `--allow-deploy` | off | Let the bootstrap token create and destroy sandboxes, not only call them. That makes it a shell as your user; think twice. |
| `--openapi` | off | Print the OpenAPI 3.1 document and exit. |
| `--otlp-endpoint URL` | `$OTEL_EXPORTER_OTLP_ENDPOINT` | Push metrics to an OpenTelemetry collector (`/v1/metrics` is added). |
| `--otlp-interval DURATION` | `60s` | How often to push. |
| `--usage-webhook URL` | none | POST batches of usage events for billing, at least once. |
| `--usage-interval DURATION` | `10s` | How often to deliver them. |

## `zygo mcp` — tools for an AI agent host

```text
zygo mcp [-f PATH] [--workspace DIR] [--python-image I] [--node-image I]
         [--sh-image I] [limit and sandbox flags]
```

Speaks the Model Context Protocol on stdin and stdout, which is how an agent
host starts a tool server. The flags are a **ceiling** the model cannot
raise: no tool accepts an image, mount, network or limit.

| Flag | Default | Meaning |
|---|---|---|
| `--workspace DIR` | a temporary folder, removed on exit | Mounted read-write at `/work` for every `run_code`. |
| `--python-image` | `python:3.12-slim` | Image for `language: "python"`. |
| `--node-image` | `node:22-slim` | Image for `language: "node"`. |
| `--sh-image` | `alpine:3` | Image for `language: "sh"`. |
| limit and sandbox flags | see above | Applied to every `run_code`. |

## `zygo shell` — a debug shell inside a warm sandbox

```text
zygo shell NAME [-- COMMAND…]
```

Starts a fresh process inside the function's namespaces: it sees the
sandbox's files, processes, network and host name. The warm zygote is not
touched and keeps serving. The shell holds no capabilities, but it is on
purpose **not** under seccomp, Landlock or the function's cgroup, so the
memory limit cannot kill your debugging session. It runs `bash`, `sh` or
busybox `sh`, or `COMMAND` if given.

## `zygo doctor` — can this host run sandboxes?

```text
zygo doctor [--fix [--yes]]
```

Tries each requirement for real rather than reading a setting: kernel
version, user namespaces, `/proc`, cgroup v2 delegation, whether moving a
process into a cgroup can stall (`cgroup moves`), overlayfs, Landlock,
seccomp, subordinate uids, KVM, the guest kernel, `runsc`, and the network
helpers. Each line says `ok`, `degraded`, `-` (absent) or `FAIL`, with the
fix under it. On a Mac it checks the host side and then the VM's own doctor.
[Chapter 11](11-getting-started.md) walks through it.

| Flag | Default | Meaning |
|---|---|---|
| `--fix` | off | Apply the fixes that are one command each — the AppArmor user-namespace rule, cgroup delegation, the `passt` and `nftables` packages, the AppArmor profile on `pasta`, and cgroup2's `favordynmods` option (on the host, never in a container) — after printing every command and what it costs, and asking. |
| `--yes` | off | With `--fix`: do not ask. |

**Exit:** `0` if no check failed.

## `zygo backend` — optional isolation backends

```text
zygo backend list
zygo backend install gvisor | vm
```

`list` shows `ns`, `gvisor` and `vm` and whether this host can use each.
`install gvisor` downloads gVisor's `runsc` from Google's release bucket,
checks its SHA-512 before unpacking, and installs it under the data folder.
`install vm` downloads nothing: it needs a build with the `vm` feature and a
guest kernel in place, and says how to get one.

## `zygo agent test` — check an agent against the protocol

```text
zygo agent test BINARY [--script F] [--script-spawn F] [--pool-script F] [-- ARGS…]
```

Runs your agent and has the conversation the supervisor would have: `READY`,
`PING`, a fork per request, the wait for `GO`, output kept apart, two
requests at once, a bad message, cancel, streaming, heartbeats, shutdown.
[Chapter 18](18-writing-an-agent.md) describes each check. Exit `1` if any
failed.

| Flag | Meaning |
|---|---|
| `--script FILE` | A script in the agent's language, to check that a script sent with a request is loaded in the child. |
| `--script-spawn FILE` | A script whose top level starts a program, to check the child's filter is on before the script's first line. |
| `--pool-script FILE` | The agent holds no handler: send this file with every request (the runtime-pool shape). |
| `-- ARGS…` | Arguments for the agent. |

## `zygo bench` — measure this host

```text
zygo bench warm  [--n 10000] [--rate R] [--cpu C] [--no-cgroup] [--pool [--scripts 1000]] [-- CMD…]
zygo bench cold  [--n 50] [--image python:3.12-slim] [--command "…"]
zygo bench load  [--seconds 10] [--concurrency 4] [--cpu C]
zygo bench all   [--quick]
```

`warm` measures the warm path (or warm-exec with `-- CMD`, or a pool with
`--pool`); `cold` measures `zygo run` with a pulled image; `load` measures
throughput with several clients; `all` runs every published measurement and
compares it with the numbers in [chapter 25](25-performance.md). Each prints
PASS or FAIL against its budget. `all` exits `2` — "no verdict" — when the
machine was throttled or busy while it ran, because such a number is not
about Zygo.

## `zygo completion` — shell completion

```text
zygo completion bash | zsh | fish | elvish | powershell
```

Prints a completion script, generated from the parser itself so it can never
drift from the real flags.

```bash
zygo completion zsh > "${fpath[1]}/_zygo"
```

## `zygo supervisor` — the warm pool's own process (hidden)

```text
zygo supervisor run | status | stop
```

Not shown in `--help`, because `serve`, `up`, `api`, `token` and `secrets`
start a supervisor when they need one. `run` keeps it in the foreground,
which is how you see why it will not start. `status` prints its version, pid
and socket (exit `1` if none). `stop` stops the supervisor only; it goes
through the socket, or through the pid file and `SIGTERM` when a version
mismatch means the socket will not listen.

## Which commands start or need a supervisor

| Starts one if needed | Needs one running (else exit `125`) | Fine without one |
|---|---|---|
| `serve`, `up`, `api`, `token …`, `secrets set/ls/rm` | `exec`, `logs`, `top`, `stats`, `shell`, `stop NAME` | `run`, `ps`, `down`, `stop --all`, `pull`, `images`, `image …`, `login`, `doctor`, `spec …`, `backend …`, `completion`, `secrets keygen` |

## On a Mac

The `zygo` on a Mac is a small forwarder. `completion`, `doctor`, `agent
test` and `api --openapi` run on the Mac itself; everything else runs inside
the Linux VM Zygo manages, with the same arguments, folder and streams. Every
path you pass — mounts, `-f`, `--requirements`, `--outcome`, handler files —
must be under your home folder, because only that is shared with the VM.
[Chapter 11](11-getting-started.md) explains the VM.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [18. Writing an agent](18-writing-an-agent.md) · [Contents](README.md) · **Next: [20. `sandbox.toml`, field by field](20-sandbox-toml.md) →**
