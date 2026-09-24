# 2. Namespaces

A namespace changes what a process can *see*. It does not limit how much it
can *use* — that is [control groups](03-cgroups.md), the next chapter.

## What a namespace is

Normally every process sees the same machine: the same list of processes, the
same files, the same network, the same host name. A namespace gives a group of
processes their own copy of one of those things. Inside a new PID namespace,
for example, a process sees only its own children and thinks it is pid 1.
Nothing is copied and nothing is emulated; the kernel just keeps a separate
table and answers from it. Linux has eight kinds, and each one can be used
alone or with the others. A "container" is simply a process that has its own
copy of most of them.

```text
            the host sees                         the sandbox sees
 ┌──────────────────────────────────┐     ┌─────────────────────────────┐
 │ pids   1 systemd … 4200 zygo     │     │ pids   1 python   2 sh      │
 │        4201 python  4202 sh      │     │                             │
 │ files  /home /etc /var …         │     │ files  / of the image only  │
 │ net    eth0 wlan0 lo  + LAN      │     │ net    lo                   │
 │ name   my-laptop                 │     │ name   sandbox              │
 │ users  0 root … 1000 you         │     │ users  0 root  (= you, 1000)│
 └──────────────────────────────────┘     └─────────────────────────────┘
        same processes 4201 and 4202 — two different views of them
```

| Namespace | Its own copy of | Since |
|---|---|---|
| mount | the list of mounted file systems | 2.4.19 (2002) |
| UTS | host name | 2.6.19 (2006) |
| IPC | shared memory, message queues | 2.6.19 |
| PID | process numbers | 2.6.24 |
| network | interfaces, addresses, routes, firewall | 2.6.29 |
| user | user and group numbers | 3.8 |
| cgroup | the view of the cgroup tree | 4.6 |
| time | boot-time clocks | 5.6 |

## Mount namespace

The mount namespace gives a process its own list of mounted file systems. It
can mount and unmount things, and the rest of the machine does not see it.
This was the first namespace, added in 2002, and it is why the flag is still
called just `CLONE_NEWNS` — nobody expected others. A sandbox uses it to build
a whole new file tree from an image, then switch its root to that tree, so the
host's files are simply not there. [Chapter 4](04-other-locks.md#the-root-filesystem)
explains how the switch is done.

## PID namespace

The PID namespace gives a process its own process numbers. The first process
inside becomes pid 1, and it can only see and signal processes in the same
namespace, or in namespaces below it. From outside, the same processes are
still visible, with their normal host numbers. So a sandboxed program cannot
list, watch or kill anything on the host. When pid 1 of the namespace exits,
the kernel kills everything else inside it, which makes clean-up easy.

## Network namespace

The network namespace gives a process its own network: its own interfaces, its
own addresses, its own routes and its own firewall rules. A new one has only a
loopback interface (`lo`), so by default the process can reach nothing but
itself. To give it more, something outside has to connect it — a virtual cable
to the host, or a program that moves packets in user space.
[Chapter 4](04-other-locks.md#the-network) covers both, and Zygo's choice.

## UTS namespace

UTS is an old name for "the system's name". This namespace gives a process its
own host name and domain name. It is the smallest of the eight and hides no
secret; it mostly stops a program from learning the host's name, or changing
it. Container tools set it so that `hostname` inside prints something sensible.

## IPC namespace

IPC means "inter-process communication". This namespace gives a process its
own System V message queues, shared memory segments and POSIX message queues.
Without it, two programs on one machine could meet through a shared memory
segment with a guessable key. It is old and rarely thought about, but a
sandbox that skips it leaves a small side door open.

## User namespace

The user namespace gives a process its own list of users. Inside it, a process
can be uid 0 — root — while outside it is still your own normal uid. A *uid
map* (`/proc/<pid>/uid_map`) says which inside number matches which outside
number. Root inside a user namespace has full capabilities, but only over
things that belong to that namespace: its own mounts, its own network, never
the host's. This is the namespace that lets a normal user create all the
others, and it is what makes *rootless* containers — and all of Zygo — possible.

```text
  inside the sandbox          uid_map            on the host
  ──────────────────     ─────────────────     ──────────────
  uid 0    "root"   ───▶   0     1000   1  ───▶  uid 1000 (you)
  uid 1000 "app"    ───▶   (a second line, via newuidmap, if the host allows)
  any other uid     ───▶   not mapped: shows as "nobody", owns nothing

  root inside  = every capability over the sandbox's own mounts, network, …
               = no power at all over anything that belongs to the host
```

## Cgroup namespace

The cgroup namespace changes how a process sees the control-group tree from
[chapter 3](03-cgroups.md). Inside it, the process's own group looks like the
top of the tree, so it cannot learn where it sits on the host. It hides
information; it does not set any limit. Zygo creates one, and in addition does
not mount the cgroup file system inside the sandbox at all.

## Time namespace

The newest one, added in Linux 5.6. It lets a process see a different value
for the clocks that count time since boot. Its main use is moving a running
container to another machine without its clocks jumping. Sandboxes for short
jobs rarely need it, and Zygo does not create one.

## Creating and joining

Three syscalls do the work. `clone` (and the newer `clone3`) starts a child in
new namespaces. `unshare` moves the calling process into new namespaces.
`setns` joins a namespace that already exists, given a file that points to it,
such as `/proc/<pid>/ns/net`. Zygo uses `clone3` to build a sandbox, because
it makes the child pid 1 of its new PID namespace in one step. It uses `setns`
on the warm path, to put a new process into a sandbox that is already built.

```text
  clone3(flags)            unshare(flags)            setns(fd)
  ─────────────            ──────────────            ─────────
  parent                   process                   process      existing
    │                        │                         │          namespace
    └─▶ child, born         moves itself into         └──────────▶ ┌─────┐
        inside NEW          NEW namespaces                         │ net │
        namespaces                                                 └─────┘
  used by Zygo to          used by `unshare(1)`      used by Zygo to enter
  BUILD a sandbox          and many tools            a WARM sandbox
```

## What namespaces do not do

Namespaces hide things; they do not count or limit anything. A process in its
own namespaces can still use all the memory, fill the process table with a
fork bomb, or keep every CPU busy. Namespaces also do nothing about which
syscalls a process may call, so the whole kernel is still in reach. Those
gaps are filled by control groups ([chapter 3](03-cgroups.md)) and by
seccomp and Landlock ([chapter 4](04-other-locks.md)). You need all of them
together; any one alone is not a sandbox.

## Seeing them yourself

`lsns` lists the namespaces on the machine. `ls -l /proc/self/ns` shows the
ones your shell is in, each as a number. `unshare --user --map-root-user
--pid --fork --mount-proc bash` starts a shell where you are root and pid 1 —
without being root on the host. Type `ps` inside it and you will see two
processes. That small command is most of the idea behind every tool in this
book.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [1. The kernel and the process](01-kernel-and-process.md) · [Contents](README.md) · **Next: [3. Control groups](03-cgroups.md) →**
