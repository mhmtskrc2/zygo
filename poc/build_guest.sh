#!/bin/sh
# Builds the Linux `zygo` the macOS shim forwards into — inside the Lima VM
# itself, so a Mac with no Docker on it can still build its own guest.
#
#   make guest-build          # from the Mac; runs this in the VM
#   sh poc/build_guest.sh     # from inside the VM
#
# Same artifact as `make poc/zygo-linux-musl`, same name, same static musl
# link, same size check — only the machine that compiles it differs: the VM
# that will run it rather than a `rust:1-alpine` container. The toolchain
# (rustup, the musl target, musl-gcc and a C compiler for the two `-sys`
# crates) is installed into the VM on first use and kept; the target
# directory lives under the VM's own `~/.cache` so it never collides with the
# Mac's `target/`, which is the same directory seen from the other side.
#
# The repository is reached at the path the Mac has it, because `$HOME` is
# shared at the same path — the one rule the shim enforces, used in reverse.
set -eu

cd "$(dirname "$0")/.."

ARCH=$(uname -m)
case "$ARCH" in
    aarch64 | x86_64) ;;
    *)
        echo "build_guest.sh: no musl target for $ARCH" >&2
        exit 2
        ;;
esac
TARGET="$ARCH-unknown-linux-musl"

export PATH="$HOME/.cargo/bin:$PATH"

if ! command -v cargo >/dev/null 2>&1; then
    echo "installing rustup into the VM (once)" >&2
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --no-modify-path >/dev/null
fi

if ! command -v musl-gcc >/dev/null 2>&1 || ! command -v cc >/dev/null 2>&1; then
    echo "installing musl-tools and a C compiler into the VM (once)" >&2
    sudo apt-get update -qq
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq musl-tools build-essential >/dev/null
fi

if ! rustup target list --installed | grep -qx "$TARGET"; then
    rustup target add "$TARGET"
fi

# The C in `bzip2-sys` and `zstd-sys` has to be compiled against musl too, or
# the link is not static. `cc` reads this variable for exactly that target.
CC_VAR="CC_$(echo "$TARGET" | tr '-' '_')"
export "$CC_VAR=musl-gcc"

export CARGO_TARGET_DIR="$HOME/.cache/zygo/guest-target"

# `--offline` first and the network only as a fallback, for the same reason
# `make poc/zygo-linux-musl` does it: the workspace `[patch]` names a git
# repository this build does not compile, and cargo would otherwise fetch it
# every time.
cargo build --release --locked --offline -p zygo-cli --target "$TARGET" 2>/dev/null ||
    cargo build --release --locked -p zygo-cli --target "$TARGET"

cp "$CARGO_TARGET_DIR/$TARGET/release/zygo" poc/zygo-linux-musl
sh poc/check_dist.sh poc/zygo-linux-musl
