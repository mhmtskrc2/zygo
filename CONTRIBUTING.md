# Contributing to Zygo

Thank you for looking. Zygo is young and has one maintainer, so a bug report
with a reproducer, a fixed typo in the book, or a question that shows where
the docs fall short is as useful as code.

**Security problems are not issues.** A sandbox escape, a policy bypass or a
secret that leaks goes through [SECURITY.md](SECURITY.md), privately.

## Where to start

| You want to… | Do this |
|---|---|
| report a bug | open an issue with the *Bug report* form; `zygo doctor` output and `uname -r` save a round trip |
| ask a question or float an idea | open an issue with the *Feature request* form, or a discussion if they are enabled |
| fix something small | send a pull request straight away |
| change behaviour, add a flag, or add a backend | open an issue first, so the design is agreed before the code |
| pick something up | look for the `good first issue` label |

## Building

You need Rust 1.88 or newer. Everything that needs a Linux kernel — the
sandbox itself — runs in Docker, so a Mac works for all of it. Or open the
repository in its dev container (VS Code, or a Codespace), where the tools are
installed and sandboxes run directly.

```bash
make build          # the zygo binary
make test           # Rust, agent and SDK suites
make lint           # rustfmt, clippy with warnings as errors, rustdoc with warnings as errors, and the book's links
make check-linux    # type-check the Linux-only code from a non-Linux host
make test-linux     # the full suite inside a Linux container
make lint-linux     # clippy against the Linux-only code, where most unsafe lives
```

The lint policy is the `[workspace.lints]` table in `Cargo.toml`; a lint goes
in only after clippy is clean with it on macOS and on Linux, and the comment
there says which ones were tried and left out. CI also builds with the
`rust-version` in `Cargo.toml`, so a change that needs a newer compiler must
bump it.

The suites that attempt real things against a real kernel:

```bash
make verify-linux             # isolation and limit checks
make verify-supervisor-linux  # end-to-end supervisor lifecycle checks
make escape-linux             # every known escape vector, attempted
make fuzz-linux               # every syscall number, against all three seccomp profiles
make seccomp-matrix-linux     # real packages, and Node, under default and strict
make conformance              # the agent protocol suite, against all three reference agents
```

`make help` lists the rest. A change to the launcher, seccomp, Landlock, the
mounts or the network should come with `make verify-linux escape-linux
fuzz-linux` passing; say in the pull request which kernel you ran them on.

## Where things are

```
crates/zygo-core     the library; the CLI and the bindings sit on top
  spec/              sandbox.toml surface, layering, validation
  image/             OCI references, content-addressed store, registry client
  sandbox/           mount plan and resource limits, backend independent
  cgroup.rs          the two-level cgroup v2 hierarchy
  backend/           ns | gvisor | vm
  protocol/          the warm execution wire protocol
  doctor.rs          environment probing
crates/zygo-cli      the `zygo` binary
agents/python        the reference Python agent and its conformance suite
agents/node          the reference Node agent: a worker pool, not a fork
spec/protocol.md     the wire protocol
sdk/python           the Python client, and the async one beside it
sdk/node             the Node client, with types and no build step
packaging/oci        the container image, and a worker image built on it
tests/linux          the suites that need a real kernel: escapes, syscalls,
                     seccomp, the supervisor, the API; see tests/README.md
tests/poc            the Phase 0 proofs of concept the design was checked on
tools                the seccomp syscall table generator
bench                benchmark records behind chapter 25
```

## The rules the code is held to

[AGENTS.md](AGENTS.md) is the full list, written for people and coding agents
alike. The ones a first pull request most often trips on:

* **The book is the documentation.** A change a user can see updates
  [the book](docs/book/README.md) in the same commit. AGENTS.md has a table
  that says which chapter. If nothing needs updating, say so in the commit
  message ("no user-visible change").
* **A test attempts the thing, not a setting.** Reading a flag passes on a
  kernel that ignores the flag. An escape test runs the escape.
* **A negative check first proves the thing ran.** "Connection refused" is
  also what you get when nothing ran at all.
* **Nothing allocates between `clone3`/`fork` and `execve`.** The child side
  of the launcher is async-signal-safe; see the notes at the top of
  `crates/zygo-core/src/backend/ns/child.rs`.
* **No `unwrap()`, `expect()` or `panic!` outside tests.** Every `unsafe`
  block carries a `SAFETY:` comment, and every `unsafe fn` a `# Safety`
  section.
* **Measured, not asserted.** A number about Zygo comes from
  [chapter 25](docs/book/25-performance.md) and names the host it was
  measured on.

## Pull requests

* One change per pull request, with a commit message that says *why*.
  The history is written as sentences — "Warm-exec is born in its cgroup" —
  not as `fix: …` prefixes.
* `make test` and `make lint` pass. CI runs the rest on three kernels and two
  architectures.
* Add a line under *Unreleased* in [CHANGELOG.md](CHANGELOG.md) for anything a
  user would notice.
* New source files start with `// SPDX-License-Identifier: Apache-2.0`
  (or `#` for Python and shell).

By sending a pull request you agree that your contribution is licensed under
the [Apache License 2.0](LICENSE), the same as the rest of the project.

## Conduct

Everyone taking part is expected to follow the
[Code of Conduct](CODE_OF_CONDUCT.md).
