# Working on Zygo

Rules for anyone changing this repository — people and coding agents alike.

## The book is the documentation

All user-facing documentation lives in one place: [`docs/book/`](docs/book/README.md).
Its first page is the project [`README.md`](README.md); the second is
[`docs/book/README.md`](docs/book/README.md). There are no other doc pages;
`docs/SUMMARY.md` is only the mdBook table of contents for it. The book is
the reference a developer using Zygo reads first, and the **source of
truth**: where the book and the code disagree, one of them
is a bug.

It has six parts: Container 101 (the kernel, namespaces, cgroups, the other
locks, Docker), Zygo explained, using Zygo, the reference, security and speed,
and the decisions. Part IV documents every command, flag, `sandbox.toml`
field, environment variable, file, exit code, and — in chapter 17 — every
HTTP route.

## Every change keeps the book true

**A change that alters anything a user can see updates the book in the same
commit.** Find the chapter from this table:

| If you change… | Update |
|---|---|
| a command, subcommand, flag, alias or default (`crates/zygo-cli/src/cli.rs`) | [19-commands.md](docs/book/19-commands.md), and the teaching chapter that uses it (11–18) |
| a `sandbox.toml` field or section, its default, syntax or validation (`crates/zygo-core/src/spec/`) | [20-sandbox-toml.md](docs/book/20-sandbox-toml.md), and 14/15 where it is taught |
| an environment variable, a file or folder written, a path, an exit code, an `--outcome` field | [21-environment-files-exit-codes.md](docs/book/21-environment-files-exit-codes.md) |
| an HTTP route, its auth, body or status codes; an SDK method; an MCP tool | [17-api-sdk-mcp.md](docs/book/17-api-sdk-mcp.md) |
| the agent protocol or the handler contract | [13-warm-functions.md](docs/book/13-warm-functions.md), [18-writing-an-agent.md](docs/book/18-writing-an-agent.md) |
| an error message a user acts on, or a `doctor` check | [22-troubleshooting.md](docs/book/22-troubleshooting.md) |
| a security control, an escape test, a seccomp profile | [23-security.md](docs/book/23-security.md), [24-seccomp-profiles.md](docs/book/24-seccomp-profiles.md) |
| a measured number | [25-performance.md](docs/book/25-performance.md) first, then every chapter that quotes it, and the README |
| the pitch, install steps, status or layout | [README.md](README.md) — page one — and the chapter that teaches it (11, 16) |
| how a mechanism works — the fork path, the cgroup tree, the network, the mounts | Part I–II chapters that explain it (01–07) |
| a design decision | a new ADR in [`docs/book/adr/`](docs/book/adr/) and a section in [26-decisions.md](docs/book/26-decisions.md) |
| a new term | [glossary.md](docs/book/glossary.md) |

A new chapter goes into `docs/book/README.md` and `docs/SUMMARY.md`; then
`make docs-nav` rewrites the previous / next links at the foot of every page
from `SUMMARY.md`'s order (`make lint` fails while they are out of date —
never edit them by hand). If a
change needs none of this, say so in the commit message ("no user-visible
change"), so a reviewer knows it was considered rather than forgotten.

## How the book is written

* **Container 101.** The reader is a developer, not a kernel expert. Teach;
  never assume. Explain a technical term the first time a chapter uses it, and
  add it to the glossary.
* **Plain English.** Use the most common few thousand words. Short sentences,
  active voice.
* **Plain words before jargon.** If an everyday phrase says it, use that:
  "usually 2.1 ms · 1 in 100: 2.9 ms", not "2.07 ms p50, 2.92 ms p99". Keep
  terms like p50, RSS or PSS for chapter 25, where they are defined. An
  explanation that needs a paragraph to explain itself is too complicated —
  cut it down to two sentences and a picture.
* **Short sections.** Five to ten sentences under a heading — a bird's-eye
  view first, then the detail, then a link. Tables and diagrams do not count.
* **A diagram wherever one helps.** Text diagrams in ```` ```text ```` blocks,
  drawn with box characters, so they render on GitHub, in mdBook and in a
  terminal. Every box's borders line up; lines stay under 100 columns.
* **Written from the code.** Defaults, flags and limits are checked against
  the source, not copied from an older page.
* **Measured, not asserted.** A number about Zygo comes from chapter 25 and
  names what it was measured on. A number about another project is marked as
  its claim.
* **Say what is not built.** A limitation belongs in the book as much as a
  feature does.
* **English only**, like everything else in the repository.

## Before you commit

* `make test` passes; `make lint` is clean.
* Every new flag appears in chapter 19 with its default, and
  `zygo <command> --help` agrees with it.
* Every new `sandbox.toml` field appears in chapter 20.
* Links inside `docs/` and the README resolve, and every diagram's boxes
  line up.
