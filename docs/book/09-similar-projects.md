# 9. Similar projects

Many projects build a sandbox from the parts in chapters 2 to 4. They differ
in three questions, and once you ask those, most comparisons answer
themselves.

## The three questions

1. **Where is the wall?** The host kernel with locks on it, a second kernel in
   user space, or a virtual machine with its own kernel.
2. **What is ready when a request arrives?** Nothing — the sandbox is built
   each time; the sandbox — a new process enters it; or the sandbox *and* the
   loaded program — a copy is forked.
3. **Who has to be root?** The tool, a daemon, the admin once, or nobody.

The claims about other projects below are theirs or commonly measured, not
measured here. They all move quickly; check before quoting.

## The map

| Wall ↓ · Ready on arrival → | nothing: build it all | the sandbox: enter it | the loaded program: fork it |
|---|---|---|---|
| **Virtual machine** | Firecracker, Kata, microsandbox, Zygo `vm` | — | Firecracker from a memory snapshot (a VM per restore) |
| **Second kernel** | gVisor, Zygo `gvisor` | — | — |
| **Host kernel** | Docker, Podman, runc, nsjail, bubblewrap, firejail, minijail, kern, Zygo `run` | `docker exec` (shared state), Zygo warm-exec | **Zygo `exec`** |
| **Process confinement only** | nono, Landlock-based tools: they confine a process you already run | | |

The right-hand column is almost empty, and that is the space Zygo was built
for. Everything else on this page is a good tool for a nearby job.

```text
   per-call cost of a small Python function, roughly (log scale, not exact)
   1 ms          10 ms          100 ms          1 s
   ├──────────────┼───────────────┼───────────────┤
   ▪ Zygo exec (fork of a warm zygote)
                  ▪─────▪ one-shot host-kernel sandboxes + Python start
                          (nsjail, bubblewrap, kern, Zygo run)
                           ▪────▪ microVM or gVisor + Python start
                                     ▪──────────▪ docker run
```

## nsjail

nsjail, from Google, is the closest older relative of Zygo's one-shot half.
It puts a process into namespaces, cgroups, rlimits and a seccomp filter,
which you write in a small policy language called Kafel. It can run a
command once, run it again and again, or listen on a TCP port and start a
fresh jail for every connection, which made it the standard tool for hosting
CTF challenges. Windmill and other job runners can wrap each job in it. It
has no images — you give it a folder or bind-mount the host's — and no warm
path: every run starts the program from nothing. Choose it when you want a
battle-tested, very configurable jail around a command and you manage the
root file system yourself.

## bubblewrap

bubblewrap (`bwrap`) is the small sandbox tool under Flatpak. It creates
namespaces, builds a root from the bind mounts you list, and then runs a
command — nothing more, on purpose. It has no cgroup limits and no seccomp
policy of its own; you pass a compiled filter if you want one. That smallness
is its strength: it is easy to audit and it runs everywhere, which is why many
desktop and developer tools use it as a building block. Zygo covers what
bubblewrap leaves to the caller: images, limits, the filter, the network
allowlist, and the warm path.

## firejail

firejail sandboxes desktop programs — a browser, a PDF reader — using ready
profiles for hundreds of applications. It is installed setuid root, so any
user can start a sandbox, but its own code runs with root's power, and bugs in
it have mattered in the past. It is made for confining the apps on your
desktop, not for running server-side functions at high rates. Zygo needs no
setuid program at all, because user namespaces give it what it needs.

## minijail

minijail is Google's sandbox library and tool for ChromeOS and Android system
services. It applies namespaces, capabilities, seccomp and user changes to a
service before it starts, from a policy file. It is part of the operating
system's own plumbing and is written for that setting: known services,
written by the same team. Zygo is aimed at the reverse: code written by
someone else, arriving at request time.

## systemd-nspawn and systemd-run

systemd-nspawn starts a whole operating-system tree in namespaces — "chroot
on steroids", in its own words — and is good for booting a distribution in a
container for testing. `systemd-run` can put any command in a transient unit
with limits and many sandbox options. Both mostly need root, and both think in
units and machines rather than requests. Zygo uses systemd only where it has
to: to get a delegated cgroup under your login.

## kern

kern is the closest match to Zygo's one-shot half today: rootless,
daemonless, one static Rust binary, OCI images, namespaces with a seccomp
allowlist and cgroup v2, a box made and thrown away per call in single-digit
milliseconds by its own figures. If you want `docker run` without the daemon
and without the wait, both answer that well. The difference is what happens
next: kern makes the box cheap and throws it away, while Zygo notes that the
expensive part is the interpreter *inside* the box, and keeps a loaded copy to
fork. kern also has virtual resource slices for bare host processes, which
Zygo does not. [The comparison](../comparison.md#kern) goes deeper.

## nono

nono applies Landlock and seccomp — Seatbelt on macOS — to a process you are
already starting, such as a coding agent working in your own folder. There is
no image, no namespace and no cgroup: it narrows what *your* process can do,
and it works natively on a Mac. That is the right tool for an agent that is
meant to edit your files. It is not a place to run code the agent wrote,
because that code would still run as you, in your files. The two fit
together: nono around the agent, Zygo for the code it produces.

## microsandbox

microsandbox gives every sandbox its own small virtual machine through
libkrun — the same library behind Zygo's `vm` backend — with OCI images, SDKs,
an MCP server, and the power to snapshot and branch a live sandbox, which Zygo
cannot do. Its wall is hardware for everything; Zygo's default is the host
kernel, with hardware one flag away. For truly hostile code where ~100 ms per
call is fine, its default is the safer one. For thousands of short calls from
your own users' scripts, Zygo's warm path is the faster one.

## gVisor

gVisor is a kernel written in Go that runs in user space. Your program's
syscalls go to it, not to the host, and it answers most of them itself, using
only a small, filtered set of host syscalls. That gives a much smaller attack
surface than the host kernel, without needing a virtual machine. The cost is
speed on syscall-heavy work, and some programs that need rare kernel features.
Its runtime, `runsc`, is an OCI runtime, and Zygo's `gvisor` backend uses it
for one-shot runs.

## Firecracker, and platforms on it

Firecracker is the microVM monitor behind AWS Lambda and Fargate: a tiny
virtual machine per workload, with a real kernel of its own, booting in about
125 ms or restoring from a memory snapshot faster still. It is the strongest
wall on this page, and it needs KVM, so it rarely runs inside another cloud
VM. Platforms such as E2B build hosted sandboxes for AI agents on top of it.
Its snapshot restore is the one thing here that resembles Zygo's fork, one
level down: a whole VM restored instead of a process copied.

## Kata Containers

Kata Containers runs each container, or each Kubernetes pod, inside a
lightweight virtual machine, while still looking like a normal container to
Kubernetes. It gives you a hardware wall without changing how you deploy. The
price is a VM's start-up time and memory for every pod. It is built for
long-lived services on a cluster, where Zygo is built for short calls on one
machine.

## runc, crun and youki

These are the low-level OCI runtimes: given a folder and a `config.json`,
they do the list at the end of [chapter 4](04-other-locks.md#putting-it-together)
and exec the program. runc is written in Go, crun in C, youki in Rust. They
are the part of Docker and Podman closest to `zygo run`, and on their own they
are fast. But they expect someone else to prepare the bundle, pull images,
keep records and clean up — which is the chain from
[chapter 5](05-docker.md#the-chain-behind-docker-run). Zygo does its own
launching, so it needs none of them.

## Workflow engines: Windmill and friends

Windmill, Temporal and similar engines run users' scripts, and they need a
sandbox for each run. Today that is often nsjail or a container per job. These
engines are exactly the users Zygo is designed for: a worker calls
`zygo serve` once per script and each run becomes a `fork()`.
[`examples/workflow-engine/`](../../examples/workflow-engine) is such a worker.

## Everything in one table

| | Wall | Images | Limits | Warm fork | Root needed | Main use |
|---|---|---|---|---|---|---|
| **Zygo** | host kernel · gVisor · VM | OCI | cgroups, mandatory, per request | **yes** | no | short functions, others' code |
| Docker | host kernel | OCI | cgroups, opt-in | no | daemon (or rootless mode) | services, packaging |
| Podman | host kernel | OCI | cgroups, opt-in | no | no | services, without a daemon |
| nsjail | host kernel | no (a folder) | cgroups, rlimits | no | depends on features | CTFs, job runners |
| bubblewrap | host kernel | no (bind mounts) | none | no | no (or setuid) | desktop apps, a building block |
| firejail | host kernel | no | some | no | setuid root | desktop apps |
| minijail | host kernel | no | some | no | usually | OS services |
| kern | host kernel | OCI | cgroups | no | no | fast throwaway boxes |
| nono | host kernel (Landlock) | no | no | n/a | no | confining an agent |
| gVisor | second kernel | OCI | cgroups | no | usually | safer containers |
| microsandbox | VM | OCI | the VM's | no (snapshots) | no | hostile code |
| Firecracker | VM | no (a disk image) | the VM's | no (snapshots) | KVM access | serverless platforms |
| Kata | VM | OCI | the VM's | no | yes | safer Kubernetes pods |
| FreeBSD jail | host kernel | no (a folder) | rctl, opt-in | no | yes | long-lived services on FreeBSD |

## Choosing, in short

For long-lived services, use Docker, Podman or Kubernetes. For hostile code
where 100 ms or more per call is fine, use a VM wall: Firecracker, Kata,
microsandbox, or Zygo's `vm` backend. For confining a tool you already run,
look at nono or bubblewrap. For a quick throwaway sandbox around one command,
nsjail, kern and `zygo run` all do well. For many short calls to code other
people wrote, where each call must start clean and costs must stay in
milliseconds, that is the right-hand column of the map, and that is Zygo.
