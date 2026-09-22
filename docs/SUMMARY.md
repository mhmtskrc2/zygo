# Summary

An mdBook table of contents; every page is plain Markdown and reads fine
without mdBook.

- [Quickstart](quickstart.md)
- [The guide](guide.md)
- [Concepts](concepts.md)

# Reference

- [`sandbox.toml`](spec-reference.md)
- [What Zygo costs](performance.md)
- [The embedder’s benchmark](bench-embed.md)
- [Troubleshooting](troubleshooting.md)

# Security

- [Threat model](threat-model.md)
- [Seccomp profiles](seccomp-profiles.md)

# Embedding it

- [The Python and Node SDKs](sdk.md)
- [The MCP server](mcp.md)
- [Writing an agent](agents.md)

# Around it

- [Comparison with Docker, gVisor, Firecracker and Lambda](comparison.md)

# Decisions

- [ADR 0001 — The embedded runtime](adr/0001-embedded-runtime.md)
- [ADR 0002 — Warm paths stay on `ns`](adr/0002-warm-paths-stay-on-ns.md)
- [ADR 0003 — No Deno or Bun agent](adr/0003-no-deno-or-bun-agent.md)
- [ADR 0004 — A supervisor upgrade re-warms](adr/0004-no-supervisor-reexec.md)
