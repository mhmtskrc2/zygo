# Packaging

| | |
|---|---|
| [`homebrew/zygo.rb.in`](homebrew/zygo.rb.in) | The Homebrew formula, as a template. |
| [`homebrew/render.sh`](homebrew/render.sh) | Fills the digests in from a built release. |

The formula is a template rather than a formula because a formula's `sha256`
lines are of archives that do not exist until the release is built. The
`homebrew` job in [`.github/workflows/release.yml`](../.github/workflows/release.yml)
renders it from the artifacts and attaches the result to the release, so what
a user installs is a formula whose digests were computed from the files they
are about to download.

Installing it by hand, from a release:

```bash
brew install lima
curl -fsSLO https://github.com/zygo-dev/zygo/releases/latest/download/zygo.rb
brew install --formula ./zygo.rb
```

The macOS archive carries a Linux binary as well as the macOS one. That is not
padding: on a Mac `zygo` is a shim that forwards every sandbox command into a
Linux VM, and it looks for the Linux build at `../share/zygo/zygo-linux-<arch>`
relative to itself. Installing the macOS binary alone installs something that
cannot run a sandbox.
