# Summary

An mdBook table of contents; every page is plain Markdown and reads fine
without mdBook. All of Zygo's documentation lives in the book; its first page
is the project README.

[Zygo — start here](../README.md)
[About this book](book/README.md)

# Part I — Container 101

- [1. The kernel and the process](book/01-kernel-and-process.md)
- [2. Namespaces](book/02-namespaces.md)
- [3. Control groups](book/03-cgroups.md)
- [4. The other locks](book/04-other-locks.md)
- [5. Docker](book/05-docker.md)

# Part II — Zygo, explained

- [6. How Zygo works](book/06-how-zygo-works.md)
- [7. Where the time and memory are saved](book/07-where-zygo-saves.md)
- [8. The rules Zygo is built on](book/08-principles.md)
- [9. FreeBSD jails, and Zygo](book/09-jails.md)
- [10. Similar projects, and Docker side by side](book/10-similar-projects.md)

# Part III — Using Zygo

- [11. Getting started](book/11-getting-started.md)
- [12. One-shot sandboxes](book/12-one-shot-sandboxes.md)
- [13. Warm functions](book/13-warm-functions.md)
- [14. Limits, networking and secrets](book/14-limits-network-secrets.md)
- [15. Images and dependencies](book/15-images-and-dependencies.md)
- [16. Deploying and running in production](book/16-production.md)
- [17. The HTTP API, the SDKs and MCP](book/17-api-sdk-mcp.md)
- [18. Writing an agent](book/18-writing-an-agent.md)

# Part IV — Reference

- [19. Every command](book/19-commands.md)
- [20. `sandbox.toml`, field by field](book/20-sandbox-toml.md)
- [21. Environment, files and exit codes](book/21-environment-files-exit-codes.md)
- [22. Troubleshooting](book/22-troubleshooting.md)

# Part V — Security and speed

- [23. Security: the threat model](book/23-security.md)
  - [Fork safety, question by question](book/fork-safety.md)
- [24. Seccomp profiles](book/24-seccomp-profiles.md)
- [25. What Zygo costs](book/25-performance.md)

# Part VI — Decisions

- [26. Why it is built this way](book/26-decisions.md)
  - [ADR 0001 — The embedded runtime](book/adr/0001-embedded-runtime.md)
  - [ADR 0002 — Warm paths stay on `ns`](book/adr/0002-warm-paths-stay-on-ns.md)
  - [ADR 0003 — No Deno or Bun agent](book/adr/0003-no-deno-or-bun-agent.md)
  - [ADR 0004 — A supervisor upgrade re-warms](book/adr/0004-no-supervisor-reexec.md)
  - [ADR 0005 — One warm zygote per script version](book/adr/0005-one-warm-zygote-per-script-version.md)
  - [ADR 0006 — The memory limit is each request's](book/adr/0006-memory-limit-per-request.md)
- [Glossary](book/glossary.md)
