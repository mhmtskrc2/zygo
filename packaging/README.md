# Packaging

| | |
|---|---|
| [`homebrew/zygo.rb.in`](homebrew/zygo.rb.in) | The Homebrew formula, as a template. |
| [`homebrew/render.sh`](homebrew/render.sh) | Fills the digests in from a built release. |

The formula is a template rather than a formula because a formula's `sha256`
lines are of archives that do not exist until the release is built. The
`homebrew` job in [`.github/workflows/release.yml`](../.github/workflows/release.yml)
renders it from the artifacts, attaches the result to the release, and the
`tap` job copies it into the tap repository, so what a user installs is a
formula whose digests were computed from the files they are about to download.

Homebrew only installs formulas from a *tap* — a repository named
`homebrew-<something>` — and refuses a formula file on its own ("Homebrew
requires formulae to be in a tap"). So users install with:

```bash
brew install mhmtskrc2/zygo/zygo
```

### Setting up the tap, once

1. Create the public repository `mhmtskrc2/homebrew-zygo`, with an empty
   `Formula/` directory (a README is enough to make the first commit).
2. Create a fine-grained GitHub token with **Contents: read and write** on
   that one repository, and nothing else.
3. Add it to this repository's Actions secrets as `HOMEBREW_TAP_TOKEN`.

Without the secret the `tap` job prints a warning and changes nothing; the
rest of the release is unaffected, and `brew install` keeps offering the
previous version.

The macOS archive carries a Linux binary as well as the macOS one. That is not
padding: on a Mac `zygo` is a shim that forwards every sandbox command into a
Linux VM, and it looks for the Linux build at `../share/zygo/zygo-linux-<arch>`
relative to itself. Installing the macOS binary alone installs something that
cannot run a sandbox.
