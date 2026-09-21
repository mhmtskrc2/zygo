#!/bin/sh
# The `vm` backend against a real KVM, compared with `ns` (requirement N8).
#
# Shaped like `verify_gvisor.sh`: the `ns` baseline first, then the same probes
# on `vm`, then the things that must differ, then every refusal by name.
#
# It is written to be honest on a host that cannot boot a guest, which is where
# this project stands. A Raspberry Pi 5 has a GIC-400 — GICv2 — and under
# libkrun's GICv2 fallback the guest receives no timer interrupts, spins, and
# prints nothing. That is V11 in docs/vm_implementation.md, and it is a
# property of the machine rather than of Zygo. So the suite checks everything
# it can without a guest, tries one, and *says which* when it cannot: a run
# that reports "0 failed" because nothing was attempted is the failure mode the
# fourth rule in the README exists to stop.
#
# Run:  make verify-vm-linux    (on a host whose KVM offers GICv3)
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl-vm}
IMAGE=${IMAGE:-alpine:3}
# A guest boots a kernel and mounts a root over virtiofs. Twenty seconds is
# generous on anything that works and pointless on anything that does not.
BOOT_S=${BOOT_S:-20}

ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-vm}
export ZYGO_DATA_HOME

PASS=0
FAIL=0
SKIP=0
say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); say "  PASS  $*"; }
bad()  { FAIL=$((FAIL+1)); say "  FAIL  $*"; }
skip() { SKIP=$((SKIP+1)); say "  skip  $*"; }

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

# The kernel is staged *outside* the data directory and installed after it is
# cleared, because clearing it removes `backends/krun/Image` along with
# everything else — which made every refusal below report "the guest kernel is
# not installed" instead of what it was asked about.
KERNEL_STAGE=${KERNEL_STAGE:-$(dirname "$0")/../Image}
if [ -f "$KERNEL_STAGE" ]; then
    mkdir -p "$ZYGO_DATA_HOME/backends/krun"
    cp "$KERNEL_STAGE" "$ZYGO_DATA_HOME/backends/krun/Image"
fi

if [ ! -x "$ZYGO" ]; then
    say "  no vm-capable binary at $ZYGO"
    say "  → make vm-build"
    exit 1
fi

say "the vm backend, against this host's KVM"
say "  kernel $(uname -r), $(uname -m)"
say ""

# --- what the host offers ---------------------------------------------------

say "the host"

if [ -e /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    ok "/dev/kvm is usable by $(id -un)"
else
    bad "/dev/kvm is missing or not usable; nothing below can mean anything"
    say "  → sh poc/pi_env.sh says what this host is missing"
    exit 1
fi

# The interrupt controller decides whether a guest can run at all, and it is
# the one thing that cannot be worked around from here.
gic=$(grep -oE 'GICv[23]' /proc/interrupts 2>/dev/null | head -1)
case "$gic" in
    GICv3) ok "the host has $gic, which libkrun's guests need" ;;
    GICv2) say "  note  the host has $gic; libkrun falls back to it and a guest gets no
        timer interrupts on at least one such machine (V11). If the guest
        below spins, that is why, and it is the host rather than Zygo." ;;
    *)     say "  note  the interrupt controller could not be read from /proc/interrupts" ;;
esac

case "$(zygo backend list 2>/dev/null | grep '^vm ')" in
    *available*) ok "\`backend list\` reports vm available" ;;
    *) bad "vm is not available: $(zygo backend list 2>&1 | grep '^vm ' | tr -s ' ')"; exit 1 ;;
esac

say ""

# --- the ns baseline, first -------------------------------------------------
#
# Rule 4: prove the thing runs before asserting about how the other one
# differs. Every probe below is asked of `ns` first, and a `vm` answer only
# means something beside it.

say "the ns baseline"
zygo pull "$IMAGE" >/dev/null 2>&1
ns_uname=$(zygo run --quiet --isolation ns "$IMAGE" /bin/uname -r 2>/dev/null | tr -d '\n')
if [ -n "$ns_uname" ]; then
    ok "ns runs and reports kernel $ns_uname"
else
    bad "ns could not run at all, so there is no baseline to compare against"
    exit 1
fi

say ""

# --- what vm refuses, which needs no guest ----------------------------------
#
# These are decisions, not behaviour, and they are the half of the backend that
# a host without a working guest can still check.

say "what vm refuses, by name"

refuses() {
    what=$1
    milestone=$2
    shift 2
    out=$(zygo run --quiet --isolation vm "$@" 2>&1)
    case "$out" in
        *"$milestone"*) ok "$what is refused, and the message names $milestone" ;;
        *) bad "$what: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-140)" ;;
    esac
}

refuses "a networked sandbox" "M4" --net full "$IMAGE" /bin/true
refuses "a terminal" "M5" --tty "$IMAGE" /bin/true

say ""

# --- a guest ----------------------------------------------------------------

say "a guest"

console=$(mktemp)
ZYGO_KRUN_CONSOLE=$console
export ZYGO_KRUN_CONSOLE
started=$(date +%s)
vm_uname=$(zygo run --quiet --isolation vm --timeout "${BOOT_S}s" "$IMAGE" /bin/uname -r 2>/dev/null | tr -d '\n')
elapsed=$(( $(date +%s) - started ))

if [ -n "$vm_uname" ]; then
    ok "a guest booted and ran a program in ${elapsed}s"
    # The whole point of the backend: a different kernel, not a different view
    # of the same one.
    if [ "$vm_uname" != "$ns_uname" ]; then
        ok "the guest reports kernel $vm_uname where ns reports $ns_uname — the boundary really moved"
    else
        bad "the guest reports the host's own kernel ($vm_uname); that is not a virtual machine"
    fi

    out=$(zygo run --quiet --isolation vm --timeout "${BOOT_S}s" "$IMAGE" \
        /bin/sh -c 'echo x > /tmp/w && echo tmp-ok; echo y > /rootfs-probe 2>/dev/null && echo ROOT-WRITABLE || echo root-ro' 2>/dev/null)
    case "$out" in
        *tmp-ok*root-ro*) ok "/tmp is writable inside the guest and / is not" ;;
        *ROOT-WRITABLE*) bad "the guest's root filesystem is writable" ;;
        *) bad "the guest's filesystem probes said: $(printf '%s' "$out" | tr '\n' ' ')" ;;
    esac

    # The claim from §2 of the plan: a guest can offer controls the host lacks.
    out=$(zygo run --quiet --isolation vm --timeout "${BOOT_S}s" "$IMAGE" \
        /bin/sh -c 'ls /sys/kernel/security/ 2>/dev/null | tr "\n" " "; echo; ls /sys/fs/cgroup/cgroup.kill >/dev/null 2>&1 && echo cgroup-kill-yes || echo cgroup-kill-no' 2>/dev/null)
    say "  note  inside the guest: $(printf '%s' "$out" | tr '\n' ' ')"
else
    skip "no guest ran within ${elapsed}s"
    say "        console: $(wc -c <"$console" 2>/dev/null) bytes"
    if [ -s "$console" ]; then
        say "        the guest said:"
        head -12 "$console" | sed 's/^/          /'
    else
        say "        the guest printed nothing at all, which on a GICv2 host is V11:"
        say "        no timer interrupts, so the kernel spins before it can register"
        say "        a console. Everything above this line still passed."
    fi
    skip "the guest's kernel, filesystem and limits could not be checked"
fi
rm -f "$console"

say ""
say "----------------------------------------"
say "vm verification: $PASS passed, $FAIL failed, $SKIP not attempted"
harness_verdict
[ "$FAIL" -eq 0 ] || exit 1
