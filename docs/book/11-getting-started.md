# 11. Getting started

This chapter takes you from nothing to three warm functions behind an HTTP
API. You install Zygo, ask it whether your machine can run sandboxes, run one
program in a sandbox, and then keep a function warm and call it. Each step
is short, and each one links to the chapter that explains it in full.

## The road through this chapter

```text
  install ──▶ zygo doctor ──▶ zygo run ──▶ zygo serve ──▶ sandbox.toml ──▶ zygo api
  (a file)    (can this       (one        (one warm      (three          (call them
               host do it?)    sandbox)    function)      functions)       over HTTP)
```

You can stop after any step. A CI job that only needs `zygo run` never has
to learn about warm functions, and a Mac user can do all of it without
setting up Linux by hand.

## What you need

Zygo is one *static binary*: a single file that carries everything it needs,
with no libraries or runtimes to install beside it. It builds sandboxes out
of Linux kernel features, so the sandboxes themselves always run on Linux.

| Your machine | What happens |
|---|---|
| Linux, kernel 5.3 or newer | Zygo runs directly. Nothing needs root. |
| macOS | Zygo starts a small Linux virtual machine for you and runs inside it. |
| A dev container or a Codespace | The repository's `.devcontainer` sets one up with sandboxes working; [see below](#in-a-dev-container-or-a-codespace). |
| Anything else | Run Zygo in a Linux VM or container; either is fine. |

On Linux you also need *unprivileged user namespaces* (a normal user may
make the private views from [chapter 2](02-namespaces.md)) and *cgroup v2
delegation* (a normal user may own part of the cgroup tree from
[chapter 3](03-cgroups.md#delegation-cgroups-without-root)). Most modern
distributions have the first; the second often needs one small fix, which
`zygo doctor` can apply for you.

## Installing on Linux

Release builds exist for x86_64 and aarch64 (64-bit ARM, such as a Raspberry
Pi 5 or a Graviton server). Both are static *musl* builds: musl is a small C
library that is linked into the binary, so it does not depend on the
host's own libraries.

```bash
url=https://github.com/mhmtskrc2/zygo/releases/latest/download
curl -fsSLO "$url/zygo-$(uname -m)-unknown-linux-musl.tar.gz"
curl -fsSL "$url/SHA256SUMS" | sha256sum -c --ignore-missing   # prints "OK"
tar xzf zygo-*-unknown-linux-musl.tar.gz
sudo install -m 0755 zygo-*/zygo /usr/local/bin/zygo
```

`uname -m` prints `x86_64` or `aarch64`, which picks the right file. The
second line checks the archive against the `SHA256SUMS` file that every
release carries, so you know it is the one that was published. The `sudo` is
only for copying the file into `/usr/local/bin`; Zygo itself never runs as
root.

`SHA256SUMS` is itself signed, in every release after 0.1.1. With
[cosign](https://docs.sigstore.dev/) installed, this proves the checksums came
from this project's release workflow, not only from the same download page:

```bash
curl -fsSLO "$url/SHA256SUMS"
curl -fsSLO "$url/SHA256SUMS.sigstore.json"
cosign verify-blob SHA256SUMS --bundle SHA256SUMS.sigstore.json \
  --certificate-identity-regexp '^https://github\.com/.*/\.github/workflows/release\.yml@refs/tags/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Each release also carries an SBOM — a list of every library compiled into the
binary, with its version — as `zygo-<version>.cdx.json`, in the CycloneDX
format that security scanners read.

## Installing on macOS

On a Mac, Homebrew installs three things: a small `zygo` *shim* for macOS (a
thin program that passes your commands on), the Linux build of Zygo that the
shim forwards into, and Lima, the tool that starts the Linux VM.

```bash
brew install mhmtskrc2/zygo/zygo
```

The formula lives in a *tap* (a small GitHub repository of Homebrew formulas),
`mhmtskrc2/homebrew-zygo`; Homebrew finds it from the name. The release build
writes the formula there, with checksums computed from the same archives, not
typed in by hand. The section
[How Zygo runs on a Mac](#how-zygo-runs-on-a-mac) below explains what the VM
is and what it costs.

## Other ways to install

```bash
# from crates.io, anywhere with a Rust toolchain
cargo install zygo-cli

# from a checkout of the repository
cargo build --release
sudo install -m 0755 target/release/zygo /usr/local/bin/zygo
```

`cargo install zygo-cli` builds the default set of features. The `vm`
backend is not in it. That backend links a virtual machine monitor from a
pinned git tag, and a crate published on crates.io may not point at a git
tag. So `make vm-build` from a checkout is the way to get it, and
`zygo doctor` tells you this on a host where `vm` would otherwise work.
[Chapter 16](16-production.md#choosing-an-isolation-backend) explains the
backends.

## The container image

Zygo also ships as a container image, `ghcr.io/mhmtskrc2/zygo`. It is Alpine
Linux plus the static `zygo` binary and the few helpers networking needs.
It does not need `--privileged`, but it does need a few specific things from
Docker, because Zygo builds sandboxes inside it:

```bash
docker run --user 0:0 --security-opt seccomp=unconfined \
    --security-opt systempaths=unconfined --security-opt apparmor=unconfined \
    --cgroupns=host --cgroup-parent=/zygo -v /sys/fs/cgroup/zygo:/sys/fs/cgroup/zygo:rw \
    -p 7700:7700 -e ZYGO_API_TOKEN=... ghcr.io/mhmtskrc2/zygo
```

| Option | Why Zygo needs it |
|---|---|
| `seccomp=unconfined` | Docker's own seccomp filter refuses to create a user namespace. |
| `systempaths=unconfined` | Docker hides parts of `/proc`, and then the kernel refuses a fresh `/proc` inside a user namespace. |
| `--cgroupns=host --cgroup-parent=/zygo` and the `/sys/fs/cgroup/zygo` mount | A sandbox needs a cgroup to live in. This gives the container one subtree of its own, not the whole host tree. |
| `--user 0:0` | The cgroup folder belongs to root. No sandbox runs as that root: each has a user namespace of its own. |
| `apparmor=unconfined` | Only on a host with AppArmor, such as Ubuntu, whose default Docker profile refuses the mounts a sandbox makes. Harmless elsewhere. |

Sandboxes with a network need `--device /dev/net/tun` as well, and a volume
keeps pulled images across restarts; [chapter 16](16-production.md#running-zygo-inside-a-container)
has the full command. `zygo doctor` names anything that is missing, inside
the container just as on a host. [`packaging/oci/`](../../packaging/oci) has the details,
and [chapter 16](16-production.md) covers running it in production.

## In a dev container or a Codespace

The repository has a `.devcontainer` for VS Code's *Reopen in Container* and
for GitHub Codespaces. It builds Zygo, installs the tools the tests and
networked sandboxes need, and runs `zygo doctor`. Sandboxes work inside it:
one-shot runs, warm functions and `egress` networking were checked in it.

It needs two things a default container does not give. The container is
started `--privileged`, so Zygo may create user namespaces and mount inside
them. And a small script arranges the container's own cgroup tree at every
start, because cgroup v2 hands controllers only to a cgroup with no
processes in it. `zygo` in that container is a wrapper that starts the real
binary in the empty cgroup the script left for it.

```text
  /sys/fs/cgroup   (the container's own root: memory, pids, cpu for children)
  ├── init/        every process the container started, and each new shell
  └── launch/      empty until zygo starts in it
      └── zygo.slice/ …  the sandboxes, as on a host
```

What does not work there: the `vm` backend (no `/dev/kvm`), and on a host
whose kernel is older than 5.13, Landlock; `zygo doctor` names both. It is a
development machine, not a production shape — [chapter 16](16-production.md#running-zygo-inside-a-container)
covers running Zygo in a container for real, without `--privileged`.

## How Zygo runs on a Mac

macOS has no namespaces or cgroups, so a Mac cannot build a Linux sandbox
itself. Zygo solves this by keeping a small Linux *virtual machine* (VM): a
whole computer simulated in software, with its own Linux kernel. You never
log into it. The `zygo` command on your Mac passes every sandbox command to a
Linux `zygo` inside the VM, with the same arguments, the same working folder
and the same input and output. The exit status comes back out to your shell.

```text
  your Mac                                     Lima VM "zygo" (Ubuntu 24.04)
  ┌───────────────────────────────┐            ┌─────────────────────────────────┐
  │ $ zygo run python:3.12 ...    │   SSH      │ zygo run python:3.12 ...        │
  │   zygo (macOS shim) ──────────┼───────────▶│   └─▶ sandbox (namespaces,      │
  │                               │  ~22 ms    │        cgroup, seccomp ...)     │
  │ output + exit status ◀────────┼────────────┼── output + exit status          │
  │                               │            │                                 │
  │ /Users/you/project  ◀─────────┼── same ────┼─▶ /Users/you/project            │
  │                               │   path     │   (your home, mounted inside)   │
  └───────────────────────────────┘            └─────────────────────────────────┘
```

## The Mac VM in detail

| | |
|---|---|
| Name | `zygo`, managed by Lima |
| System | Ubuntu 24.04 |
| Size | 2 CPUs, 4 GiB of memory, 20 GiB of disk |
| First start | about a minute: the VM is created on the first command that needs it |
| Later starts | about 16 seconds, after the VM was stopped |
| Cost per command, once it is up | about 22 ms, over the SSH connection Lima already holds |
| Stopping it | `zygo stop --all` stops everything, the VM included |

Your home folder is mounted inside the VM at *the same path*, and it is
writable. So `./handler.py` is one file, seen from two sides. That is also the
limit, and Zygo enforces it: every host path must be under `$HOME`. A command
run from outside your home folder is refused, and the message names both
folders instead of quietly running somewhere else.

A few commands never need Linux, so they run on the Mac itself:
`zygo completion`, `zygo doctor`, `zygo agent test` and `zygo api --openapi`.
If the VM cannot be reached, a command exits with status **111**;
[chapter 22](22-troubleshooting.md) says what to do.

## What the Mac VM costs

A one-shot `zygo run` typed in a Mac shell takes about 29 ms end to end, when
a supervisor is running in the VM. About 6 ms of that is the sandbox, and
about 22 ms is the trip into the VM. These are medians of nine runs on the Mac
that [chapter 25](25-performance.md) names. The same run through Docker
Desktop on the same Mac took 397 ms.

The trip is paid per *command*, not per request. The millisecond warm path is
still there when you call functions through the HTTP API or the SDKs,
because then the calls happen inside the VM and the connection pays the trip
once. `zygo api` running inside the VM is the answer for anything that must
be fast on a Mac.

## The Mac VM from a checkout

Without Homebrew you need the same two parts: Lima, and a Linux build of Zygo
to put inside the VM.

```bash
brew install lima            # what starts the VM
make guest-build             # the Linux build that runs inside it, compiled in the VM
```

`make guest-build` needs nothing but the VM. It installs a Rust toolchain
inside the VM on first use. On a Mac that already has Docker,
`make poc/zygo-linux-musl` builds the same binary in a Docker container
instead.

## Checking the host: `zygo doctor`

`zygo doctor` is the first thing to run on any new machine. It checks
everything a sandbox needs, prints one line per check, and prints the fix for
anything that is missing. It *tries* each thing rather than reading a setting.
For example, it really builds a user namespace and mounts inside it. So a
kernel that has a feature switched on but ignores it is caught here, not later
inside a sandbox.

```bash
zygo doctor           # can this host run sandboxes?
zygo doctor --json    # the same report, for a script or a health check
```

On a Mac, `doctor` checks both sides: the Mac's side (can it reach the VM?)
and the VM's kernel. You get one report with one verdict.

## What `doctor` checks

| Check | What it asks |
|---|---|
| `kernel` | Is the kernel 5.3 or newer? Below 6.1 it says `degraded`. |
| `kernel age` | How old is this kernel series? An old one is behind on hardening, and one out of upstream support may lack security fixes. |
| `user namespaces` | Can a normal user create one *and mount inside it*? |
| `procfs (fully visible)` | Can a fresh `/proc` be mounted inside a sandbox? |
| `cgroup v2` | Is the unified cgroup tree there, with controllers delegated to you? |
| `cgroup moves` | On Linux 6.0+, is cgroup2 mounted with `favordynmods`? Without it, about 1 warm request in 100 waits several ms ([chapter 22](22-troubleshooting.md#1-request-in-100-takes-10-ms-and-the-rest-take-15)). |
| `overlayfs (userns)` | Can image layers be stacked inside a user namespace? |
| `landlock` | Which Landlock version (ABI) does the kernel offer? |
| `seccomp` | Can a seccomp filter be installed? |
| `subuid/subgid` | Does your user own a range of extra user ids to map? |
| `kvm` | Is `/dev/kvm` there, for the `vm` backend? |
| `guest kernel` | Is a kernel for the `vm` backend's guest available? |
| `runsc` | Is gVisor installed, for the `gvisor` backend? |
| `egress (pasta + nft + tc)` | Are the programs that give a sandbox a filtered network present? |

The last lines of the report list the *backends* this host can use right
now: the host has what each one needs *and* this build of Zygo includes it.

## Reading the report

Each line ends in one of four words. The exit status of `zygo doctor` is 0
exactly when no line says `FAIL`, and the JSON field `ok` says the same thing.

| Status | Meaning |
|---|---|
| `ok` | Present and working. |
| `degraded` | Works, but something is missing or old, and a feature is weaker or slower. |
| `absent` (shown as `-`) | An optional part is not there, such as `kvm`. Nothing is broken. |
| `FAIL` | Sandboxes cannot run until this is fixed. The fix is printed under the line. |

`network = "none"`, the default, needs no networking tools at all. So a
missing `pasta` is `absent`, not `FAIL`, and only matters when you turn a
sandbox's network on.

## Which kernel version gives what

Zygo needs kernel **5.3 or newer**. Later kernels unlock extra features rather
than block Zygo. **6.1 or newer is recommended**, because only there are
Landlock's network rules, `cgroup.kill` and `memory.peak` all present.

```text
  5.3 ─────────── 5.11 ─────────── 5.13 ───────────────────── 6.1 ─────────▶
  minimum         overlayfs in a    Landlock                  recommended:
  to run          user namespace    (file access rules)       Landlock network rules,
                  (below: layers                              cgroup.kill, memory.peak
                  are flattened,
                  more disk, slower
                  first run)
```

## Letting `doctor` fix things: `--fix`

`zygo doctor --fix` applies the fixes it knows. It first prints a plan: each
change, why it is needed, what it costs, and the exact commands. Then it asks
`apply these? [y/N]`. Nothing happens unless you type `y`. In a script, where
nobody can answer, add `--yes`.

```bash
zygo doctor --fix           # show the plan, ask, then apply
zygo doctor --fix --yes     # the same, without asking
```

| What it can fix | What it does | Needs root |
|---|---|---|
| The AppArmor block on user namespaces | Sets `kernel.apparmor_restrict_unprivileged_userns=0` now and in a file under `/etc/sysctl.d/`, so it stays after a reboot. **Machine-wide.** | yes, through `sudo` |
| No cgroup delegation | Writes `~/.config/systemd/user/user@.service.d/delegate.conf` and reloads your systemd user manager | no |
| Missing `pasta` or `nft` | Installs the `passt` and `nftables` packages with apt, dnf, pacman or apk | yes, through `sudo` |
| The AppArmor profile that blocks `pasta` | Runs `aa-complain` on the `pasta` profile, so it logs instead of blocks | yes, through `sudo` |
| Slow cgroup moves (`cgroup moves … degraded`) | Remounts cgroup2 with `favordynmods` now, and installs `zygo-cgroup-favordynmods.service` to do it at every boot. Every fork and exit gets slightly slower. **Machine-wide.** Not offered inside a container | yes, through `sudo` |

## Ubuntu and Debian: two AppArmor rules

AppArmor is a Linux security module that limits what programs may do. Ubuntu
and Debian ship two AppArmor rules that get in Zygo's way. Both come from the
distribution, not from Zygo.

The first is `kernel.apparmor_restrict_unprivileged_userns=1`. It lets a
normal process create a user namespace, and then refuses the first mount
inside it. That mount is the first thing every sandbox does. `zygo doctor`
finds this by trying the mount, and prints the one-line fix. Read
[chapter 23](23-security.md) first: the fix turns off a protection for
*every* process on the machine, not only for Zygo.

The second is an AppArmor profile for `pasta`, the program Zygo uses to give
a sandbox a network. Where that profile is enforced, it keeps `pasta` out of
the sandbox's user namespace. Then `network = "egress"` and `"full"` cannot
start, even though `/dev/net/tun` works. The error says so and names
`aa-complain`. The default, `network = "none"`, does not use `pasta` at all.

## cgroup delegation, the usual gap

On a machine with systemd, delegation is the most common missing piece, and it
has two halves. First, your *user manager* (the systemd process that looks
after your login) has to pass the controllers down to you. This is the fix
`doctor --fix` writes, and you can also do it by hand:

```bash
mkdir -p ~/.config/systemd/user/user@.service.d
printf '[Service]\nDelegate=cpu cpuset io memory pids\n' \
    > ~/.config/systemd/user/user@.service.d/delegate.conf
systemctl --user daemon-reexec
```

Second, the process that runs Zygo has to *sit in* a delegated cgroup, and a
login shell over SSH does not. **Zygo handles this half itself**, the way
Podman does. A command that builds a sandbox restarts itself inside a
temporary systemd *scope* of its own, which is a delegated cgroup made for
one command. [Chapter 12](12-one-shot-sandboxes.md#the-systemd-scope) has
the details.

```text
  your SSH login shell's cgroup      (not delegated: cannot hold a sandbox)
     │
     └─ zygo run ...  ──re-exec──▶  systemd-run --user --scope -p Delegate=yes
                                        └─ zygo run ...   (delegated: builds the sandbox)
```

One result surprises people. `zygo doctor` run from a plain SSH session can
report `cgroup v2 … FAIL`, because that is true of the cgroup it stands in.
`zygo run` from the same shell still works, because it moves into a scope
first. [Chapter 22](22-troubleshooting.md) has the rest.

## Your first one-shot sandbox

A *one-shot* sandbox is built for one program and thrown away when the
program ends. This is `zygo run`, and it reads like `docker run`:

```bash
zygo run python:3.12-slim python3 -c 'print("hello")'
```

The first run downloads the image, just as `docker run` does. After that, a
run takes about 12 ms (median, with the image already pulled, measured on the
hosts in [chapter 25](25-performance.md)). The sandbox has a read-only root,
no network, no capabilities, and memory, CPU and process limits. A few more
to try:

```bash
zygo run --mem 128M --pids 16 --timeout 10s alpine:3 /bin/sh   # tighter limits
zygo run --tty alpine:3 /bin/sh                                # with a terminal of its own
zygo run --dry-run --json python:3.12-slim                     # the plan, without running it
```

[Chapter 12](12-one-shot-sandboxes.md) covers `zygo run` in full: mounts,
environment, exit codes and more.

## Your first warm function

A *warm function* is a sandbox that is built once and kept ready, so each
request skips the start-up. You write a *handler*: a function that takes one
event and returns a result.

```bash
mkdir demo && cd demo
cat > handler.py <<'EOF'
def handler(event):
    return {"doubled": event.get("n", 0) * 2}
EOF

zygo serve handler.py --name double      # pulls python:3.12-slim, then warms it
zygo exec double '{"n": 21}'             # {"doubled": 42}
zygo ps                                  # what is running
```

`serve` starts a *supervisor* in your session if none is running: the
background process that keeps warm functions alive. Then it warms a *zygote*:
the Python interpreter with `handler.py` already imported, waiting. Every
`exec` is a `fork()` of it, which means a copy made in a moment. So there is
no import cost per request and no state left over between requests.
[Chapter 6](06-how-zygo-works.md#the-warm-path) explains the idea.

## What the warm function costs

```text
  zygo serve                         zygo exec          zygo exec          zygo exec
  ├─ build sandbox                   ├─ fork            ├─ fork            ├─ fork
  ├─ start Python, import handler    └─ reply           └─ reply           └─ reply
  └─ park the zygote                   ~1.4 ms            ~1.4 ms            ~1.4 ms
     ~150 ms, paid once
```

On a Raspberry Pi 5, a Python handler that imports nothing is warm in about
150 ms, including starting the supervisor. The median overhead per request is
about 1.4 ms, measured on a Lima VM on an Apple M1 Max. Both numbers
and their machines are in [chapter 25](25-performance.md). The same trick
works from a one-line file:

```bash
echo 'def handler(event): return {"got": event}' > handler.py
zygo serve ./handler.py --name echo
zygo exec echo '{"n": 1}'
zygo bench all            # every published number, measured again on this host
```

## A project with three functions

A real project describes its functions in a file called `sandbox.toml`, so
you do not repeat flags. Here are three functions that each show a different
shape: a Python function with system packages, a Go binary, and a function
that may call one outside API.

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
image  = "alpine:3"                     # no runtime → warm-exec
mounts = ["./bin/parse:/app/parse:ro"]  # a static Go binary
cmd    = ["/app/parse"]

[fn.fetch]
entry   = "./fetch.py"
network = "egress"
allow   = ["api.example.com:443", "*.cdn.example.com:443"]
secrets = ["API_KEY"]
```

`[defaults]` applies to every function; each `[fn.NAME]` table adds to it or
overrides it. [Chapter 20](20-sandbox-toml.md) lists every field.

## What each function in the file does

| Function | Shape | What is special |
|---|---|---|
| `resize` | Python handler (`entry`) | `requirements` becomes a Python *venv* (a folder of installed packages), built once inside the image. `system` installs apt packages once, as an extra image layer. It gets 512 MB instead of the default 256 MB. |
| `parse` | *warm-exec* (`cmd`) | A Go binary starts fast, so only the sandbox is kept warm and each request runs the program fresh. It uses `alpine:3` because it needs no Python. |
| `fetch` | Python handler with network | `network = "egress"` opens only the names and ports in `allow`. `API_KEY` is read from your shell and given to each request as a file, never as an environment variable. |

```text
  sandbox.toml
     │
     ├─ [fn.resize] ──▶ python:3.12-slim + libwebp7 layer + /venv ──▶ zygote: fork per request
     ├─ [fn.parse]  ──▶ alpine:3 + your Go binary, mounted ─────────▶ sandbox: exec per request
     └─ [fn.fetch]  ──▶ python:3.12-slim, network: only the allow list,
                        /run/secrets/API_KEY per request ───────────▶ zygote: fork per request
```

[Chapter 13](13-warm-functions.md) covers the shapes,
[chapter 14](14-limits-network-secrets.md) the network and secrets, and
[chapter 15](15-images-and-dependencies.md) the venv and the apt layer.

## Bringing the project up

```bash
export API_KEY=…                       # read from this shell, delivered as a file
zygo up                                # every [fn.*] warm
zygo exec fetch '{"path": "/v1/ping"}'
zygo logs fetch -f                     # follow the function's log
zygo down                              # stop them all
```

`zygo up` warms every function in the file. Run it again after an edit, and
only the functions that changed are restarted.

## Calling the functions over HTTP

`zygo api` puts an HTTP server in front of the functions. By default it
listens on `127.0.0.1:7700`, and every caller needs a *bearer token*: a secret
string sent in the `Authorization` header.

```bash
export ZYGO_API_TOKEN=$(openssl rand -hex 16)
zygo api                               # 127.0.0.1:7700

curl -H "Authorization: Bearer $ZYGO_API_TOKEN" \
     -d '{"n": 4}' http://127.0.0.1:7700/fn/double
```

The token is needed on every kind of listener, unix sockets included.
`--no-auth` turns it off, and Zygo refuses `--no-auth` anywhere except a unix
socket or a loopback address such as `127.0.0.1`.

```text
  curl / your app ──HTTP──▶ zygo api ──▶ supervisor ──▶ double  (zygote)
      Authorization:        127.0.0.1:7700              resize  (zygote)
      Bearer <token>                                    parse   (warm sandbox)
                                                        fetch   (zygote)
```

## What the API answers

| Route or status | Meaning |
|---|---|
| `POST /fn/<name>` | Run one request with the body as the event. |
| `POST /fn/<name>/batch` | Run a list of events. |
| `GET /metrics` | Numbers in Prometheus text format, for a monitoring system to collect. |
| `408` | The request ran past its deadline. |
| `429` | This tenant's queue is full. The `Retry-After` header says when to try again. |

[Chapter 17](17-api-sdk-mcp.md) has every route, status code and header, and
the Python and Node clients.

## Sending metrics to OpenTelemetry

Instead of waiting to be scraped at `/metrics`, Zygo can *push* the same
numbers to an OpenTelemetry collector. OpenTelemetry (OTel) is a common
standard for sending metrics and traces to monitoring tools.

```bash
zygo api --otlp-endpoint http://localhost:4318
```

`OTEL_EXPORTER_OTLP_ENDPOINT` does the same as the flag. The numbers go as
OTLP/HTTP JSON once a minute, and `OTEL_EXPORTER_OTLP_HEADERS` adds a header,
for example for authentication. Only metrics are sent. There are no
per-request traces (spans) yet.

## When something is wrong

```bash
zygo logs fetch --failed                    # the requests that failed, with their stderr
zygo shell fetch                            # a shell inside the warm sandbox
zygo spec explain fetch                     # every limit and mount, fully resolved
zygo run --dry-run --json python:3.12-slim  # the mount plan, without running it
zygo doctor                                 # is the host still able to run sandboxes?
```

[Chapter 22](22-troubleshooting.md) lists the errors people actually meet,
with the fix for each.

## Where to go next

| If you want to… | Read |
|---|---|
| see all three ways working in one small app | [`examples/web-api`](../../examples/web-api) — a web API whose three endpoints use a warm function, a pool and a fresh sandbox |
| use `zygo run` well | [12. One-shot sandboxes](12-one-shot-sandboxes.md) |
| write handlers in more languages | [13. Warm functions](13-warm-functions.md) |
| set limits, network and secrets | [14. Limits, network and secrets](14-limits-network-secrets.md) |
| understand images, venvs and apt layers | [15. Images and dependencies](15-images-and-dependencies.md) |
| run it on a server | [16. Production](16-production.md) |
| call it from a program or an AI agent | [17. API, SDK and MCP](17-api-sdk-mcp.md) |
| look up a field of `sandbox.toml` | [20. `sandbox.toml`](20-sandbox-toml.md) |
| see measured speed and memory | [25. Performance](25-performance.md) |
| compare with Docker, gVisor and Firecracker | [10. Similar projects](10-similar-projects.md) |
| read the security model | [23. Security](23-security.md) |

The [`examples/`](../../examples) folder has complete projects: a small web
API, a webhook, a CI job, an LLM tool, a Go program, and agents in Node and
POSIX sh.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [10. Similar projects, and Docker side by side](10-similar-projects.md) · [Contents](README.md) · **Next: [12. One-shot sandboxes](12-one-shot-sandboxes.md) →**
