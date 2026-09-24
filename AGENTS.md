# Working on Zygo

Rules for anyone changing this repository — people and coding agents alike.
(`docs/agents.md` is a different thing: how to write a Zygo *runtime agent*.)

## The book is the source of truth

[`docs/book/`](docs/book/README.md) is the reference a developer using Zygo
reads first, and it has to stay true. Part II of it —
[the user's reference](docs/book/10-using-zygo.md) onwards — documents every
command, every flag, every `sandbox.toml` field, every environment variable,
every file Zygo writes, every exit code and every API route.

**A change that alters anything a user can see updates the book in the same
change.** That includes:

* a command, subcommand, flag, alias or default added, removed or renamed
  (`crates/zygo-cli/src/cli.rs`);
* a `sandbox.toml` field or section, its default, its syntax or its
  validation (`crates/zygo-core/src/spec/`);
* an environment variable read, a file or directory written, or a path moved;
* an exit code, an `--outcome` field, or an error message a user acts on;
* an HTTP route, its auth, its body or its status codes; an MCP tool; an SDK
  method;
* a measured number that the book quotes (`docs/performance.md` first, then
  the book);
* a change in how a mechanism works that Part I explains — the fork path, the
  cgroup tree, the network, the seccomp profile.

If a change needs none of this, say so in the commit message ("no user-visible
change"), so a reviewer knows it was considered rather than forgotten.

## How the book is written

* **Plain English.** Use the most common few thousand words. Explain a
  technical term the first time it appears, and add it to
  [the glossary](docs/book/glossary.md).
* **Short sections.** Five to ten sentences under a heading; a bird's-eye view,
  with a link to the full detail.
* **A diagram wherever one helps.** Text diagrams in ```` ```text ```` blocks,
  drawn with box characters, so they render on GitHub, in mdBook and in a
  terminal. Keep every box's borders aligned; keep lines under 100 columns.
* **Measured, not asserted.** A number about Zygo comes from
  `docs/performance.md` and names what it was measured on. A number about
  another project is marked as its claim.
* **Say what is not built.** A limitation belongs in the book as much as a
  feature does.
* **English only**, like everything else in the repository.

## Before you commit

* `make test` passes; `make lint` is clean.
* Every new flag appears in [the command reference](docs/book/11-commands.md)
  with its default, and `zygo <command> --help` agrees with it.
* Every new `sandbox.toml` field appears in
  [the spec reference](docs/book/12-sandbox-toml.md) and in
  `docs/spec-reference.md`.
* Links inside `docs/` resolve.
