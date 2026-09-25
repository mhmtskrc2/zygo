#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# PoC 6 — is overlayfs usable inside a user namespace on this kernel?
#
#
# Unprivileged overlayfs landed in Linux 5.11. Below that, a rootless sandbox
# cannot present image layers as an overlay and has to fall back to a flattened
# rootfs, which costs disk and first-run time. This PoC establishes which
# behaviour a given kernel has, so the store's `rootfs_view` decision is based
# on a measurement rather than on a version number alone.
#
# Run:  docker run --rm --privileged -v "$PWD/poc:/poc:ro" debian:12 \
#           sh /poc/poc6_overlayfs_userns.sh
set -u

say() { printf '%s\n' "$*"; }

KERNEL=$(uname -r)
say "PoC 6 — overlayfs inside a user namespace"
say "  kernel $KERNEL"

MAJOR=$(echo "$KERNEL" | cut -d. -f1)
MINOR=$(echo "$KERNEL" | cut -d. -f2)
say "  expected by version: $([ "$MAJOR" -gt 5 ] || { [ "$MAJOR" -eq 5 ] && [ "$MINOR" -ge 11 ]; } && echo 'supported (>= 5.11)' || echo 'NOT supported (< 5.11)')"
say "  overlay in /proc/filesystems: $(grep -c overlay /proc/filesystems)"
say "  max_user_namespaces: $(cat /proc/sys/user/max_user_namespaces 2>/dev/null || echo '?')"
say ""

WORK=/tmp/poc6
rm -rf "$WORK"
mkdir -p "$WORK/lower1" "$WORK/lower2" "$WORK/merged" "$WORK/upper" "$WORK/work"
echo base   > "$WORK/lower1/a"
echo shadow > "$WORK/lower2/a"
echo only2  > "$WORK/lower2/b"

# --- test 1: read-only overlay (no upperdir), which is what Zygo uses --------

say "test 1 — read-only overlay (lowerdir only) inside unshare -Urm"
if unshare -Urm sh -c "
    mount -t overlay overlay -o lowerdir=$WORK/lower2:$WORK/lower1 $WORK/merged 2>/dev/null || exit 10
    cat $WORK/merged/a
    cat $WORK/merged/b
" > /tmp/poc6.out 2>/tmp/poc6.err; then
    say "  merged view: $(tr '\n' ' ' < /tmp/poc6.out)"
    if [ "$(head -1 /tmp/poc6.out)" = "shadow" ]; then
        say "  RESULT: supported — the upper lowerdir correctly shadows the lower one"
        OVERLAY_RO=yes
    else
        say "  RESULT: mounted but the layer order is wrong"
        OVERLAY_RO=broken
    fi
else
    say "  mount failed: $(cat /tmp/poc6.err 2>/dev/null | head -2)"
    say "  RESULT: NOT supported — the flatten fallback is required on this kernel"
    OVERLAY_RO=no
fi
say ""

# --- test 2: writable overlay, needed for the derived system layer ----------

say "test 2 — writable overlay (upperdir) inside unshare -Urm"
say "  used when building a derived apt/nix layer (design doc §3.7)"
if unshare -Urm sh -c "
    mount -t overlay overlay \
        -o lowerdir=$WORK/lower1,upperdir=$WORK/upper,workdir=$WORK/work \
        $WORK/merged 2>/dev/null || exit 10
    echo written > $WORK/merged/new
    rm $WORK/merged/a
    ls $WORK/upper
" > /tmp/poc6b.out 2>/tmp/poc6b.err; then
    say "  upperdir now holds: $(tr '\n' ' ' < /tmp/poc6b.out)"
    say "  RESULT: supported"
    OVERLAY_RW=yes
else
    say "  mount failed: $(head -2 /tmp/poc6b.err 2>/dev/null)"
    say "  RESULT: NOT supported"
    OVERLAY_RW=no
fi
say ""

# --- test 3: whiteouts, the reason rootless extraction is awkward -----------

say "test 3 — can an unprivileged user namespace create an overlayfs whiteout?"
say "  (char device 0:0 — how overlayfs records a deleted file)"
if unshare -Urm sh -c "mknod $WORK/wh c 0 0" 2>/tmp/poc6c.err; then
    say "  RESULT: yes — whiteouts could be written directly into the layer store"
    WHITEOUT=yes
else
    say "  mknod failed: $(head -1 /tmp/poc6c.err 2>/dev/null)"
    say "  RESULT: no — confirms why the store records whiteouts in a sidecar"
    WHITEOUT=no
fi
say ""

rm -rf "$WORK"

say "----------------------------------------"
say "PoC 6 on kernel $KERNEL:"
say "  read-only overlay in userns : $OVERLAY_RO"
say "  writable overlay in userns  : $OVERLAY_RW"
say "  whiteout mknod in userns    : $WHITEOUT"
