# seccomp profiles

Every sandbox runs under a seccomp-bpf **allowlist**: a syscall that is not
named returns `EPERM`. That direction matters — a denylist silently gains a
hole every time the kernel grows a syscall, and it grows several per release.

Three profiles, chosen per function with `seccomp = "…"` or `--seccomp`:

| Profile | What it is | Who it is for |
|---|---|---|
| `default` | ~190 syscalls: the set five reference packages exercise their real code paths under — numpy's BLAS threads, Pillow's codecs, pandas' file I/O, pydantic's Rust core, requests' TLS setup. `clone` is allowed only with every `CLONE_NEW*` flag clear, so a sandbox cannot make a namespace; `ioctl` is allowed except for `TIOCSTI` and its relatives. `bpf`, `io_uring_*`, `userfaultfd`, `keyctl`, `perf_event_open`, `ptrace`, `mount` and `unshare` are absent. | Everyone (T1, T2) |
| `strict` | `default` minus the calls that reach the network — `socket`, `connect`, `bind`, `listen`, `accept4` — and minus `ptrace`, `mount`, `umount2`. In the agent's forked child, additionally minus `execve`, `execveat`, `fork`, `vfork` and any `clone` without `CLONE_THREAD` (see [the child filter](#the-child-filter)). | A `network = "none"` function whose author wants the kernel to refuse a socket, not merely the namespace to have nothing behind it |
| `permissive` | `default` plus `clone3`, `ptrace`, `unshare`, `setns`, `mount`, `pivot_root`, `chroot`, `mknod`, `process_vm_readv`/`writev`, `personality` and the rest of Docker's default profile. Those eight are the *only* appendix-B exclusions it grants, and a test asserts the list. | Debugging a package the tighter profiles break, and Zygo's own derived-layer builds, where `dpkg` uses the legacy `chown`/`chmod`/`mknod` calls. **Not a tenant profile.** |

## The compatibility matrix

`make seccomp-matrix-linux` installs six packages into one venv, inside the
image, and then imports and exercises each under both profiles — and then does
the same for Node, in a second container, because a matrix built entirely out
of CPython says nothing about a runtime that reaches the kernel differently.
Every cell is an attempt: the package is made to do the thing people use it
for, and the cell is what the sandbox returned.

| package | `default` | `strict` |
|---|---|---|
| requests 2.32.3 | works | works — the socket is refused with `EPERM`, which `requests` reports as its own `ConnectionError`; nothing hangs |
| httpx 0.27.2 | works | works — refused at client construction rather than at send, and reported as its own error |
| pydantic 2.9.2 | works | works |
| numpy 2.1.3 | works | works |
| pandas 2.2.3 | works | works |
| Pillow 11.0.0 | works | works |
| sqlite3 (stdlib) | works | works — on disk, so `fcntl` locking, `fsync` and `ftruncate` are all exercised |

| Node | `default` | `strict` |
|---|---|---|
| `worker_threads` | works | works — a thread is a `clone` **with** `CLONE_THREAD`, which the child filter permits |
| `crypto` + `zlib` + `fs` | works | works |
| `child_process` | works | refused, which is the point of the profile |

Measured on Linux 5.10 (aarch64, Docker Desktop). "Works" means the handler
returned `{"ok": true}` from a real operation — a validation error caught, a
million-element mean, a CSV grouped, an image resized and encoded, a worker
thread joined — not that the module imported.

## What the matrix found on its first run

**Every threaded program was dead under `default`.** `clone3` was only in
`permissive`, so under the other two profiles it returned `EPERM` like any
other unlisted syscall. glibc's `pthread_create` tries `clone3` first and falls
back to `clone` **only on `ENOSYS`**; on `EPERM` it gives up, and the program
sees `RuntimeError: can't start new thread`. `pip` starts a thread for its
progress bar on any download over a few megabytes, which is how the venv build
of this very matrix failed at `numpy`'s 13.6 MB wheel — and `numpy` itself
starts several on import. The profile had been "validated" against these
packages earlier, but with a JSON profile applied by a different tool, not
with the filter that ships.

`clone3` cannot simply be allowed: it takes its flags in a struct the filter
cannot read, so a profile that inspects `clone`'s namespace flags cannot let
`clone3` through unread. It now answers `ENOSYS`, which is what Docker's
profile does for the same reason, and glibc takes the `clone` path — where the
flags *are* checked.

**`strict` killed every function before its handler ran.** The first version
removed the whole socket family, data calls included: `sendto`, `recvfrom`,
`sendmsg`, `setsockopt`. The agent speaks to the supervisor over a socket it
*inherited*, and a socket it cannot `recvfrom` is a supervisor it cannot hear —
so every `strict` function died at start-up with "expected READY from the
agent, got end of stream". Transferring bytes on a descriptor a process was
handed is not a capability; opening one is. `strict` now removes creation and
keeps transfer, and a unit test holds the line in both directions.

## The child filter

The sandbox's filter cannot remove `execve`: the launcher installs it
immediately before exec-ing the agent, so a profile without `execve` is a
sandbox that cannot start. So that tightening goes where it belongs — in the agent's *forked child*, which is already running the
interpreter and never needs another program.

Under `strict` the supervisor builds a second program
([`STRICT_CHILD_REMOVED`] plus a flag check on `clone`) and hands it to the
agent as `ZYGO_CHILD_SECCOMP`: base64 of the raw `struct sock_filter` array,
so an agent in any language can install it without knowing a syscall number.
The reference agent decodes it once at start-up and, in every child, after
`GO` and before the handler, installs it with one `prctl(PR_SET_SECCOMP)`.
Filters stack; the kernel takes the strictest answer, so the sandbox's own
filter stays in force underneath.

What the child then cannot do: `execve` and `execveat` (no new program);
`fork`, `vfork` and a `clone` without `CLONE_THREAD` (no new process — a
handler cannot fork-bomb its way to `pids.max`, and `subprocess.run` fails
with `PermissionError` at the `clone`, or at the `execve` on a platform whose
libc uses `vfork`). What it still can: start a thread, which is a `clone`
*with* the flag. An agent that cannot install the filter fails the request
rather than running it unfiltered, and a malformed value is a start-up error
the supervisor sees.

Which agents honour it, and how. The Python agent installs it directly:
`ctypes`, one `prctl`, decoded once at start-up. The Node agent cannot —
there is no FFI in Node's standard library, and the filter has to go in
*after* the last `execve`, so a launcher that installs it and then execs
cannot work either. It therefore does one of two things and says which in
`READY`:

* **`seccomp`** — it loads
  [`agents/node/zygo_child_seccomp.c`](../agents/node/zygo_child_seccomp.c),
  forty lines built with one `cc` command, through `process.dlopen`, whose
  constructor installs the program before the "not a Node addon" error is
  thrown. The worker verifies it by reading `Seccomp_filters` from
  `/proc/self/status` and refuses to run the request if the count did not go
  up. This is the real thing: a kernel filter that survives arbitrary code
  execution inside the worker.
* **`node-permission`** — no helper object in the image, so the worker runs
  under Node's own permission model instead: no child processes, no native
  addons, no WASI, and worker threads still allowed, because the filter it is
  standing in for permits a `clone` with `CLONE_THREAD`. It removes what
  `strict` asks to remove, but it is Node enforcing it rather than the kernel,
  so a V8 escape gets past it where it would not get past a filter.

The `sh` example agent does neither and refuses every request under `strict`,
which is the protocol's other conforming answer. `zygo agent test` checks all
of this rather than trusting it: it starts a second copy of the agent with
`ZYGO_CHILD_SECCOMP` set, asks the handler to start a program, and fails an
agent that runs the request anyway.

The Rust and Python suites, `make verify-supervisor-linux` (a `subprocess.run`
under `default` and then under `strict`, and a thread under `strict`) and
every `strict` cell of the matrix above run the child under it.

## What is not done

* **The packages people will ask about next are not in the matrix**: anything
  with a JIT (`numba`, PyTorch), anything that opens a browser, and on the
  Node side `sharp` and `axios`. Add a row before relying on the answer.
* **`permissive` grants eight of appendix B's exclusions.** That is what the
  profile is for — it is how `dpkg` builds a derived layer — but it means
  `permissive` is an operator's debugging tool, not something to hand a
  tenant. A test asserts the exact list, so one more joining it is a decision
  rather than a detail: `io_uring_setup`, `io_uring_enter`,
  `io_uring_register` and `userfaultfd` were in that list until the escape
  suite started attempting every appendix-B vector against all three profiles
  and found them reachable.

[`STRICT_CHILD_REMOVED`]: ../crates/zygo-core/src/backend/ns/seccomp.rs
