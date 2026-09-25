# Changelog

Everything a user would notice, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) — before 1.0, a minor version may
break things and will say so here.

## [Unreleased]

### Security

- A warm request no longer finds files the previous request left in its
  temp folder. `/tmp` is one tmpfs per sandbox — per runtime pool, across
  tenants — and nothing cleared it. Both agents now give each request its own
  folder under `/work`, point `TMPDIR`/`TMP`/`TEMP` (and Python's `tempfile`)
  at it, and remove it afterwards. A literal `/tmp/...` path is still shared.
- A read-only bind mount is now read-only all the way down. Before, a mount
  *below* the source directory stayed writable inside the sandbox; Landlock
  hid that on 5.13 and later, nothing did on older kernels. Uses
  `mount_setattr(AT_RECURSIVE)` on 5.12+, one remount per submount below.
- Every bind mount, `:rw` included, is now `nosuid,nodev`, recursively.
- Under `egress` and `full`, the final reject covers IPv6 as well; an IPv6
  connection outside the allowlist used to meet a silent drop instead of an
  immediate refusal. Multicast, `0.0.0.0/8` and `240.0.0.0/4` joined the
  ranges that stay closed without `--allow-private-net`.
- A launch whose parent dies at the wrong moment is now caught. The old check
  (`getppid() == 1`) could never fire inside a new pid namespace.

### Changed

- `zygo doctor --fix` on Ubuntu 24.04 installs an AppArmor profile that
  lets the `zygo` binary alone use user namespaces, instead of turning
  `kernel.apparmor_restrict_unprivileged_userns` off for every process. The
  sysctl remains the fallback where no profile can be loaded; the profile is
  also shipped as `packaging/apparmor/zygo`.
- **Breaking, Python SDK:** the module is `zygo_sdk`, not `zygo`. PyPI's
  `zygo` belongs to another project, and two packages that install the same
  module overwrite each other. `import zygo_sdk as zygo` keeps existing code
  working. Both SDKs are 0.1.1, and the release publishes them to PyPI and
  npm as `zygo-sdk`.
- The seccomp syscall table covers every syscall in the Linux 6.10 headers.
  Each syscall Linux 5.11–6.10 added was decided on: `fchmodat2`,
  `epoll_pwait2`, the `futex_*` family, Landlock, `mseal` and
  `map_shadow_stack` are allowed; the new mount API, `pidfd_getfd`,
  `memfd_secret`, `cachestat`, `statmount`, `listmount` and `lsm_*` answer
  `EPERM`. Only syscalls newer than 6.10 answer `ENOSYS`.
- The seccomp compiler no longer has a length limit near 250 instructions.

### Added

- Chapter 17: adding `zygo mcp` to Claude Code and Codex.
- Chapter 11: Windows through WSL2 — systemd, cgroup v2 only, then the
  Linux install. Not yet tested by the project, and marked so.
- A `.devcontainer` for VS Code and Codespaces in which sandboxes run: it
  arranges the container's cgroup tree at start and wraps `zygo` to begin in
  it.
- `make bench-record` and `bench/`: the raw JSON behind chapter 25's numbers,
  one folder per run, with records from Linux 6.8 and 5.10. Chapter 25 also
  breaks the warm request's 1.4 ms into its phases.
- A book page on fork safety: memory, ASLR, secrets, threads, random
  numbers, inherited connections, seccomp and shared pages, question by
  question. The pages on secrets now say that requests of the same function
  running at once share its secret files.
- `ROADMAP.md`: where the plan stands and what is next. The ADRs point at it
  instead of at planning notes that were never in the repository.
- The book as a searchable website on GitHub Pages, built by mdBook on every
  pull request and published from `main`; `make docs-site` builds it locally.
- Releases carry an SBOM (`zygo-<version>.cdx.json`, CycloneDX) and a
  keyless cosign signature over `SHA256SUMS`, so every tarball is verifiable,
  not only the image.
- `CONTRIBUTING.md`, a code of conduct, issue and pull request templates, and
  this changelog. `NOTICE`, and an SPDX licence line in every source file.
- `cargo deny` in CI and as `make deny`: RustSec advisories, licences and
  sources for the whole dependency tree. Dependabot keeps the pinned actions
  and the crates current.

### Fixed

- `zygo doctor --fix` never offered to take `pasta` out of AppArmor's
  enforce mode unless it ran as root: it read a file only root may read. It
  now falls back to the profile on disk, so the user who needs `egress` on
  Ubuntu is told.
- `spec/openapi.json` said version 0.1.0 in the 0.1.1 release.
- The docs disagreed with themselves: the escape suite's size (16, 17 or 22),
  the container's flags (5, 7 or 9), and the cost of reaching the Mac's VM
  (100 ms or 22 ms). One answer each now, taken from the suite and chapter 25.

## [0.1.1] — 2026-09-25

The first release with every artefact published: static Linux binaries for
x86_64 and aarch64, macOS binaries, a Homebrew formula, a signed multi-arch
image on ghcr.io, and `zygo-core` / `zygo-cli` on crates.io. The code is
v0.1.0's; the release job that built v0.1.0 failed part-way, so v0.1.0 has
no image.

## [0.1.0] — 2026-09-25

The first public version.

- `zygo run`: a one-shot sandbox from an OCI image — namespaces, cgroup v2,
  seccomp and Landlock in one process, rootless, no daemon.
- `zygo serve` and `zygo exec`: warm functions. An interpreter starts once;
  each request is a `fork()` of it, with its own cgroup, deadline and secrets.
- Python and Node reference agents, a POSIX sh agent, and a language-neutral
  agent protocol with a conformance suite (`zygo agent test`).
- An HTTP API, Python (sync and async) and Node clients, and an MCP server.
- `egress` networking with an allowlist, a resolver of its own, and private
  ranges closed by default.
- `gvisor` and `vm` backends for one-shot sandboxes.
- `zygo doctor`, and the book.

[Unreleased]: https://github.com/mhmtskrc2/zygo/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/mhmtskrc2/zygo/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/mhmtskrc2/zygo/releases/tag/v0.1.0
