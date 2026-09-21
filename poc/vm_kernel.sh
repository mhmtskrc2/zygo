#!/bin/sh
# The guest kernel, as a file (docs/vm_implementation.md, M0.2 and D8).
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
# it is found out here rather than in M2.
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
image=$(find /build/libkrunfw -name 'Image' -o -name 'vmlinux' -o -name 'bzImage' 2>/dev/null |
        grep -v '/tools/' | head -1)
if [ -z "$image" ]; then
    echo "no kernel image came out of the build:"
    find /build/libkrunfw -maxdepth 2 -name '*.so*' -o -maxdepth 2 -name 'Image*' | head -10
    exit 1
fi

cp "$image" "$OUT/Image"
sha=$(sha256sum "$OUT/Image" | cut -d' ' -f1)
echo ""
echo "  $OUT/Image  $(stat -c %s "$OUT/Image") bytes"
echo "  sha256:$sha"

# What the guest will and will not be able to do. Every one of these is
# something `guest-init` needs in M2, so a missing line is a milestone that
# cannot land rather than a surprise inside a sandbox.
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
