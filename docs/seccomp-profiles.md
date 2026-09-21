# seccomp profiles

Every sandbox runs under a seccomp-bpf **allowlist**: a syscall that is not
named returns `EPERM`. That direction matters — a denylist silently gains a
hole every time the kernel grows a syscall, and it grows several per release.

Three profiles, chosen per function with `seccomp = "…"` or `--seccomp`:

| Profile | What it is | Who it is for |
|---|---|---|
| `default` | ~190 syscalls: the set five reference packages exercise their real code paths under — numpy's BLAS threads, Pillow's codecs, pandas' file I/O, pydantic's Rust core, requests' TLS setup. `clone` is allowed only with every `CLONE_NEW*` flag clear, so a sandbox cannot make a namespace; `ioctl` is allowed except for `TIOCSTI` and its relatives. `bpf`, `io_uring_*`, `userfaultfd`, `keyctl`, `perf_event_open`, `ptrace`, `mount` and `unshare` are absent. | Everyone (T1, T2) |
| `strict` | `default` minus socket creation — `socket`, `socketpair`, `connect`, `bind`, `listen`, `accept4` — and minus `ptrace`, `mount`, `umount2`. In the agent's forked child, additionally minus `execve`, `execveat`, `fork`, `vfork` and any `clone` without `CLONE_THREAD` (see [the child filter](#the-child-filter)). | A `network = "none"` function whose author wants the kernel to refuse a socket, not merely the namespace to have nothing behind it |
| `permissive` | `default` plus `clone3`, `ptrace`, `unshare`, `mount`, `chroot`, `mknod`, `personality` and the rest of Docker's default profile. | Debugging a package the tighter profiles break, and Zygo's own derived-layer builds, where `dpkg` uses the legacy `chown`/`chmod`/`mknod` calls |

## The compatibility matrix

`make seccomp-matrix-linux` installs the five packages into one venv, inside
the image, and then imports and exercises each under both profiles. Every cell
is an attempt: the package is made to do the thing people use it for, and the
cell is what the sandbox returned.

| package | `default` | `strict` |
|---|---|---|
| requests 2.32.3 | works | works — the socket is refused with `EPERM`, which `requests` reports as its own `ConnectionError`; nothing hangs |
| pydantic 2.9.2 | works | works |
| numpy 2.1.3 | works | works |
| pandas 2.2.3 | works | works |
| Pillow 11.0.0 | works | works |

Measured on Linux 5.10 (aarch64, Docker Desktop). "Works" means the handler
returned `{"ok": true}` from a real operation — a validation error caught, a
million-element mean, a CSV grouped, an image resized and encoded — not that
the module imported.

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
sandbox that cannot start. The design (§3.4.1) puts that tightening where it
belongs — in the agent's *forked child*, which is already running the
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

Which agents honour it: the Python reference agent does. The Node and sh
example agents do not — neither can call `prctl` without native code — so a
`strict` function on those agents has the sandbox filter and not this one. The
Rust and Python suites, `make verify-supervisor-linux` (a `subprocess.run`
under `default` and then under `strict`, and a thread under `strict`) and
every `strict` cell of the matrix above run the child under it.

## What is not done

* **Two packages that people will ask about are not in the matrix**: anything
  with a JIT (`numba`, PyTorch) and anything that opens a browser or a
  subprocess. Add a row before relying on the answer. (A subprocess is now a
  known answer under `strict`: refused.)
* **`zygo agent test` does not check the child filter.** It would need a
  handler contract for "try to start a program", and the suite deliberately
  asks handlers for three lines and nothing else.

[`STRICT_CHILD_REMOVED`]: ../crates/zygo-core/src/backend/ns/seccomp.rs
