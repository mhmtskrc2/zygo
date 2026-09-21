#!/bin/sh
# What a host needs before the `vm` backend can be worked on, as a script.
#
# Section 2 of docs/vm_implementation.md is a table somebody produced by
# sshing around by hand. This prints the same table, so the next person does
# not — and so a host that has quietly changed says so before a milestone is
# blamed for it.
#
# It also refuses to let a run start on top of somebody else's: another
# session's `zygo` on the same machine shares the data directory, the cgroup
# tree and the port, and risk V9 is that the two blame each other. Pass
# `--force` when the processes are yours.
#
# Run:  sh poc/pi_env.sh [--force]
#       ssh <pi> 'sh ~/zygo/poc/pi_env.sh'
set -u

FORCE=${1:-}
READY=0
BLOCKED=0

say()  { printf '%s\n' "$*"; }
ok()   { READY=$((READY+1));   say "  ok        $*"; }
no()   { BLOCKED=$((BLOCKED+1)); say "  MISSING   $*"; }
note() { say "  note      $*"; }

say "host for the \`vm\` backend — $(uname -n)"
say ""

# --- the machine ------------------------------------------------------------

say "machine"
note "$(uname -srm)"
note "$(grep -m1 'model name\|Model' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || echo 'cpu model unknown')"
note "$(nproc) cores, $(awk '/MemTotal/ {printf "%.1f GB", $2/1048576}' /proc/meminfo 2>/dev/null)"
case "$(uname -m)" in
    aarch64|x86_64) ok "architecture $(uname -m) is one libkrunfw publishes a kernel for" ;;
    *) no "architecture $(uname -m): no libkrunfw release, so the guest kernel is a build" ;;
esac

# The page size decides whether libkrunfw's kernel will boot at all: a 16 KiB
# host cannot run a 4 KiB guest image on aarch64.
page=$(getconf PAGE_SIZE 2>/dev/null || echo unknown)
note "page size ${page} bytes"

say ""

# --- KVM, attempted rather than inspected -----------------------------------
#
# The whole point of this file. `/dev/kvm` existing, and even opening, says
# nothing: a nested or restricted hypervisor hands out the device and then
# refuses `KVM_CREATE_VM`. So the check creates a virtual machine and closes
# it, which is what `zygo doctor` now does too (V10).

say "kvm"
if [ ! -e /dev/kvm ]; then
    no "/dev/kvm does not exist; this host cannot run the \`vm\` backend"
elif [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
    no "/dev/kvm is not readable and writable by $(id -un) — sudo usermod -aG kvm \$USER, then log in again"
else
    probe=$(python3 - <<'PYEOF' 2>&1
import fcntl, os, sys

# KVM_CREATE_VM is _IO(0xAE, 0x01) on every architecture; the machine type
# argument of zero means "this host's default".
KVM_CREATE_VM = 0xAE01
KVM_GET_API_VERSION = 0xAE00
KVM_CHECK_EXTENSION = 0xAE03
KVM_CAP_NR_VCPUS = 9

try:
    kvm = os.open("/dev/kvm", os.O_RDWR)
except OSError as e:
    print("open failed:", e)
    sys.exit(1)
try:
    api = fcntl.ioctl(kvm, KVM_GET_API_VERSION, 0)
    try:
        vcpus = fcntl.ioctl(kvm, KVM_CHECK_EXTENSION, KVM_CAP_NR_VCPUS)
    except OSError:
        vcpus = 0
    vm = fcntl.ioctl(kvm, KVM_CREATE_VM, 0)
    os.close(vm)
    print(f"api {api}, up to {vcpus} vcpus, KVM_CREATE_VM ok")
except OSError as e:
    print("KVM_CREATE_VM failed:", e)
    sys.exit(1)
finally:
    os.close(kvm)
PYEOF
    )
    if [ $? -eq 0 ]; then
        ok "a virtual machine was created and closed — $probe"
    else
        no "/dev/kvm opens but a VM could not be created: $probe"
    fi
fi

if [ -e /dev/vhost-vsock ]; then
    ok "/dev/vhost-vsock (the agent's transport in the guest)"
else
    note "/dev/vhost-vsock absent — libkrun's own vsock does not need it, but check M3 against that"
fi

say ""

# --- what the host side still provides --------------------------------------
#
# The VMM runs inside the `ns` backend's cgroups and network namespace (D2),
# so everything `ns` needs is still needed.

say "the host side the VMM lives in"
delegated=$(cat /sys/fs/cgroup/user.slice/user-$(id -u).slice/cgroup.controllers 2>/dev/null ||
            cat /sys/fs/cgroup/cgroup.controllers 2>/dev/null || echo "")
case "$delegated" in
    *memory*pids*|*pids*memory*) ok "cgroup v2 controllers delegated: $delegated" ;;
    "") no "no cgroup v2 controllers found; the launcher has nowhere to put limits" ;;
    *) no "cgroup v2 is delegated but without memory and pids: $delegated" ;;
esac

for tool in pasta nft; do
    if command -v "$tool" >/dev/null 2>&1; then
        ok "$tool $(command -v $tool)"
    else
        no "$tool is not installed; a networked sandbox will not start (M4)"
    fi
done

say ""

# --- room to work -----------------------------------------------------------

say "room"
free_kb=$(df -Pk / | awk 'NR==2 {print $4}')
free_gb=$(( free_kb / 1048576 ))
note "$(df -h / | awk 'NR==2 {print $4" free of "$2}') on /"
if [ "$free_gb" -lt 4 ]; then
    no "under 4 GB free: a guest kernel, a flattened rootfs and the image store will not fit comfortably"
else
    ok "${free_gb} GB free"
fi

say ""

# --- somebody else's session ------------------------------------------------

say "other sessions"
mine=$$
others=$(pgrep -a zygo 2>/dev/null | grep -v "^$mine " || true)
if [ -n "$others" ]; then
    say "$others" | sed 's/^/            /'
    if [ "$FORCE" = "--force" ]; then
        note "another zygo is running; --force given, continuing"
    else
        no "another zygo is running on this host. It shares the data directory, the cgroup tree and the supervisor socket, and a run on top of it measures both. Stop it, or pass --force if it is yours."
    fi
else
    ok "no other zygo is running"
fi

say ""
say "----------------------------------------"
if [ "$BLOCKED" -eq 0 ]; then
    say "$READY ready, nothing blocking"
    exit 0
fi
say "$READY ready, $BLOCKED blocking"
exit 1
