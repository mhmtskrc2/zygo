#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# One request's out-of-memory kill stays inside that request.
#
# A warm function or pool serves several requests at once from one sandbox.
# Its memory limit used to sit on the function's cgroup, with
# `memory.oom.group` there too, so a single request allocating past `mem` was
# killed together with the zygote, the agent's parked workers and every
# request in flight beside it — in a runtime pool, other tenants' requests.
# Found with three sleeping requests and one 2 GB allocation in one pool: all
# four died and the pool rewarmed.
#
# Now `mem` is written on each request's own cgroup and on the zygote's, and
# the function above carries no memory ceiling. This suite runs that shape for
# real, for the Python agent (a `fork()` per request) and the Node agent (a
# parked worker per request): three requests sleep while one allocates far
# past the limit, and only the one may die.
#
# Run:  make verify-oom-linux
#   or, on a host: systemd-run --user --scope -p Delegate=yes -- \
#                  SRC=$PWD sh tests/linux/verify_oom_containment.sh
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/tests/linux/bin/zygo-linux-musl}
PY_IMAGE=${PY_IMAGE:-python:3.12-slim}
NODE_IMAGE=${NODE_IMAGE:-node:22-slim}
PASS=0
FAIL=0

ZYGO_DATA_HOME=/tmp/zdata-oom
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

say() { printf '%s\n' "$*"; }
ok()  { PASS=$((PASS+1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); say "  FAIL  $*"; }

start_supervisor() {
    zygo_supervisor /tmp/oom-supervisor.log
    SUPERVISOR=$SUPERVISOR_PID
    i=0
    while [ $i -lt 100 ]; do
        "$ZYGO" supervisor status >/dev/null 2>&1 && return 0
        i=$((i+1)); sleep 0.1
    done
    return 1
}

# The cgroup a function was given, found by name under the slice the harness
# let the supervisor build.
function_cgroup() {
    find /sys/fs/cgroup -type d -path "*/zygo.slice/tenants/default/$1" 2>/dev/null | head -1
}

# The pid of the warm process in a function's zygote leaf: the agent.
zygote_pid() {
    dir=$(function_cgroup "$1")
    [ -n "$dir" ] || return 1
    cat "$dir"/g*/zygote/cgroup.procs 2>/dev/null | head -1
}

# Three sleepers and one hog, at once. Each client records its own exit code.
# Prints "<sleepers that finished 0> <hog exit>".
hog_beside_sleepers() {
    name=$1
    rm -f /tmp/oom.*
    pids=""
    for i in 1 2 3; do
        ( "$ZYGO" exec "$name" '{"sleep": 3}' >/dev/null 2>&1; echo $? > "/tmp/oom.sleep.$i" ) &
        pids="$pids $!"
    done
    # Let the sleepers be admitted first, so the hog cannot be the only
    # request in the sandbox when it dies.
    sleep 0.5
    ( "$ZYGO" exec "$name" '{"hog": true}' >/dev/null 2>&1; echo $? > /tmp/oom.hog ) &
    pids="$pids $!"
    for p in $pids; do wait "$p"; done
    slept=$(cat /tmp/oom.sleep.* 2>/dev/null | grep -c '^0$')
    printf '%s %s\n' "$slept" "$(cat /tmp/oom.hog 2>/dev/null)"
}

# The whole check, for one agent: serve, look at the tree, run the four,
# and see who is still there.
contained() {
    name=$1
    shift
    if ! "$ZYGO" serve "$@" --name "$name" --concurrency 4 --mem 256M >"/tmp/serve-$name.log" 2>&1; then
        bad "could not serve \`$name\`: $(tail -2 "/tmp/serve-$name.log" | tr '\n' ' ' | cut -c1-200)"
        return
    fi
    "$ZYGO" exec "$name" '{}' >/dev/null 2>&1   # warm it, and put a zygote in the tree

    fdir=$(function_cgroup "$name")
    if [ -z "$fdir" ]; then
        bad "$name: no function cgroup under zygo.slice/tenants/default"
        return
    fi
    if [ "$(cat "$fdir/memory.max")" = max ] && [ "$(cat "$fdir/memory.oom.group")" = 0 ]; then
        ok "$name: the function cgroup has no memory ceiling and no group kill"
    else
        bad "$name: the function cgroup still carries memory.max=$(cat "$fdir/memory.max") oom.group=$(cat "$fdir/memory.oom.group")"
    fi
    zleaf=$(ls -d "$fdir"/g*/zygote 2>/dev/null | head -1)
    if [ "$(cat "$zleaf/memory.max" 2>/dev/null)" = 268435456 ] && [ "$(cat "$zleaf/memory.oom.group" 2>/dev/null)" = 1 ]; then
        ok "$name: the zygote leaf carries mem and the group kill"
    else
        bad "$name: the zygote leaf has memory.max=$(cat "$zleaf/memory.max" 2>/dev/null) oom.group=$(cat "$zleaf/memory.oom.group" 2>/dev/null)"
    fi

    before=$(zygote_pid "$name")
    result=$(hog_beside_sleepers "$name")
    slept=${result% *}
    hog=${result#* }
    if [ "$slept" -eq 3 ]; then
        ok "$name: the three requests beside the hog finished"
    else
        bad "$name: only $slept of 3 requests beside the hog finished (hog exit $hog)"
    fi
    if [ "$hog" != 0 ]; then
        ok "$name: the hog itself failed (exit $hog)"
    else
        bad "$name: the hog returned 0 — the limit did not bite"
    fi
    after=$(zygote_pid "$name")
    if [ -n "$before" ] && [ "$before" = "$after" ] && kill -0 "$before" 2>/dev/null; then
        ok "$name: the zygote (pid $before) survived the hog"
    else
        bad "$name: the zygote was replaced (before: $before, after: $after)"
    fi
    kills=$(sed -n 's/^oom_kill //p' "$fdir/memory.events" 2>/dev/null)
    if [ -n "$kills" ] && [ "$kills" -ge 1 ]; then
        ok "$name: the kernel reports the kill under the function ($kills killed)"
    else
        bad "$name: memory.events under the function says oom_kill=$kills"
    fi
    if "$ZYGO" exec "$name" '{}' >/dev/null 2>&1; then
        ok "$name: the function answers again afterwards"
    else
        bad "$name: the function does not answer after the hog"
    fi
    "$ZYGO" stop "$name" >/dev/null 2>&1
}

say "one request's OOM stays in its cgroup"
say "  kernel $(uname -r)"

mkdir -p /tmp/oom-work && cd /tmp/oom-work || exit 1
cat > hog.py <<'PY'
import time


def handler(event):
    event = event or {}
    if event.get("hog"):
        block = bytearray(64 * 1024 * 1024)
        held = []
        while True:
            held.append(bytearray(block))   # 64 MB a step, past any limit
    if event.get("sleep"):
        time.sleep(event["sleep"])
    return {"ok": True}
PY
cat > hog.js <<'JS'
module.exports = async function handler(event) {
  event = event || {};
  if (event.hog) {
    const held = [];
    for (;;) held.push(Buffer.alloc(64 * 1024 * 1024, 1)); // 64 MB a step
  }
  if (event.sleep) await new Promise((r) => setTimeout(r, event.sleep * 1000));
  return { ok: true };
};
JS

if ! start_supervisor; then
    say "  could not start a supervisor: $(tail -3 /tmp/oom-supervisor.log)"
    exit 1
fi
pull_image "$PY_IMAGE" || exit 1
pull_image "$NODE_IMAGE" || exit 1

say ""
say "python agent: a fork per request"
contained hog-py hog.py --image "$PY_IMAGE"

say ""
say "node agent: a parked worker per request"
contained hog-node hog.js --image "$NODE_IMAGE"

"$ZYGO" supervisor stop >/dev/null 2>&1
kill "$SUPERVISOR" 2>/dev/null
wait "$SUPERVISOR" 2>/dev/null

say ""
say "$PASS passed, $FAIL failed"
harness_verdict
[ "$FAIL" -eq 0 ]
