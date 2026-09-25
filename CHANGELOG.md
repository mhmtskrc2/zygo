# Changelog

Everything a user would notice, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) — before 1.0, a minor version may
break things and will say so here.

## [Unreleased]

### Security

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

- Releases carry an SBOM (`zygo-<version>.cdx.json`, CycloneDX) and a
  keyless cosign signature over `SHA256SUMS`, so every tarball is verifiable,
  not only the image.
- `CONTRIBUTING.md`, a code of conduct, issue and pull request templates, and
  this changelog. `NOTICE`, and an SPDX licence line in every source file.
- `cargo deny` in CI and as `make deny`: RustSec advisories, licences and
  sources for the whole dependency tree. Dependabot keeps the pinned actions
  and the crates current.

### Fixed

- `spec/openapi.json` said version 0.1.0 in the 0.1.1 release.

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
