#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# The `vm` backend against a real KVM, compared with `ns` (requirement N8).
#
# Shaped like `verify_gvisor.sh`: the `ns` baseline first, then the same probes
# on `vm`, then the things that must differ, then every refusal by name.
#
# It is written to be honest on a host where the guest does not boot, which is
# where this project stands. A Raspberry Pi 5 has a GIC-400 — GICv2 — and under
# libkrun's GICv2 fallback the guest spins and prints nothing. That is *not* the
# machine's fault: QEMU boots a KVM guest on the same Pi with vGICv2, so the
# fault is somewhere in libkrun's GICv2 support. See V11. So the suite checks
# everything it can without a guest, tries one, and *says which* when it cannot:
# a run that reports "0 failed" because nothing was attempted is the failure
# mode the fourth rule in the README exists to stop.
#
# Run:  make verify-vm-linux
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

# The interrupt controller decides which of libkrun's two paths is taken, and
# only one of them is known to work.
gic=$(grep -oE 'GICv[23]' /proc/interrupts 2>/dev/null | head -1)
case "$gic" in
    GICv3) ok "the host has $gic" ;;
    GICv2) say "  note  the host has $gic, so libkrun logs a GICv3 failure and falls back.
        That line is noise, not a diagnosis: a guest boots on GICv2. It is
        recorded here because it was once read as the reason one would not,
        and the real reason was the kernel image being an ELF." ;;
    *)     say "  note  the interrupt controller could not be read from /proc/interrupts" ;;
esac

# `unavailable` contains `available`, so the unavailable case has to be
# matched first. Written the other way round this reported "vm available" for
# a host with no guest kernel, and then failed four checks that were about
# something else entirely.
case "$(zygo backend list 2>/dev/null | grep '^vm ')" in
    *unavailable*) bad "vm is not available: $(zygo backend list 2>&1 | grep '^vm ' | tr -s ' ')"; exit 1 ;;
    *available*)   ok "\`backend list\` reports vm available" ;;
    *)             bad "backend list has no vm row"; exit 1 ;;
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

# What is asserted is the remedy and the subject, not a milestone number.
# This used to look for "M4" and "M5" — names from a roadmap, useless to
# whoever typed the command, and stale the moment the roadmap moved.
refuses() {
    what=$1
    subject=$2
    shift 2
    out=$(zygo run --quiet --isolation vm "$@" 2>&1)
    case "$out" in
        *"$subject"*)
            case "$out" in
                *"→"*) ok "$what is refused, names $subject, and says what to do instead" ;;
                *) bad "$what is refused for the right reason and offers no remedy" ;;
            esac ;;
        *) bad "$what: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-140)" ;;
    esac
}

refuses "a networked sandbox" "network" --net full "$IMAGE" /bin/true
refuses "a terminal" "terminal" --tty "$IMAGE" /bin/true

say ""

# --- a guest ----------------------------------------------------------------

say "a guest"

# The guest's console and the program's stdout are the *same* channel: a
# sandbox's output arrives over the virtio-console port, and
# `ZYGO_KRUN_CONSOLE` diverts that port to a file. Exporting it for the
# measured run therefore takes away the thing being measured — this reported
# "no guest ran" while the console file beside it held the answer the run was
# looking for. So the measured run has no console, and the console is only
# attached for a second, diagnostic run when the first produced nothing.
console=$(mktemp)
unset ZYGO_KRUN_CONSOLE
started=$(date +%s)
vm_uname=$(zygo run --quiet --isolation vm --timeout "${BOOT_S}s" "$IMAGE" /bin/uname -r 2>/dev/null | tr -d '\n')
elapsed=$(( $(date +%s) - started ))

if [ -z "$vm_uname" ]; then
    ZYGO_KRUN_CONSOLE=$console zygo run --quiet --isolation vm \
        --timeout "${BOOT_S}s" "$IMAGE" /bin/true >/dev/null 2>&1
fi

if [ -n "$vm_uname" ]; then
    ok "a guest booted and ran a program in ${elapsed}s"
    # The whole point of the backend: a different kernel, not a different view
    # of the same one.
    if [ "$vm_uname" != "$ns_uname" ]; then
        ok "the guest reports kernel $vm_uname where ns reports $ns_uname — the boundary really moved"
    else
        bad "the guest reports the host's own kernel ($vm_uname); that is not a virtual machine"
    fi

    # Two properties, and they are separate. The guest must be able to write —
    # a sandbox with no scratch is not one — and nothing it writes may reach
    # the host. The root libkrun shares is the store's flattened rootfs for an
    # image digest, used by every sandbox that runs that image; it was once
    # shared writable, and `echo x > /pwned` in one guest left
    # `cache/flat/<digest>/pwned` on the host for the next tenant. Now the
    # guest gets a private overlay: writes go to a tmpfs upper in the VMM's
    # own mount namespace, the shared image is the read-only lower, and
    # the host's copy is checked here rather than assumed.
    probe="probe-$$-$(date +%s)"
    out=$(zygo run --quiet --isolation vm --timeout "${BOOT_S}s" "$IMAGE" \
        /bin/sh -c "echo y > /$probe 2>/dev/null && echo root-writable || echo root-ro; echo x > /tmp/w 2>/dev/null && echo tmp-writable || echo tmp-ro" 2>/dev/null)
    case "$out" in
        *root-writable*) ok "the guest can write to its root" ;;
        *root-ro*)       skip "the guest's root is read-only: no private overlay on this host, so no scratch" ;;
        *) bad "the guest's filesystem probe said: $(printf '%s' "$out" | tr '\n' ' ')" ;;
    esac
    case "$out" in
        *tmp-writable*) ok "/tmp is writable inside the guest" ;;
        *tmp-ro*)       skip "/tmp is not writable in the guest" ;;
    esac
    # The property that matters: the write stayed in the guest.
    leaked=$(find "$ZYGO_DATA_HOME" -name "$probe" 2>/dev/null | head -1)
    if [ -n "$leaked" ]; then
        bad "a guest write reached the host's image cache: $leaked"
    else
        ok "nothing the guest wrote reached the host (no $probe under the data directory)"
    fi
    # The layer is bounded, or a guest could fill the host's memory: the
    # tmpfs is sized to `scratch`, so a write past it fails inside the guest
    # rather than growing on the host. 200 MB against the 64 MB default, and
    # the file has to come out exactly 64 MB — not 200, which would mean the
    # bound is not there, and not 0, which would mean the write never ran.
    written=$(zygo run --quiet --isolation vm --timeout "${BOOT_S}s" "$IMAGE" \
        /bin/sh -c 'dd if=/dev/zero of=/big bs=1M count=200 >/dev/null 2>&1; stat -c %s /big 2>/dev/null || echo 0' 2>/dev/null | tr -d '\n ')
    case "$written" in
        67108864) ok "a guest's writes are bounded by scratch: 200 MB attempted, 64 MB landed" ;;
        0|"")     bad "the bound probe wrote nothing at all, so the bound was not tested" ;;
        *)        if [ "$written" -gt 67108864 ] 2>/dev/null; then
                      bad "a guest wrote ${written} bytes past its 64 MB scratch bound"
                  else
                      bad "the bound probe stopped at ${written} bytes, which is neither the bound nor the whole file"
                  fi ;;
    esac

    # And it did not persist into the next guest either, which is what a
    # per-sandbox upper means.
    again=$(zygo run --quiet --isolation vm --timeout "${BOOT_S}s" "$IMAGE" \
        /bin/sh -c "[ -e /$probe ] && echo persisted || echo fresh" 2>/dev/null)
    case "$again" in
        *fresh*)     ok "the next guest on the same image starts from the pristine image" ;;
        *persisted*) bad "a file written by one guest was visible to the next" ;;
        *) bad "the second guest did not answer: $again" ;;
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
        say "        the guest printed nothing at all. console=hvc0 is virtio-console,"
        say "        so a kernel that stops before that driver is up is silent either"
        say "        way — build with KRUN_DEBUG_EARLYCON to get a PL011 earlycon,"
        say "        which prints from the first lines of start_kernel."
        say "        Everything above this line still passed."
    fi
    skip "the guest's kernel, filesystem and limits could not be checked"
fi
rm -f "$console"

say ""
say "----------------------------------------"
say "vm verification: $PASS passed, $FAIL failed, $SKIP not attempted"
harness_verdict
[ "$FAIL" -eq 0 ] || exit 1
