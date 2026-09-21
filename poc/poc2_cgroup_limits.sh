#!/bin/sh
# PoC 2 — cgroup v2 limits actually contain a hostile tenant.
#
# Acceptance: a fork bomb and a runaway allocation inside the cgroup must not
# affect the host. This is requirement N4 — if limits are not enforced, nothing
# else Zygo claims matters.
#
# Every workload joins its cgroup from Python rather than from a subshell: in
# POSIX sh, `$$` inside a subshell is the *parent* shell's pid, so
# `echo $$ > cgroup.procs` moves the wrong process and the load never enters
# the cgroup — a test that then proves nothing while appearing to pass.
#
# Run:  docker run --rm --privileged -v "$PWD/poc:/poc:ro" python:3.12-slim \
#           sh /poc/poc2_cgroup_limits.sh
set -u

CG=/sys/fs/cgroup
ROOT="$CG/zygo-poc2"
PASS=0
FAIL=0

say() { printf '%s\n' "$*"; }
ok()  { PASS=$((PASS+1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); say "  FAIL  $*"; }

say "PoC 2 — cgroup v2 limit enforcement"
say "  kernel $(uname -r)"
say ""

# --- preconditions ---------------------------------------------------------

[ -f "$CG/cgroup.controllers" ] || { say "no cgroup v2 at $CG"; exit 2; }
say "controllers available: $(cat "$CG/cgroup.controllers")"

# cgroup v2's "no internal processes" rule: a cgroup may hold processes *or*
# enable controllers for its children, not both. The root we see here is the
# container's namespaced root and holds this shell, so everything has to move
# aside first. The launcher will have to do exactly this.
mkdir -p "$CG/init" 2>/dev/null
moved=0
for p in $(cat "$CG/cgroup.procs" 2>/dev/null); do
    echo "$p" > "$CG/init/cgroup.procs" 2>/dev/null && moved=$((moved+1))
done
say "moved $moved processes out of the root cgroup"

# Controllers must be enabled at *every* level between the root and the leaf.
enable_controllers() {
    dir="$1"
    for c in memory pids cpu; do
        echo "+$c" > "$dir/cgroup.subtree_control" 2>/dev/null
    done
    say "  $dir/cgroup.subtree_control = $(cat "$dir/cgroup.subtree_control" 2>/dev/null)"
}

enable_controllers "$CG"
mkdir -p "$ROOT" 2>/dev/null || { say "cannot create $ROOT"; exit 2; }
enable_controllers "$ROOT"

missing=""
for c in memory pids cpu; do
    case " $(cat "$ROOT/cgroup.subtree_control" 2>/dev/null) " in
        *" $c "*) ;;
        *) missing="$missing $c" ;;
    esac
done
if [ -n "$missing" ]; then
    say ""
    say "  controllers that could NOT be delegated:$missing"
    say "  this is risk R2 — limits would silently not apply"
fi
say ""

cleanup_cgroup() {
    dir="$1"
    if [ -f "$dir/cgroup.kill" ]; then
        echo 1 > "$dir/cgroup.kill" 2>/dev/null
    else
        for p in $(cat "$dir/cgroup.procs" 2>/dev/null); do kill -9 "$p" 2>/dev/null; done
    fi
    sleep 1
    rmdir "$dir" 2>/dev/null
}

# --- workload helpers -------------------------------------------------------

cat > /tmp/bomb.py <<'PYEOF'
import os, sys, time
try:
    with open("/sys/fs/cgroup/zygo-poc2/bomb/cgroup.procs", "w") as f:
        f.write(str(os.getpid()))
except OSError as e:
    sys.stderr.write(f"could not join the cgroup: {e}\n")
    sys.exit(3)

spawned = 0
try:
    while spawned < 400:
        if os.fork() == 0:
            time.sleep(30)
            os._exit(0)
        spawned += 1
except OSError:
    pass                      # fork() refused: this is pids.max working
print(spawned, flush=True)
time.sleep(30)
PYEOF

cat > /tmp/hog.py <<'PYEOF'
import os, sys
try:
    with open("/sys/fs/cgroup/zygo-poc2/mem/cgroup.procs", "w") as f:
        f.write(str(os.getpid()))
except OSError as e:
    sys.stderr.write(f"could not join the cgroup: {e}\n")
    sys.exit(3)

blocks = []
try:
    while True:
        # bytearray is touched memory, not a lazy reservation.
        blocks.append(bytearray(8 * 1024 * 1024))
        if len(blocks) > 512:          # 4 GB without being stopped = failure
            sys.exit(4)
except MemoryError:
    sys.exit(42)
PYEOF

cat > /tmp/spin.py <<'PYEOF'
import os, sys
try:
    with open("/sys/fs/cgroup/zygo-poc2/cpu/cgroup.procs", "w") as f:
        f.write(str(os.getpid()))
except OSError as e:
    sys.stderr.write(f"could not join the cgroup: {e}\n")
    sys.exit(3)
while True:
    pass
PYEOF

# --- test 1: pids.max stops a fork bomb ------------------------------------

say "test 1 — fork bomb against pids.max = 16"
BOMB="$ROOT/bomb"
mkdir -p "$BOMB"
if echo 16 > "$BOMB/pids.max" 2>/dev/null; then
    echo "67108864" > "$BOMB/memory.max" 2>/dev/null

    python3 /tmp/bomb.py > /tmp/bomb.out 2>/tmp/bomb.err &
    BOMB_PID=$!
    sleep 3

    peak=$(cat "$BOMB/pids.current" 2>/dev/null || echo "?")
    max=$(cat "$BOMB/pids.max" 2>/dev/null)
    spawned=$(cat /tmp/bomb.out 2>/dev/null || echo "?")
    say "  pids.current reached $peak against pids.max=$max"
    say "  the bomb managed ${spawned:-?} forks before fork() was refused (it asked for 400)"
    [ -s /tmp/bomb.err ] && say "  bomb stderr: $(cat /tmp/bomb.err)"

    kill -9 "$BOMB_PID" 2>/dev/null
    cleanup_cgroup "$BOMB"

    if [ -s /tmp/bomb.err ]; then
        bad "the bomb never joined the cgroup — the test proved nothing"
    elif [ "$peak" != "?" ] && [ "$peak" -le 20 ] 2>/dev/null; then
        ok "the fork bomb was capped at $peak processes (${spawned:-?} forks)"
    else
        bad "the fork bomb reached $peak processes"
    fi
else
    bad "pids.max is not writable — the pids controller was not delegated"
    rmdir "$BOMB" 2>/dev/null
fi
say ""

# --- test 2: memory.max OOM-kills only the offender -------------------------

say "test 2 — unbounded allocation against memory.max = 64M"
MEM="$ROOT/mem"
mkdir -p "$MEM"
if echo "67108864" > "$MEM/memory.max" 2>/dev/null; then
    echo 0 > "$MEM/memory.swap.max" 2>/dev/null
    echo 1 > "$MEM/memory.oom.group" 2>/dev/null
    echo 64 > "$MEM/pids.max" 2>/dev/null

    avail_before=$(awk '/MemAvailable/{print $2}' /proc/meminfo)
    python3 /tmp/hog.py 2>/tmp/hog.err
    hog_status=$?
    sleep 1

    oom_kills=$(awk '/^oom_kill /{print $2}' "$MEM/memory.events" 2>/dev/null || echo 0)
    oom_events=$(awk '/^oom /{print $2}' "$MEM/memory.events" 2>/dev/null || echo 0)
    avail_after=$(awk '/MemAvailable/{print $2}' /proc/meminfo)

    say "  hog exit status: $hog_status (137 = SIGKILL, 42 = MemoryError, 3 = setup failed, 4 = NOT stopped)"
    say "  memory.events: oom=$oom_events oom_kill=$oom_kills"
    say "  host MemAvailable: $((avail_before / 1024)) MB before, $((avail_after / 1024)) MB after"
    [ -s /tmp/hog.err ] && say "  hog stderr: $(cat /tmp/hog.err)"

    case "$hog_status" in
        3) bad "the hog never joined the cgroup — the test proved nothing" ;;
        4) bad "the hog allocated 4 GB without being stopped" ;;
        137|42)
            if [ "$oom_kills" -gt 0 ] || [ "$hog_status" = 42 ]; then
                ok "the allocation was stopped by its own cgroup (status $hog_status)"
            else
                bad "the hog stopped but memory.events shows no OOM kill"
            fi
            ;;
        *) bad "unexpected hog exit status $hog_status" ;;
    esac

    lost=$(( (avail_before - avail_after) / 1024 ))
    if [ "$lost" -lt 512 ]; then
        ok "the host lost only ${lost} MB of available memory"
    else
        bad "the host lost ${lost} MB — containment leaked"
    fi

    cleanup_cgroup "$MEM"
else
    bad "memory.max is not writable — the memory controller was not delegated"
    rmdir "$MEM" 2>/dev/null
fi
say ""

# --- test 3: cpu.max caps a busy loop ---------------------------------------

say "test 3 — infinite loop against cpu.max = 0.5 core"
CPU="$ROOT/cpu"
mkdir -p "$CPU"
if echo "50000 100000" > "$CPU/cpu.max" 2>/dev/null; then
    echo 64 > "$CPU/pids.max" 2>/dev/null

    python3 /tmp/spin.py 2>/tmp/spin.err &
    SPIN_PID=$!

    sleep 2
    before=$(awk '/usage_usec/{print $2}' "$CPU/cpu.stat" 2>/dev/null || echo 0)
    sleep 2
    after=$(awk '/usage_usec/{print $2}' "$CPU/cpu.stat" 2>/dev/null || echo 0)
    kill -9 "$SPIN_PID" 2>/dev/null
    cleanup_cgroup "$CPU"

    used=$(( (after - before) / 1000 ))
    say "  CPU consumed over 2 s of wall clock: ${used} ms (0.5 core would be ~1000 ms)"
    [ -s /tmp/spin.err ] && say "  spinner stderr: $(cat /tmp/spin.err)"

    if [ -s /tmp/spin.err ]; then
        bad "the spinner never joined the cgroup — the test proved nothing"
    elif [ "$used" -gt 300 ] && [ "$used" -lt 1500 ]; then
        ok "the busy loop was throttled to roughly half a core"
    else
        bad "the busy loop used ${used} ms in 2 s"
    fi
else
    bad "cpu.max is not writable — the cpu controller was not delegated"
    rmdir "$CPU" 2>/dev/null
fi
say ""

# --- kernel features the design depends on ----------------------------------

say "kernel features (design doc appendix C)"
probe="$ROOT/probe"
mkdir -p "$probe" 2>/dev/null
for f in cgroup.kill memory.peak cgroup.freeze memory.oom.group; do
    if [ -f "$probe/$f" ]; then say "  $f  present"; else say "  $f  MISSING — fallback required"; fi
done
rmdir "$probe" 2>/dev/null
say ""

rmdir "$ROOT" 2>/dev/null
say "----------------------------------------"
say "PoC 2: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
