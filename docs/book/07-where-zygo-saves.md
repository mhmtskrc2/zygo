# 7. Where the time and memory are saved

Zygo is not faster because it found a faster kernel feature. It is faster
because it stops doing work that does not need to be done on every request.
This chapter lists each piece of that work, how much it costs, and what Zygo
does instead.

## The whole picture

```text
  what one request pays, for a small Python function (not to scale)
  ─────────────────────────────────────────────────────────────────────────────
  docker run   ░░░░░░░ chain ░░░░░░░▒▓▓▓▓ Python + imports ▓▓▓▓█   300–1000 ms
  docker exec  ░░ daemon ░░█                                        50–100 ms, shared state
  zygo run     ▒▓▓▓▓ Python + imports ▓▓▓▓█                         12 ms, Python included
  zygo exec    ▪█                                                   1.4 ms, clean state
  ─────────────────────────────────────────────────────────────────────────────
  ░ programs talking to programs   ▒ the isolation itself (about the same everywhere)
  ▓ interpreter start-up           ▪ a fork            █ your code
```

The isolation is not what costs. Each saving below removes one of the other
blocks.

## Saving 1: no chain of programs

`docker run` passes through a CLI, a root daemon, `containerd`, a shim and
`runc`, with a socket or a new process at each step, and each of them keeps
its own records. `zygo run` is one process that makes the syscalls itself.
There is nothing to ask and nothing to wait for. This alone takes a one-shot
sandbox from hundreds of milliseconds down to about 12 ms, of which most is
the namespace set, the cgroup and the mounts — work the kernel has to do
whoever asks for it.

## Saving 2: no container object to create or remove

Docker makes a record for every container, a writable layer on disk, and log
files, and later has to delete them. Zygo makes none of these. The root is
read-only and shared; the one writable place is a tmpfs that disappears when
the process does. There is no `zygo rm` because there is nothing to remove.
On a busy machine this also saves disk writes and the slow build-up of old
containers that someone has to clean.

## Saving 3: the interpreter starts once, not per request

This is the big one. A Python function that imports a few modules needs
roughly 100 to 500 ms before it can run a single line of your code. A
container per request pays that every time, however fast the container is. A
Zygo zygote pays it once, at `zygo serve`, and every request after that is a
`fork()` of a process where it is already done: about 1.4 ms. The same holds
for Node with a large dependency tree, or any runtime whose start-up is slow.

```text
  per-request cost, same handler, same host (from the embedder's benchmark)
  ────────────────────────────────────────────────────────────────────────
  a container per request   ████████████████████████████████████████  542.4 ms
  a one-shot sandbox        █████▏                                     70.8 ms
  a warm fork               ▏                                           2.8 ms
```

The one-shot row is still slow here because it starts Python each time, and
the warm row includes starting the `zygo` CLI itself — through the API it is
closer to the 1.4 ms above. [The embedder's benchmark](25-performance.md#the-embedders-benchmark) has
the full setup.

## Saving 4: memory is shared, not copied

A forked child shares every page of the zygote until it writes to one
([chapter 1](01-kernel-and-process.md#copy-on-write)). A request that reads a
lot and writes little costs very little new memory. The zygotes also share
with each other: a hundred warm Python scripts on the same image share almost
all of the interpreter's pages through the image's files. Measured, each warm
script costs about 11 MB of its *own* memory, not the 21 MB its process seems
to use, so around 300 warm scripts fit in 4 GB.
[ADR 0005](adr/0005-one-warm-zygote-per-script-version.md) has the numbers.

## Saving 5: compiled bytecode is built once

The official `python:*-slim` images ship no compiled `.pyc` files, and a
read-only root means Python cannot save the ones it compiles. So every run
compiled every module it imported again — `import ssl` alone took 66 ms. Zygo
compiles the standard library once, into a layer of its own, the first time
it sees such an image. Importing ten common modules went from 165 ms to
35 ms. [What Zygo costs](25-performance.md#python-bytecode)
has the details.

## Saving 6: no image builds for dependencies

With Docker, a new Python package means a new Dockerfile step, a build, a new
image and often a push. With Zygo, you list the packages and it builds a venv
once, keyed on the image and the exact list, and shares it with every function
that asks for the same thing. Changing a function's code touches no image at
all. This saves the build time, the registry space, and the "which image has
which version" work that grows with every function.

## Saving 7: clean-up is one write

Killing a process tree safely is hard: children can fork while you list them.
Zygo puts each request in its own cgroup and ends it with one write to
`cgroup.kill`. No scanning, no race, no leftover process. A timeout is cheap,
certain, and it never touches the zygote or another request.

## Saving 8: nothing to run or guard

There is no root daemon to keep running, patch, watch and protect. Access to
Docker's socket is the same as root on the host; Zygo has no such socket.
The supervisor is a normal process under your user, started when needed. That
is a saving in people's time, not in milliseconds — but on a real team it is
often the largest one.

## The advantages, in one table

| | What you get | Where it comes from |
|---|---|---|
| **Speed** | ~1.4 ms per warm request; ~12 ms per fresh sandbox | no chain of programs, and a fork instead of a start |
| **Clean state** | request *n* cannot see anything request *n−1* did | every request is a copy of a zygote that never served one |
| **A limit per request** | memory, CPU, processes and a deadline for each request, not each container | one cgroup per request |
| **Density** | hundreds of warm functions per machine | copy-on-write sharing between and inside zygotes |
| **Safe defaults** | no network, read-only root, no capabilities, every limit set | the defaults are chosen for code you did not write |
| **No root, no daemon** | nothing to install as a service, nothing to protect as root | user namespaces and delegated cgroups |
| **No builds** | dependencies declared, built once, shared | venvs and derived layers keyed on content |
| **One wall, three strengths** | `ns`, `gvisor` or `vm` with the same spec and command | backends behind one flag |

## What is not saved

Your own code costs what it costs; Zygo only removes the work around it. A
warm function uses memory while it waits — about 11 to 21 MB for a Python
zygote — and after `cold_after` it is dropped and the next request pays the
warm-up again. The `ns` backend shares the host's kernel, so a kernel bug
still defeats it, as it defeats every container. And Zygo is one machine: it
does not spread work across servers, publish ports, or run long-lived
services. [The threat model](23-security.md) and
[the comparison](10-similar-projects.md#what-zygo-does-not-do) say where the
edges are.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [6. How Zygo works](06-how-zygo-works.md) · [Contents](README.md) · **Next: [8. The rules Zygo is built on](08-principles.md) →**
