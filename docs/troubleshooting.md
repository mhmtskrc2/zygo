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

`zygo doctor` detects it by attempting that mount and prints the fix. Read the
[threat model](threat-model.md) before applying it: the fix turns off a
protection for every process on the machine, not only Zygo's.

**On a Mac**, and the path is outside your home directory: the Linux VM mounts
`$HOME` and nothing else, so a path elsewhere has no counterpart inside it.
System temporary directories are the usual culprit, because macOS puts them
under `/var/folders`. Move the directory under `$HOME`.

### "no cgroup controllers" or "there is no `memory.max` here"

Your shell is in a cgroup that cannot delegate. On a systemd machine an ssh
login sits in a `session-N.scope` that systemd owns, and an unprivileged
process cannot create a cgroup inside it.

Zygo re-executes itself inside a transient scope when it finds this, so it
usually resolves itself. When it cannot:

```bash
systemd-run --user --scope -p Delegate=yes -- zygo run alpine:3 /bin/true
```

If that works and a bare `zygo run` does not, your `systemd-run` is refusing
the delegation. `loginctl enable-linger $USER` is often the missing piece.

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
sudo aa-complain /usr/bin/pasta
```

Or use `network = "none"`, the default, which needs no `pasta` at all.

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

## On a Mac

### Everything is slow

Crossing into the Linux VM costs about 100 ms per command, and that is the
floor for anything typed at a Mac shell. The millisecond warm path is real and
is reached through the HTTP API or the SDKs, where the hop is paid once by the
connection rather than once per request. See
[what Zygo costs](performance.md#on-a-mac).

### "limactl is not installed"

```bash
brew install lima
```

### "the Linux build is missing"

```bash
make poc/zygo-linux-musl
```

That is the binary that runs inside the VM. A release ships it beside the Mac
one; from a checkout it is one `make`.

### A command is refused because of where it was run

The VM mounts your home directory at the same path and nothing else. A command
from outside `$HOME` is refused, with both directories named, rather than
quietly running against a directory you are not looking at.

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
