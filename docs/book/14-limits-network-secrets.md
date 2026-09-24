# 14. Limits, networking and secrets

A sandbox is only as good as the walls you give it. This chapter shows how
you set those walls in `sandbox.toml`: how much a function may use, what it
may reach on the network, and how a password or API key gets to it without
leaking. Every wall has a safe value even if you set nothing.

## The spec file in one minute

A *spec file* is `sandbox.toml`: one file that describes every function in a
project. `[defaults]` holds values for all functions, and each `[fn.<name>]`
section describes one function and may change any of them.
[Chapter 20](20-sandbox-toml.md) lists every field; this chapter explains the
ones that set limits, the network and secrets.

```toml
[defaults]
image     = "python:3.12-slim"
isolation = "ns"          # ns | gvisor | vm
mem       = "256M"
cpu       = 1.0
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
image  = "alpine:3"                # no runtime → warm-exec
mounts = ["./bin/parse:/app/parse:ro"]
cmd    = ["/app/parse"]

[fn.fetch]
entry       = "./fetch.py"
network     = "egress"             # nothing else is reachable
allow       = ["api.stripe.com:443", "*.example.com:443", "203.0.113.0/24:5432"]
connections = 32
bandwidth   = "2M"
secrets     = ["STRIPE_KEY"]
```

## Which value wins

A value can come from four places. The highest one that sets a field wins.

```text
  highest ┌───────────────────────────────┐
          │ a CLI flag (or an API call)   │   --mem 512M
          ├───────────────────────────────┤
          │ [fn.<name>]                   │   mem = "512M"
          ├───────────────────────────────┤
          │ [defaults]                    │   mem = "256M"
          ├───────────────────────────────┤
  lowest  │ Zygo's built-in default       │   256M
          └───────────────────────────────┘
```

Lists and tables — `allow`, `mounts`, `env`, `secrets`, `system` — **replace**
the one below them. They are never added together. So an `allow` list in
`[fn.fetch]` is the whole list for that function, and the one in `[defaults]`
no longer counts. Two commands show you the result:

```bash
zygo spec explain resize     # every field of `resize`, after the merge
zygo spec validate           # check the whole file, run nothing
```

## Every limit has a value

A *limit* is a ceiling the kernel enforces, most of them through the
function's cgroup ([chapter 3](03-cgroups.md)). Every limit has a default,
and you cannot turn one off by leaving it out. The only way to remove one is
a flag named `--allow-unlimited`. This is on purpose: a sandbox with no memory
limit is not a sandbox, and people usually end up with one by forgetting, not
by deciding.

| Field | What it bounds | Default | What happens when it is hit |
|---|---|---|---|
| `mem` | memory | `256M` | the kernel kills the request; exit 137, `oom_killed` |
| `cpu` | CPU time, in cores | `1.0` | the request is slowed down, not killed |
| `pids` | processes and threads | `64` | the next `fork` fails |
| `timeout` | wall-clock time | `30s` | the whole request is killed; exit 137, `timed_out` |
| `scratch` | the writable `/tmp` | the smaller of `64M` and half of `mem` | writes fail: "No space left on device" |
| `nofile` | open files | `1024` | the next `open` fails: "Too many open files" |
| `connections` | TCP connections at once | `256` | the next connection is refused |
| `bandwidth` | bytes a second sent | unlimited, with a warning | traffic is slowed down |
| `io_read`, `io_write` | disk bytes a second | unlimited, with a warning | disk reads or writes are slowed down |

The sections below take them one at a time.

## Memory: `mem`

`mem` is the most memory the request may use, counted by the kernel for every
process it starts. At 90% of it the kernel starts to push back and reclaim
memory; at 100% it kills the request. There is no swap, so memory cannot
quietly spill to disk and slow the host down. The smallest allowed value is
`8M`. The whole process tree dies together, and the warm zygote and the
other requests keep running.

```text
  0 ─────────────────────────────── 90% ───────────── 100%  (mem)
  normal use                        memory.high:      memory.max:
                                    kernel reclaims,  request killed,
                                    request slows     exit 137, oom_killed
```

## CPU: `cpu`

`cpu` is how many CPU cores the request may use, as a number: `0.5` is half a
core, `2` is two. The kernel checks it every 100 ms. A request that tries to
use more is simply made to wait for the rest of that period. So a busy loop
slows only itself, never the host, and it is not killed for it. This is also
why a function driven past its quota shows a slow tail of tens of
milliseconds; [chapter 25](25-performance.md) explains that number.

## Processes: `pids`

`pids` is how many processes and threads the request may have at once. It is
the defence against a *fork bomb*: a program that copies itself again and
again until the machine can do nothing else. When the limit is reached, the
next `fork` or new thread simply fails, and the program gets an error. `0` is
not allowed.

## Time: `timeout`

`timeout` is the wall-clock time a request may run, from start to end. When
it runs out, Zygo kills the request through its cgroup, so every process the
handler started dies with it, not just the one Zygo can see. The request
exits with code 137, and the outcome says `timed_out`. `timeout = 0` means
"no limit" and needs `--allow-unlimited`; `zygo up` never accepts it.

## Scratch space: `scratch`

A sandbox's root file system is read-only. The one place it may write is
`/tmp`, which lives in memory, and `scratch` is its size. Because it is
memory, it counts against `mem`, so `scratch` must be smaller than `mem`, and
Zygo warns you when it is more than half. The default is the smaller of
`64M` and half of `mem`. `scratch` is also the biggest single file a process
may write, and `/tmp` holds 10 000 files at most. When it is full, writes
fail with "No space left on device".

## Open files, connections, bandwidth and disk

`nofile` (default 1024) is how many files, sockets and pipes one process may
have open; past it, `open` fails with "Too many open files". `connections`
(default 256) is how many TCP connections a networked function may hold at
once; the firewall inside the sandbox answers the next one with a reset. It
cannot be `0`. `bandwidth` is how many bytes a second the function may send,
and on hosts with an `ifb` device also receive. `io_read` and `io_write` limit
disk reads and writes a second. These three have no default, so Zygo warns
you when a served function leaves them unset.

## Thread pools follow `cpu`

Many number-crunching libraries — OpenBLAS, OpenMP, MKL, numexpr, polars,
Rayon — start one thread per CPU on the *machine*. In a sandbox with
`cpu = 2` on a big host, those threads then fight over two cores. On a 5-CPU
host, eight numpy requests at once under `cpu = 2` ran 24–38 a second with
the host's count and 155–173 a second with one thread each. So Zygo sets
these variables to `cpu`, rounded up:

```text
OMP_NUM_THREADS   OPENBLAS_NUM_THREADS   MKL_NUM_THREADS      NUMEXPR_NUM_THREADS
VECLIB_MAXIMUM_THREADS   RAYON_NUM_THREADS   POLARS_MAX_THREADS
PYTHON_CPU_COUNT   GOMAXPROCS
```

If the image or the spec sets any of them, that value wins.

## Knowing which limit ended a request

A request that Zygo or the kernel stopped exits with 137, both for time and
for memory, because both are a `SIGKILL`. When you need to know which, Zygo
tells you: `zygo run --outcome FILE` writes `timed_out`, `oom_killed`, peak
memory and wall time to a file, and a warm request reports the same fields.
[Chapter 12](12-one-shot-sandboxes.md) shows the one-shot case in full.

## Networking: four modes

A sandbox starts with **no network at all**. `network` opens it, one step at a
time:

```toml
network = "none"     # the default
```

| Mode | What the sandbox can reach |
|---|---|
| `none` | nothing: an empty network namespace with only loopback |
| `egress` | exactly what `allow` names, plus DNS for those names |
| `full` | the public internet (`bridge`, Docker's word, is accepted and printed back as `full`) |
| `host` | everything the host can reach; no network namespace. Needs `--allow-host-net` |

*Egress* means traffic going out. No mode lets anything connect *in* to a
sandbox: Zygo never accepts connections for your code.

## How `egress` works

`egress` and `full` give the sandbox's network namespace to
[`pasta`](https://passt.top). `pasta` is a small program that moves the
sandbox's packets out through the host's normal sockets, running as your own
user. Inside the namespace, Zygo installs an *nftables* firewall — the
kernel's packet filter — that lets only allowed traffic out. No root is
needed anywhere. Under `egress`, Zygo also runs its own tiny DNS resolver
inside the sandbox, and that resolver is what fills the firewall.

```text
  ┌─ sandbox network namespace ─────────────────────────────────────────┐
  │                                                                     │
  │  handler                                                            │
  │    │ 1. "where is api.stripe.com?"                                  │
  │    ▼                                                                │
  │  ┌──────────────────────┐  2. on the allow list?                    │
  │  │ Zygo's DNS resolver  │     no  → "no such name" (NXDOMAIN)       │
  │  │ (on loopback)        │     yes → look it up, then …              │
  │  └──────────┬───────────┘                                           │
  │             │ 3. … add its addresses to the firewall FIRST,         │
  │             ▼    then answer the handler                            │
  │  ┌──────────────────────────────────────────────────────┐           │
  │  │ nftables firewall                                    │           │
  │  │  loopback, replies to allowed connections   → pass   │           │
  │  │  more than `connections` open               → reset  │           │
  │  │  10.x, 192.168.x, 169.254.169.254 …         → reject │           │
  │  │  addresses the resolver added, on that port → pass   │           │
  │  │  anything else                              → reject │           │
  │  └──────────┬───────────────────────────────────────────┘           │
  │             │ 4. connect to api.stripe.com:443                      │
  │             │ 5. out through tap0, the sandbox's network card       │
  └─────────────┼───────────────────────────────────────────────────────┘
                ▼
         ┌──────────────┐
         │ pasta        │──▶ normal sockets on the host ──▶ the internet
         │ (your user)  │
         └──────────────┘
```

Because the answer only comes back after the firewall is updated, a wildcard
is checked against the name the handler really asked for. A service that
changes its address keeps working, because each lookup adds the new one. The
handler cannot use a resolver of its own to get around the list: under
`egress` the only resolver it can reach is Zygo's. The host's search domains
never enter the sandbox. A blocked connection is rejected at once, not
silently dropped, so a program fails fast instead of waiting for a timeout.

## The allowlist

`allow` is the list of places an `egress` function may reach. It is only
valid with `network = "egress"`. An `egress` function with an empty list can
reach nothing, and Zygo warns you.

```toml
allow = ["api.stripe.com:443", "*.example.com:443", "203.0.113.0/24:5432"]
```

| Form | Example | Matches |
|---|---|---|
| `host:port` | `api.stripe.com:443` | that name, that port |
| `host` | `api.stripe.com` | that name, every port |
| `*.domain:port` | `*.example.com:443` | every name under `example.com`, but **not** `example.com` itself |
| `CIDR:port` | `203.0.113.0/24:5432` | that range of addresses, that port |
| IPv6 | `[2001:db8::1]:443`, `2001:db8::/32` | use brackets when a port follows |

A *CIDR* is a way to write a range of addresses: `203.0.113.0/24` means every
address that starts with `203.0.113.`.

## Private ranges and the cloud metadata address

*Private* addresses are the ones used inside a home or company network, such
as `10.x.x.x` or `192.168.x.x`. *Link-local* ones include `169.254.169.254`,
where cloud servers answer questions like "what are my credentials?". That
address is the first thing a compromised handler tries. So in every mode
with a namespace — `none`, `egress` and `full` — these ranges stay closed:

```text
10.0.0.0/8   172.16.0.0/12   192.168.0.0/16   127.0.0.0/8   169.254.0.0/16
100.64.0.0/10   fe80::/10   fc00::/7
```

Only `--allow-private-net` opens them. An `allow` rule with a CIDR inside one
of these ranges is refused unless you pass that flag.

## If `pasta` or `nft` is missing

A networked sandbox needs both `pasta` and `nft` on the host. If either is
missing, the sandbox **does not start**. Zygo never falls back to starting it
with an open network. `zygo doctor` tells you what is missing, and
[chapter 22](22-troubleshooting.md) covers the common problems.

## Secrets: why not environment variables

A *secret* is a value that must not leak: an API key, a database password, a
token. The usual way to pass one is an environment variable, but that is a
poor fit for a sandbox. The zygote would hold it for its whole life, every
forked request would inherit it, and a handler that prints its environment
would print it. Zygo takes a different path: a secret is a **file**, and it
exists only while one request runs.

## Declaring a secret

You name the secret in the spec, and give its value in the shell that runs
`zygo serve` or `zygo up`:

```toml
[fn.fetch]
secrets = ["STRIPE_KEY"]
```

```bash
export STRIPE_KEY=sk_live_...
zygo up
```

The value is read from **your** shell, not from the supervisor's
environment. If a name has no value, the command refuses and says which name
is missing. The handler reads the secret as a file:

```python
def handler(event):
    key = open("/run/secrets/STRIPE_KEY").read().strip()
```

## A secret lives for one request

For each request, the supervisor writes the file `/run/secrets/<NAME>` from
*outside* the sandbox. It is created with mode 0400 — readable only by its
owner — from the first moment, not changed to that mode afterwards, so there
is no instant when anyone else could read it. Only the request's own process
can read it, and the file is removed when the request ends.

```text
  supervisor (holds the value)
      │
      │ request 7 arrives
      ├──▶ write /run/secrets/STRIPE_KEY  (0400, only request 7 can read it)
      │        │
      │        ▼
      │    ┌─────────────────────────────┐
      │    │ request 7: handler(event)   │  reads the file, uses the key
      │    └─────────────────────────────┘
      │        │ request 7 ends
      ├──▶ file removed
      │
      │ request 8 arrives ──▶ a new file, only request 8 can read it

  never in: the environment · the zygote's memory · the control socket
```

A request that is taken over can read its own secret, but not the next
request's, and not a secret that another function uses. This matters most for
AI agent tools: a model that writes the code cannot print a secret it never
had in its environment.

## `env` or `secrets`?

| | `env` | `secrets` |
|---|---|---|
| Where it appears | environment variables | the file `/run/secrets/<NAME>` |
| Who can see it | the zygote and every request | one request, while it runs |
| Where the value comes from | the spec file | your shell, or the tenant store |
| Use it for | settings: `LOG_LEVEL`, a region | keys, tokens, passwords |

A name cannot be in both lists. **Never put a secret in `env`.**

## The tenant secret store

Reading secrets from a shell works for one person at a terminal. It does not
work for a platform whose customers each bring their own keys. A *tenant* is
one such customer, with its own functions and tokens
([chapter 17](17-api-sdk-mcp.md#who-is-calling-tokens-and-tenants)). For
them Zygo keeps an encrypted *secret store*, one set of secrets per tenant,
saved on disk under Zygo's data folder.

```bash
zygo secrets keygen                              # print a new key (32 bytes, hex)
export ZYGO_SECRETS_KEY=...                      # before the supervisor starts
zygo secrets set acme STRIPE_KEY                 # prompts, echo off
echo "$KEY" | zygo secrets set acme STRIPE_KEY --stdin
zygo secrets ls acme                             # names only, never values
zygo secrets rm acme STRIPE_KEY
```

Over the HTTP API the same is `PUT /tenants/<id>/secrets/<name>`. Functions
you serve from the CLI belong to the tenant named `default`.

## How the store fills in values

When a function is served, Zygo first takes the values the client sent — for
the CLI, from your shell. Then it fills in, from the tenant's store, any name
the client did **not** send. The client wins where both have a value. Only
the names the spec lists are delivered; anything extra is dropped.

```text
  spec: secrets = ["STRIPE_KEY", "DB_PASSWORD"]

  sent by the client:   STRIPE_KEY=sk_test_…   ─┐
                                                ├─▶  STRIPE_KEY  = sk_test_…  (client wins)
  tenant store (acme):  STRIPE_KEY=sk_live_…   ─┤    DB_PASSWORD = …          (from store)
                        DB_PASSWORD=…          ─┘
  a name with no value anywhere → the serve is refused, naming it
```

## The store's key

The key comes from `ZYGO_SECRETS_KEY`, or from a file named by
`ZYGO_SECRETS_KEY_FILE`. It must be 32 bytes, written as hex or base64.
**A passphrase is refused**: turning a password into a key safely needs extra
machinery, and a store that quietly accepted `hunter2` would be worse than one
that says no. Zygo never saves the key. If you lose it, every stored secret
is unreadable for good. Without a key, the supervisor still serves every
function with no stored secrets; only the store refuses, and says which
variable to set.

## What the store protects, and what it does not

Each value is encrypted with ChaCha20-Poly1305, a modern cipher, with the
tenant and the name bound to it. So a value cannot be moved to another tenant
or another name by renaming files. A value may be at most 64 KiB; anything
bigger is a file, not a secret. Values cannot be read back, only listed by
name. The encryption protects the bytes **on disk**. It does not protect them
from a process that can read the supervisor's memory, and it is not a
hardware security module; if you need that, use your cloud's key service.

## Loosening, by name

Three things are refused unless you type a flag that says exactly what it
does:

| Flag | What it allows |
|---|---|
| `--allow-host-net` | `network = "host"` |
| `--allow-private-net` | private and link-local addresses, and `allow` rules inside them |
| `--allow-unlimited` | `timeout = 0` |

The flags exist on `zygo run`, `zygo serve` and `zygo mcp`. **`zygo up` has
none of them.** A spec that needs one has to be served on purpose, one
function at a time, with the flag typed by a person.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [13. Warm functions](13-warm-functions.md) · [Contents](README.md) · **Next: [15. Images and dependencies](15-images-and-dependencies.md) →**
