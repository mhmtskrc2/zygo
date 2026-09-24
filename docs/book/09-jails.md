# 8. FreeBSD jails, and Zygo

Linux was not first. FreeBSD had a working "container" in 2000, thirteen years
before Docker, and many ideas in this book are easier to see there.

## Where jails came from

Jails arrived in FreeBSD 4.0, in March 2000, written by Poul-Henning Kamp for
a hosting company that wanted to give each customer "root" without giving
them the machine. The paper that describes them has a telling title:
*Jails: Confining the omnipotent root*. The idea was to take `chroot`, which
only changes where file paths start, and close every way out of it. A jail
became one clear thing in the kernel: a group of processes with its own
files, its own host name, its own addresses, and a root user who is not
really root.

## How a jail works

A jail is one object in the kernel, made with the `jail(2)` syscall, usually
through the `jail(8)` tool and a config file, `/etc/jail.conf`. Every process
carries a pointer to its jail, and the kernel checks that pointer wherever it
matters. A jailed process sees only processes in its own jail. Root inside may
not mount file systems, load kernel modules, change the network, or reach
raw devices. `jls` lists the jails; `jexec` runs a command inside one.

```text
  FreeBSD: one kernel object                  Linux: many parts, put together
  ──────────────────────────                  ───────────────────────────────
  ┌──────────── jail ────────────┐            mount ns ─┐
  │ root path   /jails/web       │            pid ns   ─┤
  │ host name   web.example      │            net ns   ─┤
  │ addresses   10.0.0.5 / vnet  │            uts ns   ─┤
  │ root is limited              │            ipc ns   ─┼─▶  what a tool
  │ processes see only the jail  │            user ns  ─┤    calls a
  │ limits via rctl              │            cgroup   ─┤    "container"
  └──────────────────────────────┘            caps     ─┤    or "sandbox"
       made by jail(2), by root                seccomp  ─┤
                                               Landlock ─┘
```

## What jails have grown since

Jails kept growing. *VNET* (since FreeBSD 12 in the default kernel) gives a
jail a whole network stack of its own, like a Linux network namespace.
*rctl* limits a jail's memory, CPU and process count, like cgroups.
Jails can nest inside other jails. *Capsicum*, a separate FreeBSD feature,
lets a process lock itself down to only the files and sockets it already
holds — close in spirit to seccomp and Landlock together. Tools such as
iocage and Bastille manage jails the way Docker manages containers, and
Podman now runs OCI images on FreeBSD with jails underneath.

## One idea, two designs

FreeBSD and Linux solved the same problem in opposite ways. FreeBSD made one
strong, complete object: you ask for a jail and you get all of it. Linux made
many small parts — each namespace, cgroups, seccomp — and left it to tools to
put them together. The FreeBSD way is easier to reason about: there is one
thing to check, and it is hard to forget a piece. The Linux way is more
flexible — a tool can take only a network namespace, or only a cgroup — but
every tool must assemble the parts correctly, and a missing part is a hole.
Much of Zygo's test suite exists because of that second fact.

## What Zygo shares with a jail

At the level of "what does the program inside see", a Zygo `ns` sandbox and
a jail are very close. Both give a group of processes its own root file tree,
its own view of processes, its own network (or none), its own host name, and a
root user without real power. Both share the host's kernel, so both are only
as strong as that kernel. Both are cheap: no second kernel, no virtual
hardware. And both come from the same instinct: the process, not a machine,
is the thing to confine.

## Where they differ

| | FreeBSD jail | Zygo sandbox (`ns`) |
|---|---|---|
| What it is | one kernel object | a process with many Linux locks on it |
| Who can create one | root | any user (user namespaces + delegated cgroups) |
| Users inside | the host's own uids; jail root is uid 0, limited | its own uids, mapped to yours; root inside is you outside |
| Usual life | long: a web server, a mail server, for months | short: one program, or one request, then gone |
| Syscall filter | none per jail (Capsicum is per process) | a seccomp allowlist on every sandbox |
| File access rules | the jail's root path | the root, **plus** Landlock as a second wall |
| Limits | rctl, optional | cgroups, mandatory, one set per request |
| Images | a folder or ZFS dataset you prepare | OCI images from any registry |
| Warm start | none: you start processes in the jail | a zygote, forked per request in ~1.4 ms |
| Network default | the addresses you give it | nothing at all |

## The biggest difference: who holds the key

On FreeBSD, making a jail needs root, and root inside a jail is the host's uid
0, only with fewer rights. The safety comes from the kernel's list of what
jailed root may not do. On Linux with a user namespace, "root" inside is just
*your* user outside, so there is nothing of the host's to lose even if a
check were missed. That is why Zygo can run without any privilege, and why a
normal user can start a thousand sandboxes without asking an admin. The cost
is on the other side: user namespaces open a lot of kernel code to normal
users, which is exactly why Zygo's seccomp filter refuses to let a sandbox
create new ones.

## Can Zygo be called "jails for Linux"?

Partly — and it is worth being exact. As a *picture* it fits well: a program
locked in its own small world, sharing the kernel, cheap to make. People who
know FreeBSD will understand Zygo's `ns` backend in one sentence that way. But
a jail is usually a long-lived home for a service, built by an admin, and
Zygo is the opposite: short-lived, built by any user, often one per request,
and it does not run services at all. The honest phrase is **"a throwaway jail
for every request"**: the jail's walls, with the lifetime of a function call.

## Other relatives

FreeBSD was not alone. Solaris *Zones* (2005) took the same idea further,
with its own resource controls and a strong admin model. On Linux,
*Linux-VServer* and *OpenVZ* were jail-like kernel patches used by hosting
companies long before namespaces were finished. *LXC* (2008) was the first
widely used tool to build jail-like "system containers" from the new Linux
parts, and Docker began as a layer on top of it. Every one of these, like
Zygo, shares one kernel between everything it hosts.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [8. The rules Zygo is built on](08-principles.md) · [Contents](README.md) · **Next: [10. Similar projects, and Docker side by side](10-similar-projects.md) →**
