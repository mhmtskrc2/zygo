#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# One scenario, repeated: a request queued behind a function being replaced.
#
# `verify_supervisor.sh` checks this once, inside a thirty-five minute run, and
# on the Raspberry Pi it failed **once** — the queued request ran on the *old*
# function although the replacement had been ready for seven seconds of the
# eight-second window. Every ordering in `Supervisor::serve` and `Gate::close`
# reads as though that cannot happen, and the container passes every time. What
# was missing was a reproduction tight enough to run repeatedly and instrument;
# this is that.
#
# It does the scenario and nothing else, `$ROUNDS` times (default 5), and on a
# failure it dumps what the supervisor thought was happening: the function's
# log, `ps`, and the timings of every step. A run that passes prints one line
# per round with the numbers, so a round that was *close* is visible even when
# it did not fail.
#
# Run:  sh tests/linux/repro_blue_green.sh [rounds]
#       make repro-blue-green-linux
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/tests/linux/bin/zygo-linux-musl}
ROUNDS=${1:-5}

# How long the in-flight request holds the only slot. The queued request has to
# still be waiting when the replacement lands, so this has to comfortably
# exceed the time `up` takes — which on a Pi is seconds, not milliseconds.
HOLD_S=${HOLD_S:-8}
# How long the queued request can possibly still be queued when `up` returns.
#
# Two bounds, and the *smaller* one decides. The hold is one: once it expires
# the slot frees and the request runs wherever it is. `QUEUE_WAIT` is the
# other, and it is the one that usually bites — a request waits five seconds
# for a slot and is then told to retry, by design, because a queue that holds
# someone past their own patience has turned backpressure into a timeout.
#
# So a round only decides anything if `up` returns inside *both*. A Raspberry
# Pi where `up` takes twelve seconds cannot answer this question at all, and
# saying so is the whole reason this number is computed rather than assumed.
QUEUE_WAIT_MS=${QUEUE_WAIT_MS:-5000}
# The settle time is subtracted, not ignored. The queued request may connect
# and start waiting the instant it is launched, so its five seconds can begin
# a full `SETTLE_MS` before `up` does — and the gate has to close inside what
# is left. Rounds where `up` landed at 4.3 s of a 5 s wait were being called
# failures on that arithmetic, when the request had simply run out of patience
# first. It is the difference between measuring the supervisor and measuring
# the Raspberry Pi.
SETTLE_MS=1500
up_clock=""
up_done_clock=""
HOLD_LEFT_MS=$(( HOLD_S * 1000 - SETTLE_MS ))
WINDOW_MS=$(( QUEUE_WAIT_MS - SETTLE_MS ))
[ "$HOLD_LEFT_MS" -lt "$WINDOW_MS" ] && WINDOW_MS=$HOLD_LEFT_MS

ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-bluegreen}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

PASS=0
FAIL=0
REFUSED=0
INCONCLUSIVE=0
say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); say "  PASS  $*"; }
bad()  { FAIL=$((FAIL+1)); say "  FAIL  $*"; }
hmm()  { INCONCLUSIVE=$((INCONCLUSIVE+1)); say "  ----  $*"; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"; "$ZYGO" stop --all >/dev/null 2>&1' EXIT
cd "$WORK" || exit 1
# With SUPERVISOR_LOG set, the supervisor is started here, in the foreground,
# with debug tracing to that file — instead of being auto-started by the first
# `up` with its stderr piped to nobody. The gate logs every admission, queue,
# wake and close against the function's name and generation, and a failing
# round's story is in there. Without it the script measures the bug from
# outside, which is how it went undiagnosed.
if [ -n "${SUPERVISOR_LOG:-}" ]; then
    ZYGO_LOG=${ZYGO_LOG:-zygo_core::supervisor=debug} zygo supervisor run >/dev/null 2>"$SUPERVISOR_LOG" &
    SUPERVISOR_PID=$!
    i=0
    while [ $i -lt 100 ]; do
        zygo supervisor status >/dev/null 2>&1 && break
        i=$((i + 1)); sleep 0.1
    done
    say "supervisor pid $SUPERVISOR_PID, tracing to $SUPERVISOR_LOG"
fi


# The marker directory is mounted writable, because the handler writes it from
# inside the sandbox — which is the point: a marker the host wrote would say
# nothing about whether the request reached the handler.
cat > sandbox.toml <<TOML
[defaults]
image = "python:3.12-slim"

[fn.v]
entry = "v.py"
concurrency = 1
timeout = "60s"
mounts = ["$WORK:/marker:rw"]
TOML

# The handler writes a marker the moment a request *starts*, so the script can
# wait for the slot to be genuinely held rather than sleeping and hoping.
#
# That distinction is the whole reproduction. Without it the second `zygo exec`
# can win the race to the only slot — the first one is still starting up, a
# `zygo exec` takes a few hundred milliseconds to connect on a Pi — and then it
# never queued at all, ran on the function that was current when it ran, and
# the round "failed" for a reason that has nothing to do with the supervisor.
# The first version of this script did exactly that and reported two failures
# it could not have distinguished from correct behaviour.
write_handler() {
    cat > v.py <<PYEOF
import os, time

MARKER = "/marker/started"


def handler(event):
    if event.get("sleep"):
        open(MARKER, "w").write(str(os.getpid()))
        time.sleep(event["sleep"])
    return {"version": $1}
PYEOF
}

# Wait for the in-flight request to be inside the handler, so the only slot is
# provably held before anything queues behind it.
wait_for_marker() {
    i=0
    while [ $i -lt 200 ]; do
        [ -f "$WORK/started" ] && return 0
        i=$((i + 1))
        sleep 0.05
    done
    return 1
}

say "blue/green reproduction — $ROUNDS rounds, ${HOLD_S}s hold"
say "  a round decides something only if \`up\` returns inside ${WINDOW_MS} ms"
say "  (QUEUE_WAIT ${QUEUE_WAIT_MS} ms, less the ${SETTLE_MS} ms the request may already have waited)"
say "  kernel $(uname -r)"

# The data directory was cleared, so the image has to come back first. Done
# before the timing starts: a pull inside a round would land in the middle of
# the window this measures.
zygo pull python:3.12-slim >/dev/null 2>&1

write_handler 0
if ! zygo up >/dev/null 2>&1; then
    say "  the project could not be brought up; nothing below would mean anything."
    zygo up 2>&1 | tail -5
    exit 1
fi

# Rule 4: prove the thing runs before asserting about how it fails.
first=$(zygo exec v '{}' 2>/dev/null | tr -d '\n ')
case "$first" in
    *'"version":0'*) say "  the function answers; starting" ;;
    *) say "  the function does not answer ($first); nothing below would mean anything."; exit 1 ;;
esac
say ""

round=0
while [ "$round" -lt "$ROUNDS" ]; do
    round=$((round + 1))
    next=$round

    # The slot-holder. Nothing else starts until it is provably inside the
    # handler: `concurrency = 1`, so from that moment every later request must
    # queue, which is the premise the whole round rests on.
    rm -f "$WORK/started"
    holder_started=$(date +%s%N)
    zygo exec v "{\"sleep\": $HOLD_S}" >"$WORK/inflight.out" 2>&1 &
    inflight=$!
    if ! wait_for_marker; then
        hmm "round $round: the slot-holder never reached the handler, so nothing queued"
        wait $inflight 2>/dev/null
        continue
    fi
    held_ms=$(( ($(date +%s%N) - holder_started) / 1000000 ))

    # Now the queued one. It cannot run: the only slot is held for another
    # $HOLD_S seconds.
    queued_started=$(date +%s%N)
    queued_clock=$(date -u +%H:%M:%S.%N | cut -c1-12)
    zygo exec v '{}' >"$WORK/queued.out" 2>&1 &
    queued=$!
    # Long enough for it to have connected and queued. It has nothing else it
    # could be doing, and `WINDOW_MS` accounts for it having started at once.
    sleep 1.5

    write_handler "$next"
    up_started=$(date +%s%N)
    up_clock=$(date -u +%H:%M:%S.%N | cut -c1-12)
    # `--json` so the round can tell a replacement from a no-op: an `up` that
    # decided nothing had changed leaves the old function in place, and a
    # request queued behind it is then correctly told to retry.
    zygo --json up >"$WORK/up.out" 2>&1
    up_rc=$?
    up_ms=$(( ($(date +%s%N) - up_started) / 1000000 ))
    up_done_clock=$(date -u +%H:%M:%S.%N | cut -c1-12)
    # Absolute times on every round, so a round can be laid against the
    # supervisor's own log line by line. Relative timings hid the thing that
    # mattered: whether `up` was *invoked* before the queued request gave up.
    say "        clocks (UTC): queued request launched ${queued_clock:-?}, up invoked ${up_clock}, up returned ${up_done_clock}"

    # What the supervisor believed at the moment the replacement landed. Read
    # now rather than after the round, because a later `ps` shows the settled
    # state and this is about the transition.
    ps_at_swap=$(zygo --json ps 2>/dev/null | tr -d '\n ')

    wait $queued
    queued_rc=$?
    queued_ms=$(( ($(date +%s%N) - queued_started) / 1000000 ))
    queued_out=$(tr -d '\n ' <"$WORK/queued.out")
    wait $inflight

    # How much of the queue window was still open when `up` returned. A
    # negative number means the hold had already expired, and the round proves
    # nothing either way.
    left_ms=$(( WINDOW_MS - up_ms ))

    case "$queued_rc:$queued_out" in
        0:*"\"version\":$next"*)
            ok "round $round: the queued request ran on the replacement (held after ${held_ms} ms, up ${up_ms} ms, ${left_ms} ms of window left, waited ${queued_ms} ms)"
            ;;
        0:*'"version":'*)
            if [ "$left_ms" -le 0 ]; then
                hmm "round $round: the hold expired before \`up\` returned (up ${up_ms} ms > ${WINDOW_MS} ms), so this round decides nothing"
            else
                bad "round $round: the queued request ran on the OLD function with ${left_ms} ms of window left (up ${up_ms} ms, waited ${queued_ms} ms)"
                say "        answer: $queued_out"
                say "        up exit $up_rc: $(tail -3 "$WORK/up.out" | tr '\n' ' ')"
                say "        ps at the swap: $ps_at_swap"
                say "        the function's log around the swap:"
                zygo logs v -n 20 2>/dev/null | sed 's/^/          /'
            fi
            ;;
        *busy*|*retry*)
            # The gate turned it away. Whether that is a failure depends
            # entirely on the clock: a request waits `QUEUE_WAIT` for a slot
            # and is then told to retry, which is the documented behaviour.
            if [ "$left_ms" -le 0 ]; then
                hmm "round $round: \`up\` took ${up_ms} ms, past the ${WINDOW_MS} ms this request could wait, so being told to retry is correct and this round decides nothing"
            else
                REFUSED=$((REFUSED + 1))
                FAIL=$((FAIL + 1))
                say "  FAIL  round $round: the queued request was refused although the replacement landed ${up_ms} ms in, with ${left_ms} ms of its wait left (it waited ${queued_ms} ms)"
                                say "        answer: $queued_out"
                say "        up said: $(tr -d '\n' <"$WORK/up.out" | sed 's/  */ /g')"
                say "        ps at the swap: $ps_at_swap"
            fi
            ;;
        *)
            bad "round $round: the queued request failed outright: exit $queued_rc, $queued_out"
            say "        up exit $up_rc: $(tail -3 "$WORK/up.out" | tr '\n' ' ')"
            zygo logs v -n 20 2>/dev/null | sed 's/^/          /'
            ;;
    esac
done

say ""
say "----------------------------------------"
say "blue/green: $PASS on the replacement, $((FAIL - REFUSED)) on the old one, $REFUSED refused, $INCONCLUSIVE inconclusive"
if [ "$PASS" -eq 0 ] && [ "$FAIL" -eq 0 ]; then
    say ""
    say "  No round decided anything: on this machine \`up\` outlasts the five"
    say "  seconds a request will wait for a slot, so the queued request is"
    say "  told to retry before the replacement can take it — which is correct"
    say "  and says nothing about the question. This needs a faster host."
fi
harness_verdict
[ "$FAIL" -eq 0 ] || exit 1
