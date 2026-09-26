# 4. The other locks

Namespaces decide what a process sees, and cgroups decide how much it uses.
This chapter is about the rest: what it is *allowed to do*, and what its world
is built from.

## Why more locks are needed

A process in its own namespaces, inside a tight cgroup, can still call almost
every syscall the kernel has. Some of those syscalls are large and complex,
and most serious container escapes of the last ten years went through one of
them. So a sandbox adds locks that work at a finer level: which powers the
process holds, which syscalls it may make, and which files it may touch. Each
lock is simple on its own. Together they mean that one mistake is not enough
to get out.

```text
  ┌──────────────────────────────────────────────────────────────┐
  │ namespaces        what it can SEE                            │
  │  ┌────────────────────────────────────────────────────────┐  │
  │  │ cgroup          how much it can USE                    │  │
  │  │  ┌──────────────────────────────────────────────────┐  │  │
  │  │  │ capabilities + no_new_privs  what POWER it holds │  │  │
  │  │  │  ┌────────────────────────────────────────────┐  │  │  │
  │  │  │  │ Landlock      which FILES / PORTS          │  │  │  │
  │  │  │  │  ┌──────────────────────────────────────┐  │  │  │  │
  │  │  │  │  │ seccomp     which SYSCALLS           │  │  │  │  │
  │  │  │  │  │         ┌──────────────┐             │  │  │  │  │
  │  │  │  │  │         │ your program │             │  │  │  │  │
  │  │  │  │  │         └──────────────┘             │  │  │  │  │
  │  │  │  │  └──────────────────────────────────────┘  │  │  │  │
  │  │  │  └────────────────────────────────────────────┘  │  │  │
  │  │  └──────────────────────────────────────────────────┘  │  │
  │  └────────────────────────────────────────────────────────┘  │
  └──────────────────────────────────────────────────────────────┘
      to get out, a program has to beat every layer — or the kernel itself
```

## Dropping capabilities

As [chapter 1](01-kernel-and-process.md#capabilities) said, capabilities are
root's power cut into pieces. Inside a user namespace a process can hold all
of them — over that namespace. A sandbox then drops every one it does not
need, from every set the kernel keeps, so they cannot come back. Docker keeps
fourteen by default, to be useful to normal software. Zygo keeps none, because
a function has no need to change the network or create device files.

## no_new_privs

Some programs on disk are marked *setuid*: they run as their owner, often
root, whoever starts them. `sudo` and `passwd` work this way. Inside a
sandbox, that would be a way to gain power back. The `no_new_privs` flag,
set once on a process, tells the kernel that nothing this process or its
children exec may ever gain power, setuid or not. It cannot be unset. Every
serious sandbox sets it, and seccomp needs it before a normal user may install
a filter.

## seccomp

*seccomp* ("secure computing") lets a process install a small filter that the
kernel runs on every syscall it makes. The filter is a tiny program, written
in classic BPF, that looks at the syscall number and its arguments and
answers "allow", "fail with an error" or "kill". Once installed it cannot be
removed, and children inherit it. Docker's default filter lists what is
*blocked* and allows about 350 syscalls. Zygo's lists what is *allowed* —
about 215 names, of which 190 exist on an arm64 kernel and all on x86_64 — so
a syscall added to the kernel next year is blocked until
someone chooses to allow it. [Seccomp profiles](24-seccomp-profiles.md) has the
full lists.

```text
  program ── syscall(nr, args) ──▶ ┌─────────────────────────┐
                                   │ seccomp filter (BPF)    │
                                   │  read, write, openat …  │──▶ allow ──▶ kernel does it
                                   │  clone + CLONE_NEW*     │──▶ EPERM ──▶ "not permitted"
                                   │  bpf, io_uring, ptrace, │──▶ EPERM
                                   │  mount, unshare …       │
                                   │  anything not listed    │──▶ EPERM
                                   └─────────────────────────┘
```

## Landlock

Landlock, added in Linux 5.13, lets a normal process limit its own access to
files, and from 6.7 its network connections too. The process says, for
example, "from now on I may read under `/usr` and write under `/tmp`, and
nothing else", and the kernel enforces it, even against root inside the
sandbox. Like seccomp, it can only be tightened, never loosened. It is a
second wall behind the mount namespace: if a mistake ever made a host path
visible inside, Landlock would still refuse to open it. Zygo applies it
wherever the kernel has it.

## AppArmor and SELinux

These are *security modules*: rules, loaded by the system's administrator,
that the kernel checks for every process. Docker on Ubuntu loads an AppArmor
profile for each container; on Fedora and Red Hat, SELinux labels do the same
job. They are strong, but they need root to set up, so a rootless tool like
Zygo cannot use them for its own sandboxes. They can get in its way, though:
Ubuntu's AppArmor rules limit unprivileged user namespaces, and
[troubleshooting](22-troubleshooting.md) explains the fix.

## Resource limits (rlimits)

`rlimit`s are the old, per-process limits that came before cgroups: how many
files a process may have open, how big a file it may write, how much stack
it may use. You see them with `ulimit -a`. They are weaker than cgroups
because they count per process, not per group. They are still useful for the
few things cgroups do not cover, such as open files. Zygo sets `nofile` (1024
by default) this way.

## The root filesystem

Inside a mount namespace a sandbox builds a new file tree, usually from an
image, and makes it the root. The old way is `chroot`, which only changes where
path lookups start and has well-known ways out. The better way is
`pivot_root`: it swaps the old root for the new one in the mount namespace,
and then the old root can be unmounted, so the host's files are not just
hidden but gone from this view. Zygo uses `pivot_root`, mounts the root
read-only, and gives the sandbox one small writable place, `/tmp`, in memory.

## Layers: overlayfs, bind mounts and tmpfs

Three kinds of mount do most of the work. *overlayfs* stacks folders on top of
each other and shows them as one, which is how an image made of several
layers becomes one tree without copying anything; a normal user may use it
from Linux 5.11. A *bind mount* shows an existing file or folder at a second
place — this is how your code gets into a sandbox, and it can be made
read-only. *tmpfs* is a file system in memory, which vanishes when the last
process using it exits. Build the root from overlayfs, add your files with
bind mounts, give it a tmpfs to write to, and you have a container's file
system.

```text
  what the program sees as /             where it really comes from
  ──────────────────────────             ──────────────────────────
  /app/handler.py   (read-only)   ◀───── bind mount of ./handler.py on the host
  /tmp              (writable)    ◀───── tmpfs, in memory, 64M, gone at exit
  /usr /lib /bin …  (read-only)   ◀───── overlayfs of the image's layers:
                                           ┌───────────────────────┐
                                           │ layer 3  pip packages │
                                           │ layer 2  python       │
                                           │ layer 1  debian base  │
                                           └───────────────────────┘
                                           stored once, shared by every sandbox
```

## The network

A new network namespace has only loopback. There are two common ways to
connect it. The first is a *veth pair*: a virtual cable with one end inside
and one on the host, joined to a bridge with NAT — Docker's way, which needs
root on the host side. The second is a program that moves packets between the
namespace and the host's normal sockets, in user space and as your own user;
`slirp4netns` and `pasta` do this. Zygo uses `pasta`, and puts an nftables
firewall *inside* the sandbox's own namespace, where your user is allowed to.
That firewall is how "this function may reach `api.example.com:443` and
nothing else" is enforced.

```text
  Docker (bridge, root on the host)            Zygo (pasta, your own user)
  ─────────────────────────────────            ───────────────────────────
  ┌ container netns ┐                          ┌ sandbox netns ─────────────┐
  │ eth0            │                          │ tap0                       │
  └──┬──────────────┘                          │ nftables: allow only       │
     │ veth pair                               │   api.example.com:443      │
  ┌──▼──────────────┐                          │ no 10.x / 192.168.x /      │
  │ docker0 bridge  │ ◀─ iptables NAT          │   169.254.169.254          │
  └──┬──────────────┘                          └──┬─────────────────────────┘
     ▼                                            │ packets as data
  host network: LAN, cloud metadata,           ┌──▼──────────────┐
  the internet — all reachable by default      │ pasta (as you)  │──▶ normal sockets
                                               └─────────────────┘    on the host
```

## Putting it together

A sandbox on Linux is all of these, set up in the right order in the moment
between `fork` and `exec`:

```text
clone3 into new namespaces (user, mount, pid, net, ipc, uts, cgroup)
  → write the uid map                     (from the parent)
  → join a cgroup with limits
  → build the root: overlayfs + bind mounts + tmpfs, then pivot_root
  → set rlimits, drop every capability, set no_new_privs
  → install Landlock, then the seccomp filter
  → execve your program
```

Each step is cheap — a namespace set is about a millisecond, a cgroup write a
tenth of that, a filter microseconds. The whole list is the *sandbox*. The
rest of this book is about who runs this list, how often, and what they put
around it.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [3. Control groups](03-cgroups.md) · [Contents](README.md) · **Next: [5. Docker](05-docker.md) →**
