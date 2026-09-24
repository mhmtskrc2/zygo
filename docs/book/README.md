# The Zygo book

**← Previous page:** [the project README](../../README.md) — the book's first
page, and the five-minute version of everything below.

All of Zygo's documentation, in one place, written to be read from the start.

It begins with **Container 101**: what the Linux kernel gives you to build a
sandbox, and what Docker builds from it. Then it explains Zygo — how it
works, where it saves time and memory, and how it compares with everything
around it. Then it teaches you to use it, and ends with the full reference:
every command, every flag, every field, every file and every exit code. You
do not need to know any of it already. If you can use a shell and you know
what a process is, you can read every page.

Each section is short on purpose — a few sentences, a picture where one
helps, and a link to the detail. Read Parts I and II once; after that, the
headings work as a reference. **This book is the source of truth**: when Zygo
changes, the book changes in the same commit ([`AGENTS.md`](../../AGENTS.md)).

## Part I — Container 101

| | | |
|---|---|---|
| 0 | [The project README](../../README.md) | What Zygo is, why it exists, and how to try it — the first page. |
| 1 | [The kernel and the process](01-kernel-and-process.md) | What a process is, how one is born, and who may do what. |
| 2 | [Namespaces](02-namespaces.md) | How a process gets its own view of the machine. |
| 3 | [Control groups](03-cgroups.md) | How a group of processes gets a limit on what it can use. |
| 4 | [The other locks](04-other-locks.md) | Capabilities, seccomp, Landlock, the root filesystem, the network. |
| 5 | [Docker](05-docker.md) | What a container is, what an image is, and where Docker's time goes. |

## Part II — Zygo, explained

| | | |
|---|---|---|
| 6 | [How Zygo works](06-how-zygo-works.md) | The one-shot sandbox, the warm zygote, and the parts around them. |
| 7 | [Where the time and memory are saved](07-where-zygo-saves.md) | Each saving, how big it is, and what it costs. |
| 8 | [The rules Zygo is built on](08-principles.md) | Eight principles, and the price of each. |
| 9 | [FreeBSD jails, and Zygo](09-jails.md) | The older idea, and whether "jails for Linux" is fair. |
| 10 | [Similar projects, and Docker side by side](10-similar-projects.md) | nsjail, bubblewrap, kern, gVisor, Firecracker…; `docker run` against `zygo run`, flag by flag. |

## Part III — Using Zygo

| | | |
|---|---|---|
| 11 | [Getting started](11-getting-started.md) | Install, check the host, run a sandbox, warm a function. |
| 12 | [One-shot sandboxes](12-one-shot-sandboxes.md) | `zygo run`, step by step. |
| 13 | [Warm functions](13-warm-functions.md) | Handlers, warm-exec, runtime pools, and living with them. |
| 14 | [Limits, networking and secrets](14-limits-network-secrets.md) | What each limit does, the egress allowlist, secrets as files. |
| 15 | [Images and dependencies](15-images-and-dependencies.md) | Registries, venvs, apt layers, bytecode, cleaning up. |
| 16 | [Deploying and running in production](16-production.md) | `zygo up`, watching, upgrading, containers, Kubernetes, capacity. |
| 17 | [The HTTP API, the SDKs and MCP](17-api-sdk-mcp.md) | Calling Zygo from a program or an AI agent. |
| 18 | [Writing an agent](18-writing-an-agent.md) | A warm path for a language of your own. |

## Part IV — Reference

| | | |
|---|---|---|
| 19 | [Every command](19-commands.md) | Every command and flag, with defaults and exit codes. |
| 20 | [`sandbox.toml`, field by field](20-sandbox-toml.md) | Every section and field, its default and its rules. |
| 21 | [Environment, files and exit codes](21-environment-files-exit-codes.md) | Every variable read, every file written, every way it ends. |
| 22 | [Troubleshooting](22-troubleshooting.md) | The errors people hit, and the fix for each. |

## Part V — Security and speed

| | | |
|---|---|---|
| 23 | [Security: the threat model](23-security.md) | Every attack, what stops it, and whether a test tries it. |
| 24 | [Seccomp profiles](24-seccomp-profiles.md) | The three syscall profiles, and which to choose. |
| 25 | [What Zygo costs](25-performance.md) | Every measured number, and the machine it came from. |

## Part VI — Decisions

| | | |
|---|---|---|
| 26 | [Why it is built this way](26-decisions.md) | The design decisions in plain words; the full records are in [`adr/`](adr/). |
| | [Glossary](glossary.md) | Every term in the book, in one line each. |

## Where to start

```text
  first time here? ────────────────▶ the README, then this page
  new to containers? ──────────────▶ Part I, then Part II
  know Docker, new to Zygo? ───────▶ chapters 6, 7, 10, then 11
  want to use it today? ───────────▶ chapter 11, then 13
  looking something up? ───────────▶ Part IV
  deciding whether to trust it? ───▶ chapter 23
```

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

Every number about Zygo in this book comes from [chapter 25](25-performance.md),
which names the machines and the commands. Numbers about other projects are
their own claims or commonly measured ranges, and they are marked that way.
`zygo bench all` repeats Zygo's numbers on your own machine.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [Zygo — start here](../../README.md) · **Next: [1. The kernel and the process](01-kernel-and-process.md) →**
