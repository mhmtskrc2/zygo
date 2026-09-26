#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# The guest kernel, as a file.
#
# libkrun can boot a kernel from a path — `krun_set_kernel` — and that is what
# keeps the GPL kernel out of an Apache-2.0 binary and a ten-to-twenty megabyte
# `Image` out of a fifteen megabyte budget. libkrunfw is the kernel libkrun is
# tested against, and it ships as a shared library with the image embedded in
# it rather than as a bare file, so this builds it and takes the `Image` out.
#
# It also dumps the config, because M0.2's real question is whether that kernel
# has what a Zygo sandbox needs *inside* the guest: cgroups with the pids
# controller, seccomp, Landlock, overlayfs, nftables, virtiofs and vsock.
# Anything missing is a custom kernel build, which is a week and needs disk, so
# it is found out here rather than while building the guest side.
#
# Run:  make vm-kernel
set -eu

REF=${LIBKRUNFW_REF:-v4.10.0}
OUT=${OUT:-/out}

echo "libkrunfw $REF, for $(uname -m)"
mkdir -p "$OUT"

git clone --quiet https://github.com/containers/libkrunfw.git /build/libkrunfw 2>&1 | tail -2 || true
cd /build/libkrunfw
if ! git rev-parse --verify --quiet "refs/tags/$REF" >/dev/null; then
    echo "published tags, newest last:"
    git tag --sort=version:refname | tail -6 | sed 's/^/  /'
    REF=$(git tag --sort=version:refname | tail -1)
    echo "the requested ref was not published; using $REF"
fi
git checkout --quiet "$REF"
echo "at $(git rev-parse --short HEAD)"

echo ""
echo "building the kernel — this is the long part"
make -j"$(nproc)" >/build/kernel.log 2>&1 || {
    echo "the build failed; the last 30 lines:"
    tail -30 /build/kernel.log
    exit 1
}

# The image, wherever this version put it.
#
# The *boot* image, specifically, and the distinction is the whole of this
# block. A kernel build leaves both `vmlinux` — an ELF, for debuggers — and
# `arch/<arch>/boot/Image`, the thing a bootloader can actually jump to. This
# used to be one `find` with three `-name`s and a `head -1`, which took
# whichever the directory walk reached first: `vmlinux`. libkrun then loaded
# the ELF headers as if they were a kernel, the vCPU executed them, and the
# guest spun at 100% and printed nothing — for which the interrupt controller
# was blamed for a long time.
#
# So: the boot image by its real path, and never `vmlinux`.
arch=$(uname -m)
case "$arch" in
    aarch64) want='arch/arm64/boot/Image' ;;
    x86_64)  want='arch/x86/boot/bzImage' ;;
    *)       want='' ;;
esac

image=""
if [ -n "$want" ]; then
    image=$(find /build/libkrunfw -path "*/$want" 2>/dev/null | grep -v '/tools/' | head -1)
fi
if [ -z "$image" ]; then
    image=$(find /build/libkrunfw \( -name 'Image' -o -name 'bzImage' \) 2>/dev/null |
            grep -v '/tools/' | head -1)
fi
if [ -z "$image" ]; then
    echo "no bootable kernel image came out of the build (looked for $want):"
    find /build/libkrunfw -name 'Image' -o -name 'bzImage' -o -name 'vmlinux' | head -10
    exit 1
fi

cp "$image" "$OUT/Image"

# And then check it, rather than trusting the path it came from. libkrun
# decides what to do with this file from its first bytes, and the formats it
# accepts on aarch64 are raw Image, gzip and the compressed variants — not
# ELF. A build that produces something else should fail here, loudly, and not
# three weeks later as a guest that will not boot.
magic=$(od -An -tx1 -N4 "$OUT/Image" | tr -d ' \n')
arm64=$(od -An -tx1 -j56 -N4 "$OUT/Image" | tr -d ' \n')
case "$magic" in
    7f454c46)
        echo "REFUSED: $image is an ELF (vmlinux), which libkrun cannot load on aarch64."
        echo "         The bootable image is arch/arm64/boot/Image."
        rm -f "$OUT/Image"
        exit 1 ;;
esac
if [ "$arch" = aarch64 ] && [ "$arm64" != "41524d64" ] && [ "${magic%????}" != "1f8b" ]; then
    echo "REFUSED: $image has no arm64 Image header (ARM\\x64 at offset 56) and is not gzip."
    echo "         first bytes: $magic, offset 56: $arm64"
    rm -f "$OUT/Image"
    exit 1
fi

sha=$(sha256sum "$OUT/Image" | cut -d' ' -f1)
echo ""
echo "  from $image"
echo "  $OUT/Image  $(stat -c %s "$OUT/Image") bytes"
echo "  sha256:$sha"

# What the guest will and will not be able to do. Every one of these is
# something `guest-init` will need, so a missing line is work that cannot
# land rather than a surprise inside a sandbox.
echo ""
echo "config, for the controls a Zygo guest needs:"
config=$(find /build/libkrunfw -name '.config' | head -1)
if [ -n "$config" ]; then
    for option in CONFIG_CGROUPS CONFIG_CGROUP_PIDS CONFIG_MEMCG CONFIG_SECCOMP_FILTER \
                  CONFIG_SECURITY_LANDLOCK CONFIG_OVERLAY_FS CONFIG_NF_TABLES \
                  CONFIG_VIRTIO_FS CONFIG_VSOCKETS CONFIG_VIRTIO_VSOCKETS; do
        value=$(grep -E "^$option=" "$config" 2>/dev/null || true)
        if [ -n "$value" ]; then
            echo "  ok       $value"
        else
            echo "  MISSING  $option — a guest cannot do what this enables"
        fi
    done
else
    echo "  (no .config found; the checks above could not be made)"
fi

echo ""
# Single quotes: backticks inside a double-quoted string run a command, and
# this one ran `zygo` and printed "not found" in the middle of a result.
echo 'Pin this in `zygo backend install vm`:'
echo "  libkrunfw $REF, sha256:$sha"
