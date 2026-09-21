#!/bin/sh
# Run the `vm` suite on the Raspberry Pi, from this Mac.
#
# The Pi is the test target and not the build host: it has no toolchain and
# little disk, so the binary and the guest kernel are built here and copied
# over. That is the arrangement the VM plan settled on, and
# this is it written down instead of remembered.
#
# It refuses to run on top of somebody else's session (V9): the data directory,
# the cgroup tree and the supervisor socket are shared, and two runs measure
# each other.
#
# Run:  make verify-vm-pi
set -eu

HOST=${ZYGO_PI:-192.168.1.32}
REMOTE=${ZYGO_PI_DIR:-zygo-vm}
BINARY=poc/zygo-linux-musl-vm
KERNEL=poc/vm-out/Image

[ -x "$BINARY" ] || { echo "no $BINARY — run \`make vm-build\`" >&2; exit 1; }
[ -f "$KERNEL" ] || { echo "no $KERNEL — run \`make vm-kernel\`" >&2; exit 1; }

echo "host $HOST, into ~/$REMOTE"
ssh "$HOST" "mkdir -p ~/$REMOTE/data/backends/krun ~/$REMOTE/poc"

# The checklist first. It refuses when another session is running, and that
# refusal is the point.
scp -q poc/pi_env.sh poc/cgroup_harness.sh poc/verify_vm.sh "$HOST:~/$REMOTE/poc/"
if ! ssh "$HOST" "sh ~/$REMOTE/poc/pi_env.sh"; then
    echo ""
    echo "the host is not ready; nothing was run. Pass --force to pi_env.sh if" >&2
    echo "the processes it found are yours." >&2
    exit 1
fi

echo ""
echo "copying the binary and the kernel"
scp -q "$BINARY" "$HOST:~/$REMOTE/zygo-vm"
# Only when it has changed: the kernel is 24 MB and the link is a home network.
# Staged beside the binary, not inside the data directory: the suite clears
# that and installs the kernel itself.
local_sum=$(shasum -a 256 "$KERNEL" | cut -d' ' -f1)
remote_sum=$(ssh "$HOST" "sha256sum ~/$REMOTE/Image 2>/dev/null | cut -d' ' -f1" || true)
if [ "$local_sum" != "$remote_sum" ]; then
    scp -q "$KERNEL" "$HOST:~/$REMOTE/Image"
    echo "  kernel copied (sha256:$(printf '%s' "$local_sum" | cut -c1-16)…)"
else
    echo "  kernel already there"
fi

echo ""
ssh "$HOST" "cd ~/$REMOTE && SRC=\$HOME/$REMOTE ZYGO=\$HOME/$REMOTE/zygo-vm \
    ZYGO_DATA_HOME=\$HOME/$REMOTE/data KERNEL_STAGE=\$HOME/$REMOTE/Image \
    sh poc/verify_vm.sh"
