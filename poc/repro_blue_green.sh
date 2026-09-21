#!/bin/sh
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
# Run:  sh poc/repro_blue_green.sh [rounds]
#       make repro-blue-green-linux
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux}
ROUNDS=${1:-5}

# How long the in-flight request holds the only slot. The queued request has to
# still be waiting when the replacement lands, so this has to comfortably
# exceed the time `up` takes — which on a Pi is seconds, not milliseconds.
HOLD_S=${HOLD_S:-8}
# The queue is only open for what is left after the two settling sleeps.
WINDOW_MS=$(( HOLD_S * 1000 - 600 ))

ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-bluegreen}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

PASS=0
FAIL=0
INCONCLUSIVE=0
say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); say "  PASS  $*"; }
bad()  { FAIL=$((FAIL+1)); say "  FAIL  $*"; }
hmm()  { INCONCLUSIVE=$((INCONCLUSIVE+1)); say "  ----  $*"; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"; "$ZYGO" stop --all >/dev/null 2>&1' EXIT
cd "$WORK" || exit 1

cat > sandbox.toml <<'TOML'
[defaults]
image = "python:3.12-slim"

[fn.v]
entry = "v.py"
concurrency = 1
timeout = "60s"
TOML

write_handler() {
    printf 'import time\n\n\ndef handler(event):\n    time.sleep(event.get("sleep", 0))\n    return {"version": %s}\n' "$1" > v.py
}

say "blue/green reproduction — $ROUNDS rounds, ${HOLD_S}s hold, ${WINDOW_MS}ms window"
say "  kernel $(uname -r)"

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

    # The slot-holder, then the request that queues behind it.
    zygo exec v "{\"sleep\": $HOLD_S}" >"$WORK/inflight.out" 2>&1 &
    inflight=$!
    sleep 0.3
    queued_started=$(date +%s%N)
    zygo exec v '{}' >"$WORK/queued.out" 2>&1 &
    queued=$!
    sleep 0.3

    write_handler "$next"
    up_started=$(date +%s%N)
    zygo up >"$WORK/up.out" 2>&1
    up_rc=$?
    up_ms=$(( ($(date +%s%N) - up_started) / 1000000 ))

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
            ok "round $round: the queued request ran on the replacement (up ${up_ms} ms, ${left_ms} ms of window left, waited ${queued_ms} ms)"
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
        *)
            bad "round $round: the queued request failed outright: exit $queued_rc, $queued_out"
            say "        up exit $up_rc: $(tail -3 "$WORK/up.out" | tr '\n' ' ')"
            zygo logs v -n 20 2>/dev/null | sed 's/^/          /'
            ;;
    esac
done

say ""
say "----------------------------------------"
say "blue/green: $PASS on the replacement, $FAIL on the old one, $INCONCLUSIVE inconclusive"
if [ "$INCONCLUSIVE" -eq "$ROUNDS" ]; then
    say ""
    say "  Every round was inconclusive: \`up\` took longer than the queue was"
    say "  open for. Raise HOLD_S and run it again — as it stands this says"
    say "  nothing about the supervisor."
fi
harness_verdict
[ "$FAIL" -eq 0 ] || exit 1
