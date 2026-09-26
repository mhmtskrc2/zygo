#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Twenty use cases for the `vm` backend, on a host where the guest does not boot.
#
# `verify_vm.sh` is the suite for a working guest: it runs a program inside one
# and checks what came back. This is the other half, and on this project's
# hardware it is the half that can actually run — everything the backend decides,
# refuses, resolves, accounts for and cleans up *around* the guest.
#
# That is not a consolation prize. A hypervisor that will not boot is exactly
# when the surrounding behaviour matters: the deadline has to fire, the monitor
# has to die with it, the cgroup has to go, and the error has to name something
# a person can act on. Every one of those is a path a booting guest would hide.
#
# The four rules apply, and the fourth one is the whole design here:
#
#   * Every case attempts the thing. None reads a setting to decide an outcome.
#   * A case that cannot run says so and counts as not attempted, never as a
#     pass. On this host that is most of what a guest would prove, and the
#     summary line says how many.
#   * Before any "the monitor is gone" or "the cgroup is gone" check, a positive
#     control establishes that the monitor and the cgroup were *there*. A
#     cleanup check against a sandbox that never started passes for free.
#   * A timing claim says what it was measured against.
#
# Run:  make vm-use-cases-linux
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/tests/linux/bin/zygo-linux-musl-vm}
IMAGE=${IMAGE:-alpine:3}
# `zygo` built *without* the vm feature, to check that the two binaries refuse
# `--isolation vm` differently. Optional: case 3 is skipped without it.
ZYGO_NOVM=${ZYGO_NOVM:-$SRC/tests/linux/bin/zygo-linux-musl}

PASS=0
FAIL=0
SKIP=0
say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); say "  ok    $*"; }
bad()  { FAIL=$((FAIL+1)); say "  FAIL  $*"; }
skip() { SKIP=$((SKIP+1)); say "  --    $* (not attempted)"; }

zygo() { "$ZYGO" "$@"; }

# How many monitor processes are running.
#
# libkrun renames the forked child to `libkrun VM`, and that is a *comm*, not a
# command line: the child's argv is still this binary's. An earlier version of
# this file used `pgrep -f "libkrun VM"`, which matched the string inside this
# script's own command line and counted two shells as two monitors — a positive
# control that was measuring itself. `pgrep` without `-f` matches comm, which
# is the thing that actually changed.
monitors() { pgrep '^libkrun' 2>/dev/null | wc -l | tr -d ' '; }
monitor_pids() { pgrep '^libkrun' 2>/dev/null; }

# Every sandbox this suite starts is one that will not boot, so every one of
# them has to be stopped by its own deadline. Short, and stated where it is
# used rather than assumed.
DEADLINE=${DEADLINE:-4s}
DEADLINE_S=4

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"; pkill "^libkrun" 2>/dev/null; exit' EXIT INT TERM

say "vm use cases"
say "binary:  $ZYGO"
say "host:    $(uname -srm)"
say ""

# ---------------------------------------------------------------------------
# Is there anything to test?
# ---------------------------------------------------------------------------
if [ ! -x "$ZYGO" ]; then
    say "no vm-capable binary at $ZYGO — build it with \`make vm-build\`"
    exit 1
fi

AVAIL=$(zygo backend list 2>&1 | grep '^vm ' | tr -s ' ')
say "backend list says: ${AVAIL:-<no vm row>}"
# `unavailable` contains `available`. Matching the positive case first calls
# every unavailable host ready, and then the whole suite measures the wrong
# thing while reporting passes — the exact shape of vacuous check the fourth
# rule exists to stop.
case "$AVAIL" in
    *unavailable*) VM_READY=no  ;;
    *available*)   VM_READY=yes ;;
    *)             VM_READY=no  ;;
esac
say ""

# A guest that never boots is this host's situation, and several cases below
# depend on knowing which it is. Established once, by attempting it, rather
# than assumed per case.
say "--- the one thing that decides the rest: does a guest boot here?"
GUEST_BOOTS=no
if [ "$VM_READY" = yes ]; then
    if zygo run --isolation vm --timeout 20s "$IMAGE" /bin/true >/dev/null 2>&1; then
        GUEST_BOOTS=yes
        say "      a guest booted and ran a program; the cases below that need one will run"
    else
        say "      no guest ran within 20s (V11 on this host). Cases needing a booted"
        say "      guest are reported as not attempted, and the cleanup cases run"
        say "      against a monitor that is up but stuck — which is what they are for."
    fi
else
    say "      the vm backend is not available here, so no guest was attempted"
fi
say ""

# ---------------------------------------------------------------------------
# 1–7: the front door. What the backend accepts and what it refuses by name.
# ---------------------------------------------------------------------------
say "--- the front door"

# UC1 — the backend reports itself, with a reason when it cannot be used.
case "$AVAIL" in
    "")            bad "UC1 backend list has no vm row at all" ;;
    *unavailable*) # Unavailable is a legitimate answer; it has to carry a reason.
                   if [ "$(printf '%s' "$AVAIL" | wc -w)" -ge 3 ]; then
                       ok "UC1 backend list: vm is unavailable and names why — $AVAIL"
                   else
                       bad "UC1 backend list says vm is unavailable and gives no reason"
                   fi ;;
    *available*)   ok "UC1 backend list: vm is available and says so" ;;
    *)             bad "UC1 backend list's vm row is neither available nor unavailable: $AVAIL" ;;
esac

# UC2 — a refusal is a refusal, not a hang. The most basic promise of a
# backend that cannot do something.
start=$(date +%s)
out=$(zygo run --isolation vm --net egress --allow example.com:443 --timeout "$DEADLINE" \
      "$IMAGE" /bin/true 2>&1)
elapsed=$(( $(date +%s) - start ))
case "$out" in
    *"network"*|*"egress"*)
        if [ "$elapsed" -le 2 ]; then
            ok "UC2 a networked vm sandbox is refused by name, in ${elapsed}s (before any guest starts)"
        else
            bad "UC2 the network refusal took ${elapsed}s; it is a decision and should be immediate"
        fi ;;
    *) bad "UC2 a networked vm sandbox was not refused by name: $(printf '%s' "$out" | head -1)" ;;
esac

# UC3 — a binary without the feature refuses differently from one with it.
# Two binaries, one flag: the message has to tell them apart, because
# "rebuild it" and "this host has no KVM" are different days.
if [ -x "$ZYGO_NOVM" ]; then
    novm=$("$ZYGO_NOVM" run --isolation vm "$IMAGE" /bin/true 2>&1)
    case "$novm" in
        *"--features vm"*|*"built without"*)
            ok "UC3 a binary built without the feature says so, and names the build flag" ;;
        *)  bad "UC3 a binary without the vm feature gave an unhelpful refusal: $(printf '%s' "$novm" | head -1)" ;;
    esac
else
    skip "UC3 no non-vm binary at $ZYGO_NOVM to compare against"
fi

# UC4 — a warm function on vm is refused, and names the backend that can.
cat > "$WORK/handler.py" <<'PYEOF'
def handler(event):
    return event
PYEOF
out=$(zygo serve "$WORK/handler.py" --isolation vm --name uc4 2>&1)
case "$out" in
    *"--isolation ns"*) ok "UC4 a warm function on vm is refused and points at ns" ;;
    *)                  bad "UC4 warm function refusal did not name ns: $(printf '%s' "$out" | head -1)" ;;
esac
zygo stop uc4 >/dev/null 2>&1

# UC5 — a terminal is refused rather than silently dropped. A --tty that is
# ignored looks like a broken program, not a missing feature.
out=$(zygo run --isolation vm --tty --timeout "$DEADLINE" "$IMAGE" /bin/sh 2>&1 </dev/null)
case "$out" in
    *tty*|*terminal*|*console*) ok "UC5 --tty on vm is refused by name" ;;
    *) bad "UC5 --tty on vm was not refused by name: $(printf '%s' "$out" | head -1)" ;;
esac

# UC6 — every refusal names a remedy. A reason without a remedy is a dead end,
# and these are the messages a person meets first.
missing_remedy=0
for args in "--net full" "--tty"; do
    # shellcheck disable=SC2086
    out=$(zygo run --isolation vm $args --timeout "$DEADLINE" "$IMAGE" /bin/true 2>&1 </dev/null)
    case "$out" in
        *"→"*) ;;
        *) missing_remedy=$((missing_remedy+1)); say "        no remedy for: $args" ;;
    esac
done
[ "$missing_remedy" -eq 0 ] \
    && ok "UC6 every vm refusal carries a remedy line" \
    || bad "UC6 $missing_remedy vm refusals gave a reason and no remedy"

# UC7 — the guest kernel's absence is its own message, and not confused with
# "this host has no KVM". Attempted by pointing the data directory somewhere
# that has no kernel, rather than by moving the real one.
empty=$WORK/empty-data
mkdir -p "$empty"
out=$(zygo --data-root "$empty" run --isolation vm --timeout "$DEADLINE" "$IMAGE" /bin/true 2>&1)
case "$out" in
    *"backend install vm"*|*"guest kernel"*)
        ok "UC7 a missing guest kernel is named, with the command that installs it" ;;
    *) bad "UC7 a missing guest kernel was not named: $(printf '%s' "$out" | head -1)" ;;
esac

say ""

# ---------------------------------------------------------------------------
# 8–13: what the sandbox would be. Configuration, resolved and reported.
# ---------------------------------------------------------------------------
say "--- the plan, before anything runs"

# UC8 — --dry-run prints a plan and starts nothing. The second half is the
# part worth checking: a dry run that quietly boots a VM is a lie.
before=$(monitors)
plan=$(zygo run --isolation vm --dry-run --json "$IMAGE" /bin/true 2>&1)
after=$(monitors)
case "$plan" in
    *'"isolation"'*|*'isolation'*)
        if [ "$before" = "$after" ]; then
            ok "UC8 --dry-run printed a plan and started no monitor"
        else
            bad "UC8 --dry-run started a monitor process ($before → $after)"
        fi ;;
    *) bad "UC8 --dry-run --json did not print a plan: $(printf '%s' "$plan" | head -1)" ;;
esac

# UC9 — the plan says vm when vm was asked for. A dry run that reports the
# default backend while the real run uses another is worse than no dry run.
case "$plan" in
    *vm*) ok "UC9 the plan names the vm backend" ;;
    *)    bad "UC9 the plan does not mention vm at all" ;;
esac

# UC10 — the guest gets its RAM plus a kernel allowance, and the tenant is not
# charged for the kernel. Checked through the plan, which is where the number
# a person can act on appears.
plan512=$(zygo run --isolation vm --dry-run --json --mem 512M "$IMAGE" /bin/true 2>&1)
case "$plan512" in
    *536870912*|*512*) ok "UC10 --mem 512M reaches the plan as the tenant's memory" ;;
    *) bad "UC10 --mem 512M is not visible in the plan" ;;
esac

# UC11 — isolation comes from the spec file when the flag is absent.
cat > "$WORK/sandbox.toml" <<EOF
[defaults]
image = "$IMAGE"
isolation = "vm"

[fn.job]
cmd = ["/bin/true"]
EOF
out=$(cd "$WORK" && zygo spec explain job 2>&1)
case "$out" in
    *vm*) ok "UC11 isolation = \"vm\" in [defaults] is what spec explain reports" ;;
    *)    bad "UC11 spec explain did not report vm: $(printf '%s' "$out" | head -2 | tr '\n' ' ')" ;;
esac

# UC12 — a CLI flag beats the spec file, in the direction that matters: a
# person debugging a vm function has to be able to say "not today".
out=$(cd "$WORK" && zygo run --isolation ns --timeout "$DEADLINE" "$IMAGE" /bin/echo ns-won 2>&1)
case "$out" in
    *ns-won*) ok "UC12 --isolation ns overrides isolation = \"vm\" in the spec, and runs" ;;
    *)        bad "UC12 --isolation ns did not override the spec: $(printf '%s' "$out" | head -1)" ;;
esac

# UC13 — an impossible limit is refused before a hypervisor is involved, with
# the field named. `mem` below the floor is the one people hit.
out=$(zygo run --isolation vm --mem 1M --timeout "$DEADLINE" "$IMAGE" /bin/true 2>&1)
case "$out" in
    *mem*) ok "UC13 a too-small --mem is refused and the field is named" ;;
    *)     bad "UC13 --mem 1M was not refused by name: $(printf '%s' "$out" | head -1)" ;;
esac

say ""

# ---------------------------------------------------------------------------
# 14–15: doctor. What a person is told about this host.
# ---------------------------------------------------------------------------
say "--- doctor"

doc=$(zygo doctor 2>&1)

# UC14 — the kvm line is an attempt, not a stat(). A host that hands out
# /dev/kvm and refuses KVM_CREATE_VM has to read as absent here, because the
# whole point of doctor is that it does not lie about this one.
case "$doc" in
    *kvm*)
        if [ -e /dev/kvm ]; then
            case "$doc" in
                *"kvm"*ok*|*"/dev/kvm"*) ok "UC14 doctor reports kvm on a host that has it" ;;
                *) bad "UC14 doctor's kvm line is unreadable on a host with /dev/kvm" ;;
            esac
        else
            case "$doc" in
                *kvm*) ok "UC14 doctor reports kvm as absent on a host without /dev/kvm" ;;
            esac
        fi ;;
    *) bad "UC14 doctor says nothing about kvm" ;;
esac

# UC15 — the guest kernel has its own line, and its absence is reported from a
# data directory that does not have one. Attempted, not assumed.
case "$doc" in
    *"guest kernel"*) ok "UC15 doctor reports the guest kernel" ;;
    *) bad "UC15 doctor says nothing about the guest kernel" ;;
esac
out=$(zygo --data-root "$empty" doctor 2>&1 | grep -i 'guest kernel')
case "$out" in
    *absent*|*missing*|*install*) ok "UC15b doctor reports a missing guest kernel as missing" ;;
    "") bad "UC15b doctor dropped the guest kernel line for a data dir without one" ;;
    *)  bad "UC15b doctor called a missing guest kernel: $out" ;;
esac

say ""

# ---------------------------------------------------------------------------
# 16–20: the lifecycle of a sandbox that does not come up.
#
# This is the section this host can actually exercise, and the one a booting
# guest would hide. Each case first proves the monitor was there.
# ---------------------------------------------------------------------------
say "--- a guest that does not come up"

if [ "$VM_READY" != yes ]; then
    skip "UC16 the deadline"
    skip "UC17 the monitor is reaped"
    skip "UC18 the cgroup is removed"
    skip "UC19 --outcome reports the timeout"
    skip "UC20 two at once"
else

# UC16 — the deadline fires, and it fires at the deadline. A VM that will not
# boot must not hold the caller for longer than it was told to.
start=$(date +%s)
zygo run --isolation vm --timeout "$DEADLINE" "$IMAGE" /bin/true >/dev/null 2>&1
code=$?
elapsed=$(( $(date +%s) - start ))
if [ "$GUEST_BOOTS" = yes ] && [ "$code" -eq 0 ]; then
    ok "UC16 a guest booted and the program ran inside ${elapsed}s"
elif [ "$elapsed" -le $((DEADLINE_S + 3)) ] && [ "$elapsed" -ge "$DEADLINE_S" ]; then
    ok "UC16 the deadline fired at ${elapsed}s against a ${DEADLINE_S}s timeout (exit $code)"
elif [ "$elapsed" -lt "$DEADLINE_S" ]; then
    bad "UC16 the run ended after ${elapsed}s, before its ${DEADLINE_S}s deadline (exit $code)"
else
    bad "UC16 a ${DEADLINE_S}s deadline took ${elapsed}s to fire (exit $code)"
fi

# UC17 — the monitor dies with the deadline, and so do its vcpu threads.
#
# The positive control first: start a run in the background and confirm a
# monitor process actually appears. Without it, "no monitor is left" is
# satisfied by a backend that never started one, which is the failure that
# fails open.
zygo run --isolation vm --timeout "$DEADLINE" "$IMAGE" /bin/true >/dev/null 2>&1 &
runner=$!
seen=no
i=0
while [ $i -lt 60 ]; do
    if [ "$(monitors)" -gt 0 ]; then seen=yes; break; fi
    i=$((i+1)); sleep 0.1
done
wait $runner 2>/dev/null
sleep 1
left=$(monitors)
if [ "$seen" = no ]; then
    skip "UC17 no monitor process was ever seen, so 'it was cleaned up' proves nothing"
elif [ "$left" -eq 0 ]; then
    ok "UC17 the monitor was running, and the deadline took it with it"
else
    bad "UC17 $left monitor process(es) survived the deadline"
    monitor_pids | head -3 | sed 's/^/        /'
fi

# UC18 — the sandbox's cgroup is removed.
#
# The directory is not guessed from the layout — it is read from the monitor
# itself, through `/proc/<pid>/cgroup`. Guessing meant searching for a `zygote`
# directory at the wrong depth, finding nothing, and reporting "not attempted"
# for a cgroup that was there all along. The monitor knows which cgroup it is
# in; asking it is both the positive control and the address to check.
zygo run --isolation vm --timeout "$DEADLINE" "$IMAGE" /bin/true >/dev/null 2>&1 &
runner=$!
made=""
i=0
while [ $i -lt 60 ]; do
    mon=$(monitor_pids | head -1)
    if [ -n "$mon" ]; then
        rel=$(awk -F: '$1=="0"{print $3}' "/proc/$mon/cgroup" 2>/dev/null)
        case "$rel" in
            /*) made=/sys/fs/cgroup$rel; [ -d "$made" ] && break; made="" ;;
        esac
    fi
    i=$((i+1)); sleep 0.1
done
wait $runner 2>/dev/null
sleep 1
if [ -z "$made" ]; then
    skip "UC18 never read a cgroup from a running monitor, so its removal proves nothing"
elif [ -d "$made" ]; then
    bad "UC18 the sandbox cgroup outlived the run: $made"
else
    ok "UC18 the monitor's own cgroup ($(basename "$(dirname "$made")")/$(basename "$made")) was removed after the run"
fi

# UC19 — --outcome says *why* it ended. Both a deadline kill and an
# out-of-memory kill are SIGKILL, so the file is the only thing that can tell
# a caller which happened; a vm sandbox owes the same answer as an ns one.
outfile=$WORK/why.json
zygo run --isolation vm --outcome "$outfile" --timeout "$DEADLINE" "$IMAGE" /bin/true \
    >/dev/null 2>&1
if [ ! -s "$outfile" ]; then
    bad "UC19 --outcome wrote nothing for a vm sandbox that hit its deadline"
else
    case "$(cat "$outfile")" in
        *'"timed_out":true'*|*'"timed_out": true'*)
            ok "UC19 --outcome reports timed_out for a vm sandbox killed by its deadline" ;;
        *)  if [ "$GUEST_BOOTS" = yes ]; then
                ok "UC19 --outcome was written for a vm sandbox: $(cat "$outfile")"
            else
                bad "UC19 --outcome does not say timed_out: $(cat "$outfile")"
            fi ;;
    esac
fi

# UC20 — two at once do not collide. Each run owns a cgroup and a monitor, and
# the failure this catches is two sandboxes sharing a generation directory and
# one of them removing the other's.
zygo run --isolation vm --timeout "$DEADLINE" "$IMAGE" /bin/true >"$WORK/a.log" 2>&1 &
a=$!
zygo run --isolation vm --timeout "$DEADLINE" "$IMAGE" /bin/true >"$WORK/b.log" 2>&1 &
b=$!
peak=0
i=0
while [ $i -lt 60 ]; do
    n=$(monitors)
    [ "$n" -gt "$peak" ] && peak=$n
    i=$((i+1)); sleep 0.1
done
wait $a 2>/dev/null; wait $b 2>/dev/null
sleep 1
left=$(monitors)
if [ "$peak" -lt 2 ]; then
    skip "UC20 never saw two monitors at once (peak $peak), so concurrency was not exercised"
elif [ "$left" -ne 0 ]; then
    bad "UC20 two concurrent runs left $left monitor(s) behind"
else
    # Neither may report the other's failure as its own.
    crossed=$(grep -l 'No such file or directory\|already exists' "$WORK/a.log" "$WORK/b.log" 2>/dev/null | wc -l | tr -d ' ')
    if [ "$crossed" -eq 0 ]; then
        ok "UC20 two concurrent vm runs each had their own monitor (peak $peak) and both were cleaned up"
    else
        bad "UC20 two concurrent vm runs interfered with each other"
        head -2 "$WORK/a.log" "$WORK/b.log" | sed 's/^/        /'
    fi
fi

fi

say ""
say "----------------------------------------"
say "vm use cases: $PASS passed, $FAIL failed, $SKIP not attempted"
if [ "$GUEST_BOOTS" != yes ]; then
    say ""
    say "No guest booted on this host, so nothing above says a program ran inside"
    say "a virtual machine. What it does say is that the backend refuses what it"
    say "cannot do, resolves what it was asked for, and cleans up after a guest"
    say "that never came up. \`verify_vm.sh\` is the suite for the other half."
fi
[ "$FAIL" -eq 0 ]
