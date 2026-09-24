# 1. The kernel and the process

Before namespaces and containers, a few older ideas. Every sandbox on Linux is
built from them, and most confusion about sandboxes comes from skipping them.

## The kernel and user space

The kernel is the one program that talks to the hardware. It owns the memory,
the CPU time, the disks and the network cards, and it decides who gets which.
Everything else — your shell, Python, a database — runs in *user space* and
cannot touch the hardware directly. When a program in user space needs
something, it has to ask the kernel. That split is the first and oldest
boundary on the machine, and every sandbox in this book is a way of making the
kernel say "no" more often.

```text
 USER SPACE     ┌───────┐  ┌────────┐  ┌──────────┐  ┌──────────┐
 (programs)     │ shell │  │ python │  │ postgres │  │ your job │
                └───┬───┘  └───┬────┘  └────┬─────┘  └────┬─────┘
                    │ syscalls │            │             │
 ═══════════════════╪══════════╪════════════╪═════════════╪══════ the boundary
                    ▼          ▼            ▼             ▼
 KERNEL         ┌─────────────────────────────────────────────────┐
                │ processes · memory · files · network · devices  │
                └─────────────────────────────────────────────────┘
 HARDWARE          CPU          RAM          disk         network card
```

## System calls

The way a program asks the kernel for something is a *system call*, or
*syscall*. Opening a file is `openat`, reading from it is `read`, starting a
process is `clone`, and there are about 450 of them on a modern Linux. A
syscall is a small, well-defined door: the program puts a number and some
arguments in place and the kernel answers. Because every request passes
through these doors, the kernel can check each one. It is also why the number
of doors matters for safety — each is code in the kernel that a hostile
program can try to confuse.

## Processes

A process is a running program: its memory, its open files, its user, and the
kernel's notes about it. Every process has a number, the *pid*, and a parent.
The first process on the machine is pid 1, and every other one descends from
it, so the processes form a tree. `ps -ef --forest` shows that tree. A sandbox
is, at the end, a branch of this tree that the kernel treats differently from
the rest.

## fork and exec

Linux starts a new program in two steps. `fork` makes a copy of the calling
process — same memory, same open files — and both copies carry on from the
same line. `exec` (`execve`) then replaces the copy's memory with a new program
loaded from disk. A shell that runs `ls` forks itself, and the child execs
`ls`. The split looks strange, but it gives you a moment between the two steps
where the child can change its own situation — close files, change user, enter
a sandbox — before the new program starts. Every sandbox in this book does its
work in that moment.

```text
 shell (pid 100)
    │
    │ fork()
    ├──────────────────▶ copy of shell (pid 101)
    │                        │
    │                        │  ◀── the moment: change user, close files,
    │                        │      enter namespaces, join a cgroup,
    │                        │      install filters …
    │                        │
    │                        │ execve("/bin/ls")
    │                        ▼
    │                     ls (still pid 101)
    │                        │ exit
    ▼ wait() ◀───────────────┘
```

## Copy-on-write

A fork does not really copy the memory. Parent and child share the same
physical pages, marked read-only, and a page is copied only when one of them
writes to it. So forking a 50 MB process takes well under a millisecond and
costs almost no new memory. The child still behaves as if it had its own full
copy: what it writes, the parent never sees. This one trick is the base of
Zygo's warm path, and [chapter 6](06-how-zygo-works.md#the-warm-path) comes
back to it.

```text
 right after fork                   after the child writes to page B
 ────────────────                   ────────────────────────────────
     parent   child                      parent   child
        │       │                           │       │
        ▼       ▼                           ▼       ▼
     ┌───┬───┬───┐                       ┌───┬───┬───┐   ┌────┐
     │ A │ B │ C │                       │ A │ B │ C │   │ B' │
     └───┴───┴───┘                       └───┴───┴───┘   └────┘
   one set of pages, shared           parent sees  A  B  C
   and marked read-only               child sees   A  B' C   (one page copied)
```

## Users and root

Every process runs as a user, which the kernel knows as a number, the *uid*.
Files have an owner uid and permission bits, and the kernel checks them on
every access. uid 0, *root*, has always been special: for most checks, root
simply passes. That made root the prize of every attack, because one bug in a
root program gave away the whole machine. Much of the history of Linux
security is the slow work of making root less all-powerful.

## Capabilities

*Capabilities* split root's power into about forty named pieces. `CAP_NET_ADMIN`
lets a process change the network, `CAP_SYS_ADMIN` lets it mount file systems
and much more, `CAP_KILL` lets it signal anyone. A process can hold some pieces
and not others, and it can drop the ones it does not need, for good. A sandbox
usually drops all of them. Zygo does: a program inside it has an empty
capability set.

## Everything is a file, and /proc

Linux shows a lot of its state as files. `/proc` is a file system the kernel
makes up on the fly: `/proc/1234/` describes process 1234, `/proc/meminfo`
describes memory. `/sys` does the same for devices and, as
[chapter 3](03-cgroups.md) shows, for control groups. This matters for
sandboxes in two ways. Many controls are set by writing a small file, which is
fast and needs no special tool. And a sandbox must be careful what parts of
`/proc` and `/sys` it shows, because some of those files are doors of their
own.

## One kernel

Here is the fact the rest of the book keeps coming back to. On one Linux
machine, every process — in a container or not — talks to the *same* kernel.
Namespaces, control groups and filters are all rules *inside* that kernel.
If the kernel itself has a bug that lets a process break its rules, every
rule fails at once. That is why some sandboxes add a second kernel (gVisor) or
a whole virtual machine (Firecracker, Zygo's `vm` backend); [chapter 9](09-similar-projects.md)
compares them.

```text
  containers / Zygo `ns`        gVisor                   microVM (Firecracker, Zygo `vm`)
  ──────────────────────        ──────                   ────────────────────────────────
  ┌─────┐ ┌─────┐ ┌─────┐       ┌─────┐                  ┌─────┐          ┌─────┐
  │ app │ │ app │ │ app │       │ app │                  │ app │          │ app │
  └──┬──┘ └──┬──┘ └──┬──┘       └──┬──┘                  └──┬──┘          └──┬──┘
     │       │       │          ┌──▼─────────────┐       ┌──▼────────┐    ┌──▼────────┐
     │       │       │          │ user-space     │       │ guest     │    │ guest     │
     │       │       │          │ kernel         │       │ kernel    │    │ kernel    │
     │       │       │          └──┬─────────────┘       └──┬────────┘    └──┬────────┘
  ┌──▼───────▼───────▼──┐       ┌──▼─────────────┐       ┌──▼─────────────────▼───────┐
  │     host kernel     │       │  host kernel   │       │  host kernel + KVM         │
  └─────────────────────┘       └────────────────┘       └────────────────────────────┘
  one kernel bug = all out      a bug must pass two      a bug must pass the guest
                                                         kernel AND the hardware wall
```
