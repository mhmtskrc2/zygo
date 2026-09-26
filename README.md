# Zygo — a warm sandbox per request

**Zygo forks a warm, sandboxed interpreter for every request: 1.4 ms, and
every request starts from a process that has never served one.**
Rootless, OCI images, and no daemon to install: the one long-lived process is a
supervisor under your own user, not a system service.

*This page is the first page of [the Zygo book](docs/book/README.md), also on
the web at [mhmtskrc2.github.io/zygo](https://mhmtskrc2.github.io/zygo/).*

```bash
zygo serve ./handler.py --name resize        # a warm zygote: ~150 ms, once
zygo exec resize '{"url": "..."}'            # a fresh, sandboxed process: 1.4 ms
zygo exec resize '{"url": "..."}'            # and again, from the same clean copy
```

## Why

A warm worker that serves many requests is fast and dirty: request *n* sees
whatever request *n-1* left behind. A container per request is clean and
slow. Zygo is the third thing. `zygo serve` starts an interpreter, lets it do
its imports, and parks it inside a sandbox. `zygo exec` forks it. Each child
has its own cgroup, deadline and secrets, and is thrown away afterwards.

The same import-heavy Python script, run fresh for each call on one host
(a Linux 6.8 VM, 2 vCPU; [chapter 25](docs/book/25-performance.md#the-result-on-the-lima-vm)
has the method, and `make bench-embed` repeats it):

| | usually | what it pays for |
|---|---:|---|
| `docker run --rm` | 542 ms | daemon, containerd, shim, runc, a container object to remove |
| `zygo run` | 70.8 ms | a fresh sandbox — namespaces, cgroup, mounts — in one process |
| **`zygo exec`** | **2.8 ms** | a `fork()` of the warm interpreter, plus starting the CLI |

Most of the 70.8 ms is Python importing those sixteen modules: the sandbox
itself is about 3.6 ms, and `python3 -c pass` inside one is 12 ms. Through
the HTTP API rather than the CLI, a warm request is **1.44 ms** usually and
10.5 ms for 1 in 100, and one function sustains 1,108 requests a second.
`zygo bench all` reproduces every number on your own machine.

The zygote idea is older than Zygo: Android starts apps that way, and
serverless research forked handlers from a pre-imported process in SOCK
(Oakes et al., USENIX ATC 2018) and restored them from a snapshot in
Catalyzer (Du et al., ASPLOS 2020). Zygo's part is the packaging — one
static binary, rootless, every limit on, an agent protocol any language can
speak — not the idea.

## Why not bubblewrap, nsjail, nono, sandbox-runtime or kern?

They are good at what they do, and none of them keeps a sandboxed process warm
and forks it per request.

| | What it is | What Zygo adds |
|---|---|---|
| bubblewrap, nsjail | building blocks for one confined process | images, mandatory limits, an egress allowlist — and the warm fork |
| nono, sandbox-runtime | confinement for a command you were running anyway (Landlock and seccomp, or bubblewrap and Seatbelt) | a sandbox with its own root filesystem and limits, for code you did not write |
| kern | a daemonless, rootless container per call, in a few ms | a warm interpreter: no interpreter start and no imports on the request path |
| E2B, Modal, Daytona | a microVM per session, in their cloud | runs on your hardware, and costs a fork rather than a VM per call |

[Similar projects](docs/book/10-similar-projects.md) compares each of them,
flag by flag, with measurements against nsjail and kern.

## The boundary

The default backend, `ns`, is the host kernel: namespaces, cgroup v2, a
seccomp allowlist, Landlock, no capabilities and a read-only root. That is
the right wall for code that is **semi-trusted** — your customers' scripts,
an agent's tools. A kernel bug is a way through it, as it is for every
container. For hostile code the same spec runs on `gvisor` (a kernel in user
space) or `vm` (libkrun), one-shot only.

Every vector in [the threat model](docs/book/23-security.md) is attempted
by `make escape-linux` — 19 vectors, 0 escapes — and every syscall number is
swept against the seccomp profiles. The same chapter says where the boundary
is weaker than it looks. **No external audit has been done.**
[Fork safety, question by question](docs/book/fork-safety.md) covers what
a fork shares with its parent and what it does not.

## Install

```bash
# Linux, x86_64 or aarch64: one static binary, checked against the release's checksums
url=https://github.com/mhmtskrc2/zygo/releases/latest/download
curl -fsSLO "$url/zygo-$(uname -m)-unknown-linux-musl.tar.gz"
curl -fsSL "$url/SHA256SUMS" | sha256sum -c --ignore-missing
tar xzf zygo-*-unknown-linux-musl.tar.gz && sudo install -m 0755 zygo-*/zygo /usr/local/bin/

brew install mhmtskrc2/zygo/zygo      # macOS: the shim, and a Linux VM it manages
cargo install zygo-cli                # from source
```

Linux needs kernel 5.3 or newer (6.1 recommended), unprivileged user
namespaces and delegated cgroup v2. On a Mac every command runs in a Linux VM,
about 22 ms away. There is also a signed container image.
[Getting started](docs/book/11-getting-started.md) covers all of it,
signatures included.

```bash
zygo doctor                           # can this host run sandboxes? prints the fix if not
echo 'def handler(event): return {"got": event}' > handler.py
zygo serve ./handler.py --name echo && zygo exec echo '{"n": 1}'
zygo run --mem 128M --timeout 10s python:3.12-slim python3 -c 'print("hello")'
```

## What else is in the box

- **One file per project.** `sandbox.toml` declares functions, their images,
  limits, network and secrets; `zygo up` deploys it blue/green and pins image
  digests in `zygo.lock`. [Chapter 20](docs/book/20-sandbox-toml.md)
- **Every limit is on by default** — memory, CPU, pids, wall clock, scratch,
  open files — and the deadline kills the request's whole process tree.
- **The network is off by default.** `egress` is an allowlist of names;
  private ranges and the cloud metadata address stay closed.
  [Chapter 14](docs/book/14-limits-network-secrets.md)
- **Secrets are files**, written from outside the sandbox for one request:
  never environment variables, never in the warm process's memory.
- **Any language.** Python and Node agents ship; anything else is a fresh
  process per request in a held sandbox (about 1.4 ms), or an agent of your own
  against [the protocol](spec/protocol.md).
- **For programs:** an HTTP API, dependency-free Python and Node clients
  (`zygo-sdk`), and an MCP server. [Chapter 17](docs/book/17-api-sdk-mcp.md)
- **For a multi-tenant product:** tenants with their own tokens and budgets,
  a script or a whole workspace (a tar) sent with each request, streamed
  output, and cancellation — all over the API.

**Building on it?** Read [13](docs/book/13-warm-functions.md) (warm
functions), then [17](docs/book/17-api-sdk-mcp.md) (the API, the SDKs, MCP),
then [23](docs/book/23-security.md) (the threat model), in that order.

## Status

v0.1.x: one machine; Linux in production, macOS for development. The warm
path is `ns`-only by decision ([ADR 0002](docs/book/adr/0002-warm-paths-stay-on-ns.md)).
[ROADMAP.md](ROADMAP.md) says what comes next.

**Not for:** an interactive session or a REPL (a request is one call, not a
shell you keep); more than one machine (no scheduler, no cluster); GPUs;
Windows without WSL2.

## More

[Contributing](CONTRIBUTING.md) · [Security policy](SECURITY.md) ·
[Changelog](CHANGELOG.md) · [Roadmap](ROADMAP.md) · Licence: Apache-2.0

Zygo is not affiliated with Zygo Corporation, the metrology company.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

**Next: [The Zygo book](https://mhmtskrc2.github.io/zygo/book/) →** · [in the repository](docs/book/README.md)
