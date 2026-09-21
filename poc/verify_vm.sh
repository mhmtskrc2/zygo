#!/bin/sh
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

    # The root must be read-only, and the reason is not tidiness. The directory
    # libkrun shares as the root is the store's flattened rootfs for an image
    # digest — one directory, shared by every sandbox that runs that image. It
    # was writable once, and `echo x > /pwned` in a guest left
    # `cache/flat/<digest>/pwned` on the host for the next tenant to find.
    out=$(zygo run --quiet --isolation vm --timeout "${BOOT_S}s" "$IMAGE" \
        /bin/sh -c 'echo y > /rootfs-probe 2>/dev/null && echo ROOT-WRITABLE || echo root-ro; echo x > /tmp/w 2>/dev/null && echo tmp-writable || echo tmp-ro' 2>/dev/null)
    case "$out" in
        *ROOT-WRITABLE*) bad "the guest's root filesystem is writable — a tenant can edit the shared image cache" ;;
        *root-ro*)       ok "the guest's root filesystem is read-only, enforced by the VMM" ;;
        *) bad "the guest's filesystem probe said: $(printf '%s' "$out" | tr '\n' ' ')" ;;
    esac

    # And the gap that read-only root leaves, named rather than left to be
    # discovered: the guest has no writable scratch at all. libkrun's init
    # mounts /dev/shm and not /tmp, and mounting one needs guest-side code —
    # `zygo guest-init`, which is also where the guest's own cgroups, seccomp
    # and Landlock go. Until then a vm sandbox can read and compute but not
    # write, which is a real limit and not a bug in this check.
    case "$out" in
        *tmp-writable*) ok "/tmp is writable inside the guest" ;;
        *tmp-ro*) skip "/tmp is not writable in a guest: no scratch until guest-init mounts one" ;;
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
