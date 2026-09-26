# 24. Seccomp profiles

Every Zygo sandbox runs under a seccomp filter, and you pick one of three
*profiles* for it: `default`, `strict` or `permissive`. This chapter says what
each one allows and refuses, which real packages have been tested under each,
and how to choose. [Chapter 4](04-other-locks.md#seccomp) explains what seccomp
is, if you have not met it yet.

## An allowlist, not a denylist

A *syscall* is a request from a program to the kernel: open a file, start a
thread, make a socket. *seccomp* lets a process install a small filter that
the kernel runs on every syscall, and that answers "allow" or "refuse". Zygo's
filter is an **allowlist**: a syscall that is not named returns `EPERM`
("Operation not permitted"). The direction matters. A *denylist* names what is
blocked, so it quietly gains a hole every time the kernel adds a syscall, and
it adds several per release. An allowlist blocks a new syscall until someone
decides to allow it.

```text
  denylist (Docker)                      allowlist (Zygo)
  ─────────────────                      ────────────────
  "block these"                          "allow these"
  kernel adds new_syscall() ──▶ ALLOWED  kernel adds new_syscall() ──▶ refused
  until someone notices                  until someone chooses to allow it
```

## Refused, or never heard of

A refusal comes in two forms, and the difference matters to a program. Zygo
carries a table of every syscall in the Linux 6.10 headers. A syscall in that
table that the profile does not allow answers `EPERM`: "this exists, and you
may not". A number *above* the table — a syscall newer than this build —
answers `ENOSYS`: "there is no such syscall here". That is the truth, and it is
the one answer a C library falls back from. glibc tries `fchmodat2` for
`chmod` and uses the old call only on `ENOSYS`; on `EPERM`, `python3 -m venv`
fails.

Each syscall that Linux 5.11 to 6.10 added was decided on its own. The ones
that are a newer form of something already allowed are allowed: `fchmodat2`,
`epoll_pwait2`, the `futex_*` family. So are Landlock and `mseal`, which only
take power away from the caller. The new mount API, `mount_setattr`,
`pidfd_getfd`, `memfd_secret`, `cachestat`, `statmount`, `listmount` and the
`lsm_*` calls are refused. A test fails when the table grows past what has
been decided.

```text
  syscall number
  0 ─────────────────── in the table (≤ 462) ──────────────────┬──── above ────▶
  allowed by the profile ──▶ runs                               │
  in the table, not allowed ──▶ EPERM  "exists, and refused"    │ ENOSYS
                                                                │ "no such call"
```

## The three profiles at a glance

You choose a profile per function with `seccomp = "…"` in `sandbox.toml`, or
with `--seccomp` on the command line. The three are nested: `strict` is a
subset of `default`, which is a subset of `permissive`. Anything outside all
three is refused under every profile, `permissive` included.

```text
  ┌───────────────────────────────────────────────────────────────────┐
  │ never allowed: anything in no list (a new syscall, most of the    │
  │ kernel's rarely used calls)                                       │
  │  ┌─────────────────────────────────────────────────────────────┐  │
  │  │ permissive = default + clone3, ptrace, unshare, setns,      │  │
  │  │   mount, pivot_root, chroot, mknod, process_vm_readv/writev,│  │
  │  │   personality … (Docker's default set)   NOT for tenants    │  │
  │  │  ┌───────────────────────────────────────────────────────┐  │  │
  │  │  │ default ≈ 215 syscalls   for everyone (T1, T2)        │  │  │
  │  │  │ clone only without CLONE_NEW*; ioctl minus TIOCSTI    │  │  │
  │  │  │  ┌─────────────────────────────────────────────────┐  │  │  │
  │  │  │  │ strict = default − socket, connect, bind,       │  │  │  │
  │  │  │  │   listen, accept4, ptrace, mount, umount2       │  │  │  │
  │  │  │  │   (+ child filter: no execve, no new process)   │  │  │  │
  │  │  │  └─────────────────────────────────────────────────┘  │  │  │
  │  │  └───────────────────────────────────────────────────────┘  │  │
  │  └─────────────────────────────────────────────────────────────┘  │
  └───────────────────────────────────────────────────────────────────┘
```

| Profile | What it is | Who it is for |
|---|---|---|
| `default` | ~215 syscalls: the set five reference packages exercise their real code paths under — numpy's BLAS threads, Pillow's codecs, pandas' file I/O, pydantic's Rust core, requests' TLS setup. `clone` is allowed only with every `CLONE_NEW*` flag clear, so a sandbox cannot make a namespace; `ioctl` is allowed except for `TIOCSTI` and its relatives. `bpf`, `io_uring_*`, `userfaultfd`, `keyctl`, `perf_event_open`, `ptrace`, `mount` and `unshare` are absent. | Everyone (T1, T2) |
| `strict` | `default` minus the calls that reach the network — `socket`, `connect`, `bind`, `listen`, `accept4` — and minus `ptrace`, `mount`, `umount2`. In the agent's forked child, additionally minus `execve`, `execveat`, `fork`, `vfork` and any `clone` without `CLONE_THREAD` (see [the child filter](#the-child-filter)). | A `network = "none"` function whose author wants the kernel to refuse a socket, not merely the namespace to have nothing behind it — and **every runtime pool by default**, because a pool's child runs a script that arrived over an API |
| `permissive` | `default` plus `clone3`, `ptrace`, `unshare`, `setns`, `mount`, `pivot_root`, `chroot`, `mknod`, `process_vm_readv`/`writev`, `personality` and the rest of Docker's default profile. Those eight are the *only* appendix-B exclusions it grants, and a test asserts the list. **It is not "no filter"**: a syscall outside all three lists is refused under `permissive` too. | Debugging a package the tighter profiles break, and Zygo's own derived-layer builds, where `dpkg` uses the legacy `chown`/`chmod`/`mknod` calls. **Not a tenant profile.** |

(T1 and T2 are the trust classes from [chapter 23](23-security.md#the-three-trust-classes):
your own team, and known, paying customers. "Appendix B" is the design's list
of syscalls a sandbox should never need.)

## `default`

`default` is what a function gets when its author does not choose. It names
about 215 syscalls (216 in the source; 190 of them exist on aarch64 and all
of them on x86_64, and a name the kernel does not have is left out of the
filter), found by running five reference packages through their real work:
numpy's BLAS threads, Pillow's image codecs, pandas' file I/O, pydantic's
Rust core, and requests' TLS setup. `clone`, the call that makes a
new process or thread, is allowed only when no `CLONE_NEW*` flag is set, so a
sandbox cannot make a new namespace. `ioctl` is allowed except for `TIOCSTI`
and its relatives, which could push keystrokes into a terminal. `bpf`,
`io_uring_*`, `userfaultfd`, `keyctl`, `perf_event_open`, `ptrace`, `mount`
and `unshare` are not in it; each is a large, complex part of the kernel
that has had serious bugs.

## `strict`

`strict` is `default` without the calls that *open* network connections —
`socket`, `connect`, `bind`, `listen`, `accept4` — and without `ptrace`,
`mount` and `umount2`. It is for a `network = "none"` function whose author
wants the kernel itself to refuse a socket, not just a namespace with nothing
behind it. It is also the default for **every runtime pool**, because a pool's
child runs a script that arrived over an API. In the agent's forked child,
`strict` also removes `execve`, `execveat`, `fork`, `vfork` and any `clone`
without `CLONE_THREAD`; [the child filter](#the-child-filter) explains how.

## `permissive`

`permissive` is `default` plus `clone3`, `ptrace`, `unshare`, `setns`,
`mount`, `pivot_root`, `chroot`, `mknod`, `process_vm_readv`/`writev`,
`personality` and the rest of Docker's default profile. Those eight are the
*only* appendix-B exclusions it grants, and a test checks the exact list. It
is for debugging a package that the tighter profiles break, and for Zygo's own
derived-layer builds, where `dpkg` uses the old `chown`/`chmod`/`mknod` calls.
**It is not a tenant profile.**

## `permissive` is not "seccomp off"

A reader once used this profile wrongly and drew the wrong conclusion, so it
is worth saying plainly. `permissive` means "the default plus namespaces,
mounts, ptrace and friends" — the set Docker's default profile allows. It is
**not** "no filter", and it is not the "turn seccomp off" step when you debug.
An early user tried `--seccomp permissive` against a failure whose
cause (`listxattr`, below) was in *no* list, saw no change, and decided the
flag did nothing. It did work; the syscall was simply missing from all three
profiles. If something fails under `permissive` too, follow the
[troubleshooting entry for `EPERM`](22-troubleshooting.md#errno-1-operation-not-permitted-naming-a-file-that-exists-and-is-readable),
which shows how to find the syscall.

## Seeing which profile applies

`zygo run --dry-run` prints the resolved profile, where the choice came from,
and how many syscalls it names. So you can see that the flag took effect
without running anything.

## The extended-attribute family

*Extended attributes* are small name–value labels stored on a file, next to
its normal owner and mode. Every profile allows the whole family —
`getxattr`, `listxattr`, `setxattr`, `removexattr` and their `l`/`f` forms —
since an early user found `listxattr` missing. `shutil.copy2` calls
it, and `pip install --target` is one `copy2` per file, so a profile without it
broke every Python package install into a mounted folder, with a traceback
about `RECORD`.

`strict` keeps them on purpose. Reading and listing attributes on a filesystem
the sandbox owns leaks nothing. A `user.*` write is bounded by the mount. The
kernel refuses `trusted.*` and `security.*` to an unprivileged uid before any
filter is asked. And a `network = "none"` function copies files like any other.
A unit test holds all twelve calls under all three profiles, and
`make verify-seccomp-profiles-linux` does the `copy2` and the `pip install`
on a real kernel.

## How to choose

```text
  Is the code from someone you do not know, arriving over an API?
     │
     ├─ yes ─▶ strict   (the default for a [runtime.<name>] pool)
     │
     └─ no ──▶ Does the function need no network, and do you want the
               kernel itself to refuse sockets?
                 │
                 ├─ yes ─▶ strict
                 │
                 └─ no ──▶ default   (the default for a [fn.<name>])
                             │
                             └─ a package breaks under default?
                                  ▶ try permissive ONCE, on your own machine,
                                    to find the cause; then report the syscall.
                                    Never hand permissive to a tenant.
```

| you want | profile |
|---|---|
| a normal function, your own or a known customer's | `default` |
| a function with `network = "none"` that must not even make a socket | `strict` |
| a runtime pool running scripts from an API | `strict` (already the default) |
| to find out whether seccomp is why a package fails | `permissive`, once, then report |
| to build a derived system layer with `dpkg` | `permissive` (Zygo does this itself) |

## The compatibility matrix

`make seccomp-matrix-linux` installs six packages into one venv inside the
image, then imports and uses each one under every profile. Then it does the
same for Node, in a second container, because a matrix built only from CPython
says nothing about a runtime that talks to the kernel differently. Every cell
is an attempt: the package is made to do the thing people use it for, and the
cell is what the sandbox returned.

The third column is the same code again, run as a **script in a runtime
pool**: one warm zygote holding no tenant code, the script written into the
sandbox per request, and `strict` as the pool's default rather than something
the caller asked for.

| package | `default` | `strict` | pool (`strict`) |
|---|---|---|---|
| requests 2.32.3 | works | works — the socket is refused with `EPERM`, which `requests` reports as its own `ConnectionError`; nothing hangs | works |
| httpx 0.27.2 | works | works — refused at client construction rather than at send, and reported as its own error | works |
| pydantic 2.9.2 | works | works | works |
| numpy 2.1.3 | works | works | works |
| pandas 2.2.3 | works | works | works |
| Pillow 11.0.0 | works | works | works |
| sqlite3 (stdlib) | works | works — on disk, so `fcntl` locking, `fsync` and `ftruncate` are all exercised | works |

| Node | `default` | `strict` | pool (`strict`) |
|---|---|---|---|
| `worker_threads` | works | works — a thread is a `clone` **with** `CLONE_THREAD`, which the child filter permits | works |
| `crypto` + `zlib` + `fs` | works | works | works |
| `child_process` | works | refused, which is the point of the profile | refused |

Measured on Linux 6.8 (aarch64, the Lima VM) on 26 September 2026, with the
same result as on Linux 5.10 (Docker Desktop), where it was first run.
"Works" means the handler
returned `{"ok": true}` from a real operation — a validation error caught, a
mean over a million numbers, a CSV grouped, an image resized and encoded, a
worker thread joined — not just that the module imported.

## Checking the profiles themselves

`make verify-seccomp-profiles-linux` is the short form of the same idea, for
the *profiles* rather than the packages. From inside a sandbox, `unshare`
succeeds under `permissive` and returns `EPERM` under `default`, and `socket`
succeeds under `default` and returns `EPERM` under `strict`. The unit tests
prove the lists differ; this proves the sandbox does. `make fuzz-linux` is the
long form: every syscall number under every profile (see
[chapter 23](23-security.md#how-the-controls-are-tested)).

```text
  unit tests                 the lists in the code differ
  verify-seccomp-profiles    a real sandbox answers differently per profile
  seccomp-matrix             real packages do real work under each profile
  fuzz-linux                 all 469 syscall numbers × 3 profiles
```

## What the matrix found on its first run

The first run of the matrix found two bugs that made whole profiles useless.
Both are fixed, and tests now hold the fixes in place.

### Every threaded program was dead under `default`

`clone3` was only in `permissive`, so under the other two profiles it returned
`EPERM`, like any other unlisted syscall. glibc's `pthread_create` tries
`clone3` first and falls back to `clone` **only on `ENOSYS`**; on `EPERM` it
gives up, and the program sees `RuntimeError: can't start new thread`. `pip`
starts a thread for its progress bar on any download over a few megabytes.
That is how the venv build of this very matrix failed at `numpy`'s 13.6 MB
wheel — and `numpy` itself starts several threads on import. The profile had
been "validated" against these packages earlier, but with a JSON profile
applied by a different tool, not with the filter that ships.

`clone3` cannot simply be allowed. It takes its flags in a struct in memory,
which the filter cannot read, so a profile that checks `clone`'s namespace
flags cannot let `clone3` through unchecked. It now answers `ENOSYS`, which is
what Docker's profile does for the same reason, and glibc takes the `clone`
path, where the flags *are* checked.

```text
  pthread_create
     │
     ▼
  clone3(...) ──▶ filter ──▶ EPERM  ──▶ glibc gives up: "can't start new thread"   (before)
                         └─▶ ENOSYS ──▶ glibc falls back
                                          │
                                          ▼
                                   clone(flags) ──▶ filter checks: no CLONE_NEW* ──▶ ok
```

### `strict` killed every function before its handler ran

The first version of `strict` removed the whole socket family, including the
calls that only move data: `sendto`, `recvfrom`, `sendmsg`, `setsockopt`. But
the agent talks to the supervisor over a socket it *inherited*, and a socket
it cannot `recvfrom` is a supervisor it cannot hear. So every `strict`
function died at start-up with "expected READY from the agent, got end of
stream". Moving bytes on a descriptor a process was handed is not a new power;
opening one is. `strict` now removes *creation* and keeps *transfer*, and a
unit test holds the line in both directions.

## The child filter

The sandbox's own filter cannot remove `execve`. The launcher installs the
filter just before it execs the agent, so a profile without `execve` would be
a sandbox that cannot start. So that extra lock goes where it belongs: in the
agent's *forked child*, which is already running the interpreter and never
needs another program.

Under `strict`, the supervisor builds a second filter program
([`STRICT_CHILD_REMOVED`](../../crates/zygo-core/src/backend/ns/seccomp.rs)
plus a flag check on `clone`). It hands it to the agent as
`ZYGO_CHILD_SECCOMP`: base64 of the raw `struct sock_filter` array, so an
agent in any language can install it without knowing a syscall number. The
reference agent decodes it once at start-up. Then, in every child, after `GO`
and before the handler, it installs it with one `prctl(PR_SET_SECCOMP)`.
Filters stack, and the kernel takes the strictest answer, so the sandbox's own
filter stays in force underneath.

```text
  sandbox (seccomp: strict)
  ┌───────────────────────────────────────────────────────────────────┐
  │ agent / zygote        reads ZYGO_CHILD_SECCOMP once at start-up   │
  │    │ fork() per request                                           │
  │    ▼                                                              │
  │ child ── GO ──▶ prctl(PR_SET_SECCOMP, child filter) ──▶ handler   │
  │                                                                   │
  │ the child now has TWO filters; the kernel takes the strictest:    │
  │   sandbox filter  (strict)                                        │
  │ + child filter    (no execve, execveat, fork, vfork,              │
  │                    no clone without CLONE_THREAD)                 │
  └───────────────────────────────────────────────────────────────────┘
```

### What the child can and cannot do

The child cannot `execve` or `execveat`: no new program. It cannot `fork`,
`vfork`, or `clone` without `CLONE_THREAD`: no new process. So a handler cannot
fork-bomb its way up to `pids.max`, and `subprocess.run` fails with
`PermissionError` at the `clone` (or at the `execve`, on a platform whose libc
uses `vfork`). It still can start a thread, which is a `clone` *with* the flag.
An agent that cannot install the filter fails the request rather than running
it unfiltered. A malformed value is a start-up error that the supervisor sees.

### Which agents honour it, and how

The **Python agent** installs it directly: `ctypes`, one `prctl`, decoded once
at start-up. The **Node agent** cannot do that. Node's standard library has no
FFI (a way to call C functions), and the filter must go in *after* the last
`execve`, so a launcher that installs it and then execs cannot work either.
So the Node agent does one of two things, and says which in `READY`:

- **`seccomp`** — it loads
  [`agents/node/zygo_child_seccomp.c`](../../agents/node/zygo_child_seccomp.c),
  forty lines built with one `cc` command, through `process.dlopen`. Its
  constructor installs the filter before the "not a Node addon" error is
  thrown. The worker checks it by reading `Seccomp_filters` from
  `/proc/self/status`, and refuses to run the request if the count did not go
  up. This is the real thing: a kernel filter that survives any code running
  inside the worker.
- **`node-permission`** — the image has no helper object, so the worker runs
  under Node's own permission model instead: no child processes, no native
  addons, no WASI, and worker threads still allowed, because the filter it
  stands in for permits a `clone` with `CLONE_THREAD`. It removes what
  `strict` asks to remove, but it is Node enforcing it, not the kernel, so a
  V8 escape gets past it where it would not get past a filter.

The **`sh` example agent** does neither, and refuses every request under
`strict`. That is the protocol's other correct answer. `zygo agent test`
checks all of this rather than trusting it: it starts a second copy of the
agent with `ZYGO_CHILD_SECCOMP` set, asks the handler to start a program, and
fails an agent that runs the request anyway.

| agent | under `strict` | enforced by |
|---|---|---|
| Python | installs the child filter with `ctypes` + `prctl` | the kernel |
| Node, helper object in the image | loads `zygo_child_seccomp.c`, checks `Seccomp_filters`; `READY` says `seccomp` | the kernel |
| Node, no helper object | Node's permission model; `READY` says `node-permission` | Node (weaker against a V8 escape) |
| `sh` example | refuses every request | — |

## `strict` is the default for a runtime pool

A `[fn.<name>]` gets `default` unless its author says otherwise, and a
`[runtime.<name>]` gets **`strict`**. The difference is who the child is. A
function's child runs code that the function's own author deployed, and
`default` is the profile that author chose by not choosing. A pool's child runs
a script that arrived over an API, from somebody who may never have met the
operator.

A pool may name a lower profile — an operator may know their tenants — and
then resolution prints a warning saying what that allows. The pool column of
the matrix above is the same packages run as *scripts* in a pool, so this
default is measured, not just claimed.

### The filter goes on before the script's first line

The filter is installed **before the script's first line**, not only before
the handler is called. A script's module body is request code too, and an
agent that loaded it first would give a `strict` pool nothing.
`zygo agent test --script-spawn <file>` is the check: a script whose module
body starts a program, run once without the filter to prove it can, and once
under it to prove it cannot.

The Rust and Python test suites, `make verify-supervisor-linux` (a
`subprocess.run` under `default` and then under `strict`, and a thread under
`strict`), and every `strict` cell of the matrix above all run the child under
the child filter.

## What is not done

- **The packages people will ask about next are not in the matrix**: anything
  with a JIT (`numba`, PyTorch), anything that opens a browser, and on the
  Node side `sharp` and `axios`. Add a row before relying on the answer.
- **`permissive` grants eight of appendix B's exclusions.** That is what the
  profile is for — it is how `dpkg` builds a derived layer — but it means
  `permissive` is an operator's debugging tool, not something to hand a
  tenant. A test checks the exact list, so one more joining it is a decision,
  not a detail. `io_uring_setup`, `io_uring_enter`, `io_uring_register` and
  `userfaultfd` were in that list until the escape suite started trying every
  appendix-B attack against all three profiles and found them reachable.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [Fork safety, question by question](fork-safety.md) · [Contents](README.md) · **Next: [25. What Zygo costs](25-performance.md) →**
