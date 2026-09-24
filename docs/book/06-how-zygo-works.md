# 6. How Zygo works

Zygo uses the same kernel parts as Docker. What it changes is who sets them
up, how often, and what is already waiting when a request arrives.

## Zygo in one picture

```text
                    ┌──────────────────────── zygo (your user, no root) ──────────────────────────┐
                    │                                                                             │
   zygo run ───────▶│  ONE-SHOT: build a sandbox ─▶ run the program ─▶ exit, nothing left  ~18 ms │
                    │                                                                             │
   zygo serve ─────▶│  WARM: build a sandbox once, start the interpreter, import the handler      │
                    │        and park it as a "zygote"                                   ~270 ms  │
                    │                                                                             │
   zygo exec ──────▶│        fork the zygote ─▶ the child runs one request ─▶ exit        ~1.7 ms │
   zygo exec ──────▶│        fork the zygote ─▶ the child runs one request ─▶ exit        ~1.7 ms │
                    │                                                                             │
                    └─────────────────────────────────────────────────────────────────────────────┘
```

There are two halves. The *one-shot* half is a better `docker run`: a fresh
sandbox per program, built in one process. The *warm* half is the reason the
project exists: a sandbox that is built once and then *copied* for every
request. The rest of the chapter takes them in that order.

## The one-shot sandbox: `zygo run`

`zygo run python:3.12 python3 app.py` does the whole list from
[chapter 4](04-other-locks.md#putting-it-together) itself. It reads the image
from its own store, works out a *mount plan*, calls `clone3` into seven new
namespaces, and the child builds its root, joins its cgroup, drops every
capability, installs Landlock and seccomp, and calls `execve`. There is no
daemon, no RPC, and no container record. When the program exits, the kernel
removes the namespaces and the tmpfs, Zygo removes the cgroup, and nothing
is left to clean up. With the image already pulled, this takes a median of
18 ms.

```text
  docker run                                   zygo run
  ──────────                                   ────────
  docker ─▶ dockerd ─▶ containerd              zygo
             ─▶ shim ─▶ runc ─▶ program          └─ clone3 ─▶ child: mounts, cgroup,
                                                              caps, Landlock, seccomp
  record + layer left: docker rm                              ─▶ execve ─▶ program
  300–1000 ms                                  nothing left · ~18 ms
```

## The idea of a zygote

The name comes from Android. Starting a Java app from nothing is slow, so
Android starts one process, the *Zygote*, loads the common libraries into it,
and then forks it every time an app opens. Each app gets a ready-made process
in a few milliseconds, and the loaded libraries are shared through
copy-on-write. Zygo applies the same trick to a sandbox. The expensive part of
running a Python function is not the sandbox, it is Python itself — starting
the interpreter and importing modules often takes hundreds of milliseconds.
So Zygo does that once, inside the sandbox, and forks the result.

## The warm path

`zygo serve ./handler.py` builds a sandbox, starts a small *agent* inside it,
and the agent imports your handler and waits. That waiting process is the
zygote. For each request the zygote calls `fork()`; the child runs your
`handler(event)` once, sends back the result, and exits. The child starts with
everything already loaded, because it is a copy of a process that had loaded
it. And it leaves nothing behind, because it is thrown away.

```text
            ┌──────────────── one warm sandbox (namespaces, root, filters) ─────────────────┐
            │                                                                               │
            │   zygote: python + imports done + handler loaded — never runs a request       │
            │     │                                                                         │
 request 1 ─┼──▶  ├── fork ─▶ child 1: handler(event) ─▶ reply ─▶ exit  (its writes: gone)  │
 request 2 ─┼──▶  ├── fork ─▶ child 2: handler(event) ─▶ reply ─▶ exit  (starts clean)      │
 request 3 ─┼──▶  └── fork ─▶ child 3: handler(event) ─▶ reply ─▶ exit  (starts clean)      │
            │                                                                               │
            └───────────────────────────────────────────────────────────────────────────────┘
```

## Why a fork is clean

A normal worker process that serves many requests slowly collects state: a
global that one request changed, an open connection, a patched function, a
file in `/tmp`. Request *n* runs in whatever request *n−1* left. Zygo's
children never share that problem, because each one is a copy of the zygote,
and the zygote has never served a request. So a request sees exactly what the
zygote had after its imports — every time, whatever the requests before it
did. You get the speed of a shared worker and the cleanliness of a fresh
container.

```text
  a shared worker                          zygo exec
  ───────────────                          ─────────
  worker ─ req 1 ─ req 2 ─ req 3 ─ …       zygote ──┬─ copy ─ req 1 ✗
     state piles up: req 3 sees                     ├─ copy ─ req 2 ✗
     what req 1 and 2 left behind                   └─ copy ─ req 3 ✗
                                           every copy starts from the same clean point
```

## One cgroup per request

Each child is put in a cgroup of its own, under its function's group — you saw
the tree in [chapter 3](03-cgroups.md#the-tree-as-files). That gives every
request its own memory limit, process limit and CPU share. When a request
runs past its deadline, one write to its `cgroup.kill` ends it and every
process it started, and the zygote keeps serving. When it runs out of memory,
the kernel kills *that request*, not the zygote and not a neighbour. After
each request Zygo reads the group's numbers and reports the result:
`timed_out`, `oom_killed`, peak memory, wall time.

## Warm-exec, for programs that start fast

A Go or Rust binary starts in a millisecond, so there is nothing to keep
warm inside it. For these Zygo keeps only the *sandbox* warm: namespaces,
mounts and filters built once. Each request is a new process that enters the
sandbox with `setns` and execs your `cmd`, reading the event on standard input
and writing the result on standard output. This costs a median of about 2 ms.
It works for any language and any image, with no agent at all.

| | warm-exec (`cmd`) | agent (`entry`) |
|---|---|---|
| What is kept warm | the sandbox | the sandbox **and** the loaded interpreter |
| A request is | a new process entered into the sandbox | a `fork()` of the zygote |
| Overhead | ~2 ms + the program's own start | ~1.7 ms |
| Best for | Go, Rust, C, shell | Python, Node, anything with slow start-up |

## The agent and its protocol

The agent is the small program inside the sandbox that loads the handler,
forks, and passes events and results. It talks to Zygo over a simple,
documented wire protocol ([`spec/protocol.md`](../../spec/protocol.md)), not
through a library. So any language can have an agent: Zygo ships one for
Python and one for Node, and `zygo agent test` checks a new one against the
same conversation. A *runtime pool* is the same idea with no handler loaded
in advance: the script arrives with the request, which lets one warm zygote
serve thousands of different scripts for about 0.65 ms more.

## The supervisor

Someone has to keep zygotes alive, hand out requests, watch deadlines and
collect results. That is the *supervisor*: a normal process under your own
user, which the first `zygo serve` starts when it needs one. It is not a
system service and it never runs as root. It owns the cgroup tree, keeps a
reserve of memory for itself so that busy tenants cannot starve it, and
serves the CLI, the HTTP API and the SDKs. If it goes, the sandboxes it
started go with it; nothing is left running on its own.

```text
  CLI · HTTP API · Python/Node SDK · MCP
                  │
          ┌───────▼────────┐    memory reserved,
          │   supervisor   │    so tenants cannot starve it
          │  (your user)   │
          └──┬─────┬─────┬─┘
             │     │     │   deadlines, cgroups, secrets, logs
      ┌──────▼┐ ┌──▼────┐ ┌▼──────┐
      │zygote │ │zygote │ │sandbox│   one per function (and script version)
      │resize │ │fetch  │ │parse  │
      └───────┘ └───────┘ └───────┘
```

## Images without a Dockerfile

Zygo pulls normal OCI images from any registry, into a store under your home
folder. What you would put in a Dockerfile goes in `sandbox.toml` instead:
`requirements = "./requirements.txt"` becomes a Python venv, built once inside
a sandbox with the image's own `pip`, and `system = ["libwebp7"]` becomes an
extra layer with those apt packages. Each is keyed on the image digest and
the list, built once, and shared by every function that asks for the same
thing. The image itself is never changed, and there is no build step for you
to run or to push.

```text
   python:3.12-slim  (from the registry, never changed)
          │
          ├── + bytecode layer   (.pyc for the standard library, built once)
          ├── + system layer     (apt: libwebp7)        key: image digest + list
          └── + /venv            (pip: requirements)    key: image digest + lockfile
                     │
            shared by every function that names the same image and the same list
```

## Secrets

A secret, such as an API key, is never put in the environment and never in
the zygote's memory. For each request, the supervisor writes it as a file,
`/run/secrets/NAME`, readable only by that request's process, from *outside*
the sandbox, and removes it when the request ends. A request that is
compromised can read its own secret, but not a secret of the next request,
and not one that some other function uses.

## The network, off by default

A sandbox starts with no network at all. `network = "egress"` with
`allow = ["api.example.com:443"]` opens exactly that name and port: Zygo runs
its own small DNS resolver inside the sandbox, and when a name on the list is
asked for, its addresses are added to the nftables filter before the answer
goes back. A name not on the list does not even resolve. Private addresses and
the cloud metadata address stay closed in every mode unless you ask for them
by a flag named for exactly that. [The guide](../guide.md#networking) has the
details.

## Safe by default, loosened by name

Every limit has a value even if you set none: 256 MB of memory, half a CPU,
64 processes, 30 seconds, 64 MB of scratch, 1024 open files. The root is
read-only, no capability is kept, and the seccomp allowlist is on. To remove a
limit you type a flag whose name says what it does — `--allow-unlimited`,
`--allow-host-net`, `--allow-private-net`. Docker makes you *add* safety one
flag at a time; Zygo makes you *remove* it one flag at a time.
[Concepts](../concepts.md) gives the principle and its cost.

## Three backends, one command

`isolation = "ns" | "gvisor" | "vm"` moves the wall without changing anything
else. `ns` is everything above: the host kernel with its locks, and the only
backend with warm functions. `gvisor` puts gVisor's user-space kernel between
your program and the host. `vm` boots a small virtual machine with its own
kernel through libkrun, for code you trust least, at about ten times the
start-up cost. [ADR 0002](../adr/0002-warm-paths-stay-on-ns.md) explains why
the warm path stays on `ns`.

## On a Mac

Sandboxes are a Linux feature, so on macOS Zygo starts and manages a small
Linux virtual machine through Lima. The `zygo` command on your Mac forwards
every sandbox command into it, with the same arguments, folder and streams.
Crossing into the VM adds about 20 ms per command; from a program, through the
API or the SDKs, the millisecond warm path is still there.
