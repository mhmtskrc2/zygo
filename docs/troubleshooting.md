# When something does not work

Start here:

```bash
zygo doctor
```

It probes this host for everything a sandbox needs, attempts each one rather
than reading a setting, and prints the fix for anything missing. Most of what
follows is `doctor` in longer form, for when its one line was not enough.

## A sandbox will not start

### "applying a bind mount from the spec failed: No such file or directory"

**On Ubuntu or Debian**, and the path plainly exists: this is
`kernel.apparmor_restrict_unprivileged_userns=1`. It lets an unprivileged
process create a user namespace and then refuses the first mount inside it,
which is the first thing every sandbox does.

`zygo doctor` detects it by attempting that mount and prints the fix, and
`zygo doctor --fix` applies it — printing every command and what it costs, and
asking first. It also writes `/etc/sysctl.d/60-zygo-userns.conf`, so the
machine that works today still works after a reboot.

Read the [threat model](threat-model.md) before applying it: the fix turns off
a protection for every process on the machine, not only Zygo's. `--fix` says
so, in those words, above the confirmation.

**On a Mac**, and the path is outside your home directory: the Linux VM mounts
`$HOME` and nothing else, so a path elsewhere has no counterpart inside it.
System temporary directories are the usual culprit, because macOS puts them
under `/var/folders`. Move the directory under `$HOME`. The shim refuses every
such path before forwarding — a mount, a spec or requirements file, a handler,
an `--outcome` file — and names which; an output file it let through would be
written inside the VM, where you would never find it.

### "no cgroup controllers" or "there is no `memory.max` here"

Your shell is in a cgroup that cannot delegate. On a systemd machine an ssh
login sits in a `session-N.scope` that systemd owns, and an unprivileged
process cannot create a cgroup inside it.

Zygo re-executes itself inside a transient scope when it finds this, so it
usually resolves itself — and when a supervisor is running, `zygo run` hands
the sandbox to it and never needs a scope at all. When neither applies:

```bash
systemd-run --user --scope -p Delegate=yes -- zygo run alpine:3 /bin/true
```

If that works and a bare `zygo run` does not, your `systemd-run` is refusing
the delegation. `loginctl enable-linger $USER` is often the missing piece.

### "the program does not exist inside the image", and it plainly does

Three causes, in order of likelihood:

- **It is on your host, not in the image.** Everything after the image is run
  *inside* the sandbox. Mount the file in and name its path there:
  `zygo run --mount ./hello.py:/hello.py:ro python:3.12-slim python3 /hello.py`.
- **It is a dynamically linked binary and the image has the wrong libc.** The
  kernel reports a missing *loader* as a missing program, so a glibc binary in
  `alpine:3` fails with this exact message. Use an image from the same family
  as the binary, or a static binary.
- **The path is relative** and the working directory is not what you assumed.
  `workdir` defaults to `/app` and falls back to `/` when the image lacks it;
  `zygo run --workdir /where …` sets it.

### A dependency build failed (exit 1)

`pip` could not resolve a requirement, or `apt` could not find a package. The
message carries the last forty lines of the build's own output, which is where
the reason is.

The exit code is **1**, not 125: the host is fine, the input is wrong, and a
CI job that retries on another machine would fail there too. 125 is reserved
for a build that could not *start*.

### "this host cannot run sandboxes" (exit 125)

`zygo doctor` will say which of the preconditions is missing. The common ones
are unprivileged user namespaces being disabled entirely
(`kernel.unprivileged_userns_clone=0` on Debian derivatives) and a kernel older
than 5.3.

## A networked sandbox will not start

### "pasta could not configure the sandbox's network: Couldn't open user namespace ... Permission denied"

An AppArmor profile is confining `pasta` and denying it the sandbox's user
namespace. This is the distribution's policy and has nothing to do with
`/dev/net/tun`, which is usually present and working.

```bash
sudo aa-status | grep -i passt
sudo aa-complain /usr/bin/pasta      # or: zygo doctor --fix
```

`zygo doctor --fix` offers this one too, when it finds the profile loaded and
enforcing. `aa-complain` rather than unloading the profile: it stays loaded
and keeps logging what it would have denied, and `sudo aa-enforce
/usr/bin/pasta` puts it back.

Or use `network = "none"`, the default, which needs no `pasta` at all.

### "Couldn't open PID file ... Permission denied"

The same distribution policy from the other side: Ubuntu's `passt` AppArmor
profile attaches to `/usr/bin/passt` — and to `pasta`, which is a link to it —
by path, and lets it write files only where it expects. Zygo's pid file is in
its data directory, so it is refused. The kernel enforcing the profile is the
host's, so this happens **inside a container too**, even one started with
`--security-opt apparmor=unconfined`; `dmesg` shows `apparmor="DENIED"
operation="mknod" profile="passt"`.

```bash
sudo aa-complain passt                     # on the host
cp -L /usr/bin/pasta /usr/local/bin/pasta  # in an image: a path the profile does not name
```

The Zygo container image already does the second. Remove the `/usr/bin/pasta`
link afterwards so `PATH` cannot find the confined one first.

### "Failed to set up tap device in namespace", or "did not finish configuring ... within 10 s"

There is no `/dev/net/tun`. Container runtimes allow the device but do not
create the node, so this is what a networked sandbox in a container says
first. `zygo doctor` reports it on the egress line, and a run with a network
now refuses up front rather than asking `pasta`, which in passt 2025_01 prints
the line above and then does not exit.

```bash
docker run --device /dev/net/tun …        # Docker
sudo modprobe tun                          # a host without the module
```

In Kubernetes, mount the node's `/dev/net/tun` as a `hostPath` volume of type
`CharDevice`; runc and crun allow the device by default.

### "pasta is not on PATH"

```bash
sudo apt install passt nftables      # Debian, Ubuntu
sudo dnf install passt nftables      # Fedora
```

A networked sandbox **does not start** when these are missing, rather than
starting unconfined. That is deliberate.

### A name inside the sandbox does not resolve

Under `network = "egress"` the sandbox's resolver is Zygo's own, and a name the
`allow` list does not cover does not resolve. That is the allowlist working.
Add the name:

```toml
allow = ["api.example.com:443", "*.cdn.example.com:443"]
```

### A private address is refused even with `network = "full"`

On purpose. Private and link-local ranges — your host, its neighbours, and
`169.254.169.254` — stay refused in every namespaced mode unless you pass
`--allow-private-net`. A compromised handler tries the metadata address first.

## A request fails

### "[Errno 1] Operation not permitted", naming a file that exists and is readable

The file is fine. A **syscall** was refused: every sandbox runs under a
seccomp allowlist, and a syscall the profile does not name answers `EPERM`,
which libc and Python report as "Operation not permitted" against whatever
path the call was about. The traceback names the file, never the syscall —
`pip install --target` failed this way in nine tracebacks about `RECORD` and
`WHEEL`, and the refused call was `listxattr` inside `shutil.copy2`.

Find out which syscall, in this order:

1. **`--seccomp permissive`**, once. If it works there, the profile is the
   cause. (`permissive` is `default` plus namespaces, mounts, `ptrace` and
   friends — it is not "no filter", so a call refused under both is not a
   seccomp refusal at all.)
2. **`ZYGO_LOG=debug zygo run …`**: the launcher then asks the kernel to
   log every refusal, and each one appears in the host's kernel log as
   `audit: type=1326 … comm="python3" … syscall=<n>`:

   ```bash
   ZYGO_LOG=debug zygo run --mount ./out:/out:rw python:3.12-slim python3 -c 'import shutil; shutil.copy2("/etc/hostname", "/out/x")'
   sudo journalctl -k -n 20 | grep type=1326     # or: sudo dmesg | grep seccomp
   ```

   `<n>` is the syscall number for the sandbox's architecture; the tables
   in `crates/zygo-core/src/backend/ns/syscalls.rs` map it to a name.
3. **`strace -f -e trace=%file`** on the program *outside* Zygo, when the
   kernel log is out of reach: it shows every file-related syscall the
   program makes, and the one missing from the profile is usually obvious.

Then report it. A syscall a real package needs that the profile refuses is a
bug in the profile, and the extended-attribute family was one until the first
adoption report found it.

### Exit 137, and you cannot tell why

Both a deadline kill and an out-of-memory kill are a `SIGKILL`, so both are
137 and the wait status carries nothing else. Ask for the reason:

```bash
zygo run --outcome /tmp/why.json ...
cat /tmp/why.json
```

`timed_out` comes from the launcher; `oom_killed` comes from the kernel's own
counter. Over the HTTP API the same three fields are in the answer to
`POST /run`, and the SDKs expose them as `timed_out` / `timedOut` and
`oom_killed` / `oomKilled`.

### The sandbox never started, and it looks like the program failed

`--outcome` says which. A run that could not build its sandbox — the image
is not there, the host cannot, a mount does not exist — writes the file too,
with `started: false` and the `phase` that failed (`plan` or `start`); a
program that ran and failed has `started: true` and `phase: "run"`. Over the
API the same two fields are in the answer to `POST /run`, and the SDKs expose
`started` / `phase`. Classify on those rather than on the error's text: a
sandbox that never started is *unavailable*, not the code's fault.

### "`<name>` is at its concurrency limit — retry" (HTTP 429)

Backpressure, not a failure: **the request never ran**. The function is at its
`concurrency` limit and the queue is full. Retry after a moment, or raise
`concurrency` if the function can genuinely take more.

The SDKs raise this as its own type (`Busy`) precisely so a caller can tell it
apart from a handler that failed. A handler that raised will raise again;
a `Busy` will not.

### The handler raised and the traceback is missing

It is in the answer, not on your terminal. `zygo exec` prints the error; over
the API it is the `error` field with `stdout` and `stderr` beside it; in the
SDKs it is `HandlerError`, which carries both streams.

```bash
zygo logs resize --failed -n 20
```

### A request sees state from a previous one

It should not, and this is worth reporting. Each request is a fresh `fork()`
of the warm agent, so module-level state is whatever the *zygote* had at
import time, and nothing a request writes survives it.

The one thing that does persist is anything a handler writes **outside** the
process: a file in a writable mount, a row in a database. That is your state,
not Zygo's.

## A warm function will not stay up

### It keeps restarting

```bash
zygo logs <name> -n 50
```

An agent that crashes is rewarmed automatically, with a backoff. Repeated
rewarming means the handler is failing at import time, and the zygote's own
output is in the log above the requests.

### It went cold on its own

That is `idle_timeout` and `cold_after`. A function past `idle_timeout` is
frozen, which keeps its resident memory and costs one write to wake; past
`cold_after` it is dropped entirely and the next request pays a warm-up.

```toml
idle_timeout = "10m"
cold_after   = "1h"
```

`zygo ps` shows the state. To bring one back before a real request arrives:

```bash
curl -X POST .../fn/<name>/warm       # or client.warm(name) in the SDKs
```

### "no supervisor running"

Nothing is warm. The supervisor is started by `zygo serve` or `zygo up` and
exits when the last function stops. `zygo exec`, `ps`, `logs`, `shell`, `stats`
and `top` only ever connect to one that exists.

## The image store

### "image is not in the local store"

```bash
zygo pull python:3.12-slim
```

`zygo run` pulls on first use, as `docker run` does. `zygo serve` and `zygo up`
do not: a deploy should not silently depend on a registry being reachable.

### "the image has moved" during `zygo up`

`zygo.lock` records the digest each image resolved to. The tag now points
somewhere else, and Zygo stops rather than quietly running something different.

```bash
zygo up --relock      # accept the move and rewrite the lock
```

### The store is using too much disk

```bash
zygo image prune --dry-run
zygo image prune
zygo image prune --unused-for 30d --blobs
```

Without flags it removes only what nothing can reach: layers of images that
were removed, and caches whose image is gone. `--blobs` drops the compressed
copy of every unpacked layer, which roughly halves the store and costs a
download if a layer directory is ever lost.

## The `vm` backend

### "the guest kernel is not installed"

```bash
zygo backend install vm
```

It is a separate file because it is GPL where this binary is Apache-2.0, and
because it is twenty megabytes against a fifteen megabyte budget. From a
checkout, `make vm-kernel` builds it and `zygo doctor` reports it once it is
in place.

### A `vm` sandbox's root is read-only

Then this host cannot build the guest's private layer, and the sandbox fell
back to sharing the image read-only — the log line starts "no writable scratch
for this guest" and says why. The layer needs rootless overlayfs, which is
Linux 5.11 and newer; `zygo doctor`'s `overlayfs (userns)` line is the check.
On a host that has it, a guest writes to `/` and `/tmp` freely, up to
`scratch`, and nothing it writes reaches the shared image.

### `KVM GICv3 creation failed, falling back to KVM GICv2`

Noise, not an error. On a host whose interrupt controller is GICv2 — a
Raspberry Pi, for instance — libkrun tries the newer one first and falls back.
Guests boot either way.

## On a Mac

### Everything is slow

Crossing into the Linux VM costs about 20 ms per command once the VM is up,
over the SSH connection Lima keeps open. If every command costs 100 ms or more,
that connection is not being used: `ssh -F ~/.lima/zygo/ssh.config -O check
lima-zygo` should say `Master running`. The millisecond warm path is reached
through the HTTP API or the SDKs, where the hop is paid once by the connection
rather than once per request. See [what Zygo costs](performance.md#on-a-mac).

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
one; from a checkout it is one `make`, and the next `zygo` command copies it
in.

### "Session open refused by peer", or exit 111

Every command forwarded into the VM is one session on one multiplexed SSH
connection, and the guest's `sshd` caps the sessions a connection may carry
(`MaxSessions`, ten by default). Past that, running many `zygo` commands at
once — an adopter measured it at 24 — some fraction were refused before the
guest ran anything, with SSH's own line and exit 255.

Three things stand against it now:

- **The shim retries** a session the peer refused, twice, because it is
  the one layer that knows the guest ran nothing. What still fails after
  that exits **111** with a sentence of Zygo's, and SSH's line is kept for
  `-v`. 111 is distinct from every status a program can produce and from
  125, so a caller can branch on it.
- **The template raises the cap** to 64 in the guest's
  `/etc/ssh/sshd_config.d/zygo.conf`. That reaches a VM created from this
  release's template and **not** one created before it: Lima copies the
  template once. `zygo doctor` reads the generation back from the VM's
  copy and says when it is behind.
- **For an existing VM**, either recreate it — nothing under `$HOME` is in
  it, so nothing is lost but warm functions and about a minute:

  ```bash
  limactl delete zygo      # the next zygo command builds a fresh one
  ```

  or apply the change by hand and keep the VM:

  ```bash
  limactl shell zygo -- sudo sh -c 'printf "MaxSessions 64\nMaxStartups 64:30:128\n" > /etc/ssh/sshd_config.d/zygo.conf && systemctl reload ssh'
  ```

`make verify-shim-concurrency` fires 24 at once for six rounds and wants
144 of 144.

### A command is refused because of where it was run

The VM mounts your home directory at the same path and nothing else. A
command from outside `$HOME` is refused **only when something in it depends
on where it was run**: a relative path — a mount, a handler file, `-f`, a
script — or a `sandbox.toml` found by searching upwards from there, which
the VM could not find. The message names the argument.

A command whose every path is absolute and under `$HOME`, or that names no
path at all, runs from anywhere — a server started from `/`, a systemd unit
with its default working directory. It runs in the VM's `/`, so a relative
path that slipped through would name a file that does not exist rather than
one of yours that you did not name.

### "client speaks control v13, this supervisor speaks v12"

The supervisor in the VM is from the previous release: the shim replaced the
binary but a supervisor started from the old one was still running. The shim
now stops it itself when it replaces the binary and says so. If you see the
message anyway:

```bash
zygo supervisor stop     # only the supervisor: it drains, exits, and the next serve starts a new one
```

`zygo stop --all` also works, but on a Mac it stops the whole Linux VM and
the next command pays the boot; it says so before it does it.

### What `doctor --json` says

The document a health check parses:

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
  fallback), `absent` (an optional backend that is not installed) or
  `FAIL`. `remedy` is present when there is one. On a Mac, `side` says
  whether the check is the Mac's (`host`) or the VM's (`vm`); on Linux
  there is one side and no field.
- `backends[]`: the isolation backends usable right now — the host has what
  each needs and this binary implements it. On a Mac these are the VM's.
- `ok`: no check failed. **The exit status is 0 exactly when `ok` is
  true**, and both are derived from the same list, so either can be
  trusted alone. A stopped VM is `ok: false`: nothing can vouch for the
  sandboxes until it is up, and the remedy says so.

## Still stuck

- `zygo run --dry-run --json <image>` prints the resolved configuration, the
  mount plan and the cgroup values, and runs nothing.
- `zygo spec explain <fn>` prints what a function resolved to, and where each
  value came from.
- `ZYGO_LOG=debug zygo <command>` turns on Zygo's own tracing.
- `zygo backend list` says which isolation backends this host can actually use,
  and why not for the others.

If it looks like an escape — anything reaching the host from inside a sandbox —
please report it privately. [SECURITY.md](../SECURITY.md) says how, and what is
in scope.
