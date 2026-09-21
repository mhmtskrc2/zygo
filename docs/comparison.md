# Comparison

Where Zygo sits, and where it does not. Numbers for Zygo are the project's
own measurements — see [what Zygo costs](performance.md) for the hosts and
the commands; numbers for the others are their documented or commonly
measured figures and are marked as such.

## The request path

| | Docker `run` | Docker `exec` | Firecracker | gVisor `runsc` | AWS Lambda (warm) | **`zygo run`** | **`zygo exec` (warm)** |
|---|---|---|---|---|---|---|---|
| Per-request overhead | 300–1000 ms | 50–100 ms | ~125 ms boot; 10–20 ms from a snapshot | 50–150 ms | ~1–5 ms + platform | **18 ms p50** | **1.7 ms p50, 2.8 ms p99** |
| Paid once, up front | — | a `docker run -d` | the VM's own boot, or a snapshot | — | a cold start, platform-side | — | a `zygo serve`: ~270 ms for a Python handler, plus its imports |
| Clean state per request | yes | no | yes | yes | no | **yes** | **yes** (a fresh process) |
| Daemon | yes | yes | yes (a VMM per VM) | yes (`runsc` + shim) | n/a | **no** | **no** |
| Root | daemon runs as root | same | needs `/dev/kvm` | no | n/a | **no** | **no** |
| Boundary | kernel | kernel | hardware | userspace kernel | hardware | kernel (`ns`); userspace kernel (`gvisor`) | kernel (`ns`); hardware (`vm`, one-shot only) |

The first row is the whole argument. A container's cost is orchestration and
cold interpreter start; Zygo takes the orchestration off the request path
entirely and amortises the interpreter with a warm zygote.

## `docker run` and `zygo run`, side by side

They accept the same sentence — `IMAGE [COMMAND…]` — and both use OCI images
from the same registries. The resemblance ends there.

### Who runs the command

`docker run` is a client. The request crosses a unix socket to `dockerd`, a
root daemon, which hands it to `containerd`, which starts a `containerd-shim`,
which runs `runc`, which sets the container up and exits; the shim stays for
as long as the container lives. Five processes, three RPC boundaries, all of
them root. Nearly all of the 300–1000 ms is that chain; the isolation itself —
the namespaces, the cgroup, the seccomp filter — is about a millisecond of it.

`zygo run` is one process, no RPC, no root. `zygo` itself calls `clone3` into
a fresh set of namespaces; the child applies the mount plan, does
`pivot_root`, joins its cgroup, drops every capability, installs seccomp and
Landlock, and calls `execve`. With the image cached that is a median of 18 ms.
`ps` shows `zygo` with your program directly under it, and nothing in between.

### What is left behind

`docker run` creates an *object*. When the process exits the container stays
(`docker ps -a`), its writable layer stays on disk, and something has to
`docker rm` it or have passed `--rm`. Logs accumulate in the daemon, there are
restart policies, and `docker exec` gets you back inside.

`zygo run` is a *process*. When it exits the namespaces, the cgroup and the
tmpfs go with it; there is nothing to remove and no `zygo rm`. Standard input,
output and the exit code pass straight through. The one addition is that a
sandbox Zygo or the kernel killed exits 137, and `--outcome file.json` says
which it was: `timed_out`, `oom_killed`, `peak_rss_kb`, `wall_ms`. `--dry-run
--json` prints the mount plan and the cgroup values without running anything;
Docker has no equivalent.

The container you keep around and step into is a separate idea in Zygo:
`zygo serve` and `zygo exec`, a warm function. It resembles `docker exec`
except that every request is a fresh process and costs about 2 ms.

### The defaults

Docker's defaults are for software you chose; Zygo's are for code you did not.
With no flags at all:

| | `docker run IMAGE` | `zygo run IMAGE` |
|---|---|---|
| Root filesystem | writable (a copy-on-write upper layer) | **read-only**; the only writable place is `/tmp`, a tmpfs sized by `scratch` (64M) |
| Capabilities | 14 kept (`NET_RAW`, `SYS_CHROOT`, `MKNOD`, …) | **none** |
| seccomp | a denylist-shaped profile allowing ~350 syscalls | **an allowlist of ~190**; `bpf`, `io_uring`, `userfaultfd`, `ptrace`, `mount`, `unshare` absent; `clone` refused with any namespace flag |
| Landlock | no | yes, where the kernel has it |
| Runs as | root in the container, root on the host (outside rootless mode) | the image's uid 1000, mapped to **your** uid |
| Network | bridge; everything reachable | **`none`**: loopback only |
| Memory, CPU, pids, wall clock | **unlimited** | **all mandatory**: `mem` 256M, `cpu` 0.5, `pids` 64, `timeout` 30s, `nofile` 1024 |
| Bind mounts | `-v` is **rw** unless `:ro` | `--mount` is **ro** unless `:rw` |
| cgroupfs inside | mounted read-only | not mounted at all, so `release_agent` is not reachable |
| Escape suite | — | 16 vectors attempted on every change, 0 escaping |

In Docker you *add* security: `--cap-drop ALL --read-only --security-opt
no-new-privileges --memory --pids-limit --network none`. In Zygo you *loosen*
it, and every flag that does says so in its name — `--allow-unlimited`,
`--allow-host-net`, `--allow-private-net`. There is no way to turn a limit off
without one.

The comparison is not "Zygo is safer than Docker". Docker's defaults suit
running software you chose; Zygo's suit running code you did not. The point
is that the defaults differ in the direction the use case demands, and that
every row in the Zygo column is something a test attempts rather than a
setting a test reads.

### Networking

Docker's network is built for a container that *is* a service: a bridge with
NAT, `-p 8080:80` to publish a port, containers that find each other by name,
`--network host` when you want none of it.

Zygo has four modes and none of them accepts a connection. `none` is the
default. `egress` is an allowlist **by name** — `allow = ["api.example.com:443",
"*.cdn.example.com:443"]` — enforced by nftables inside the sandbox's own
namespace, with a resolver of Zygo's own: a name off the list does not resolve,
and one on it has its addresses admitted to the filter before the answer goes
back. `full` is the public internet; `host` is no namespace and needs
`--allow-host-net`. Private ranges and `169.254.169.254` stay refused in every
namespaced mode. If `pasta` or `nft` is missing a networked sandbox does not
start, rather than starting unconfined. There is no port publishing; Zygo does
not run services.

### Images and dependencies

Both pull the same images from the same registries, and `zygo run` pulls on
first use exactly as `docker run` does. The stores differ: Docker's is
`/var/lib/docker` and root's; Zygo's is under `~/.local/share/zygo`,
content-addressed, layers unpacked, with rootless overlayfs on kernels 5.11 and
newer and a flattened copy below that. `zygo login` stores a credential for a
private registry, and an existing `~/.docker/config.json` is read too.

The real difference is the Dockerfile. In Docker, adding a dependency means
building an image. In Zygo the image is never touched: `--requirements
./requirements.txt` builds a venv inside a sandbox with the image's own `pip`
and mounts it at `/venv`; `system = ["libwebp7"]` installs apt packages into a
derived OCI layer. Both are keyed on the image digest and the list, built once,
and shared by everything that names the same thing.

### The flags you already know

| Docker | Zygo | Note |
|---|---|---|
| `-v ./x:/x` | `--mount ./x:/x:rw` | read-only is Zygo's default; a single file can be mounted too |
| `-e K=V` | `--env K=V` | secrets are not environment: they arrive as `/run/secrets/<NAME>` |
| `-w /dir` | `--workdir /dir` | default `/app`, falling back to `/` |
| `-u 1000` | `--user 1000` | |
| `-it` | `--tty` | |
| `--memory 256m --cpus 0.5 --pids-limit 64` | `--mem 256M --cpu 0.5 --pids 64` | present in Zygo whether you pass them or not |
| `timeout 30 docker run …` | `--timeout 30s` | the whole process tree is killed, through the cgroup |
| `--network none` | (the default) | `--net egress --allow host:port` for an allowlist |
| `--security-opt seccomp=…` | `--seccomp default\|strict\|permissive` | three shipped profiles; see [seccomp profiles](seccomp-profiles.md) |
| `--runtime runsc` | `--isolation gvisor` | same spec, same command; `vm` is another value of the same flag |
| `--rm` | (always) | |
| `-d`, `-p`, `--restart` | — | Zygo does not run services |

## What each is for

* **Docker** is a general-purpose packaging and orchestration system. Zygo
  uses its images and none of its runtime; if you need `docker build`,
  `docker compose`, long-running services, port publishing, networks between
  containers or restart policies, you need Docker.
* **Firecracker / Cloud Hypervisor** are microVMs: the strongest boundary,
  at a boot cost per VM. Zygo's `vm` backend is built on libkrun for the same
  purpose — anonymous code, not your own. It boots a guest and runs one-shot
  sandboxes today; warm functions, networking and writable scratch inside the
  guest are not built.
* **gVisor** is a userspace kernel: a smaller attack surface than the host
  kernel without KVM, at a syscall cost. Zygo's `gvisor` backend is built for
  one-shot runs (`zygo backend install gvisor`, then
  `zygo run --isolation gvisor`); warm functions still need `ns`.
* **Lambda and its relatives** are managed platforms. Zygo is a local runtime
  with a similar shape — a function, a warm instance, a request — and no
  platform: no billing, no scaling across machines, no ingress.
* **Windmill, Temporal, and other workflow platforms** run user scripts and
  are exactly the embedder Zygo is designed for: a worker calls `zygo serve`
  or the SDK, and each script run is a `fork()` instead of a container.

## What Zygo does not do

* Run on macOS or Windows natively. Sandboxes are Linux; on a Mac, Zygo
  manages a Linux VM for you.
* Provide ingress. No mode accepts connections; a function is called through
  the CLI, the SDKs or Zygo's own HTTP API.
* Scale past one machine. Capacity is a per-host budget and `429` past it.
* Hide the kernel. The `ns` backend is one kernel, and everything here says so.

In one sentence: `docker run` asks a root daemon to create a container object
from an image; `zygo run` runs a program as a constrained process under your
own user — the same kernel primitives, the opposite defaults, no chain in
between, and nothing left behind.
