# 10. Similar projects, and Docker side by side

Many projects build a sandbox from the parts in chapters 2 to 4. They differ
in three questions, and once you ask those, most comparisons answer
themselves. This chapter asks them, sets `docker run` and `zygo run` side by
side in detail, and then walks through the projects one by one.

## The three questions

1. **Where is the wall?** The host kernel with locks on it, a second kernel in
   user space, or a virtual machine with its own kernel.
2. **What is ready when a request arrives?** Nothing — the sandbox is built
   each time; the sandbox — a new process enters it; or the sandbox *and* the
   loaded program — a copy is forked.
3. **Who has to be root?** The tool, a daemon, the admin once, or nobody.

Numbers for Zygo in this chapter are the project's own measurements;
[chapter 25](25-performance.md) names the hosts and the commands. The claims
about other projects are theirs or commonly measured, not measured here. They
all move quickly; check before quoting.

## The map

The three questions give every project a place. The table below puts the
first two on a grid: each **row** is where the wall is, each **column** is
what is already waiting when a request comes in. Read a column from left to
right as "less work per request": on the left everything is built for each
call, on the right the program is already loaded and only copied.

```text
  what is ready when a request arrives?

  NOTHING                    THE SANDBOX                 THE SANDBOX + THE PROGRAM
  ───────                    ───────────                 ─────────────────────────
  build the sandbox          enter the sandbox           fork the loaded program
  start the program          start the program           run the request
  run the request            run the request
  tear it all down

  most tools work here       `docker exec`, warm-exec    only Zygo `exec` works here
  slowest per call                                       fastest per call
```

| Wall ↓ · Ready on arrival → | nothing: build it all | the sandbox: enter it | the program: fork it |
|---|---|---|---|
| **Virtual machine** | Firecracker, Kata, microsandbox, Zygo `vm` | — | Firecracker from a memory snapshot (a whole VM per restore) |
| **Second kernel** | gVisor, Zygo `gvisor` | — | — |
| **Host kernel** | Docker, Podman, runc, nsjail, bubblewrap, firejail, minijail, kern, Zygo `run` | `docker exec` (state is shared), Zygo warm-exec | **Zygo `exec`** |
| **Process confinement only** | nono, Landlock-based tools: they confine a process you already run | | |

The right-hand column is almost empty, and that is the space Zygo was built
for. Everything else on this page is a good tool for a nearby job.

## What one call costs, tool by tool

The chart shows roughly what one call to a small Python function costs with
each tool. The scale is **logarithmic**: each mark to the right is **ten
times** slower than the one before, so a bar twice as long is not twice as
slow but many times slower. A solid bar (█) is the usual cost; a light part
(▒) is the range above it. Every bar except the first includes starting Python
itself; the first does not, because the warm zygote started Python long
before the request.

```text
                          1 ms          10 ms         100 ms        1 s
                          │·············│·············│·············│
Zygo exec (warm fork)     ██                                           1.4 ms
Zygo run                  ███████████████                              12 ms
nsjail, bubblewrap, kern  ████████████████▒▒▒▒▒▒                       ~15–40 ms
gVisor                    █████████████████████████▒▒▒▒▒▒              ~60–160 ms
Firecracker microVM       ██████████████████████████████▒▒▒▒           ~140–250 ms
docker run                ███████████████████████████████████▒▒▒▒▒▒▒   300–1000 ms
```

Only the two Zygo rows are measured by this project: `zygo exec` is the warm
path's median, and `zygo run` is the median of `python3 -c pass` with the
image already pulled ([chapter 25](25-performance.md)). The other rows are
rough ranges — each project's own claims or common measurements, plus Python's
start-up — and are here to show the *order of size*, not exact numbers. The
lesson is in the shape: the one-shot tools cluster together, because they all
pay for building a sandbox and starting Python, and only a warm fork leaves
that cluster.

## The request path

This table compares what each tool costs on every request, and what it costs
once, up front. The Zygo columns are measured; the other columns are the
projects' documented or commonly measured figures.

How to read the Zygo numbers: we timed many requests. **"Usually"** is what
a normal request costs. **"1 in 100"** is what the slowest requests cost —
only one request in a hundred was slower than that.

```text
  fast ◀──────────────── 100 requests, sorted ────────────────▶ slow
        · · · · · · · · · · · · ▲ · · · · · · · · · · · · · · ▲ ·
                                usually                  1 in 100
```

These are the time a tool **adds**. Your own code's time comes on top.

| | Docker `run` | Docker `exec` | Firecracker | gVisor `runsc` | AWS Lambda (warm) | **`zygo run`** | **`zygo exec --runtime` (pool)** | **`zygo exec` (warm)** |
|---|---|---|---|---|---|---|---|---|
| Per-request overhead | 300–1000 ms | 50–100 ms | ~125 ms boot; 10–20 ms from a snapshot | 50–150 ms | ~1–5 ms + platform | **usually 12 ms** | **usually 1.9 ms** · 1 in 100: 11.4 ms¹ · a different script each time | **usually 1.4 ms** · 1 in 100: 10.5 ms¹ |
| Paid once, up front | — | a `docker run -d` | the VM's own boot, or a snapshot | — | a cold start, platform-side | — | a `zygo serve --runtime`: the interpreter and its dependencies, once for *every* script | a `zygo serve`: ~150 ms for a Python handler, plus its imports |
| Clean state per request | yes | no | yes | yes | no | **yes** | **yes** (a fresh process; the script is loaded in it)² | **yes** (a fresh process)² |
| Daemon | yes | yes | yes (a VMM per VM) | yes (`runsc` + shim) | n/a | **no** | **no system service**: a supervisor under your user | **no system service**: a supervisor under your user |
| Root | daemon runs as root | same | needs `/dev/kvm` | no, in rootless mode (how Zygo runs it); then its cgroup limits are advisory | n/a | **no** | **no** | **no** |
| Wall | kernel | kernel | hardware | userspace kernel | hardware | kernel (`ns`); userspace kernel (`gvisor`); hardware (`vm`) | kernel (`ns` only) | kernel (`ns` only) |

¹ On Linux 6.x, 1 request in 100 waits about 9 ms for the kernel to move it
into its cgroup; on Linux 5.10 the same is 2.6 ms (warm) and 3.2 ms (pool).
[Chapter 25](25-performance.md#why-1-in-100-is-slow-on-newer-kernels) explains
it. Zygo numbers are from a Lima VM, Linux 6.8, 25 September 2026.

² Clean as in a fresh process forked from a zygote that has never served a
request. A fork still shares what the zygote had before any request: its
memory layout, a socket opened at import time, a literal `/tmp/...` path.
[Fork safety](fork-safety.md) goes through each.

The first row is the whole argument. A container's cost is the machinery
around it and the cold start of the interpreter. Zygo takes the machinery off
the request path entirely, and it pays the interpreter's start only once, in
a warm zygote.

The three Zygo columns are three answers to "what is warm?". `zygo run` keeps
nothing warm. A **pool** keeps the interpreter and its dependencies warm, but
no code: each request brings its own script, so one pool serves thousands of
different scripts, for about 0.5 ms more than a function. A warm **function**
keeps one handler loaded, which is the fastest, but costs one zygote per
script ([chapter 13](13-warm-functions.md#runtime-pools)).

```text
  what is warm?        zygo run          pool                 warm function
                       ────────          ────                 ─────────────
  sandbox              built each time   ready                ready
  interpreter + deps   started each time ready                ready
  your code            loaded each time  loaded per request   ready
  per request          12 ms             1.9 ms               1.4 ms
```

## `docker run` and `zygo run`, side by side

Both accept the same sentence — `IMAGE [COMMAND…]` — and both use OCI images
from the same registries. The likeness ends there. The next sections compare
them on who runs the command, what is left behind, the defaults, networking,
images, and the flags you already know.

## Who runs the command

`docker run` is a client. The request crosses a unix socket to `dockerd`, a
root daemon. `dockerd` hands it to `containerd`, which starts a
`containerd-shim`, which runs `runc`. `runc` sets the container up and exits;
the shim stays for as long as the container lives. That is five processes,
three RPC boundaries (calls from one program to another), all of them root.
Nearly all of the 300–1000 ms is that chain; the isolation itself — the
namespaces, the cgroup, the seccomp filter — is about a millisecond of it.
[Chapter 5](05-docker.md#the-chain-behind-docker-run) walks the chain step by
step.

`zygo run` is one process, with no RPC and no root. `zygo` itself calls
`clone3` into a fresh set of namespaces. The child applies the mount plan,
calls `pivot_root`, joins its cgroup, drops every capability, installs
seccomp and Landlock, and calls `execve`. With the image cached, that takes a
median of 12 ms. `ps` shows `zygo` with your program directly under it, and
nothing in between.

```text
  what ps shows for docker run                what ps shows for zygo run
  ────────────────────────────                ──────────────────────────
  docker (client, your shell)                 zygo (your user)
  dockerd            (root)                     └─ your program
  containerd         (root)
  containerd-shim    (root)
    └─ your program
  (runc started it and has already exited)
```

## What is left behind

`docker run` creates an *object*. When the process exits, the container stays
(`docker ps -a`), and its writable layer stays on disk. Something has to
`docker rm` it, or you must have passed `--rm`. Logs pile up in the daemon,
there are restart policies, and `docker exec` gets you back inside.

`zygo run` is a *process*. When it exits, the namespaces, the cgroup and the
tmpfs go with it; there is nothing to remove, and there is no `zygo rm`.
Standard input, output and the exit code pass straight through. The one
addition: a sandbox that Zygo or the kernel killed exits with code 137, and
`--outcome file.json` says which it was, with `timed_out`, `oom_killed`,
`peak_rss_kb` and `wall_ms`. `--dry-run --json` prints the mount plan and the
cgroup values without running anything; Docker has nothing like it.

The container you keep around and step into is a separate idea in Zygo:
`zygo serve` and `zygo exec`, a warm function. It looks like `docker exec`,
except that every request is a fresh process and costs about 1.4 ms.

```text
  docker run IMAGE                          zygo run IMAGE
  after exit:                               after exit:
  ┌──────────────────────────────────┐      ┌──────────────────────────────────┐
  │ container record (docker ps -a)  │      │ nothing                          │
  │ writable layer on disk           │      │ exit code passed through         │
  │ logs in the daemon               │      │ 137 if killed; --outcome says    │
  │ → needs docker rm (or --rm)      │      │ timed_out / oom_killed           │
  └──────────────────────────────────┘      └──────────────────────────────────┘
```

## The defaults

Docker's defaults are made for software you chose; Zygo's are made for code
you did not. This is what you get with no flags at all.

| | `docker run IMAGE` | `zygo run IMAGE` |
|---|---|---|
| Root filesystem | writable (a copy-on-write upper layer) | **read-only**; the only writable place is `/tmp`, a tmpfs sized by `scratch` (the smaller of 64M and half of `mem`, so 64M by default) |
| Capabilities | 14 kept (`NET_RAW`, `SYS_CHROOT`, `MKNOD`, …) | **none** |
| seccomp | a denylist-shaped profile allowing ~350 syscalls | **an allowlist of ~215 names** (the `default` profile; 190 of them exist on aarch64, all on x86_64); `bpf`, `io_uring`, `userfaultfd`, `ptrace`, `mount`, `unshare` absent; `clone` refused with any namespace flag |
| Landlock | no | yes, where the kernel has it |
| Runs as | root in the container, root on the host (outside rootless mode) | the image's uid 1000, mapped to **your** uid |
| Network | bridge; everything reachable | **`none`**: loopback only |
| Memory, CPU, pids, wall clock | **unlimited** | **all mandatory**: `mem` 256M, `cpu` 1.0, `pids` 64, `timeout` 30s, `nofile` 1024 |
| Bind mounts | `-v` is **rw** unless `:ro` | `--mount` is **ro** unless `:rw` |
| cgroupfs inside | mounted read-only | not mounted at all, so `release_agent` is not reachable |
| Escape suite | — | 19 escape vectors attempted on every change, 0 escaping |

A *denylist* names what is forbidden and allows the rest; an *allowlist*
names what is allowed and forbids the rest. `release_agent` is an old cgroup
file that has been used to escape containers, so Zygo does not show the
cgroup file system inside at all.

## Adding locks versus loosening them

In Docker you *add* safety: `--cap-drop ALL --read-only --security-opt
no-new-privileges --memory --pids-limit --network none`. In Zygo you
*loosen* it, and every flag that does so says it in its name:
`--allow-unlimited`, `--allow-host-net`, `--allow-private-net`. There is no
way to turn a limit off without one of them.

This is not "Zygo is safer than Docker". Docker's defaults suit software you
chose; Zygo's suit code you did not. The point is that each set of defaults
leans the way its use case needs. And every row in the Zygo column is
something a test *attempts*, not a setting a test reads.

```text
  Docker:  open ──(--cap-drop ALL, --read-only, --memory, --network none, …)──▶ tight
  Zygo:    tight ──(--allow-unlimited, --allow-host-net, --allow-private-net)──▶ looser
```

## Networking: four modes, none of them a server

Docker's network is built for a container that *is* a service: a bridge with
NAT (address translation, so many containers share the host's address),
`-p 8080:80` to publish a port, containers that find each other by name, and
`--network host` when you want none of it.

Zygo has four modes, and none of them accepts a connection. `none` is the
default. `egress` is an allowlist **by name** —
`allow = ["api.example.com:443", "*.cdn.example.com:443"]` — enforced by
nftables inside the sandbox's own namespace, with a DNS resolver of Zygo's
own. A name off the list does not resolve; a name on it has its addresses
added to the filter before the answer goes back. `full` is the public
internet. `host` means no network namespace and needs `--allow-host-net`.

Private address ranges and `169.254.169.254` (the cloud *metadata* address,
which often hands out credentials) stay refused in every namespaced mode. If
`pasta` or `nft` is missing, a networked sandbox does not start, instead of
starting without its filter. There is no port publishing; Zygo does not run
services. [Chapter 14](14-limits-network-secrets.md) covers the modes in use.

```text
  mode      reaches                                          needs
  ────      ───────                                          ─────
  none      loopback only                                    (default)
  egress    only the host:port names on the allow list       pasta + nft
  full      the public internet; never private or metadata   pasta + nft
  host      everything the host reaches (no namespace)       --allow-host-net
```

## What the sandbox can reach

The same probe, on the same host, was run through Zygo's `--net full` and
Docker's `--network bridge`: a small script that tries to open a connection to
each target and reports what the kernel answers.

| target | Zygo `--net full` | Docker `--network bridge` |
|---|---|---|
| cloud metadata `169.254.169.254:80` | no route | **routed** (refused by the host, not blocked) |
| the host's own Postgres | no route | **reached** |
| private `192.168.1.1:80` (the LAN router) | no route | **reached** |
| private `10.0.0.1:80` | no route | no route |
| the sandbox's default gateway | no route | no route |
| public internet `1.1.1.1:53` | reached | reached |

```text
                      ┌─────────────────────┐
  Docker bridge ─────▶│ host's Postgres     │◀──── reached
                 ────▶│ LAN router          │◀──── reached
                 ────▶│ metadata address    │◀──── routed (refused by host)
                      └─────────────────────┘
  Zygo --net full ──▶ public internet only; private and metadata: no route
```

## Closing the gap in Docker: four rules

Docker needs four firewall rules to close what Zygo closes by default. They
drop traffic from containers to the metadata range and the three private
ranges.

```bash
iptables -I DOCKER-USER -d 169.254.0.0/16 -j DROP
iptables -I DOCKER-USER -d 10.0.0.0/8     -j DROP
iptables -I DOCKER-USER -d 172.16.0.0/12  -j DROP
iptables -I DOCKER-USER -d 192.168.0.0/16 -j DROP
```

In a product where the code inside the sandbox is written by a *tenant* (a
customer of the product), this is not a matter of taste.

## The egress allowlist, end to end

The same host, the same probes, four configurations: a normal HTTPS request,
and a raw socket to a public DNS server.

| configuration | `https://example.com` | raw socket to `1.1.1.1:53` |
|---|---|---|
| `--net none` | DNS fails | `PermissionError` |
| `--net egress`, no `--allow` | DNS fails | `PermissionError` |
| `--net egress --allow example.com:443` | **200** | `OSError` |
| `--net full` | 200 | reached |

One named host opens and nothing else does, with no proxy process anywhere.

## Images and dependencies

Both pull the same images from the same registries, and `zygo run` pulls on
first use exactly as `docker run` does. The stores differ. Docker's is
`/var/lib/docker` and belongs to root. Zygo's is under `~/.local/share/zygo`,
*content-addressed* (each file is named by a hash of its contents), with the
layers unpacked. It uses rootless overlayfs on kernel 5.11 and newer, and a
flattened copy below that. `zygo login` stores a password for a private
registry, and an existing `~/.docker/config.json` is read too.

The real difference is the Dockerfile. In Docker, adding a dependency means
building an image. In Zygo the image is never touched: `--requirements
./requirements.txt` builds a venv inside a sandbox with the image's own `pip`
and mounts it at `/venv`, and `system = ["libwebp7"]` installs apt packages
into a derived OCI layer. Both are keyed on the image digest and the list,
built once, and shared by everything that names the same thing.
[Chapter 15](15-images-and-dependencies.md) has the details.

```text
  Docker                                   Zygo
  Dockerfile ─▶ docker build ─▶ new image   image (untouched)
                                              ├─ --requirements ─▶ venv at /venv
                                              └─ system = [...] ─▶ derived layer
                                            built once per (image digest, list)
```

## The flags you already know

| Docker | Zygo | Note |
|---|---|---|
| `-v ./x:/x` | `--mount ./x:/x:rw` | read-only is Zygo's default; a single file can be mounted too |
| `-e K=V` | `--env K=V` | secrets are not environment: they arrive as `/run/secrets/<NAME>` |
| `-w /dir` | `--workdir /dir` | default `/app`, falling back to `/` |
| `-u 1000` | `--user 1000` | |
| `-it` | `--tty` | |
| `--memory 256m --cpus 0.5 --pids-limit 64` | `--mem 256M --cpu 0.5 --pids 64` | present in Zygo whether you pass them or not (the default `cpu` is 1.0) |
| `timeout 30 docker run …` | `--timeout 30s` | the whole process tree is killed, through the cgroup |
| `--network none` | (the default) | `--net egress --allow host:port` for an allowlist |
| `--network bridge` | `--net full` | `bridge` is accepted as a spelling of `full`; see [what the sandbox can reach](#what-the-sandbox-can-reach) for the difference |
| `--network host` | `--net host --allow-host-net` | the same removal of the wall, and it says so in its name |
| `--security-opt seccomp=…` | `--seccomp default\|strict\|permissive` | three shipped profiles; see [chapter 24](24-seccomp-profiles.md) |
| `--runtime runsc` | `--isolation gvisor` | same spec, same command; `vm` is another value of the same flag |
| `--rm` | (always) | |
| `-d`, `-p`, `--restart` | — | Zygo does not run services |

## Docker

Docker is a general-purpose system for packaging and running services. Zygo
uses its images and none of its runtime. If you need `docker build`,
`docker compose`, long-running services, port publishing, networks between
containers or restart policies, you need Docker.
[Chapter 5](05-docker.md) explains how it works.

## nsjail

nsjail, from Google, is the closest older relative of Zygo's one-shot half.
It puts a process into namespaces, cgroups, rlimits and a seccomp filter,
which you write in a small policy language called Kafel. It can run a
command once, run it again and again, or listen on a TCP port and start a
fresh jail for every connection, which made it the standard tool for hosting
CTF challenges. Windmill and other job runners can wrap each job in it. It
has no images — you give it a folder or bind-mount the host's — and no warm
path: every run starts the program from nothing. Choose it when you want a
battle-tested, very configurable jail around a command and you manage the
root file system yourself. Zygo in nsjail's place inside Windmill's workers
cost the same per job; [the measurement](#measured-zygo-against-nsjail-and-kern)
is further down.

## bubblewrap

bubblewrap (`bwrap`) is the small sandbox tool under Flatpak. It creates
namespaces, builds a root from the bind mounts you list, and then runs a
command — nothing more, on purpose. It has no cgroup limits and no seccomp
policy of its own; you pass a compiled filter if you want one. That smallness
is its strength: it is easy to audit and it runs everywhere, which is why many
desktop and developer tools use it as a building block. Zygo covers what
bubblewrap leaves to the caller: images, limits, the filter, the network
allowlist, and the warm path.

## firejail

firejail sandboxes desktop programs — a browser, a PDF reader — using ready
profiles for hundreds of applications. It is installed setuid root, so any
user can start a sandbox, but its own code runs with root's power, and bugs in
it have mattered in the past. It is made for confining the apps on your
desktop, not for running server-side functions at high rates. Zygo needs no
setuid program at all, because user namespaces give it what it needs.

## minijail

minijail is Google's sandbox library and tool for ChromeOS and Android system
services. It applies namespaces, capabilities, seccomp and user changes to a
service before it starts, from a policy file. It is part of the operating
system's own plumbing and is written for that setting: known services,
written by the same team. Zygo is aimed at the reverse: code written by
someone else, arriving at request time.

## systemd-nspawn and systemd-run

systemd-nspawn starts a whole operating-system tree in namespaces — "chroot
on steroids", in its own words — and is good for booting a distribution in a
container for testing. `systemd-run` can put any command in a transient unit
with limits and many sandbox options. Both mostly need root, and both think in
units and machines rather than requests. Zygo uses systemd only where it has
to: to get a delegated cgroup under your login.

## The three you are really choosing between

Docker, Firecracker, gVisor and Lambda are the landmarks; they are not the
shortlist. Someone looking for "run this agent's code somewhere safe" ends up
comparing Zygo with three much closer projects: kern, nono and microsandbox.
In two of the three cases the honest answer is that they solve a different
problem. Their claims below are theirs, not measured here, and all three move
quickly.

| | [kern](https://github.com/getkern/kern) | [nono](https://nono.sh) | [microsandbox](https://github.com/superradcompany/microsandbox) | **Zygo** |
|---|---|---|---|---|
| Shape | rootless container runtime, one static binary | a confinement you apply to a process you already have | microVM runtime and platform | rootless sandbox runtime **plus a warm-process protocol** |
| Wall | host kernel (namespaces, seccomp, cgroups) | host kernel (Landlock + seccomp; Seatbelt on macOS) | hardware (libkrun) | host kernel (`ns`), userspace kernel (`gvisor`), hardware (`vm`) |
| OCI images | yes | no images at all | yes | yes |
| Per-call cost | a fresh box, single-digit ms | none — it confines a process you were starting anyway | a microVM, boot under ~100 ms | a fresh sandbox, 12 ms — **or a fork into a warm one, 1.4 ms** |
| State between calls | none: the box is destroyed | whatever your process kept | a sandbox can be kept, branched and snapshotted | none, and not by destroying anything: each request is a `fork()` of a process that has never served one |
| Runs on macOS | Linux and WSL2 | yes, natively, with Seatbelt | yes | through a Linux VM it manages |
| Daemon | no | no | no | no system service; warm functions live under a supervisor that runs as your user |

## kern

kern is the closest match to Zygo's one-shot half today: rootless,
daemonless, one static Rust binary, OCI images, namespaces with a seccomp
allowlist and cgroup v2, a box made and thrown away per call in single-digit
milliseconds by its own figures. If you want `docker run` without the daemon
and without the 300 ms, both projects answer the same question, and kern's
answer is a good one. Measured side by side on a real Python workload, the two
are within 3–5% of each other; [the numbers](#measured-zygo-against-nsjail-and-kern)
are further down.

They part ways on what happens next. kern makes the box cheap enough to throw
away every time, so there is nothing to keep warm. Zygo notes that the
expensive part is not the box but the *interpreter inside it*: a Python
process with its imports done is 150 ms or more that a per-call box pays
again on
every call, whatever the box costs. `zygo serve` pays it once, and
`zygo exec` forks into it for 1.4 ms, with request *n* running on a copy of
the memory the zygote had before request *n−1* existed. That warm path, the
[protocol](../../spec/protocol.md) behind it, and the per-request cgroup,
deadline and secrets that hang off it are Zygo's real subject. The one-shot
runner is the part that had to exist underneath it.

kern also has something Zygo lacks: virtual resource slices (`vcpu:`,
`vdisk:`, `vgpio:`) declared in a config file and attachable to a bare host
process. Zygo has no equivalent and no plans for one.

```text
  kern, per call                         Zygo exec, per call
  ┌──────────────────────────────┐       ┌──────────────────────────────┐
  │ make box (cheap)             │       │ fork warm zygote     ~1.4 ms │
  │ Python start+imports ~150 ms │       │ (Python + imports paid once) │
  │ run, then destroy box        │       │ run, then child exits        │
  └──────────────────────────────┘       └──────────────────────────────┘
```

## nono

nono is not so much a rival as a different layer, and the words overlap, so it
is worth saying so. nono applies Landlock and seccomp — Seatbelt on macOS — to
a process you are starting anyway: your coding agent, running as you, with
your files. There is no image, no namespace, no cgroup and no runtime. What it
gives you is that the agent cannot read `~/.ssh` or reach a host you did not
allow, enforced by the kernel and impossible to undo once applied. It works
natively on a Mac.

That is the right tool for confining an agent that is *meant* to edit your
working folder. It is the wrong one for running code the agent wrote, because
that code still runs as you, in your files, with your environment — a
narrower version of you, but still you. Zygo is the other half: the agent
stays outside, and the code it generates goes into a sandbox with its own
root file system, its own pid namespace, a memory limit and a deadline. The
two fit together, and on a developer's machine using both is reasonable.

```text
  ┌─────────── nono: confines the agent, running as you ───────────┐
  │  coding agent (can edit your folder, cannot read ~/.ssh)       │
  │      │                                                         │
  │      └─ generated code ─▶ zygo run / zygo exec                 │
  └──────────────────────────────┬─────────────────────────────────┘
                                 ▼
             ┌─────── Zygo sandbox: own root, own pids ─────────┐
             │ memory limit · deadline · cannot see your files  │
             └──────────────────────────────────────────────────┘
```

## microsandbox

microsandbox is the closest project to Zygo's `vm` backend, and further along
that road. It is microVM-first: every sandbox is a libkrun guest with its own
kernel — libkrun is the same library behind Zygo's `vm` backend. It supports
OCI images, claims a boot under 100 ms, and can snapshot and branch a live
sandbox, which Zygo cannot do at all. It ships Python, TypeScript and Rust
SDKs and an MCP server, as Zygo does.

The difference is where the default sits. microsandbox's wall is hardware for
everything. Zygo's default is the host kernel, with hardware available as
`--isolation vm` for the work that needs it — the same spec and the same
command. That is a real trade, and it does not go one way. A microVM per
request is a wall a kernel bug does not cross, and Zygo's `ns` backend is one
kernel away from the host, as [chapter 23](23-security.md) says in as many
words. What Zygo has instead is the warm path — 1.4 ms, a fork, clean state —
which a VM per request cannot reach, and which is the only reason the project
exists.

If your code is truly hostile and 100 ms per call is affordable,
microsandbox's default is the safer one. If you run a thousand short calls a
minute from your own users' scripts, Zygo's is the faster one, and
`--isolation vm` is there for the part that is not safe to run on `ns`.
Zygo's `vm` backend is also, today, much less than microsandbox: it boots a
guest and runs one-shot sandboxes, and warm functions and networking inside
the guest are not built; [ADR 0002](adr/0002-warm-paths-stay-on-ns.md) says why.

## gVisor

gVisor is a kernel written in Go that runs in user space. Your program's
syscalls go to it, not to the host, and it answers most of them itself, using
only a small, filtered set of host syscalls. That gives a much smaller attack
surface than the host kernel, without needing KVM or a virtual machine. The
cost is speed on syscall-heavy work, and some programs that need rare kernel
features. Its runtime, `runsc`, is an OCI runtime. Zygo's `gvisor` backend
uses it for one-shot runs: `zygo backend install gvisor`, then
`zygo run --isolation gvisor`. Warm functions stay an `ns` feature, for the
reason given in [chapter 8](08-principles.md#p3-isolation-is-one-flag).

## Firecracker, Cloud Hypervisor, and platforms on them

Firecracker is the microVM monitor behind AWS Lambda and Fargate: a tiny
virtual machine per workload, with a real kernel of its own, booting in about
125 ms or restoring from a memory snapshot faster still. Cloud Hypervisor is
a similar microVM monitor. They give the strongest wall on this page, at a
boot cost per VM, and they need KVM, so they rarely run inside another cloud
VM. Platforms such as E2B build hosted sandboxes for AI agents on top of
Firecracker. Its snapshot restore is the one thing here that resembles Zygo's
fork, one level down: a whole VM restored instead of a process copied.

Zygo's `vm` backend is built on libkrun for the same purpose: anonymous code,
not your own. It boots a guest and runs one-shot sandboxes, with a private
writable layer over the image. Warm functions and guest networking are
refused on it by decision, not by omission; [ADR 0002](adr/0002-warm-paths-stay-on-ns.md)
explains why.

## Kata Containers

Kata Containers runs each container, or each Kubernetes pod, inside a
lightweight virtual machine, while still looking like a normal container to
Kubernetes. It gives you a hardware wall without changing how you deploy. The
price is a VM's start-up time and memory for every pod. It is built for
long-lived services on a cluster, where Zygo is built for short calls on one
machine.

## AWS Lambda and its relatives

Lambda and similar services are managed platforms. Zygo is a local runtime
with a similar shape — a function, a warm instance, a request — and no
platform around it: no billing, no scaling across machines, and no *ingress*
(accepting connections from outside).

## Hosted sandboxes for agents: E2B, Modal, Daytona and the rest

Since 2025 a new group of products sells *a sandbox for an AI agent*: E2B,
Modal Sandboxes, Daytona, Vercel Sandbox, Cloudflare Sandboxes, Blaxel,
Docker's own Sandboxes, and more every quarter. The README names three of
them; this section says where they sit on the map above, because the word
"sandbox" covers two different things.

**What they are.** Each gives an agent a *session*: a machine of its own
with a filesystem, a shell, packages it can install, ports it can expose,
and a lifetime of minutes to hours, billed by the second. The wall is a
microVM (Firecracker at E2B and Vercel, a custom monitor at Docker) or
gVisor (Modal), and the session can often be paused, snapshotted and
resumed. They run in the vendor's cloud; some can be self-hosted, at the
cost of running their control plane. Everything in this paragraph is their
own description, not measured here.

**Where they sit.** On the map they are the top-left cell — a virtual
machine, built for each session — with one addition: the session *stays*.
That is the right shape for an agent that writes code, runs it, reads the
error and tries again for half an hour. It is the wrong shape for what Zygo
is for: a function that runs for milliseconds, thousands of times, and must
start clean each time. A session per call would cost a VM boot, or a
snapshot restore, per call.

```text
  a hosted agent sandbox                 a Zygo warm function
  ──────────────────────                 ────────────────────
  one session, minutes to hours          one call, milliseconds
  state kept between commands            no state between calls
  a VM (or gVisor) per session           a fork per call, on one kernel
  in their cloud, billed per second      on your machine, no billing
  pause, snapshot, resume                nothing to resume: warm again
  ports, a shell, a desktop              no ingress, no shell in the request
```

**Choosing.** If an agent needs a machine to work in — install packages,
run a server, keep files between steps — use one of these, or
microsandbox on your own hardware. If a program needs to run *many small
pieces of untrusted code* — an agent's tool calls, a customer's plugin, a
workflow step — and each must be cheap and clean, that is Zygo. The two
combine: an agent living in a hosted session can still call a Zygo function
for the tool that must answer in a millisecond.

## runc, crun and youki

These are the low-level OCI runtimes: given a folder and a `config.json`,
they do the list at the end of [chapter 4](04-other-locks.md#putting-it-together)
and exec the program. runc is written in Go, crun in C, youki in Rust. They
are the part of Docker and Podman closest to `zygo run`, and on their own they
are fast. But they expect someone else to prepare the bundle, pull images,
keep records and clean up — which is the chain from
[chapter 5](05-docker.md#the-chain-behind-docker-run). Zygo does its own
launching, so it needs none of them.

## Workflow engines: Windmill and friends

Windmill, Temporal and similar workflow engines run users' scripts, and they
need a sandbox for each run. Today that is often nsjail or a container per
job. These engines are exactly the *embedder* Zygo is designed for — the
program that builds Zygo into itself. A worker calls `zygo serve` once per
script, or uses the SDK, and each script run becomes a `fork()` instead of a
container. [`examples/workflow-engine/`](../../examples/workflow-engine) is
such a worker.

## Measured: Zygo against nsjail and kern

nsjail and kern are the two one-shot runners closest to `zygo run`, so both
were measured against it under load, on 24 September 2026. Every run happened
in the same VM: Ubuntu 24.04, kernel 6.8, aarch64, 2 vCPU, 4 GB, on an M1 Max.
Every binary ran from the VM's own disk. The load generator ran outside the VM,
so its CPU counts for nobody. "CPU per job" is the CPU of the whole stack being
measured, divided by the jobs it finished.

### Inside Windmill, in place of nsjail

Windmill CE v1.817 with three general workers ran a trivial Python script and a
CPU-bound one (about 20 ms of Python) through nsjail, using Windmill's own nsjail
config. Then the same workers ran them through Zygo in nsjail's place, and
nothing else changed. Zygo ran two ways:

- **Zygo as `nsjail`**: the `zygo` binary, installed under the name `nsjail`,
  read nsjail's command line and config itself. This translation was built for
  the benchmark only and is **not in Zygo today**.
- **Zygo through a script**: a shell stand-in translated the config and called
  `zygo run`. This is what works with Zygo as it ships.

Per job, 40 runs in a row inside a worker:

| | nsjail | Zygo as `nsjail` |
|---|---|---|
| wall time | 19–20 ms | 20 ms |
| CPU | 18.8–19.0 ms | 18.8–19.3 ms |

Under load, the whole Windmill stack. nsjail was measured twice; its ranges
cover both runs.

| | nsjail | Zygo as `nsjail` | Zygo against nsjail | Zygo through a script |
|---|---|---|---|---|
| burst of 200, trivial: jobs/s | 51.4 | 50.0 | −3% | 43.6 |
| burst of 200, CPU-bound: jobs/s | 38.0–38.3 | 37.0 | −3% | 33.4 |
| CPU per job, trivial burst | 32.8 ms | 33.2 ms | +1% | 38.1 ms |
| CPU per job, CPU-bound burst | 44.7–45.4 ms | 46.2 ms | +2–3% | 50.7 ms |
| 20/s steady: usually / 1 in 100 | 55–57 / 97–102 ms | 56 / 94 ms | level | 62 / 103 ms |
| 40/s steady: usually / 1 in 100 | 57–60 / 91–99 ms | 65 / 112 ms | +8–14% / +13–23% | 87 / 156 ms |
| highest rate sustained | 48.5–49.6/s | about 47/s | −4–6% | about 41/s |
| idle memory of the stack | 384–634 MB | 534–590 MB | level | 531–565 MB |
| failed jobs | 0 | 0 of 2 600 | | 0 |
| per-job cgroup limits | no | memory, processes | | memory, processes |
| seccomp, Landlock | no | yes, yes | | yes, yes |

**Level per job, and 1–6% behind at saturation, while doing more.** Zygo gave
every job a cgroup with memory and process limits, a seccomp allowlist and a
Landlock ruleset, and Windmill's nsjail config sets none of those. The likely
cost at saturation is the cgroup Zygo creates and removes per job:
`lru_gen_online_memcg`, `cgroup_addrm_files` and `tg_set_cfs_bandwidth` show in
`perf`. That was not measured on its own. Below saturation the two cannot be
told apart. Through a shell script, the same swap costs about 15% of
throughput, because the script's `sh`, `awk`, `grep` and `env` add about 4 ms
to every job. Idle memory does not move, because neither sandbox stays resident
between jobs.

Running it found three defects in Zygo, all fixed:

- **Two runs could share a staging directory.** The three workers shared one
  Zygo store, but each had its own PID namespace, and a staging root was named
  after the PID. 2 jobs in 200 failed. Every per-process name now carries the
  PID and 64 random bits.
- **A writable mount of a single file never started.** Landlock was given
  directory rights on a regular file, and the kernel answers that with
  `EINVAL`. nsjail configs hand a job its `result.json` exactly this way. A rule
  on a file is now narrowed to the file rights.
- **The stand-in script itself cost 4 ms a job**, as described above.

### An embedder's harness, against kern

The workload was a real embedder's Python harness: it reads an event, runs a
user's handler, and writes the result through a read-write scratch mount, under
the embedder's limits. `/bin/true` measured the runtime alone. There were 64
runs at each concurrency from 1 to 32, twice. The ranges cover both passes and
every concurrency level. kern `bc822de` ran with `--security-profile untrusted`.

`python:3.12-slim` ships no bytecode. Zygo compiles it once into a layer of its
own, automatically ([chapter 15](15-images-and-dependencies.md#the-python-bytecode-layer)).
kern does not, so it is shown both as it ships and with an image precompiled by
hand.

| | Zygo | kern, precompiled image | kern, stock image |
|---|---|---|---|
| **the harness** | | | |
| runs/s | 60.6–66.8 | 63.2–68.5 | 26.5–28.5 |
| CPU per run | 29.5–32.7 ms | 28.4–31.2 ms | 69.8–75.3 ms |
| time per call, one at a time (usually) | 25.9 ms | 23.4–23.7 ms | 59.1–59.6 ms |
| **`/bin/true`** | | | |
| runs/s | 222–252 | 287–314 | |
| CPU per run | 7.5–9.4 ms | 5.5–6.6 ms | |
| time per call, one at a time (usually) | 7.0–7.7 ms | 4.0–4.2 ms | |
| failures | 0 | 0 | 0 |

**On the real workload Zygo is within 3–5% of kern at its best, and more than
twice as fast as kern as it ships.** On an empty program kern is about 2 ms of
CPU per run cheaper. That is the start-up floor: a 7.9 MB binary against
2.2 MB, a larger plan (≈1.5 ms), and Landlock, which kern does not apply by
default and Zygo keeps.

Before this comparison, Zygo made 42 harness runs a second at 45 ms of CPU each.
The comparison found these, all fixed:

1. **Zygo could not run from a delegated cgroup that also held its caller**, a
   systemd unit with `Delegate=yes` or a service in a container. It exited 125.
   `zygo.slice` now goes to the top of the tree delegated to the user.
2. **Every run re-executed under a transient systemd scope**, about 10 ms of
   CPU, because the delegation check asked about the wrong cgroup.
3. **The host probe ran on every run.** It is now cached per boot for up to ten
   minutes.
4. **Zygo moved its own process into a cgroup on every run**, 1.9–6.0 ms of a
   6–11 ms start. It now moves only when it is in the way.
5. **The sandbox child was moved into its cgroup after `clone3`**, which waits
   out an RCU grace period after a quiet spell. It is now born there with
   `CLONE_INTO_CGROUP`, the order kern uses, which also puts the limits on from
   the child's first instruction. The sandbox start went from 6–11 ms to 3–4 ms.
6. **The exit wait slept** with a backoff, so a program that exited at 13 ms was
   noticed at 25 ms. It now uses `pidfd_open` and `poll`.
7. **Every run built a registry client**, with TLS roots and a multi-threaded
   runtime, to read one local file.
8. **A failed bytecode build was retried on every run**, at 170–200 ms each. It
   is now remembered for an hour.

A run now starts 4 processes, down from 13 at the worst.

### What these numbers are not

- **They are all cold.** Every job started a fresh interpreter. Zygo's warm
  path, a fork into a zygote that has already imported everything, was not part
  of either comparison. Neither nsjail nor kern has one to compare it with.
- **One VM, one kernel.** On kernel 5.10, in Docker Desktop's VM, the same
  sandbox made Python about twice as slow as a plain container did. That cost
  belongs to the old kernel, and it would have been measured against every
  namespace-based runner alike.
- **The raw results and the load generator were not kept.** The tables here
  are their summary, and they cannot be repeated from this repository. The
  numbers in [chapter 25](25-performance.md) can: `make bench-record` writes
  them as JSON, and [`bench/`](../../bench) holds the records so far.

## Everything in one table

| | Wall | Images | Limits | Warm fork | Root needed | Main use |
|---|---|---|---|---|---|---|
| **Zygo** | host kernel · gVisor · VM | OCI | cgroups, mandatory, per request | **yes** | no | short functions, others' code |
| Docker | host kernel | OCI | cgroups, opt-in | no | daemon (or rootless mode) | services, packaging |
| Podman | host kernel | OCI | cgroups, opt-in | no | no | services, without a daemon |
| nsjail | host kernel | no (a folder) | cgroups, rlimits | no | depends on features | CTFs, job runners |
| bubblewrap | host kernel | no (bind mounts) | none | no | no (or setuid) | desktop apps, a building block |
| firejail | host kernel | no | some | no | setuid root | desktop apps |
| minijail | host kernel | no | some | no | usually | OS services |
| kern | host kernel | OCI | cgroups | no | no | fast throwaway boxes |
| nono | host kernel (Landlock) | no | no | n/a | no | confining an agent |
| gVisor | second kernel | OCI | cgroups | no | no, in rootless mode (how Zygo runs it); then its cgroup limits are advisory | safer containers |
| microsandbox | VM | OCI | the VM's | no (snapshots) | no | hostile code |
| Firecracker | VM | no (a disk image) | the VM's | no (snapshots) | KVM access | serverless platforms |
| E2B, Modal, Daytona, Docker Sandboxes | VM or gVisor, per session | OCI or their templates | the VM's | no (snapshots) | their cloud (or self-host) | an agent's working machine |
| Kata | VM | OCI | the VM's | no | yes | safer Kubernetes pods |
| FreeBSD jail | host kernel | no (a folder) | rctl, opt-in | no | yes | long-lived services on FreeBSD |

## What Zygo does not do

- **Run on macOS or Windows natively.** Sandboxes are Linux; on a Mac, Zygo
  manages a Linux VM for you.
- **Provide ingress.** No mode accepts connections; a function is called
  through the CLI, the SDKs or Zygo's own HTTP API.
- **Scale past one machine.** Capacity is a per-host budget, and requests
  past it get HTTP `429` (too many requests).
- **Hide the kernel.** The `ns` backend is one kernel, and every chapter of
  this book says so.

## Choosing, in short

For long-lived services, use Docker, Podman or Kubernetes. For hostile code
where 100 ms or more per call is fine, use a VM wall: Firecracker, Kata,
microsandbox, or Zygo's `vm` backend. For confining a tool you already run,
look at nono or bubblewrap. For a quick throwaway sandbox around one command,
nsjail, kern and `zygo run` all do well. For many short calls to code other
people wrote, where each call must start clean and costs must stay in
milliseconds, that is the right-hand column of the map, and that is Zygo.

In one sentence: `docker run` asks a root daemon to create a container object
from an image; `zygo run` runs a program as a locked-down process under your
own user — the same kernel parts, the opposite defaults, no chain in between,
and nothing left behind.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [9. FreeBSD jails, and Zygo](09-jails.md) · [Contents](README.md) · **Next: [11. Getting started](11-getting-started.md) →**
