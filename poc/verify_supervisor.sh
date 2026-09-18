#!/bin/sh
# Integration check for the supervisor (todo.md, phase 2.2).
#
# The unit tests cover the control protocol, the registry and the admission
# gate; none of them can catch what this does. The supervisor is the first part
# of Zygo that is multi-threaded *and* forks, and that combination has a failure
# mode no in-process test reaches: `PR_SET_PDEATHSIG` is delivered when the
# thread that created a child exits, not when the process does. A sandbox
# started on a connection thread therefore dies the instant its `serve` command
# returns — and everything still looks healthy until the next `exec`.
#
# So the checks here are all end-to-end, over a real socket, with the client
# process exiting between every one of them.
#
# Run:  make verify-supervisor-linux
set -u

ZYGO=/src/poc/zygo-linux
IMAGE=python:3.12-slim
PASS=0
FAIL=0

ZYGO_DATA_HOME=/tmp/zdata-sup
export ZYGO_DATA_HOME

# cgroup preparation, as in verify_launcher.sh: cgroup v2 forbids a cgroup from
# holding processes *and* delegating controllers to its children. The
# difference here is that the supervisor is long-lived, so it gets `/launch` to
# itself and every client runs from `/harness`. A systemd user session with
# `Delegate=` gives this for free; a container does not.
CG=/sys/fs/cgroup
mkdir -p "$CG/harness" "$CG/launch" 2>/dev/null
for p in $(cat "$CG/cgroup.procs" 2>/dev/null); do
    echo "$p" > "$CG/harness/cgroup.procs" 2>/dev/null
done
for c in memory pids cpu; do echo "+$c" > "$CG/cgroup.subtree_control" 2>/dev/null; done

say() { printf '%s\n' "$*"; }
ok()  { PASS=$((PASS+1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); say "  FAIL  $*"; }

# Assert on a command's exit code, which is how a script would branch.
exits() {
    want=$1; shift
    what=$1; shift
    "$@" >/dev/null 2>&1
    got=$?
    if [ "$got" -eq "$want" ]; then
        ok "$what (exit $got)"
    else
        bad "$what: exit $got, wanted $want"
    fi
}

say "supervisor verification"
say "  kernel $(uname -r)"

mkdir -p /tmp/sup-work
cd /tmp/sup-work || exit 1
printf 'def handler(event):\n    n = event.get("n", 0) if isinstance(event, dict) else 0\n    return {"doubled": n * 2}\n' > handler.py
printf 'def handler(event):\n    return 1 / 0\n' > boom.py
printf 'import time\n\n\ndef handler(event):\n    time.sleep(0.4)\n    return {"slept": True}\n' > slow.py

"$ZYGO" pull "$IMAGE" >/dev/null 2>&1

say ""
say "before anything is running"
exits 0 "\`ps\` with no supervisor is an empty answer, not an error" "$ZYGO" ps
exits 1 "\`supervisor status\` reports that there is none" "$ZYGO" supervisor status
exits 0 "\`stop --all\` with nothing to stop is a success" "$ZYGO" stop --all

# The supervisor owns `/launch`; nothing else may sit there.
sh -c 'echo $$ > /sys/fs/cgroup/launch/cgroup.procs; exec /src/poc/zygo-linux supervisor run' \
    >/tmp/supervisor.log 2>&1 &
SUPERVISOR=$!
i=0
while [ $i -lt 100 ]; do
    "$ZYGO" supervisor status >/dev/null 2>&1 && break
    i=$((i+1)); sleep 0.1
done

say ""
say "lifecycle"
exits 0 "the supervisor answers on its socket" "$ZYGO" supervisor status
exits 0 "\`serve\` warms a function" "$ZYGO" serve handler.py --name double

# The regression this file exists for. The `serve` client has exited; if the
# sandbox was created on its connection thread, it is already dead and this is
# the call that says so.
out=$("$ZYGO" exec double '{"n": 21}' 2>&1)
case "$out" in
    *'"doubled": 42'*)
        ok "a warm function survives the command that created it, and answers" ;;
    *'Broken pipe'*)
        bad "the sandbox died with its serve connection (PDEATHSIG fires on thread exit)" ;;
    *)
        bad "exec returned something unexpected: $(printf '%s' "$out" | head -1)" ;;
esac

# Several calls, each from its own process, to show the sandbox stays warm
# rather than surviving exactly one request.
alive=0
for n in 1 2 3 4 5; do
    "$ZYGO" exec double "{\"n\": $n}" 2>/dev/null | grep -q "\"doubled\": $((n*2))" && alive=$((alive+1))
done
if [ "$alive" -eq 5 ]; then
    ok "five separate clients in a row all reach the same warm sandbox"
else
    bad "only $alive of 5 sequential clients got an answer"
fi

if "$ZYGO" --json ps 2>/dev/null | grep -q '"requests": 6'; then
    ok "the request counter survives across client processes"
else
    bad "the request counter is wrong: $("$ZYGO" --json ps 2>/dev/null | tr -d '\n ')"
fi

say ""
say "errors reach the caller intact"
"$ZYGO" serve boom.py --name boom >/dev/null 2>&1
out=$("$ZYGO" exec boom '{}' 2>&1)
status=$?
case "$out" in
    *ZeroDivisionError*)
        if [ "$status" -ne 0 ]; then
            ok "a raising handler gives its traceback and a non-zero exit"
        else
            bad "a raising handler reported success"
        fi ;;
    *) bad "the traceback did not come back: $(printf '%s' "$out" | head -1)" ;;
esac

exits 4 "an unserved name is \`not found\`, distinctly from a failure" \
    "$ZYGO" exec ghost '{}'

# A handler that returns nothing still has to round-trip.
out=$("$ZYGO" exec double 'null' 2>/dev/null)
case "$out" in
    *'"doubled": 0'*) ok "an event of \`null\` is passed through, not rejected" ;;
    *) bad "null event: $(printf '%s' "$out" | head -1)" ;;
esac

say ""
say "concurrency and backpressure"
"$ZYGO" serve slow.py --name slow --concurrency 2 >/dev/null 2>&1

# Four clients at once. Each records its own exit code, because `wait` on its
# own would also wait for the supervisor this script is running in the
# background — and the point is to prove nobody was dropped or deadlocked.
rm -f /tmp/conc.*
pids=""
for i in 1 2 3 4; do
    ( "$ZYGO" exec slow '{}' >/dev/null 2>&1; echo $? > "/tmp/conc.$i" ) &
    pids="$pids $!"
done
for p in $pids; do wait "$p"; done
served=$(cat /tmp/conc.* 2>/dev/null | grep -c '^0$')
if [ "$served" -eq 4 ]; then
    ok "four simultaneous clients were all served"
else
    bad "only $served of 4 simultaneous clients were served: $(cat /tmp/conc.* | tr '\n' ' ')"
fi

# Past the queue the answer must be a distinguishable `BUSY`, not a hang and
# not a generic failure: requirement N4's limits are only useful if the caller
# is told which one it hit. `--concurrency 1` with a two-second handler makes
# the queue overflow rather than drain.
printf 'import time\n\n\ndef handler(event):\n    time.sleep(2)\n    return {"slept": True}\n' > slower.py
"$ZYGO" serve slower.py --name jam --concurrency 1 >/dev/null 2>&1
rm -f /tmp/jam.*
pids=""
for i in 1 2 3 4 5 6 7 8 9 10 11 12; do
    ( "$ZYGO" exec jam '{}' >/dev/null 2>&1; echo $? > "/tmp/jam.$i" ) &
    pids="$pids $!"
done
for p in $pids; do wait "$p"; done
busy=$(cat /tmp/jam.* 2>/dev/null | grep -c '^75$')
served=$(cat /tmp/jam.* 2>/dev/null | grep -c '^0$')
other=$(cat /tmp/jam.* 2>/dev/null | grep -vc '^\(0\|75\)$')
if [ "$busy" -gt 0 ] && [ "$served" -gt 0 ] && [ "$other" -eq 0 ]; then
    ok "12 clients against a concurrency of 1: $served served, $busy told to retry, 0 failed"
elif [ "$other" -gt 0 ]; then
    bad "$other client(s) got something other than success or backpressure: $(cat /tmp/jam.* | tr '\n' ' ')"
elif [ "$busy" -eq 0 ]; then
    bad "the queue never pushed back; every client was admitted"
else
    bad "no client was served at all: $(cat /tmp/jam.* | tr '\n' ' ')"
fi
"$ZYGO" stop jam >/dev/null 2>&1

say ""
say "replacement and shutdown"
# `double` has served several requests by now. Re-serving must give a new
# sandbox with its own counters, not keep running the code that was loaded
# before the handler was edited.
before=$("$ZYGO" --json ps 2>/dev/null | tr -d '\n ' | grep -o '"name":"double","requests":[0-9]*' | grep -o '[0-9]*$')
exits 0 "re-serving a name replaces it" "$ZYGO" serve handler.py --name double
after=$("$ZYGO" --json ps 2>/dev/null | tr -d '\n ' | grep -o '"name":"double","requests":[0-9]*' | grep -o '[0-9]*$')
if [ "${before:-0}" -gt 0 ] && [ "${after:-1}" -eq 0 ]; then
    ok "the replacement is a fresh sandbox: $before requests before, $after after"
else
    bad "re-serving did not replace the sandbox (requests $before -> $after)"
fi

exits 0 "\`stop\` removes one function" "$ZYGO" stop boom
if "$ZYGO" --json ps 2>/dev/null | grep -q '"boom"'; then
    bad "the stopped function is still listed"
else
    ok "the stopped function is gone from \`ps\`"
fi

exits 4 "calling a stopped function is \`not found\`" "$ZYGO" exec boom '{}'

exits 0 "\`stop --all\` stops everything and the supervisor with it" "$ZYGO" stop --all
i=0
while [ $i -lt 50 ]; do
    "$ZYGO" supervisor status >/dev/null 2>&1 || break
    i=$((i+1)); sleep 0.1
done
exits 1 "the supervisor is gone" "$ZYGO" supervisor status

if [ -S "$ZYGO_DATA_HOME/run/supervisor.sock" ]; then
    bad "the control socket was left behind; the next supervisor will think it has a conflict"
else
    ok "the control socket was cleaned up"
fi

# No sandbox may outlive the supervisor: that is what PDEATHSIG is for, and the
# whole reason it cannot simply be dropped.
sleep 1
leftover=0
for d in /proc/[0-9]*; do
    [ "$(cat "$d/comm" 2>/dev/null)" = "python3" ] || continue
    [ "$(awk '{print $3}' "$d/stat" 2>/dev/null)" = "Z" ] && continue
    leftover=$((leftover+1))
done
if [ "$leftover" -eq 0 ]; then
    ok "no sandbox outlived the supervisor"
else
    bad "$leftover sandbox process(es) are still running with no supervisor"
fi

wait "$SUPERVISOR" 2>/dev/null

say ""
say "----------------------------------------"
say "supervisor verification: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || {
    say ""
    say "supervisor log:"
    sed 's/^/  /' /tmp/supervisor.log
    exit 1
}
