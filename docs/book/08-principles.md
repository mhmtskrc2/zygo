# 8. The rules Zygo is built on

Zygo is built on eight rules, or *principles*. Each one is a choice, and each
choice has a cost. This chapter puts the cost right next to the rule, because
a rule whose cost is hidden is only a slogan.

## The eight rules at a glance

The rules fall into three groups: how a sandbox is made, how it stays fast,
and how it stays safe and honest. The table is the short version; the rest of
the chapter takes them one by one.

| | Rule | What it costs |
|---|---|---|
| P1 | A sandbox is a locked-down process, not a container | the wall is the Linux kernel; one kernel bug breaks every lock |
| P2 | The sandbox waits warm | a warm function uses memory while it waits |
| P3 | Isolation is one flag | `vm` is slower and does less; `gvisor` is one-shot only |
| P4 | OCI images, no Dockerfile | the first build of a derived layer takes about five seconds |
| P5 | No root, no daemon | the host must have a few programs installed |
| P6 | Safe by default | the first thing a new user hits is a limit |
| P7 | Every limit is required | disk I/O and bandwidth have no default |
| P8 | Honest about what is not proven | the status text says "not built" often |

```text
  ┌──────────── how it is made ────────────┐   ┌──────────── how it is fast ────────────┐
  │ P1 one process, no container machinery │   │ P2 the sandbox is ready before a call  │
  │ P4 any OCI image, dependencies in spec │   │ P3 one flag picks where the wall is    │
  │ P5 your user, no root, no daemon       │   │                                        │
  └────────────────────────────────────────┘   └────────────────────────────────────────┘
  ┌──────────────────────────── how it stays safe and honest ─────────────────────────────┐
  │ P6 locked by default · P7 every limit has a value · P8 every claim measured or marked │
  └───────────────────────────────────────────────────────────────────────────────────────┘
```

## P1: a sandbox is a locked-down process, not a container

A *container* in Docker's sense is an object managed by a chain of programs.
Zygo skips that chain. It builds the sandbox from kernel parts —
*namespaces* (separate views of the system), a *cgroup* (a group with resource
limits), *seccomp* (a filter on system calls) and *Landlock* (a limit on which
files a process may touch) — inside one process, with no *RPC* (a call to
another program over a socket). You met all of these in
[chapters 2 to 4](02-namespaces.md).

The steps are short. Zygo calls `clone3` to make a child process in seven new
namespaces. The child calls `pivot_root` to make the image its root folder,
drops all its *capabilities* (pieces of root's power), installs a *BPF filter*
(the small program that seccomp runs on each system call), and calls `execve`
to become your program. There is no *daemon* (a long-running background
service) to talk to, no *shim* (a helper process that babysits a container),
and no trip to an image service while a request waits.

```text
  zygo ──clone3──▶ child in 7 new namespaces
                     │
                     ├─ pivot_root onto the image
                     ├─ drop every capability
                     ├─ install the seccomp BPF filter (and Landlock)
                     └─ execve ─▶ your program
  no daemon · no shim · no RPC · no image service on the way
```

**What it costs.** The wall is the Linux kernel. A bug that lets a program
gain kernel privileges defeats every lock at once. That is why the `vm`
backend exists, for code you did not write. `zygo doctor` and the
[security chapter](23-security.md) say this plainly.

## P2: the sandbox waits warm

Nothing is built while a request waits. There are two ways Zygo does this.
For a compiled program the sandbox is *held*: namespaces, mounts and locks are
set up once, and each request is a fresh process that enters them. That costs
about 2 ms. For an interpreter such as Python, the sandbox holds an *agent*, a
small program that has already imported your handler. Each request is a
`fork()` of it — a copy of the process. The copy is *copy-on-write*: memory is
shared until one side changes it, so nothing is copied up front. And because
each request is a fresh process, no state carries over from the last one.

Measured: usually 1.4 ms at 250 requests per
second, on the machines named in [chapter 25](25-performance.md).

```text
  compiled program (warm-exec)            interpreter (agent)
  ────────────────────────────            ───────────────────
  held sandbox                            held sandbox + agent with handler loaded
     │                                       │
     ├─ new process enters ─▶ run ~1.4 ms    ├─ fork ─▶ handler(event) ~1.4 ms
     └─ new process enters ─▶ run            └─ fork ─▶ handler(event)
```

**What it costs.** A warm function stays in memory. A Python *zygote* (the
loaded process that is forked for each request) uses about 20 MB, of which
about 11 MB is its own and the rest is shared with other zygotes. The idle
rules pause it after `idle_timeout`: it is frozen but still in memory. They
drop it after `cold_after`, and the next request then pays the warm-up again.

```text
  serving ──(no calls for idle_timeout)──▶ paused: frozen, still ~20 MB in RAM
     ▲                                          │
     │                                    (no calls for cold_after)
     │                                          ▼
     └──────── next call pays warm-up ─── dropped: memory freed
```

## P3: isolation is one flag

`isolation = "ns" | "gvisor" | "vm"`. The spec, the command and the protocol
stay the same; only the *backend* changes, and the backend decides where the
wall is drawn. `ns` uses the host kernel. `vm` uses *KVM* (the kernel's
built-in support for virtual machines) with a guest kernel of its own.
`gvisor` uses a kernel that runs in user space.

```text
  isolation = "ns"       your program ─▶ host kernel
  isolation = "gvisor"   your program ─▶ gVisor (user-space kernel) ─▶ host kernel
  isolation = "vm"       your program ─▶ guest kernel ─▶ KVM ─▶ host kernel
```

**What it costs.** `ns` and `gvisor` are built. `vm` boots a guest and runs
one-shot sandboxes, at about ten times the setup cost of `ns`, and without
scratch space, network or warm functions. `zygo backend install gvisor`
downloads the gVisor runtime. The same command run on both backends gives the
same answer, while `uname -r` inside reports `4.19.0-gvisor` instead of the
host's kernel — which is the point.

`gvisor` is one-shot only so far. A warm function must be *entered* for each
request; on `ns` that is `setns` into namespaces the supervisor holds, and
gVisor's wall is not those namespaces. So Zygo refuses a warm function on
`gvisor` instead of quietly running something weaker, and it refuses a
networked sandbox there too. Rootless `runsc` (gVisor's runtime) also cannot
write cgroups, so limits there are only advisory, and Zygo says so on every
start. For a warm function today, that means `ns`.

## P4: OCI images, no Dockerfile

Any *OCI image* (the standard image format Docker also uses) from any
registry can be the file system. What a Dockerfile would add — Python
packages, apt packages — goes in the spec instead. Zygo builds it *once*,
inside the image, as a *venv* (a Python folder of installed packages) or as a
*derived layer* (an extra image layer made by running the install). Every
function that names the same thing shares the result. The image itself is
never changed.

```text
  image (never changed) ─┬─▶ + venv from requirements.txt ──▶ mounted at /venv
                         └─▶ + derived layer from apt list ──▶ shared by all who ask
  built once, keyed on the image digest and the list
```

**What it costs.** A derived layer is a copy of the image, an install, and a
diff; about five seconds the first time. *Overlayfs* (a file system that
stacks folders) would be faster, but rootless overlayfs needs kernel 5.11, and
not every host Zygo runs on has it.

## P5: no root, no daemon

Zygo needs no root at any point and no system service. The *supervisor* is a
process in your own login session that `zygo serve` starts when it needs one.
Outbound networking uses `pasta` (a user-space network helper) and
*nftables* (the kernel's packet filter) inside the sandbox's own namespace.
This works because entering a user namespace you created gives you a full set
of capabilities inside it, and nowhere else.

```text
  your login session (your uid, no root)
  └─ zygo serve ─▶ supervisor
                    └─ sandbox: own user namespace = full caps HERE only
                         ├─ nftables rules for the allowlist
                         └─ pasta ─▶ the outside network
```

**What it costs.** Some features need a program on the host: `pasta` and
`nft` for outbound networking, and `newuidmap` for a separate range of user
ids per tenant. If one is missing, Zygo refuses to start and names the
package. It never silently falls back to something weaker.

## P6: safe by default

With no flags, the network is off, the root file system is read-only, the
capability set is empty, the seccomp allowlist is on, and every limit has a
value. To loosen any of it you must type a flag: `--allow-host-net`,
`--allow-private-net`, `--allow-unlimited`. `zygo up` does not accept these
flags, so a spec that needs one has to be served on purpose.

```text
  default                         to loosen, you type
  ───────                         ───────────────────
  network off                ──▶  --allow-host-net, --allow-private-net
  seccomp allowlist on       ──▶  a looser profile, by name
  every limit has a value    ──▶  --allow-unlimited
  (none of these flags work with zygo up)
```

**What it costs.** The first thing a new user hits is a limit. The error
names the field and the fix.

## P7: every limit is required

Memory, CPU, process count, wall-clock time, scratch space, open files, and,
for a networked function, connections and bandwidth: each has a default, and
there is no "unlimited" without the flag. The deadline kills the request's
whole *process tree* (the process and every child it started), not a single
process. The per-request cgroup is what makes that one write, to its
`cgroup.kill` file.

```text
  request cgroup ── cgroup.kill ◀── one write at the deadline
     ├─ handler process        ✗
     │   └─ child it spawned   ✗
     └─ another child          ✗   the whole tree ends; the zygote keeps serving
```

**What it costs.** Disk I/O and bandwidth have no default limit, because a
good value depends on the device. Zygo warns when either is missing.

## P8: honest about what is not proven

Every claim in the README is either measured or marked. The test suites *try*
the thing instead of reading a setting. A negative check first proves that
the thing really ran. A latency number says whether it hit a limit.
[Chapter 25](25-performance.md) records what was measured and on what
hardware. Before these rules existed, several checks passed — or failed — for
the wrong reason.

```text
  weak test                           Zygo's test
  ─────────                           ───────────
  read a setting ─▶ pass              1. a negative check proves the probe really ran
                                      2. attempt the forbidden thing ─▶ must fail
                                      3. a latency number says if it hit a limit
```

**What it costs.** The status text is long, and it says "not built" more often
than a launch page would.

## The two warm modes, side by side

P2 has two shapes. *Warm-exec* keeps only the sandbox ready and runs your
`cmd` fresh each time. *Agent* mode also keeps a loaded interpreter ready and
forks it. [Chapter 13](13-warm-functions.md) shows how to write each one.

| | warm-exec (`cmd`) | agent (`entry`) |
|---|---|---|
| What is warm | the sandbox | the sandbox *and* a loaded interpreter |
| A request is | a fresh process entered into the sandbox | a `fork()` of the agent |
| Overhead | ~1.4 ms + the program's own start | ~1.4 ms |
| Needs | nothing: any image, any language | an agent that speaks the [protocol](../../spec/protocol.md); Python ships, Node and sh are in `examples/` |
| Use when | the runtime starts fast: Go, Rust, C, sh | starting the runtime is the cost: Python with imports, a JVM, Node with a dependency tree |

```text
  warm-exec:  [ sandbox ready ] ──▶ start your program ──▶ run   (fast for Go, Rust, C)
  agent:      [ sandbox ready + Python loaded ] ──▶ fork ──▶ run (skips the slow start)
```

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [7. Where the time and memory are saved](07-where-zygo-saves.md) · [Contents](README.md) · **Next: [9. FreeBSD jails, and Zygo](09-jails.md) →**
