# 12. One-shot sandboxes: `zygo run`

`zygo run` builds a fresh sandbox, runs one program in it, and removes the
sandbox when the program ends. It reads like `docker run`, but it has safe
defaults and leaves nothing behind. This chapter covers everything about it:
what happens inside, how to give the sandbox your files, and how to tell why
it ended.

## The shape of the command

```bash
zygo run [FLAGS] IMAGE [COMMAND ARGS...]
zygo run python:3.12-slim python3 -c 'print("hello")'
```

The *image* is a normal container image from any registry, such as Docker
Hub. Everything after the image is the *command*, which runs inside the
sandbox. With no command, the image's own entrypoint runs, just as with
Docker. Put Zygo's flags *before* the image; anything after it belongs to the
command.

## What happens, step by step

```text
  zygo run --mem 128M python:3.12-slim python3 app.py
     │
     ├─ 1. plan    read the flags and any sandbox.toml, check the limits,
     │             find the image in the store (pull it if missing)
     │
     ├─ 2. place   supervisor running? ── yes ─▶ hand the sandbox to it
     │                    │ no
     │                    └─▶ not in a delegated cgroup? re-exec in a systemd scope
     │
     ├─ 3. start   clone3 into new namespaces ─▶ the child builds its root,
     │             joins its cgroup, drops capabilities, adds Landlock + seccomp,
     │             and calls execve on your command
     │
     ├─ 4. run     your program runs; streams, signals and exit code pass through
     │
     └─ 5. end     the kernel removes the namespaces and the tmpfs,
                   Zygo removes the cgroup ─▶ nothing is left
```

These are the three *phases* that `--outcome` reports: `plan` (step 1),
`start` (steps 2 and 3), and `run` (steps 4 and 5). *Namespaces*, *cgroups*,
*capabilities*, *Landlock* and *seccomp* are explained in chapters
[2](02-namespaces.md), [3](03-cgroups.md) and [4](04-other-locks.md), and
[chapter 6](06-how-zygo-works.md#the-one-shot-sandbox-zygo-run) shows how
Zygo puts them together.

## What the sandbox has

Every sandbox starts closed. You open what you need, one flag at a time.

| It has | Default |
|---|---|
| A root filesystem | The image's layers, **read-only** |
| A writable `/tmp` | Sized by `--scratch`: the smaller of 64 MB and half of `--mem` |
| A home folder | `/tmp`, unless the image or you set `HOME`, so `pip` and `npm` caches have a place to write |
| Memory | `--mem 256M` |
| CPU | `--cpu 1.0`, one full core |
| Processes | `--pids 64` |
| Time | `--timeout 30s` |
| Open files | `--nofile 1024` |
| Network | none at all (`--net none`) |
| Capabilities | none |
| System calls | the `default` seccomp allowlist |

What it does **not** have: your files, your network, your other processes, or
any way to reach the image store it was built from.

## The first run and the second

The first run of an image pulls it, just as `docker run` does. After that,
the image is in Zygo's store under your home folder, and a run takes about
12 ms (median, image already pulled; the hosts are in
[chapter 25](25-performance.md)). The first run on a kernel older than 5.11
is also slower, because Zygo has to flatten the image's layers once.
[Pull policy](#pull-policy) below explains how to control pulling.

## Running your own code: mounts

The sandbox cannot see your filesystem. So a script of yours has to be
*mounted* in first. A mount makes a file or folder from your machine appear at
a path inside the sandbox. The interpreter, by contrast, comes from the image,
not from your machine — [chapter 6](06-how-zygo-works.md#why-not-zygo-venvbinpython-apppy)
explains why `zygo ./venv/bin/python app.py` is not a thing.

```bash
zygo run --mount ./hello.py:/hello.py:ro python:3.12-slim python3 /hello.py
zygo run --mount ./src:/src:ro python:3.12-slim python3 /src/main.py --flag
printf '{"n": 21}' | zygo run --mount ./src:/src:ro python:3.12-slim python3 /src/double.py
```

The form is `--mount HOST:GUEST[:ro|rw]`. *HOST* is the path on your machine,
*GUEST* is the path inside the sandbox. A mount is **read-only unless you add
`:rw`**. You can mount a single file or a whole folder, and you can repeat
`--mount` as often as you need.

```text
  your machine                          the sandbox
  ./src/main.py     ── --mount ─────▶   /src/main.py    (read-only)
  ./pkgs/           ── --mount :rw ─▶   /pkgs/          (writable, changes are real)
  everything else   ── not visible ──   (the sandbox cannot name it)
```

## Programs from your host

You can run a program from your own machine the same way: mount it, then
name it. It runs against the *image's* libraries, not your host's. So a
program that is *dynamically linked* (it loads shared libraries such as libc
when it starts) needs an image with a compatible libc. A *static* program
carries its libraries inside it and runs anywhere.

```bash
zygo run alpine:3 pwd                                             # the image's own pwd: /
zygo run --workdir /tmp alpine:3 pwd                              # /tmp
zygo run --mount /bin/pwd:/opt/pwd:ro python:3.12-slim /opt/pwd   # your host's pwd
```

A glibc `/bin/pwd` from Ubuntu runs in `python:3.12-slim`, which also uses
glibc. The same file fails in `alpine:3` with "the program does not exist",
because Alpine uses musl and the loader the program names is not there.

## Working folder, user and environment

| Flag | What it sets |
|---|---|
| `--workdir DIR` | The working folder inside the sandbox. |
| `--user UID` | The user id the program runs as inside the sandbox. The default is 1000. |
| `--env KEY=VALUE` | One environment variable. Repeat it for more. |

```bash
zygo run --env GREETING=hi --workdir /tmp alpine:3 sh -c 'echo $GREETING from $(pwd)'
```

Do not pass secrets with `--env`. Environment variables are easy to leak, for
example into logs or child processes. Warm functions have a safer way,
described in [chapter 14](14-limits-network-secrets.md).

## Limits on the command line

```bash
zygo run --mem 128M --pids 16 --timeout 10s alpine:3 /bin/sh
zygo run --cpu 0.5 --scratch 32M --nofile 256 python:3.12-slim python3 app.py
```

| Flag | Limits | Example |
|---|---|---|
| `--mem` | memory | `512M` |
| `--cpu` | CPU time, in cores | `0.5` |
| `--pids` | number of processes (stops a *fork bomb*: a program that copies itself without end) | `16` |
| `--timeout` | wall-clock time | `10s` |
| `--scratch` | size of the writable `/tmp` | `32M` |
| `--nofile` | open files | `256` |

A limit cannot be switched off unless you also pass `--allow-unlimited`.
[Chapter 14](14-limits-network-secrets.md) explains each limit.

## Isolation, seccomp and network flags

| Flag | What it chooses |
|---|---|
| `--isolation ns\|gvisor\|vm` | The *backend*: the kind of wall around the program. `ns` is the default. See [chapter 9](09-jails.md). |
| `--seccomp default\|strict\|permissive` | Which list of system calls is allowed. See [chapter 24](24-seccomp-profiles.md). |
| `--net none\|egress\|full\|host` | The network. `none` is the default. |
| `--allow HOST:PORT` | With `--net egress`, one name and port the program may reach. Repeatable. |
| `--allow-host-net` | Permits `--net host`, which removes the network wall. |
| `--allow-private-net` | Permits `--allow` rules that point at private or link-local addresses. |

```bash
zygo run --net egress --allow api.example.com:443 python:3.12-slim python3 /src/call.py
```

Each flag that makes the sandbox weaker has a name that says so. You cannot
remove a protection by accident.

## Input, output and signals

Standard input, standard output, standard error and the exit code pass
straight through, so `zygo run` fits in a shell pipe. Zygo's own messages,
such as `pulling python:3.12-slim`, go to standard error. `-q` or `--quiet`
hides them, which helps when a program or an AI model reads the output. It
never hides what the program itself writes, and errors are still reported.

Ctrl-C works in two steps. The **first** Ctrl-C (or `SIGTERM`, or `SIGHUP`)
is passed on to the program and every process it started, so a Python
program gets its `KeyboardInterrupt`. The **second** signal of any kind sends
`SIGKILL` to the sandbox's first process, which ends the whole sandbox.

```text
  Ctrl-C #1 ──▶ SIGINT to the program and its process group   ("please stop")
  Ctrl-C #2 ──▶ SIGKILL to the sandbox's pid 1 ─▶ whole sandbox gone   ("stop now")
```

A signal that the shell told `zygo` to ignore, for example under `nohup`, is
not passed on.

## A terminal of its own: `--tty`

Zygo has no daemon, so by default the sandbox *inherits your terminal*. That
is why `zygo run alpine:3 sh` feels like a local command: colours, prompts and
job control just work. The cost is that code in the sandbox holds a writable
handle to your real terminal.

`-t` or `--tty` gives the sandbox a *pseudo-terminal* of its own instead: a
fake terminal that Zygo creates. Zygo keeps the other end and copies bytes
between it and your real terminal, so the sandbox never touches the real one.

```text
  default:   your terminal ◀──────────────────────────▶ sandbox (holds your terminal)
  --tty:     your terminal ◀──▶ zygo ◀──▶ new pty ◀──▶ sandbox (never sees yours)
```

```bash
zygo run --tty alpine:3 /bin/sh
```

Why is `--tty` not the default? Because the default is already safe, and it
is what makes a sandbox feel like a local command. The dangerous use of that
handle, pushing keystrokes into your terminal with `TIOCSTI`, is blocked by
the seccomp filter either way. `--tty` goes one step further and removes the
handle completely. A run with `--tty` always stays in your shell and is never
handed to a supervisor.

## Coming from Docker

The sentence is the same as `docker run`, the defaults are the opposite, and
no container object is left behind for you to remove.
[Chapter 5](05-docker.md) and [chapter 10](10-similar-projects.md) compare the
two flag by flag.

One trap: in Zygo, `-v` means *verbose*, not *volume*. If you type
`zygo run -v $PWD:/src image` in the folder `/app`, Zygo sees that the
"image" looks like a mount and says so:

```text
`/app:/src` is a mount, not an image
  → `-v` means verbose in Zygo, not volume; use: zygo run --mount /app:/src <image> …
```

## Look before you run: `--dry-run`

```bash
zygo run --dry-run --json python:3.12-slim
```

`--dry-run` prints the plan and runs nothing. It shows:

- the resolved settings, after defaults, `sandbox.toml` and flags are merged;
- the mount plan: every path the sandbox will see, and where it comes from;
- the seccomp profile, where it came from (the flag, the spec or the
  default), and how many system calls it allows;
- whether Landlock applies on this host;
- the cgroup values that will be written, and how the deadline is enforced.

This is how you review a sandbox's walls before you trust them. `--json`
makes the plan machine-readable, so two plans can be compared with `diff`.

## Exit codes

The exit code is the program's own, with a few exceptions that belong to
Zygo.

| Status | Meaning |
|---|---|
| the program's own | It ran, and this is what it returned. |
| **137** | Zygo or the kernel killed it: the deadline or the memory limit. `--outcome` says which. |
| **2** | The spec or the flags are wrong. |
| **125** | This host cannot run sandboxes, or the chosen backend is not available. `zygo doctor` says why. |
| **1** | Any other Zygo error, such as a missing image under `--pull never`. |
| **111** | macOS only: the Linux VM could not be reached. See [chapter 22](22-troubleshooting.md). |
| **75** | `zygo exec` only: the function is at its concurrency limit. Retry. |
| **4** | `zygo exec` only: no function has that name. |

A program can also return 1, 2 or 125 itself. When the difference matters,
use `--outcome`.

## Knowing why a sandbox ended: `--outcome`

A deadline kill and an out-of-memory kill both exit **137**. Both are a
`SIGKILL`, and the exit status carries nothing more. When the difference
matters, for example in an online judge or a CI step, ask for an outcome
file:

```bash
zygo run --outcome /tmp/why.json --mem 64M --timeout 5s python:3.12-slim python3 big.py
cat /tmp/why.json
```

```json
{"exit_code":137,"timed_out":false,"oom_killed":true,"peak_rss_kb":65780,
 "wall_ms":412.7,"plan_ms":3.1,"start_ms":9.8,"started":true,"phase":"run"}
```

The three times in this example are only an illustration; they depend on
the host. The report goes to a file because standard output belongs to the
program.
Zygo writes the file whole and then renames it into place, so a reader never
sees half a file.

## The outcome fields

| Field | Meaning |
|---|---|
| `exit_code` | The exit status, as above. |
| `timed_out` | The deadline killed it. This comes from the launcher, which enforced the deadline. |
| `oom_killed` | The kernel killed something for using too much memory. This comes from the kernel's own counter in the sandbox's cgroup. |
| `peak_rss_kb` | The most memory it used at once, in KB. |
| `wall_ms` | The program's own time, from `execve` to exit, including tearing the sandbox down. |
| `plan_ms` | Time before the sandbox: reading the spec, the image store, checking the host. |
| `start_ms` | Time to build the sandbox, up to and including `execve`. |
| `started` | Whether the program ran at all. |
| `phase` | `run` if the program ran; otherwise the phase that failed, `plan` or `start`. |

Neither `timed_out` nor `oom_killed` is a guess. The three times tell you
*which* part was slow when one run takes much longer than usual.

## When the sandbox never started

The outcome file is also written when the sandbox **never started**: the
image is not there, the host cannot run sandboxes, or a mount does not exist.
Then it says `"started": false` and names the `phase` that failed. A program
that ran has `"started": true` and `"phase": "run"`.

```text
  "started": false, "phase": "plan"   ─▶ not set up (missing image, bad spec)   ─▶ unavailable
  "started": false, "phase": "start"  ─▶ the host could not build the sandbox   ─▶ unavailable
  "started": true,  "phase": "run"    ─▶ the code ran; exit_code is its own     ─▶ its own result
```

This is the difference between *the service is unavailable* and *the code
failed*. The exit status cannot carry it, and a caller should not have to
read standard error to find it.

## Pull policy

`--pull` says when `zygo run` downloads the image.

| Value | Behaviour |
|---|---|
| `missing` (default) | Pull only when the image is not in the store, like `docker run`. |
| `never` | Never pull. A missing image is refused before anything starts: exit 1, and the outcome file says `"phase": "plan"`. |
| `always` | Pull every time, to pick up a tag that has moved to a new image. |

`never` is for a caller that keeps its own clock, such as a judge with a time
limit. A pull takes minutes and a run takes seconds. Without `never`, a run
that quietly became a pull looks like a program that was too slow. With it,
the answer comes in a millisecond. Pull images ahead of time with
`zygo pull IMAGE`.

## Python packages: `--requirements`

```bash
zygo run --requirements ./requirements.txt --mount ./src:/src:ro \
    python:3.12-slim python3 /src/main.py
```

`--requirements FILE` installs the packages in a Python *venv* (a folder of
installed packages) and mounts it at `/venv`, with `/venv/bin` first on
`PATH`. The venv is built once and cached. The cache key is the image's
digest plus the bytes of the file, so the next run with the same image and
the same file reuses it. It is the same cache `zygo serve` uses, so a one-shot
run and a warm function with the same requirements share one venv.
[Chapter 15](15-images-and-dependencies.md) explains the cache.

## Installing packages into a mounted folder

Many products need an "install this customer's packages" feature. The shape
is always the same: one *networked* one-shot installs into a mount, and then
any number of sandboxes *without* network import from it.

```bash
mkdir -p ./pkgs
zygo run --net full --mount ./pkgs:/pkgs:rw python:3.12-slim \
    python3 -m pip install --target /pkgs python-dateutil
zygo run --mount ./pkgs:/pkgs:ro --env PYTHONPATH=/pkgs python:3.12-slim \
    python3 -c 'import dateutil, six; print(dateutil.__version__)'
```

```text
  step 1 (once):   network on  ──▶ pip install ──▶ ./pkgs  (mounted rw)
  step 2 (often):  network off ──▶ import from ──▶ ./pkgs  (mounted ro)
```

## What the package install costs

On the 2-core VM of an adoption test (a real multi-tenant product moved onto
Zygo), `python-dateutil` and `six` installed in **2.8 s** through Zygo,
against 3.9 s through Docker.

This used to fail with `[Errno 1] Operation not permitted` on a `RECORD`
file. `pip install --target` copies each file with Python's `shutil.copy2`,
which calls `listxattr` (a call that lists a file's extended attributes), and
the seccomp profiles did not allow it. The workaround was to point `TMPDIR`
at the same mount. Every profile now allows the whole extended-attribute
family, so neither the error nor the workaround remains, and
`make verify-seccomp-profiles-linux` checks that it stays that way.

## Using a `sandbox.toml`: `[defaults]` and `-f`

`zygo run` does not need a spec file; the flags alone are enough. But if a
`sandbox.toml` exists, `zygo run` uses it. It looks in the current folder and
then in each parent folder, or reads the file you name with `-f`.

```text
  built-in defaults ──▶ the spec's [defaults] ──▶ your flags   (later wins)
                        ([fn.*] tables are ignored by zygo run)
```

```bash
zygo run -f ./ci/sandbox.toml python:3.12-slim python3 -m pytest
```

So a team can write its limits once in `[defaults]`, and every `zygo run` in
that project uses them. The `[fn.*]` tables describe warm functions and are
ignored here. [Chapter 20](20-sandbox-toml.md) describes the file.

## Handing the run to the supervisor

On Linux, when a supervisor is already running (for example after
`zygo serve`), `zygo run` gives the sandbox to it. The supervisor already
sits in a delegated cgroup, so the run skips the systemd scope described
below. Planning and pulling still happen in your process; the supervisor only
builds and runs the sandbox, and your process passes its streams and signals
and waits.

| `zygo run python:3.12-slim python3 -c pass`, usually | |
|---|---|
| In its own systemd scope | 25.7 ms |
| Through a running supervisor | 13.5 ms |
| Measured on | Ubuntu 24.04 VM, kernel 6.8 |

The hand-off does not happen with `--dry-run` (nothing runs), with `--tty`
(the terminal must stay in your shell's session), or with an isolation other
than `ns`. `zygo run -v` tells you which path was taken: its timing line ends
in `(through the supervisor)` when the supervisor ran it.

## The systemd scope

On a systemd machine, a login shell lives in a cgroup that you are not
allowed to split up. A sandbox needs a cgroup of its own for its limits, and
Zygo refuses to run a sandbox with no limits. So `zygo run`, `pull`, `serve`,
`up` and `bench` first check where they stand. If they are not in a delegated
cgroup, they start again inside a temporary one:

```text
  $ zygo run ...                         (login shell cgroup: not delegated)
      └─▶ systemd-run --user --scope -p Delegate=yes zygo run ...
             └─▶ zygo run ...            (own scope: delegated, can make cgroups)
                    └─▶ sandbox
```

This costs about 10 to 15 ms: the scope, a second process, and a cgroup tree
built and removed. In a container, a systemd service, or a session that is
already delegated, nothing is done. A guard stops Zygo from restarting itself
forever: if the new scope is still not usable, you get Zygo's own error.

## Output for scripts

| Flag | What it does |
|---|---|
| `--json` (global) | Machine-readable output, for example with `--dry-run`. |
| `-v` (global, repeat for more) | More detail from Zygo, including the timing line. |
| `-q`, `--quiet` | Hide Zygo's own progress messages. |
| `--data-root DIR` (global) | Keep the image store and state in `DIR` instead of the default under your home folder. |
| `--outcome FILE` | Write why the sandbox ended, as JSON. |

## Every `zygo run` flag in one table

| Flag | Section |
|---|---|
| `--mem` `--cpu` `--pids` `--timeout` `--scratch` `--nofile` | [Limits](#limits-on-the-command-line) |
| `--isolation` `--seccomp` `--net` `--allow` | [Isolation, seccomp and network](#isolation-seccomp-and-network-flags) |
| `--allow-host-net` `--allow-private-net` `--allow-unlimited` | [Limits](#limits-on-the-command-line) and [network](#isolation-seccomp-and-network-flags) |
| `--mount HOST:GUEST[:ro\|rw]` | [Mounts](#running-your-own-code-mounts) |
| `--env` `--user` `--workdir` | [Working folder, user and environment](#working-folder-user-and-environment) |
| `-t`, `--tty` | [A terminal of its own](#a-terminal-of-its-own---tty) |
| `--requirements FILE` | [Python packages](#python-packages---requirements) |
| `--pull missing\|never\|always` | [Pull policy](#pull-policy) |
| `--outcome FILE` | [Knowing why a sandbox ended](#knowing-why-a-sandbox-ended---outcome) |
| `--dry-run` | [Look before you run](#look-before-you-run---dry-run) |
| `-q`, `--quiet` | [Output for scripts](#output-for-scripts) |
| `-f`, `--file` | [Using a `sandbox.toml`](#using-a-sandboxtoml-defaults-and--f) |

[Chapter 19](19-commands.md) lists every other command.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [11. Getting started](11-getting-started.md) · [Contents](README.md) · **Next: [13. Warm functions](13-warm-functions.md) →**
