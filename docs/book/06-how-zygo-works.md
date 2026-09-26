# 6. How Zygo works

Zygo uses the same kernel parts as Docker. What it changes is who sets them
up, how often, and what is already waiting when a request arrives.

## Zygo in one picture

```text
                    ┌──────────────────────── zygo (your user, no root) ──────────────────────────┐
                    │                                                                             │
   zygo run ───────▶│  ONE-SHOT: build a sandbox ─▶ run the program ─▶ exit, nothing left  ~12 ms │
                    │                                                                             │
   zygo serve ─────▶│  WARM: build a sandbox once, start the interpreter, import the handler      │
                    │        and park it as a "zygote"                                   ~150 ms  │
                    │                                                                             │
   zygo exec ──────▶│        fork the zygote ─▶ the child runs one request ─▶ exit        ~1.4 ms │
   zygo exec ──────▶│        fork the zygote ─▶ the child runs one request ─▶ exit        ~1.4 ms │
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
12 ms on a Linux 6.8 VM. That is `python3 -c pass`. The 70.8 ms on the
README's table is the same command running a script that imports sixteen
modules: the sandbox costs the same 3.6 ms, and the rest is Python doing the
imports — the work a warm function does once ([chapter 25](25-performance.md)).

```text
  docker run                                   zygo run
  ──────────                                   ────────
  docker ─▶ dockerd ─▶ containerd              zygo
             ─▶ shim ─▶ runc ─▶ program          └─ clone3 ─▶ child: mounts, cgroup,
                                                              caps, Landlock, seccomp
  record + layer left: docker rm                              ─▶ execve ─▶ program
  300–1000 ms                                  nothing left · ~12 ms
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

The same idea reached serverless before Zygo. SOCK (Oakes et al., *SOCK:
Rapid Task Provisioning with Serverless-Optimized Containers*, USENIX ATC
2018) forks Python handlers from a zygote that has already imported their
packages, and Catalyzer (Du et al., *Catalyzer: Sub-millisecond Startup for
Serverless Computing with Initialization-less Booting*, ASPLOS 2020) restores
a function from a snapshot instead of starting it. Zygo's contribution is the
packaging — one static binary, rootless, every limit on, an agent protocol any
language can speak — not the idea.

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
and the zygote has never served a request. Files are the one place that needs
more than a fork: each request gets its own temporary folder, named by
`TMPDIR` and removed afterwards, because the sandbox's `/tmp` is shared by
every request in it. So a request sees exactly what the
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
and writing the result on standard output. This costs about 1.4 ms, measured
with `sh -c cat` as the program; a bigger program adds its own start. It works
for any language and any image, with no agent at all.

| | warm-exec (`cmd`) | agent (`entry`) |
|---|---|---|
| What is kept warm | the sandbox | the sandbox **and** the loaded interpreter |
| A request is | a new process entered into the sandbox | a `fork()` of the zygote |
| Overhead | ~1.4 ms + the program's own start | ~1.4 ms |
| Best for | Go, Rust, C, shell | Python, Node, anything with slow start-up |

## The agent and its protocol

The agent is the small program inside the sandbox that loads the handler,
forks, and passes events and results. It talks to Zygo over a simple,
documented wire protocol ([`spec/protocol.md`](../../spec/protocol.md)), not
through a library. So any language can have an agent: Zygo ships one for
Python and one for Node, and `zygo agent test` checks a new one against the
same conversation. A *runtime pool* is the same idea with no handler loaded
in advance: the script arrives with the request, which lets one warm zygote
serve thousands of different scripts for about 0.5 ms more.

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

## Why not `zygo ./venv/bin/python app.py`?

It looks simpler: you already have a Python and a venv, so why name an image
at all? Because a venv is not a Python. `venv/bin/python` is only a link to
the host's interpreter, and the venv folder holds only the extra packages;
the interpreter, its standard library and the C libraries under it all stay
in the host's `/usr` and `/lib`. To run it, Zygo would have to show the
sandbox those host folders, which is exactly what a sandbox exists to hide
([chapter 5](05-docker.md#why-a-sandbox-brings-its-own-files)). So Zygo has
no mode that uses the host's own files as the root, on purpose: every sandbox
starts from an image, and sees of your machine only what you `--mount`,
read-only unless you say `:rw`.

```text
  what you think a venv is          what a venv really is
  ────────────────────────          ─────────────────────
  ┌ venv/ ──────────────┐           ┌ venv/ ──────────────┐
  │ python              │           │ bin/python ─────────┼──▶ /usr/bin/python3.12   (host)
  │ everything it needs │           │ lib/…/site-packages │      ├─▶ /usr/lib/python3.12/ (host)
  └─────────────────────┘           └─────────────────────┘      └─▶ /lib/libc.so …       (host)
```

## Why not `zygo /usr/bin/python3 app.py`?

Then skip the venv and point at the system's own Python directly? It fails
for the same reason, and trying it shows why, one wall at a time. The file
`/usr/bin/python3` is small; the Python it starts is not. On an Ubuntu 24.04
machine it needs five shared libraries from the host's `/lib`, and then its
standard library — 1,194 files, 29 MB, in the host's `/usr/lib/python3.12`.
Mount only the binary into an image, and each missing piece stops it in turn.

```text
  zygo run --mount /usr/bin/python3:/opt/python3 IMAGE /opt/python3 -c "print(1)"

  wall 1  the loader    alpine:3               "the program does not exist"   exit 125
          (musl image, and the binary asks for glibc's loader)
  wall 2  the libraries debian:bookworm-slim   "libexpat.so.1: cannot open    exit 127
                        python:3.12-slim        shared object file"
          (the image has glibc, but not the exact libraries this build wants)
  wall 3  the stdlib    python:3.12-slim       "No module named 'encodings'"  exit 1
                        + libexpat mounted too
          (it looks for /usr/lib/python3.12; the image keeps its own elsewhere)
```

Measured on the Lima VM from [chapter 25](25-performance.md), Ubuntu 24.04,
Python 3.12.3, aarch64. Notice the last row: even an image with the *same*
Python version does not help, because the host's binary looks for the host's
files, in the host's places.

## And if you mounted all of it?

You could keep mounting — `/usr/lib/python3.12`, then the libraries, then
`/etc/ssl` — until it starts. By then the sandbox can read most of the host's
`/usr` and `/lib`: every program installed, every version, which is a map for
anyone looking for a weak spot. Worse, those files are not yours to hold
still: the next `apt upgrade` replaces them under a zygote that is still
running, and a warm function breaks in a way nobody can reproduce. The
system Python is also the operating system's own tool — `apt` and other
system programs depend on it, and on Debian and Ubuntu `pip` refuses to
install into it (the "externally managed environment" error). The image's
Python belongs to your function alone, and never changes unless you change
its name.

```text
  the system's Python                     the image's Python
  ───────────────────                     ──────────────────
  owned by the OS, used by apt            owned by your function
  changes with every apt upgrade          changes only when you change its name
  different on every machine              the same bytes everywhere (a digest)
  shows the sandbox the host's /usr       shows the sandbox nothing of the host
```

## What you do instead, for Python

You keep the parts that are *yours* and take the rest from an image. Your
code comes in with `--mount ./app.py:/app/app.py`. Your packages come from
`--requirements requirements.txt`: Zygo builds a venv once, *inside the
image*, with the image's own `pip`, and shares it with every run that asks
for the same list. The Python itself comes from the image, the same bytes on
every machine. Nothing here costs time per request: the image is pulled once,
the venv is built once, and both are mounted read-only in a few milliseconds.

```bash
# not:  zygo ./venv/bin/python app.py
zygo run --mount ./app.py:/app/app.py --requirements requirements.txt \
    python:3.12-slim python3 /app/app.py
```

## Why not `zygo ./my_executable_binary`?

Now take a compiled program instead — a Go server, a C tool. There is no
interpreter this time, so does it still need an image? It depends on how the
program was **linked**, which means how it finds the library code it uses.
A *dynamically linked* program keeps only its own code; when it starts, a
small loader (`/lib64/ld-linux-x86-64.so.2`) finds `libc.so` and friends on
the machine and joins them in. That is the default for C (`gcc app.c`), and
for Go when it uses `cgo`. Such a program has exactly the venv's problem: its
libraries live in the host's `/lib`, and the sandbox is not meant to see it.
A *statically linked* program carries every library inside its own file, so
it needs almost nothing — but "almost" is not "nothing".

```text
  dynamically linked (gcc app.c)          statically linked (CGO_ENABLED=0 go build)
  ──────────────────────────────          ─────────────────────────────────────────
  ┌ my_app ───────┐                       ┌ my_app ────────────────────────┐
  │ your code     │──▶ ld-linux.so (host) │ your code                      │
  └───────────────┘──▶ libc.so.6  (host)  │ + the Go runtime               │
                   ──▶ libssl.so  (host)  │ + every library it uses        │
                                          └────────────────────────────────┘
  needs the host's /lib inside            still wants a few files around it:
  the sandbox: the venv problem again     /etc/ssl/certs, /usr/share/zoneinfo, …
```

## What a static binary still needs

Even a fully static program expects a small world around it. To call an
HTTPS API it reads CA certificates from `/etc/ssl/certs`; to show local time
it reads `/usr/share/zoneinfo`; to turn a uid into a name it reads
`/etc/passwd`; to resolve a host name it reads `/etc/resolv.conf`; if it runs
`sh -c`, it needs a `/bin/sh`. And every sandbox needs a root to stand on:
somewhere to put `/tmp`, `/proc` and `/dev`. An image gives all of this in a
few megabytes, the same on every machine, downloaded once. So Zygo keeps one
rule with no exceptions: **the root always comes from an image**, and from
your machine a sandbox sees only what you `--mount`.

## What you do instead, for Go and C

Mount the binary into a small image and name it as the command. Which image
depends on how the binary was linked, and `file ./my_app` tells you: it says
either `statically linked` or `dynamically linked, interpreter …`.

| Your binary | Build it like this | Run it in |
|---|---|---|
| Go, static | `CGO_ENABLED=0 go build -o my_app` | `alpine:3` (about 3.5 MB) — or any image |
| C, static | `gcc -static -o my_app app.c` (or `musl-gcc -static`) | `alpine:3` — or any image |
| Go or C, dynamic, built on Debian/Ubuntu (glibc) | `go build` with cgo, `gcc app.c` | a glibc image: `debian:bookworm-slim` |
| Go or C, dynamic, built on Alpine (musl) | the same, on Alpine | `alpine:3` |

```bash
file ./my_app                              # "statically linked"? then any small image works
zygo run --mount ./my_app:/app/my_app alpine:3 /app/my_app --port 8080

# a dynamic glibc binary: pick an image with glibc, not Alpine
zygo run --mount ./my_app:/app/my_app debian:bookworm-slim /app/my_app
```

The mistake to avoid is a glibc binary in `alpine:3`: it fails with "the
program does not exist", because the loader it asks for is not in the image
([chapter 12](12-one-shot-sandboxes.md#programs-from-your-host)). And for a
binary you call again and again, do not pay even the 12 ms of a fresh
sandbox: keep the sandbox warm with `cmd`, and each call costs about 1.4 ms
plus the program's own start
([warm-exec](#warm-exec-for-programs-that-start-fast)).

```toml
[fn.parse]
image  = "alpine:3"
mounts = ["./bin/parse:/app/parse:ro"]   # a static Go or C binary you built
cmd    = ["/app/parse"]                  # event on stdin, JSON result on stdout
```

## Secrets

A secret, such as an API key, is never put in the environment and never in
the zygote's memory. While a request runs, the supervisor writes it as a
file, `/run/secrets/NAME`, from *outside* the sandbox, readable only inside
that function's sandbox, and removes it when the function's last request
ends. A request that is compromised can read the secrets its own function
was given, while it runs, and never one that another function uses.

## The network, off by default

A sandbox starts with no network at all. `network = "egress"` with
`allow = ["api.example.com:443"]` opens exactly that name and port: Zygo runs
its own small DNS resolver inside the sandbox, and when a name on the list is
asked for, its addresses are added to the nftables filter before the answer
goes back. A name not on the list does not even resolve. Private addresses and
the cloud metadata address stay closed in every mode unless you ask for them
by a flag named for exactly that. [Chapter 14](14-limits-network-secrets.md) has the
details.

## Safe by default, loosened by name

Every limit has a value even if you set none: 256 MB of memory, one CPU,
64 processes, 30 seconds, up to 64 MB of scratch, 1024 open files. The root is
read-only, no capability is kept, and the seccomp allowlist is on. To remove a
limit you type a flag whose name says what it does — `--allow-unlimited`,
`--allow-host-net`, `--allow-private-net`. Docker makes you *add* safety one
flag at a time; Zygo makes you *remove* it one flag at a time.
[The principles](08-principles.md) gives the principle and its cost.

## Three backends, one command

`isolation = "ns" | "gvisor" | "vm"` moves the wall without changing anything
else. `ns` is everything above: the host kernel with its locks, and the only
backend with warm functions. `gvisor` puts gVisor's user-space kernel between
your program and the host. `vm` boots a small virtual machine with its own
kernel through libkrun, for code you trust least, at about six times the
start-up cost. [ADR 0002](adr/0002-warm-paths-stay-on-ns.md) explains why
the warm path stays on `ns`.

## On a Mac

Sandboxes are a Linux feature, so on macOS Zygo starts and manages a small
Linux virtual machine through Lima. The `zygo` command on your Mac forwards
every sandbox command into it, with the same arguments, folder and streams.
Crossing into the VM adds about 22 ms per command; from a program, through the
API or the SDKs, the millisecond warm path is still there.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [5. Docker](05-docker.md) · [Contents](README.md) · **Next: [7. Where the time and memory are saved](07-where-zygo-saves.md) →**
