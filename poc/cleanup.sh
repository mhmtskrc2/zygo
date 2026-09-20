#!/bin/sh
# Stop every Zygo process on this host and free its control sockets.
#
# For a development machine between verification runs. The suites clear their
# own data directory at the start, but they cannot clear a *supervisor* left
# over from a previous run: it holds the socket, it answers, and it is holding
# a store that the next run has just deleted. That is how one Raspberry Pi run
# came to report `image python:3.12-slim is not in the local store` for every
# function — the requests were reaching last run's supervisor.
#
# Two rules learned here, both the hard way:
#
#   * **Never `pkill -f`.** It matches whole command lines, including the ssh
#     command that invoked the cleanup. An ssh command that so much as mentions
#     the binary's path was killed by its own cleanup four separate times,
#     which is indistinguishable from the host hanging and cost more debugging
#     than the state it was clearing.
#   * **One pass is not enough.** A supervisor being torn down restarts its
#     own children, and a suite still exiting spawns more. So it loops, and it
#     says how many passes it needed rather than reporting a number that was
#     true for a moment.

PATTERN=${1:-'zygo-linux-mus[l]'}

# This script's own ancestors, which must survive it.
self=$$
ancestors=" "
p=$self
while [ -n "$p" ] && [ "$p" -gt 1 ] 2>/dev/null; do
    ancestors="$ancestors$p "
    p=$(awk '/^PPid:/{print $2}' "/proc/$p/status" 2>/dev/null)
done

killed=0
passes=0
while [ "$passes" -lt 5 ]; do
    passes=$((passes + 1))
    found=0
    for pid in $(pgrep -f "$PATTERN" 2>/dev/null); do
        case "$ancestors" in
            *" $pid "*) continue ;;
        esac
        found=$((found + 1))
        kill -9 "$pid" 2>/dev/null && killed=$((killed + 1))
    done
    [ "$found" -eq 0 ] && break
    sleep 1
done

# Both places a socket can live: the self-contained layout a suite asks for
# with `ZYGO_DATA_HOME`, and the XDG runtime directory an ordinary session
# uses. A stale socket file makes the next `serve` report that a supervisor is
# already listening when nothing is.
for dir in \
    "${ZYGO_DATA_HOME:-/tmp/zdata-sup}/run" \
    "${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/zygo" \
    "$HOME/zygo/data/run"; do
    rm -f "$dir/supervisor.sock" "$dir/supervisor.pid" 2>/dev/null
done

# Counted with the same filter the killing used. A bare `pgrep -c` here
# reported "still up 1" every time, and the one was the ssh command that had
# just asked for the cleanup — the trap this file is about, sprung by the line
# that was supposed to prove it had been avoided.
left=0
for pid in $(pgrep -f "$PATTERN" 2>/dev/null); do
    case "$ancestors" in
        *" $pid "*) continue ;;
    esac
    left=$((left + 1))
done
echo "cleanup: killed $killed over $passes pass(es), still up $left"
[ "$left" -eq 0 ]
