# The Zygo book

A short book about how a sandbox works on Linux, from the ground up.

Most sandbox projects explain their flags. This book explains the ideas under
the flags: what the Linux kernel gives you, what Docker builds from it, what
Zygo builds from it, and why the two end up so different. You do not need to
know any of it already. If you can use a shell and you know what a process is,
you can read every page.

Each section is short on purpose — a few sentences, a bird's-eye view, and a
link to the full detail when there is one. Read it front to back once; after
that, the headings work as a reference.

## The parts

| | | |
|---|---|---|
| 1 | [The kernel and the process](01-kernel-and-process.md) | What a process is, how one is born, and who is allowed to do what. |
| 2 | [Namespaces](02-namespaces.md) | How a process gets its own view of the machine. |
| 3 | [Control groups](03-cgroups.md) | How a group of processes gets a limit on what it can use. |
| 4 | [The other locks](04-other-locks.md) | Capabilities, seccomp, Landlock, the root filesystem, and the network. |
| 5 | [Docker](05-docker.md) | What a container is, what an image is, and where Docker's time goes. |
| 6 | [How Zygo works](06-how-zygo-works.md) | The one-shot sandbox, the warm zygote, and the parts around them. |
| 7 | [Where the time and memory are saved](07-where-zygo-saves.md) | Each saving, how big it is, and what it costs. |
| 8 | [FreeBSD jails, and Zygo](08-jails.md) | The older idea, what Zygo shares with it, and whether "jails for Linux" is fair. |
| 9 | [Similar projects](09-similar-projects.md) | nsjail, bubblewrap, kern, gVisor, Firecracker and the rest, one by one. |
| | [Glossary](glossary.md) | Every term in the book, in one line each. |

## Three sentences to hold on to

1. **A container is not a thing in the kernel.** It is a normal process with
   several limits put on it; the kernel has no idea what a "container" is.
2. **The limits are cheap; the tools around them are not.** Setting up the
   limits takes about a millisecond. Docker takes hundreds, and most of that is
   programs talking to programs.
3. **Zygo removes the tools, then removes the start-up.** It sets the limits up
   itself, in one process, and then keeps a ready copy of your program waiting,
   so a request costs one `fork()`.

## How the numbers are used

Every number about Zygo in this book comes from [what Zygo costs](../performance.md),
which names the machines and the commands. Numbers about other projects are
their own claims or commonly measured ranges, and they are marked that way.
`zygo bench all` repeats Zygo's numbers on your own machine.
