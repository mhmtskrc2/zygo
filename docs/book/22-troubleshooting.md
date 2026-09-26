# 22. Troubleshooting

This chapter is a list of things that go wrong, what each one looks like, and
how to fix it. The error texts are kept word for word, so you can search this
page for the line you see on your screen. Start with `zygo doctor`; most of
what follows is `doctor` in longer form.

## Start with `zygo doctor`

```bash
zygo doctor
```

`zygo doctor` checks this host for everything a sandbox needs. It does not just
read a setting: it *tries* each thing, such as making a mount inside a user
namespace, because a setting can say yes while the kernel says no. For anything
missing it prints one line with the fix. Some fixes need root, and for those
`zygo doctor --fix` can apply them for you: it prints every command and what it
costs, and asks before it runs anything.

```text
  ┌──────────────┐     all ok     ┌──────────────────────────┐
  │ zygo doctor  │ ─────────────▶ │ the host is fine: look   │
  └──────┬───────┘                │ at your spec or program  │
         │ a line says            └──────────────────────────┘
         │ FAIL / degraded
         ▼
  ┌──────────────────────────┐    ┌──────────────────────────┐
  │ read its remedy          │    │ zygo doctor --fix        │
  │ (this chapter has the    │ ─▶ │ shows each command and   │
  │  longer story)           │    │ its cost, asks, applies  │
  └──────────────────────────┘    └────────────┬─────────────┘
         ▲                                     │ or fix it by hand
         │                                     ▼
         │                        ┌──────────────────────────┐
         └─────────────────────── │ run zygo doctor again    │
                  until all ok    └──────────────────────────┘
```

## How to read an error

A Zygo error names what failed, and usually says why and what to do. The
first question is always *who* failed: the host, Zygo, or your program. The
exit status answers that most of the time (see the table below). A second
question is *when* it failed: before the sandbox started, or while your
program ran. `--outcome` answers that one ([below](#the-sandbox-never-started-and-it-looks-like-the-program-failed)).
If the error text is not clear, run the same command again with
`ZYGO_LOG=debug`, which turns on Zygo's own tracing.

```text
  error on screen
        │
        ├─ exit 125 ───────────▶ the HOST cannot do it       → zygo doctor
        ├─ exit 2 ─────────────▶ your SPEC or flags are wrong → the message names the field
        ├─ exit 1 ─────────────▶ your INPUT or PROGRAM failed → read its stderr
        ├─ exit 137 ───────────▶ KILLED: time or memory      → --outcome says which
        ├─ exit 75 / 4 ────────▶ zygo exec: busy / no such function
        └─ exit 111 (macOS) ───▶ the Linux VM was not reached → retry, see below
```

## Exit statuses that are Zygo's

Every other status is your program's own: it ran, and that is what it said.

| status | meaning | where to look |
|---|---|---|
| **1** | the input is wrong, for example a dependency build failed | the message; [A dependency build failed](#a-dependency-build-failed-exit-1) |
| **2** | the spec or the flags are wrong | the message names the field |
| **4** | `zygo exec`: no such function | [Exit 4](#exit-4-no-such-function) |
| **75** | `zygo exec`: the function is at its concurrency limit; retry | [Busy](#name-is-at-its-concurrency-limit--retry-http-429-exit-75) |
| **111** | macOS only: the Linux VM could not be reached | [Session open refused by peer](#session-open-refused-by-peer-or-exit-111) |
| **125** | this host cannot run sandboxes, or there is no supervisor to talk to | [exit 125](#this-host-cannot-run-sandboxes-exit-125) |
| **137** | Zygo or the kernel killed it: the deadline or the memory limit | [Exit 137](#exit-137-and-you-cannot-tell-why) |

## Host setup

These errors mean the machine itself is not ready. They come before your
program runs, so your code is not the problem.

### "this host cannot run sandboxes" (exit 125)

`zygo doctor` says which precondition is missing. The common ones are:

- unprivileged user namespaces are turned off completely
  (`kernel.unprivileged_userns_clone=0` on Debian and its relatives). A *user
  namespace* is the kernel feature that lets a normal user be "root" inside a
  sandbox and nobody outside it;
- the kernel is older than 5.3.

Exit 125 also comes from a command that needs a supervisor when none is
running; see ["no supervisor running"](#no-supervisor-running).

### "the program does not exist inside the image", and it plainly does

Three causes, from most to least likely:

- **It is on your host, not in the image.** Everything after the image name
  runs *inside* the sandbox. Mount the file in and name its path there:
  `zygo run --mount ./hello.py:/hello.py:ro python:3.12-slim python3 /hello.py`.
- **It is a dynamically linked binary and the image has the wrong libc.** A
  dynamically linked program needs a *loader* from the C library to start. The
  kernel reports a missing loader as a missing program, so a glibc binary in
  `alpine:3` fails with this exact message. Use an image from the same family
  as the binary, or a static binary.
- **The path is relative** and the working directory is not what you
  thought. `workdir` defaults to `/app`, and falls back to `/` when the image
  does not have it. `zygo run --workdir /where …` sets it.

### A dependency build failed (exit 1)

`pip` could not resolve a requirement, or `apt` could not find a package. The
message carries the last forty lines of the build's own output, and the
reason is there.

The exit code is **1**, not 125. The host is fine and the input is wrong, so a
CI job that retries on another machine would fail there too. 125 is kept for a
build that could not *start*.

## AppArmor and user namespaces

*AppArmor* is a set of rules, loaded by the system's administrator, that the
kernel checks for every process (see [chapter 4](04-other-locks.md#apparmor-and-selinux)).
Ubuntu uses it to limit what a normal user may do inside a user namespace, and
that gets in a sandbox's way.

### "applying a bind mount from the spec failed: No such file or directory"

**On Ubuntu or Debian**, when the path clearly exists, the cause is
`kernel.apparmor_restrict_unprivileged_userns=1`. It lets a normal process
create a user namespace, then refuses the first mount inside it. That mount is
the first thing every sandbox does.

`zygo doctor` finds this by trying that mount, and prints the fix.
`zygo doctor --fix` applies it: it prints every command and what it costs, and
asks first. Where AppArmor 4 and `apparmor_parser` are installed — Ubuntu
24.04 has both — the fix is an **AppArmor profile for the `zygo` binary**,
written to `/etc/apparmor.d/zygo` and loaded. It lets that one program use
user namespaces and leaves the restriction on for everything else. It is
attached by path, so a binary you move or install elsewhere needs
`--fix` again. [`packaging/apparmor/zygo`](../../packaging/apparmor/zygo) is
the same profile for `/usr/local/bin/zygo`, for an image or a package to ship.

Where a profile cannot be loaded, `--fix` falls back to the sysctl, and writes
`/etc/sysctl.d/60-zygo-userns.conf` so it survives a reboot. Read
[the threat model](23-security.md#on-ubuntu-2404-and-later-zygo-asks-you-to-turn-something-off)
first: that one turns a protection off for **every** process on the machine,
and `--fix` says so in those words above the question.

**On a Mac**, when the path is outside your home directory, the cause is
different. The Linux VM mounts `$HOME` and nothing else, so a path elsewhere
does not exist inside it. System temporary folders are the usual problem,
because macOS puts them under `/var/folders`. Move the folder under `$HOME`.
The shim (the small `zygo` program on the Mac that forwards commands into the
VM) refuses every such path before forwarding, and names it: a mount, a spec
or requirements file, a handler, or an `--outcome` file. An output file it let
through would be written inside the VM, where you would never find it.

## Cgroups

A *cgroup* is a kernel group of processes with limits on memory, CPU and
process count (see [chapter 3](03-cgroups.md)). Zygo must be allowed to make
its own cgroups, which is called *delegation*.

### "no cgroup controllers" or "there is no `memory.max` here"

Your shell is in a cgroup that cannot delegate. On a systemd machine, an ssh
login sits in a `session-N.scope` that systemd owns, and a normal process may
not create a cgroup inside it.

Zygo re-runs itself inside a new, short-lived systemd *scope* when it sees
this, so the problem usually fixes itself. When a supervisor is running,
`zygo run` hands the sandbox to it and never needs a scope at all. When
neither works, try this:

```bash
systemd-run --user --scope -p Delegate=yes -- zygo run alpine:3 /bin/true
```

If that works and a plain `zygo run` does not, your `systemd-run` is refusing
the delegation. `loginctl enable-linger $USER` is often the missing piece.

## Networking

A sandbox with a network uses `pasta`, a program that moves packets between the
sandbox and the host as your own user, and `nft`, which sets up the firewall
inside the sandbox (see [chapter 4](04-other-locks.md#the-network)). Most
network errors are one of these two tools being missing or blocked.

```text
  ┌ sandbox ───────────────────────┐
  │ program ──▶ tap0 ──▶ nftables  │  needs /dev/net/tun, and nft installed
  └──────────────────────┬─────────┘
                         │
               ┌─────────▼─────────┐
               │ pasta (as you)    │  needs to be on PATH, and not blocked
               └─────────┬─────────┘  by the passt AppArmor profile
                         ▼
                 host's normal sockets
```

### "pasta could not configure the sandbox's network: Couldn't open user namespace ... Permission denied"

An AppArmor profile is holding `pasta` back and refusing it the sandbox's user
namespace. This is the distribution's policy. It has nothing to do with
`/dev/net/tun`, which is usually there and working.

```bash
sudo aa-status | grep -i passt
sudo aa-complain /usr/bin/pasta      # or: zygo doctor --fix
```

`zygo doctor --fix` offers this one too, when it finds the profile loaded and
enforcing. It uses `aa-complain` rather than unloading the profile: the profile
stays loaded and keeps logging what it would have refused. `sudo aa-enforce
/usr/bin/pasta` puts it back.

Or use `network = "none"`, the default, which needs no `pasta` at all.

### "Couldn't open PID file ... Permission denied"

The same distribution policy, seen from the other side. Ubuntu's `passt`
AppArmor profile attaches by path to `/usr/bin/passt`, and to `pasta`, which is
a link to it. It lets the program write files only where it expects, and Zygo's
pid file is in Zygo's data folder, so it is refused. The profile is enforced by
the host's kernel, so this happens **inside a container too**, even one started
with `--security-opt apparmor=unconfined`. `dmesg` shows
`apparmor="DENIED" operation="mknod" profile="passt"`.

```bash
sudo aa-complain passt                     # on the host
cp -L /usr/bin/pasta /usr/local/bin/pasta  # in an image: a path the profile does not name
```

The Zygo container image already does the second. Remove the `/usr/bin/pasta`
link afterwards, so `PATH` cannot find the blocked one first.

### "Failed to set up tap device in namespace", or "did not finish configuring ... within 10 s"

There is no `/dev/net/tun`, the device a sandbox's network card is made from.
Container runtimes allow the device but do not create the file for it, so this
is the first thing a networked sandbox in a container says. `zygo doctor`
reports it on the egress line. A run with a network now refuses early, rather
than asking `pasta`: passt 2025_01 prints the line above and then never exits.

```bash
docker run --device /dev/net/tun …        # Docker
sudo modprobe tun                          # a host without the module
```

In Kubernetes, mount the node's `/dev/net/tun` as a `hostPath` volume of type
`CharDevice`. runc and crun allow the device by default.

### "pasta is not on PATH"

```bash
sudo apt install passt nftables      # Debian, Ubuntu
sudo dnf install passt nftables      # Fedora
```

When these are missing, a networked sandbox **does not start**, rather than
starting with no firewall. That is on purpose.

### A name inside the sandbox does not resolve

Under `network = "egress"`, the sandbox uses Zygo's own name resolver, and a
name that the `allow` list does not cover does not resolve. That is the
allowlist doing its job. Add the name:

```toml
allow = ["api.example.com:443", "*.cdn.example.com:443"]
```

### A private address is refused even with `network = "full"`

On purpose. Private and link-local address ranges — your host, its neighbours
on the network, and `169.254.169.254` — stay refused in every namespaced mode
unless you pass `--allow-private-net`. `169.254.169.254` is the cloud metadata
address, and it is the first thing a compromised handler tries.

## When a request fails

These are problems with one run or one request, after the sandbox started (or
while it was trying to).

### "[Errno 1] Operation not permitted", naming a file that exists and is readable

The file is fine. A **syscall** was refused. A *syscall* is a request from a
program to the kernel, and every sandbox runs under a *seccomp allowlist*: a
filter that answers `EPERM` to any syscall the profile does not name
([chapter 24](24-seccomp-profiles.md)). libc and Python report `EPERM` as
"Operation not permitted", against whatever path the call was about. So the
traceback names the file, never the syscall. `pip install --target` once
failed this way in nine tracebacks about `RECORD` and `WHEEL`; the refused
call was `listxattr`, inside `shutil.copy2`.

Find out which syscall, in this order:

1. **`--seccomp permissive`**, once. If it works there, the profile is the
   cause. `permissive` is `default` plus namespaces, mounts, `ptrace` and
   friends. It is *not* "no filter", so a call refused under both is not a
   seccomp refusal at all.
2. **`ZYGO_LOG=debug zygo run …`**. The launcher then asks the kernel to log
   every refusal, and each one shows up in the host's kernel log as
   `audit: type=1326 … comm="python3" … syscall=<n>`:

   ```bash
   ZYGO_LOG=debug zygo run --mount ./out:/out:rw python:3.12-slim python3 -c 'import shutil; shutil.copy2("/etc/hostname", "/out/x")'
   sudo journalctl -k -n 20 | grep type=1326     # or: sudo dmesg | grep seccomp
   ```

   `<n>` is the syscall number for the sandbox's CPU architecture. The tables
   in `crates/zygo-core/src/backend/ns/syscalls.rs` map it to a name.
3. **`strace -f -e trace=%file`** on the program *outside* Zygo, when you
   cannot read the kernel log. It shows every file-related syscall the program
   makes, and the one missing from the profile is usually easy to spot.

```text
  "Operation not permitted" on a file that is fine
        │
        ▼
  retry with --seccomp permissive ── works ──▶ the profile is the cause
        │ still fails                                  │
        ▼                                              ▼
  not a seccomp refusal (or the call      ZYGO_LOG=debug + journalctl -k
  is in no profile at all)                 → "type=1326 … syscall=<n>"
                                                       │
                                                       ▼
                                    look <n> up in syscalls.rs, then report it
```

Then report it. A syscall that a real package needs and the profile refuses is
a bug in the profile. The extended-attribute family was one, until the first
adoption report found it.

### Exit 137, and you cannot tell why

A deadline kill and an out-of-memory kill are both a `SIGKILL`, so both exit
137, and the wait status carries nothing else. Ask for the reason:

```bash
zygo run --outcome /tmp/why.json ...
cat /tmp/why.json
```

`timed_out` comes from the launcher. `oom_killed` comes from the kernel's own
counter. Over the HTTP API the same three fields are in the answer to
`POST /run`, and the SDKs expose them as `timed_out` / `timedOut` and
`oom_killed` / `oomKilled`.

### The sandbox never started, and it looks like the program failed

`--outcome` tells the two apart. A run that could not build its sandbox —
the image is not there, the host cannot do it, a mount does not exist — writes
the file too, with `started: false` and the `phase` that failed (`plan` or
`start`). A program that ran and failed has `started: true` and
`phase: "run"`. Over the API the same two fields are in the answer to
`POST /run`, and the SDKs expose `started` / `phase`. Decide on those fields,
not on the error's text: a sandbox that never started is *unavailable*, not
the code's fault.

```text
  started: false, phase: "plan"   ─▶ the spec could not be turned into a plan
  started: false, phase: "start"  ─▶ the sandbox could not be built     } not your code
  started: true,  phase: "run"    ─▶ your program ran, and this is its result
```

### "`<name>` is at its concurrency limit — retry" (HTTP 429, exit 75)

This is *backpressure*, not a failure: **the request never ran**. The function
is at its `concurrency` limit and its queue is full. Retry after a moment, or
raise `concurrency` if the function can really take more. Over the API it is
HTTP 429. `zygo exec` exits **75**, which is the same answer in the form a
shell understands.

The SDKs raise this as its own type, `Busy`, so a caller can tell it apart from
a handler that failed. A handler that raised will raise again; a `Busy` will
not.

### Exit 4: no such function

`zygo exec` exits **4** when the supervisor has no function by that name. A
script can branch on it. Over the API a tenant gets the same single answer for
"no such function" and for "that one belongs to somebody else", because the
difference between them is a fact about another customer. Check the name with
`zygo ps`.

### The handler raised and the traceback is missing

It is in the answer, not on your terminal. `zygo exec` prints the error. Over
the API it is the `error` field, with `stdout` and `stderr` beside it. In the
SDKs it is `HandlerError`, which carries both streams.

```bash
zygo logs resize --failed -n 20
```

### A request sees state from a previous one

It should not, and this is worth reporting. Each request is a fresh `fork()`
of the warm agent, so module-level state is whatever the *zygote* (the warm
process every request is copied from) had at import time. Nothing a request
writes survives it.

The one thing that does last is anything a handler writes **outside** the
process: a file in a writable mount, a row in a database. That is your state,
not Zygo's.

## Warm functions

A warm function is kept loaded by the *supervisor*, the background process
that `zygo serve` and `zygo up` start (see [chapter 13](13-warm-functions.md)).
These problems are about it staying up.

```text
  warm ──(idle_timeout)──▶ frozen ──(cold_after)──▶ cold (dropped)
   ▲                         │                          │
   └──── one write to wake ──┘                          │
   ▲                                                    │
   └────────── next request pays a warm-up ─────────────┘
```

### It keeps restarting

```bash
zygo logs <name> -n 50
```

An agent that crashes is warmed again automatically, with a growing pause
between tries (a *backoff*). Repeated rewarming means the handler fails at
import time. The zygote's own output is in the log, above the requests.

### It went cold on its own

That is `idle_timeout` and `cold_after`. A function past `idle_timeout` is
*frozen*: it keeps its memory and costs one write to wake. Past `cold_after`
it is dropped completely, and the next request pays a warm-up.

```toml
idle_timeout = "10m"
cold_after   = "1h"
```

`zygo ps` shows the state. To bring one back before a real request arrives:

```bash
curl -X POST .../fn/<name>/warm       # or client.warm(name) in the SDKs
```

### 1 request in 100 takes ~10 ms, and the rest take ~1.5

You see it in `zygo bench warm` or `zygo stats`: the usual request is fast,
and the slowest 1 in 100 is several times slower. `bench warm` shows where the
time goes, and says it: `admit` owns most of the slow request, followed by
`cgroup2 here has no favordynmods`. `zygo doctor` reports the same thing as

```text
cgroup moves   no favordynmods: ~1 warm request in 100 waits ms to enter its cgroup   degraded
```

On Linux 6.0 and later, moving a process into a cgroup sometimes waits for the
kernel to pass a quiet point. Only warm functions with an agent (Python,
Node) and pools move their requests; warm-exec and one-shot runs are created
inside their cgroup and do not wait. The fix is a setting of the whole
machine:

```bash
zygo doctor --fix       # remounts cgroup2 with favordynmods, now and at boot
```

It prints the four commands and what they cost before it asks: every fork and
exit on the machine gets slightly slower (about 2 µs usually, measured). On
the Lima VM it took the slow 1 in 100 from 10.3 ms to 3.4 ms. Inside a
container this is the host's setting — `doctor` says so and does not offer to
change it. To undo it, `sudo systemctl disable zygo-cgroup-favordynmods` and
reboot. [Chapter 25](25-performance.md#why-1-in-100-is-slow-on-newer-kernels)
has the numbers.

```text
  slow 1 in 100 ──▶ zygo bench warm: most of it is `admit`?
                         │
                  yes ───┴──▶ zygo doctor: "cgroup moves … degraded"?
                                   │
                            yes ───┴──▶ zygo doctor --fix  (host-wide, asks first)
```

### "tenant `acme` has no secret named STRIPE_KEY"

A runtime pool names `secrets`, and a call from `acme` arrived before that
tenant had a value stored under one of the names. The call was refused
before anything ran (`400`, `bad_spec`), because a pool's values come from
the **calling** tenant's store and nowhere else — not from the shell, not
from the pool's own tenant. Store it, then call again:

```bash
zygo secrets set acme STRIPE_KEY          # or PUT /tenants/acme/secrets/STRIPE_KEY
```

A `zygo exec --runtime` call is the `default` tenant's, so it reads
`default`'s store. A pool that names secrets on a host with no
`ZYGO_SECRETS_KEY` is refused at `serve` instead, and says so.

### "no supervisor running"

Nothing is warm. The supervisor is started by `zygo serve` or `zygo up`, and it
exits when the last function stops. `zygo exec`, `ps`, `logs`, `shell`,
`stats` and `top` only ever connect to one that already exists. A command that
needs one and finds none fails with `no supervisor at <socket>` and exit 125,
and tells you to start one with `zygo serve <handler> --name <name>`.

## The image store

Zygo keeps pulled images in its own local *store*, a folder of image layers
(see [chapter 15](15-images-and-dependencies.md)).

### "image is not in the local store"

```bash
zygo pull python:3.12-slim
```

`zygo run` pulls on first use, as `docker run` does. `zygo serve` and `zygo up`
do not: a deploy should not quietly depend on a registry being reachable.

### "the image has moved" during `zygo up`

`zygo.lock` records the *digest* (the content hash) each image resolved to.
The tag now points somewhere else, and Zygo stops rather than quietly running
something different.

```bash
zygo up --relock      # accept the move and rewrite the lock
```

### The store is using too much disk

```bash
zygo image prune --dry-run
zygo image prune
zygo image prune --unused-for 30d --blobs
```

Without flags, it removes only what nothing can reach: layers of images that
were removed, and caches whose image is gone. `--blobs` drops the compressed
copy of every unpacked layer. That roughly halves the store, and costs a
download if a layer folder is ever lost.

## The `vm` backend

The `vm` backend runs each sandbox in a small virtual machine with its own
kernel (see [chapter 12](12-one-shot-sandboxes.md)).

### "the guest kernel is not installed"

```bash
zygo backend install vm
```

The guest kernel is a separate file for two reasons: it is GPL, while this
binary is Apache-2.0, and it is twenty megabytes against a fifteen-megabyte
size budget. From a checkout, `make vm-kernel` builds it, and `zygo doctor`
reports it once it is in place.

### A `vm` sandbox's root is read-only

This host cannot build the guest's private writable layer, so the sandbox fell
back to sharing the image read-only. The log line starts with "no writable
scratch for this guest" and says why. The layer needs rootless overlayfs,
which is Linux 5.11 and newer; `zygo doctor`'s `overlayfs (userns)` line is
the check. On a host that has it, a guest writes to `/` and `/tmp` freely, up
to `scratch`, and nothing it writes reaches the shared image.

### `KVM GICv3 creation failed, falling back to KVM GICv2`

Noise, not an error. On a host whose interrupt controller is GICv2 — a
Raspberry Pi, for example — libkrun tries the newer one first and falls back.
Guests boot either way.

## On a Mac

On macOS, Zygo runs a Linux VM with Lima, and the `zygo` command on the Mac is
a *shim*: it forwards each command into the VM over one shared SSH connection.

```text
  Mac                                     Linux VM (Lima, "zygo")
  ┌──────────────────────────┐            ┌────────────────────────────┐
  │ zygo (shim)              │  one SSH   │ sshd  (MaxSessions: 64)    │
  │  checks paths are under  │ ─────────▶ │  ▼                         │
  │  $HOME, retries a        │ connection │ zygo (Linux build)         │
  │  refused session twice   │  many      │  ▼                         │
  └──────────────────────────┘  sessions  │ sandboxes, supervisor      │
   $HOME ═══════════════════ same path ══▶│ $HOME (and nothing else)   │
                                          └────────────────────────────┘
```

### Everything is slow

Crossing into the Linux VM costs about 22 ms per command once the VM is up,
over the SSH connection Lima keeps open. If every command costs 100 ms or more,
that connection is not being used. `ssh -F ~/.lima/zygo/ssh.config -O check
lima-zygo` should say `Master running`. The millisecond warm path is reached
through the HTTP API or the SDKs, where the hop is paid once per connection
rather than once per request. See [what Zygo costs](25-performance.md#on-a-mac).

### "limactl is not installed"

```bash
brew install lima
```

### "the Linux build is missing"

```bash
make guest-build             # compiled inside the VM; needs no Docker
make poc/zygo-linux-musl     # the same binary, built in a Docker container
```

That is the binary that runs inside the VM. A release ships it beside the Mac
one. From a checkout it is one `make`, and the next `zygo` command copies it
in.

### "Session open refused by peer", or exit 111

Every command forwarded into the VM is one *session* on one shared
(multiplexed) SSH connection. The guest's `sshd` limits the sessions one
connection may carry (`MaxSessions`, ten by default). Past that, when many
`zygo` commands ran at once — an adopter measured it at 24 — some were refused
before the guest ran anything, with SSH's own line
`mux_client_request_session: session request failed: Session open refused by peer`
and exit 255.

Three things stand against it now:

- **The shim retries** a session the peer refused, twice, with a short pause
  before each try. It is the one layer that knows the guest ran nothing, so a
  retry is safe. What still fails after three attempts exits **111** with a
  sentence of Zygo's, and SSH's line is kept for `-v`. 111 differs from every
  status a program can produce and from 125, so a caller can branch on it.
- **The VM template raises the limit** to 64, in the guest's
  `/etc/ssh/sshd_config.d/zygo.conf`. That reaches a VM created from this
  release's template and **not** one created before it, because Lima copies
  the template only once. `zygo doctor` reads the template generation back
  from the VM's copy and says when it is behind.
- **For an existing VM**, either recreate it — nothing under `$HOME` is in it,
  so you lose only warm functions and about a minute:

  ```bash
  limactl delete zygo      # the next zygo command builds a fresh one
  ```

  or apply the change by hand and keep the VM:

  ```bash
  limactl shell zygo -- sudo sh -c 'printf "MaxSessions 64\nMaxStartups 64:30:128\n" > /etc/ssh/sshd_config.d/zygo.conf && systemctl reload ssh'
  ```

`make verify-shim-concurrency` fires 24 commands at once for six rounds and
wants 144 of 144 to succeed.

```text
  attempt 1 ── refused ──▶ short pause ──▶ attempt 2 ── refused ──▶ longer pause
                                                                        │
      ┌─────────────────────────────────────────────────────────────────┘
      ▼
  attempt 3 ── refused ──▶ exit 111 "the Linux VM could not be reached"
      │
      └── accepted ──▶ the command runs, and its own exit status comes back
```

### A command is refused because of where it was run

The VM mounts your home folder at the same path, and nothing else. A command
run from outside `$HOME` is refused **only when something in it depends on
where it was run**: a relative path — a mount, a handler file, `-f`, a script —
or a `sandbox.toml` found by searching upwards from there, which the VM could
not find. The message names the argument.

A command whose paths are all absolute and under `$HOME`, or that names no
path at all, runs from anywhere: a server started from `/`, or a systemd unit
with its default working folder. It runs in the VM's `/`, so a relative path
that slipped through would name a file that does not exist, rather than a file
of yours that you did not mean.

### "client speaks control v14, this supervisor speaks v13"

The supervisor in the VM is from the previous release. The shim replaced the
binary, but a supervisor started from the old one was still running. The shim
now stops it itself when it replaces the binary, and says so. If you see the
message anyway:

```bash
zygo supervisor stop     # only the supervisor: it drains, exits, and the next serve starts a new one
```

`zygo stop --all` also works, but on a Mac it stops the whole Linux VM, and
the next command pays the boot. It says so before it does it.

## What `doctor --json` says

This is the document a health check reads:

```json
{
  "checks": [
    {"name": "limactl", "status": "ok", "detail": "at /opt/homebrew/bin/limactl", "side": "host"},
    {"name": "vm", "status": "ok", "detail": "instance `zygo` is running", "side": "host"},
    {"name": "kernel", "status": "ok", "detail": "6.8.0-31-generic", "side": "vm"},
    {"name": "pasta", "status": "FAIL", "detail": "not on PATH", "remedy": "apt install passt", "side": "vm"}
  ],
  "backends": ["ns"],
  "ok": false
}
```

- `checks[]`: one per probe. `status` is `ok`, `degraded` (usable, with a
  fallback), `absent` (an optional backend that is not installed) or `FAIL`.
  `remedy` is present when there is one. On a Mac, `side` says whether the
  check is the Mac's (`host`) or the VM's (`vm`); on Linux there is one side
  and no field.
- `backends[]`: the isolation backends usable right now — the host has what
  each needs, and this binary implements it. On a Mac these are the VM's.
- `ok`: no check failed. **The exit status is 0 exactly when `ok` is true.**
  Both come from the same list, so you can trust either one alone. A stopped
  VM is `ok: false`: nothing can vouch for the sandboxes until it is up, and
  the remedy says so.

## Still stuck

- `zygo run --dry-run --json <image>` prints the resolved configuration, the
  mount plan and the cgroup values, and runs nothing.
- `zygo spec explain <fn>` prints what a function resolved to, and where each
  value came from.
- `ZYGO_LOG=debug zygo <command>` turns on Zygo's own tracing.
- `zygo backend list` says which isolation backends this host can really use,
  and why not the others.

If it looks like an escape — anything reaching the host from inside a sandbox
— please report it privately. [SECURITY.md](../../SECURITY.md) says how, and
what is in scope; [chapter 23](23-security.md#reporting-a-vulnerability) has
the short version.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [21. Environment, files and exit codes](21-environment-files-exit-codes.md) · [Contents](README.md) · **Next: [23. Security: the threat model](23-security.md) →**
