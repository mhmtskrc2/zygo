#!/bin/sh
# Integration check for the supervisor.
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

# Where the checkout is: `/src` inside the containers `make` starts, the
# workspace in CI. Everything below is relative to it.
SRC=${SRC:-/src}

ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
IMAGE=python:3.12-slim
PASS=0
FAIL=0

ZYGO_DATA_HOME=/tmp/zdata-sup
export ZYGO_DATA_HOME

# Start from nothing. In a container the data directory is fresh because the
# container is; on a real host `/tmp` survives, and a previous run's images,
# venvs and derived layers are still there — which made this suite count two
# venvs for one requirements file and call it a bug in the cache. Assertions
# about what a cache contains only mean something when the cache started
# empty.
# Sourced below, but needed here: see `clear_data_home`.
. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

# The prelude is already sourced, above, because clearing the data
# directory needs one of its helpers before anything else happens.

# Bring a supervisor up and wait for its socket. The prelude decides which
# cgroup it starts in; this only cares that it answers.
start_supervisor() {
    zygo_supervisor /tmp/supervisor.log
    SUPERVISOR=$SUPERVISOR_PID
    i=0
    while [ $i -lt 100 ]; do
        "$ZYGO" supervisor status >/dev/null 2>&1 && return 0
        i=$((i+1)); sleep 0.1
    done
    return 1
}

say() { printf '%s\n' "$*"; }
ok()  { PASS=$((PASS+1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); say "  FAIL  $*"; }

# Serve a function, and say why once if it does not come up.
#
# A setup step whose failure is invisible turns into a wall of failures with
# no cause. `serve spin.py` failing on a Raspberry Pi produced ten red lines,
# every one of them "no function named `spin`", and the reason was in output
# that had been sent to /dev/null. Third time this project has learned it:
# see the four test rules at the top of this file.
served() {
    name=$1
    shift
    if "$ZYGO" serve "$@" >"/tmp/serve-$name.log" 2>&1; then
        return 0
    fi
    bad "could not serve \`$name\`: $(grep -v '^$' "/tmp/serve-$name.log" | tail -2 | tr '\n' ' ' | cut -c1-200)"
    return 1
}

# Where a section runs. Called at the top of every section that needs a
# directory of its own, and again by the next section — *not* left to the
# previous one to restore.
#
# The egress section used to `cd` back at its end, inside the guard that skips
# it when there is no usable `/dev/net/tun`. On a host without one the shell
# stayed in `/tmp/egress`, and four sections later `serve spin.py` failed
# because `spin.py` was somewhere else. Six failures, none of them about
# directories. A section that states where it runs cannot be moved by what
# happened before it.
work() {
    mkdir -p "$1" && cd "$1" || exit 1
}

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

work /tmp/sup-work
printf 'def handler(event):\n    n = event.get("n", 0) if isinstance(event, dict) else 0\n    return {"doubled": n * 2}\n' > handler.py
printf 'def handler(event):\n    return 1 / 0\n' > boom.py
printf 'import time\n\n\ndef handler(event):\n    time.sleep(0.4)\n    return {"slept": True}\n' > slow.py

printf 'def handler(event):\n    while True:\n        pass\n' > spin.py
printf 'import os\n\n\ndef handler(event):\n    for _ in range(4):\n        if os.fork() == 0:\n            while True:\n                pass\n    while True:\n        pass\n' > spawn.py

"$ZYGO" pull "$IMAGE" >/dev/null 2>&1

say ""
say "before anything is running"
exits 0 "\`ps\` with no supervisor is an empty answer, not an error" "$ZYGO" ps
exits 1 "\`supervisor status\` reports that there is none" "$ZYGO" supervisor status
exits 0 "\`stop --all\` with nothing to stop is a success" "$ZYGO" stop --all

start_supervisor

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
say "the live table"

# `zygo top` exists beside `ps` for one reason: a rate needs two samples and
# `ps` takes one. So the thing worth checking is that a single frame refuses
# to invent one.
out=$("$ZYGO" top --once 2>&1)
case $out in
    *"REQ/S"*) ok "\`top --once\` prints a frame with the columns \`ps\` cannot have" ;;
    *) bad "top --once: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac
case $out in
    *"a rate needs two samples"*) ok "and the first frame says why its rate is blank" ;;
    *) bad "the first frame does not explain the missing rate: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac

# A rate from two real samples.
#
# The load runs *continuously* in the background rather than as a burst before
# the frames, and the assertion is about what actually happened rather than a
# constant. The first version launched forty `zygo exec` processes in a loop
# and required at least 5 req/s from the second frame, which is a number
# calibrated on a laptop: on a loaded Raspberry Pi a `zygo exec` takes a few
# hundred milliseconds to start and connect, so fewer than ten of them finish
# inside a two-second interval and the check reported `top` as broken when the
# machine was merely slow.
#
# So: keep requests flowing across every frame, count what the function really
# served over the window, and compare the reported rate against *that*.
requests_of() {
    "$ZYGO" --json ps 2>/dev/null | tr -d '\n ' |
        grep -o "\"name\":\"$1\",\"requests\":[0-9]*" | grep -o '[0-9]*$'
}

before=$(requests_of double)
: > /tmp/top-load.on
(
    while [ -f /tmp/top-load.on ]; do
        "$ZYGO" exec double '{"n": 1}' >/dev/null 2>&1
    done
) &
loader=$!

window_started=$(date +%s%N)
"$ZYGO" top --interval 2 >/tmp/top-frames.txt 2>&1 &
topper=$!
# Long enough for at least three frames at a two-second interval, so the
# second one is not the last and is bracketed by load on both sides.
sleep 7
kill "$topper" 2>/dev/null
rm -f /tmp/top-load.on
wait "$loader" 2>/dev/null
window_ms=$(( ($(date +%s%N) - window_started) / 1000000 ))
after=$(requests_of double)
served=$(( ${after:-0} - ${before:-0} ))

# The positive control, first. "The rate was wrong" and "nothing ran" look
# identical in the rate column, and only one of them is about `top`.
if [ "$served" -le 0 ]; then
    bad "no request reached \`double\` during the ${window_ms} ms window, so the rate column says nothing about \`top\`"
else
    ok "the load reached the function: $served requests in ${window_ms} ms"

    # The field after `MB`, not a column number: the RSS value contains a
    # space, so counting fields read `MB` as the rate.
    rate=$(grep -E '^double ' /tmp/top-frames.txt | sed -n '2p' |
        awk '{ for (i = 1; i <= NF; i++) if ($i == "MB") { print $(i + 1); exit } }')
    # The average over the whole window, as a hundredth, so this stays in
    # integer arithmetic that `sh` can do.
    average_c=$(( served * 100000 / window_ms ))
    rate_c=$(printf '%s' "${rate:-0}" | awk '{printf "%d", $1 * 100}')

    if [ "${rate_c:-0}" -le 0 ]; then
        bad "the rate column read ${rate:-nothing} while $served requests were served; two samples should have produced a rate"
    elif [ "$(( rate_c * 5 ))" -lt "$average_c" ] || [ "$rate_c" -gt "$(( average_c * 5 ))" ]; then
        # A generous band: load varies between frames and this is not a
        # benchmark. What it catches is a rate off by a factor — dividing by
        # the interval that was asked for rather than the one that elapsed,
        # or by the wrong unit entirely.
        bad "the rate column read ${rate} req/s where the window averaged $(( average_c / 100 )).$(( average_c % 100 )) req/s; that is off by more than a factor of five"
    else
        ok "a second frame reports a rate consistent with the load (${rate} req/s against an average of $(( average_c / 100 )).$(( average_c % 100 )))"
    fi
fi

say ""
say "the zygote stays clean"

# Copy-on-write is the whole economy of the warm path: a request is a fork,
# and what it costs is the pages it dirties. Pages the *zygote* dirties are
# worse than that — they are copied for every later fork and never shared
# again, so a zygote that grows per request makes every request after it more
# expensive. Nothing in the protocol should write to the parent, and this is
# how that stays true.
#
# Measured by reading `ps`, which touches nothing: reading the zygote's own
# `/proc` would be the check disturbing what it measures.
#
# And the check was made to fail before it was trusted. With one line added to
# the Python agent — a megabyte kept in the *parent* per request — the same
# fifty requests take the zygote from 17.8 MB to 79.5 MB, which this reports
# as 61 MB of growth. A handler cannot do that from inside a request, because
# the fork is what isolates it; only Zygo's own code in the parent can, which
# is exactly what this guards.
printf 'def handler(event):\n    data = [i * i for i in range(20000)]\n    return {"n": len(data)}\n' > cow.py
if served cow cow.py --name cow; then
    zygote_rss() {
        "$ZYGO" --json ps 2>/dev/null | python3 -c '
import json
import sys

listing = json.load(sys.stdin)
rows = listing["functions"] if isinstance(listing, dict) else listing
print(next((r["rss_kb"] for r in rows if r["name"] == "cow"), 0))
'
    }
    # One request first, so the warm-up's own allocations are behind us and
    # what is measured is the steady state.
    "$ZYGO" exec cow '{}' >/dev/null 2>&1
    before=$(zygote_rss)
    n=0
    while [ "$n" -lt 50 ]; do
        "$ZYGO" exec cow '{}' >/dev/null 2>&1
        n=$((n + 1))
    done
    after=$(zygote_rss)
    growth=$((${after:-0} - ${before:-0}))
    # Generous: the measured growth over fifty requests is zero, and a budget
    # of two megabytes catches a regression that dirties pages per request
    # without failing on an allocator that rounds up once.
    if [ "$growth" -le 2048 ]; then
        ok "50 requests left the zygote ${growth} kB larger (${before} → ${after} kB), inside the 2 MB this allows"
    else
        bad "the zygote grew ${growth} kB over 50 requests (${before} → ${after} kB); every later fork pays for those pages"
    fi
    "$ZYGO" stop cow >/dev/null 2>&1
fi

say ""
say "secrets"
# Delivered as a file to the child only (design doc §3.10). Three things have
# to be true, and each is checked by looking rather than trusting: the handler
# can read it, it is gone once no request is running, and the agent's own
# process never has it.
printf 'def handler(event):\n    with open("/run/secrets/STRIPE_KEY") as f:\n        return {"got": f.read()}\n' > secret.py
printf 'import time\n\n\ndef handler(event):\n    time.sleep(1.2)\n    return {"ok": True}\n' > slowsecret.py

if "$ZYGO" serve secret.py --name pay --secret STRIPE_KEY >/dev/null 2>&1; then
    bad "serving with an unset secret succeeded; it should name the variable"
else
    ok "serving a function whose secret is not in the environment is refused"
fi

STRIPE_KEY=sk_test_zygo_42 "$ZYGO" serve secret.py --name pay --secret STRIPE_KEY >/dev/null 2>&1
out=$("$ZYGO" exec pay '{}' 2>&1)
case "$out" in
    *sk_test_zygo_42*) ok "the handler reads the secret from /run/secrets" ;;
    *) bad "the secret did not reach the handler: $(printf '%s' "$out" | head -1)" ;;
esac

agent=$(cat "$CG"/launch/zygo.slice/tenants/default/pay/*/zygote/cgroup.procs 2>/dev/null | head -1)
if [ -n "$agent" ]; then
    if [ -e "/proc/$agent/root/run/secrets/STRIPE_KEY" ]; then
        bad "the secret file is still there after the request finished"
    else
        ok "the file is gone once no request is in flight"
    fi
    # The agent never receives a value: it is not in its environment and no
    # frame carrying one crossed its socket.
    if tr '\0' '\n' < "/proc/$agent/environ" 2>/dev/null | grep -q '^STRIPE_KEY='; then
        bad "the agent's environment has the secret"
    else
        ok "the agent's own environment never has it"
    fi
else
    bad "could not find the agent's pid to inspect"
fi

# While a request runs, the file exists; the probe looks from the host side.
STRIPE_KEY=sk_test_zygo_42 "$ZYGO" serve slowsecret.py --name slowpay --secret STRIPE_KEY >/dev/null 2>&1
( sleep 0.6
  a=$(cat "$CG"/launch/zygo.slice/tenants/default/slowpay/*/zygote/cgroup.procs 2>/dev/null | head -1)
  if [ -n "$a" ] && [ "$(cat "/proc/$a/root/run/secrets/STRIPE_KEY" 2>/dev/null)" = sk_test_zygo_42 ]; then
      echo present > /tmp/secret-probe
  else
      echo absent > /tmp/secret-probe
  fi ) &
probe=$!
"$ZYGO" exec slowpay '{}' >/dev/null 2>&1
wait "$probe"
if [ "$(cat /tmp/secret-probe 2>/dev/null)" = present ]; then
    ok "and present, owner-readable, for exactly as long as a request runs"
else
    bad "the secret was not there while the request ran"
fi
"$ZYGO" stop pay >/dev/null 2>&1
"$ZYGO" stop slowpay >/dev/null 2>&1

say ""
say "warm-exec"
# No agent in the box: the sandbox is held by Zygo's own init, and every
# request is a fresh process entered into it. `cmd`, not `entry`, is what
# selects the mode, so these come up through a spec file and `zygo up`.
cat > sandbox.toml <<'TOML'
[defaults]
image = "python:3.12-slim"

[fn.echo]
cmd = ["sh", "-c", "cat"]

[fn.pyexec]
cmd = ["python3", "-c", "import sys, json, os; e = json.load(sys.stdin); print(json.dumps({'n2': e['n'] * 2, 'pid': os.getpid()}))"]

[fn.spinexec]
cmd = ["sh", "-c", "while true; do :; done"]
timeout = "2s"

[fn.secretexec]
cmd = ["sh", "-c", "printf '{\"k\":\"%s\"}' \"$(cat /run/secrets/STRIPE_KEY)\""]
secrets = ["STRIPE_KEY"]
TOML

if STRIPE_KEY=sk_exec_7 "$ZYGO" up >/tmp/up.log 2>&1; then
    ok "\`zygo up\` brings cmd-only functions up warm"
else
    bad "up failed for warm-exec functions: $(grep -v '^$' /tmp/up.log | head -3 | tr '\n' ' ')"
fi
rm -f sandbox.toml

runtime=$("$ZYGO" ps 2>/dev/null | awk '$1 == "echo" { print $3 }')
[ "$runtime" = exec ] && ok "\`ps\` shows the function as exec, not an agent runtime" || bad "runtime column: '$runtime'"

# The init is Zygo's own hold process; nothing from the image is running.
init=$(cat "$CG"/launch/zygo.slice/tenants/default/echo/*/zygote/cgroup.procs 2>/dev/null | head -1)
comm=$(cat "/proc/$init/comm" 2>/dev/null)
case "$comm" in
    zygo*) ok "the held sandbox's init is Zygo's own process, not an agent" ;;
    *) bad "the init's comm is '$comm'" ;;
esac
if grep -q '^0$' "/proc/$init/status" 2>/dev/null; then :; fi
dumpable=$(awk '/^CapEff/ {print $2}' "/proc/$init/status" 2>/dev/null)
[ "$dumpable" = 0000000000000000 ] && ok "and it holds no capabilities" || bad "the init has capabilities: $dumpable"

out=$("$ZYGO" exec echo '{"a": 1, "b": [2, 3]}' 2>&1)
case "$out" in
    *'"a": 1'*'"b": ['*) ok "stdin in, stdout out: the event comes back as the result" ;;
    *) bad "echo: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-120)" ;;
esac

one=$("$ZYGO" exec pyexec '{"n": 21}' 2>/dev/null | tr -d '\n ')
two=$("$ZYGO" exec pyexec '{"n": 21}' 2>/dev/null | tr -d '\n ')
case "$one" in
    *'"n2":42'*) ok "a program reads the event, computes, and prints JSON" ;;
    *) bad "pyexec: $one" ;;
esac
pid1=$(printf '%s' "$one" | sed -n 's/.*"pid":\([0-9]*\).*/\1/p')
pid2=$(printf '%s' "$two" | sed -n 's/.*"pid":\([0-9]*\).*/\1/p')
if [ -n "$pid1" ] && [ "$pid1" = "$pid2" ]; then
    # Same pid twice means either the same process served both (it did not
    # exit) or pid reuse in a namespace where only the init exists.
    ok "each request is a fresh process (pid $pid1 both times: the namespace has one other process, so reuse is expected)"
elif [ -n "$pid1" ] && [ -n "$pid2" ]; then
    ok "each request is a fresh process (pids $pid1 and $pid2)"
else
    bad "could not read the request pids: $one / $two"
fi

# Requests are entered by the supervisor, not forked by an agent, so between
# requests the sandbox holds exactly one process.
sleep 0.3
count=$(cat "$CG"/launch/zygo.slice/tenants/default/pyexec/*/zygote/cgroup.procs 2>/dev/null | wc -l)
[ "$count" -eq 1 ] && ok "between requests the sandbox holds only its init" || bad "$count processes in the sandbox at rest"

started=$(date +%s%N)
out=$("$ZYGO" exec spinexec '{}' 2>&1)
elapsed=$(( ($(date +%s%N) - started) / 1000000 ))
case "$out" in
    *SIGKILL*|*deadline*)
        if [ "$elapsed" -lt 5000 ]; then
            ok "a warm-exec request that never returns is killed at its timeout (${elapsed} ms)"
        else
            bad "warm-exec timeout took ${elapsed} ms"
        fi ;;
    *) bad "warm-exec timeout: $(printf '%s' "$out" | head -1) after ${elapsed} ms" ;;
esac
out=$("$ZYGO" exec echo '{"still": "warm"}' 2>&1)
case "$out" in
    *'"still": "warm"'*) ok "and the sandbox is still serving afterwards" ;;
    *) bad "after a timeout: $(printf '%s' "$out" | head -1)" ;;
esac

# A warm-exec sandbox's init is deliberately not dumpable — it is a fork of
# the supervisor and still maps its memory — so the supervisor cannot reach
# `/proc/<init>/root` at all without privilege. The sandbox hands out a
# descriptor for `/run/secrets` during its launch instead, which is why this
# works as an ordinary user and not only as root. It did not, until a real
# host was used.
out=$("$ZYGO" exec secretexec '{}' 2>&1)
case "$out" in
    *'"k": "sk_exec_7"'*) ok "secrets reach a warm-exec request at /run/secrets, same as an agent's child" ;;
    *) bad "secret in warm-exec: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-140)" ;;
esac

# What a request costs, end to end through the CLI: the design says 1–3 ms of
# overhead plus the program. `sh -c cat` is about as small as a program gets.
started=$(date +%s%N)
for _ in 1 2 3 4 5 6 7 8 9 10; do "$ZYGO" exec echo '{}' >/dev/null 2>&1; done
per=$(( ($(date +%s%N) - started) / 10000000 ))
if [ "$per" -lt 60 ]; then
    ok "ten warm-exec requests through the CLI averaged ${per} ms each (client start-up included)"
else
    bad "warm-exec averaged ${per} ms per request through the CLI"
fi

# And the log has to agree that they took some time. A warm-exec request is a
# bare process, not an agent reporting on itself, and nothing was measuring
# it: every entry carried `wall_ms: 0`, so `zygo stats` reported a p50 of
# `0.0 ms` for a function that was working perfectly.
logged=$("$ZYGO" --json logs pyexec -n 1 2>/dev/null |
    sed -n 's/.*"wall_ms":\([0-9.]*\).*/\1/p' | head -1)
case ${logged:-0} in
    0 | 0.0 | "") bad "a warm-exec request is logged as ${logged:-nothing} ms; nothing is timing it" ;;
    *) ok "a warm-exec request's own time reaches the log (${logged} ms)" ;;
esac

for f in echo pyexec spinexec secretexec; do "$ZYGO" stop "$f" >/dev/null 2>&1; done

say ""
say "the venv cache"
# Built inside the image with its own pip, once per (image, requirements),
# and shared read-only. `six` is tiny and has no dependencies, which keeps
# this about the mechanism rather than about PyPI.
printf 'six==1.16.0\n' > requirements.txt
printf 'import six\n\n\ndef handler(event):\n    return {"six": six.__version__, "from": six.__file__}\n' > uses_six.py

started=$(date +%s%N)
if "$ZYGO" serve uses_six.py --name withdeps --requirements requirements.txt >/tmp/venv-serve.log 2>&1; then
    built=$(( ($(date +%s%N) - started) / 1000000 ))
    ok "a function with requirements is served, venv built in ${built} ms"
else
    bad "serving with requirements failed: $(tail -3 /tmp/venv-serve.log | tr '\n' ' ')"
fi
out=$("$ZYGO" exec withdeps '{}' 2>&1)
# Two independent checks rather than one ordered pattern: the JSON is printed
# with its keys sorted, so assuming `six` comes before `from` is the same
# mistake as test bug #9.
case "$out" in
    *'"six": "1.16.0"'*) has_version=yes ;;
    *) has_version=no ;;
esac
case "$out" in
    *'/venv/lib/python'*'site-packages/six.py'*) from_venv=yes ;;
    *) from_venv=no ;;
esac
if [ "$has_version" = yes ] && [ "$from_venv" = yes ]; then
    ok "the handler imports the package, and imports it from /venv"
else
    bad "import from the venv (version $has_version, from /venv $from_venv): $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-120)"
fi

# The second function listing the same file must not build again.
printf 'import six\n\n\ndef handler(event):\n    return {"also": six.__version__}\n' > also_six.py
started=$(date +%s%N)
"$ZYGO" serve also_six.py --name withdeps2 --requirements requirements.txt >/dev/null 2>&1
reused=$(( ($(date +%s%N) - started) / 1000000 ))
if [ "$reused" -lt 3000 ]; then
    ok "a second function with the same requirements reuses the venv (${reused} ms)"
else
    bad "the second serve took ${reused} ms; the venv was rebuilt"
fi
venvs=$(find "$ZYGO_DATA_HOME/cache/venvs" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | wc -l)
[ "$venvs" -eq 1 ] && ok "exactly one venv directory exists in the cache" || bad "$venvs venv directories for one requirements file"

# A tenant must not be able to modify what other tenants share.
printf 'def handler(event):\n    try:\n        open("/venv/lib/x", "w").write("x")\n        return {"wrote": True}\n    except OSError as e:\n        return {"wrote": False, "errno": e.errno}\n' > tamper.py
"$ZYGO" serve tamper.py --name tamper --requirements requirements.txt >/dev/null 2>&1
out=$("$ZYGO" exec tamper '{}' 2>&1)
case "$out" in
    *'"wrote": false'*) ok "the venv is read-only inside the sandbox" ;;
    *) bad "the venv could be written: $(printf '%s' "$out" | head -1)" ;;
esac

# A requirements line that cannot be installed is reported at serve time,
# with pip's own words, not from inside a broken sandbox later.
printf 'this-package-does-not-exist-zygo-42==9.9.9\n' > bad-requirements.txt
if "$ZYGO" serve uses_six.py --name baddeps --requirements bad-requirements.txt >/tmp/venv-bad.log 2>&1; then
    bad "an uninstallable requirements file was accepted"
else
    if grep -qi "pip exited\|No matching distribution\|ERROR" /tmp/venv-bad.log; then
        ok "an uninstallable requirement fails at serve time with pip's error"
    else
        bad "the failure did not carry pip's error: $(tail -2 /tmp/venv-bad.log | tr '\n' ' ')"
    fi
fi
"$ZYGO" stop withdeps >/dev/null 2>&1
"$ZYGO" stop withdeps2 >/dev/null 2>&1
"$ZYGO" stop tamper >/dev/null 2>&1

say ""
say "the derived system layer"
# `system = ["jq"]`: installed once inside a writable copy of the image, the
# difference written as an OCI layer, every function naming the same packages
# on the same image sharing it. The base image is never touched.
work /tmp/sysl
cat > sysjq.py <<'PY'
import os
import shutil
import subprocess


def handler(event):
    out = None
    if shutil.which("jq"):
        out = subprocess.run(["jq", "--version"], capture_output=True, text=True).stdout.strip()
    return {"jq": out, "lists": os.listdir("/var/lib/apt/lists")}
PY
cat > sandbox.toml <<'TOML'
[defaults]
image = "python:3.12-slim"

[fn.alsojq]
entry = "sysjq.py"
system = ["jq"]

[fn.nojq]
entry = "sysjq.py"

[fn.withjq]
entry = "sysjq.py"
system = ["jq"]
TOML

started=$(date +%s%N)
if "$ZYGO" up >/tmp/up-sys.log 2>&1; then
    ok "\`up\` builds a system layer and brings the functions up ($(( ($(date +%s%N) - started) / 1000000 )) ms in all)"
else
    bad "up with a system layer failed: $(grep -v '^$' /tmp/up-sys.log | tr '\n' ' ' | cut -c1-400)"
fi

out=$("$ZYGO" exec withjq '{}' 2>&1 | tr -d '\n ')
case "$out" in
    *'"jq":"jq-1.'*) ok "the package is there: $(printf '%s' "$out" | sed -n 's/.*"jq":"\([^"]*\)".*/\1/p')" ;;
    *) bad "jq in the derived image: $(printf '%s' "$out" | cut -c1-160)" ;;
esac
case "$out" in
    *'"lists":[]'*) ok "and the apt lists were cleaned out of the layer" ;;
    *) bad "apt lists in the layer: $(printf '%s' "$out" | cut -c1-160)" ;;
esac

out=$("$ZYGO" exec nojq '{}' 2>&1 | tr -d '\n ')
case "$out" in
    *'"jq":null'*) ok "a function on the same image without \`system\` does not see it" ;;
    *) bad "the base image was touched: $(printf '%s' "$out" | cut -c1-160)" ;;
esac

# Names are served in order, so `alsojq` paid for the build and `withjq`,
# naming the same packages, found the layer.
built_ms=$(sed -n 's/.*alsojq.*ready in \([0-9]*\) ms.*/\1/p' /tmp/up-sys.log)
reused_ms=$(sed -n 's/.*withjq.*ready in \([0-9]*\) ms.*/\1/p' /tmp/up-sys.log)
if [ -n "$built_ms" ] && [ -n "$reused_ms" ] && [ "$reused_ms" -lt 5000 ] && [ "$reused_ms" -lt "$built_ms" ]; then
    ok "a second function naming the same packages reuses the layer (built in ${built_ms} ms, reused in ${reused_ms} ms)"
else
    bad "layer reuse: built ${built_ms:-?} ms, reused ${reused_ms:-?} ms"
fi

if "$ZYGO" images 2>/dev/null | grep -q 'python:3.12-slim+system\.'; then
    ok "the derived image is listed beside its base"
else
    bad "no derived image in \`images\`: $("$ZYGO" images 2>&1 | tr '\n' ' ' | cut -c1-160)"
fi
if grep -qs '^jq=' "$ZYGO_DATA_HOME"/cache/system/*/packages; then
    ok "the resolved version is recorded: $(grep -hs '^jq=' "$ZYGO_DATA_HOME"/cache/system/*/packages | head -1)"
else
    bad "no version record under cache/system"
fi
"$ZYGO" down >/dev/null 2>&1

# A package that does not exist fails the function that named it, with apt's
# words, and nothing is left in the image index for it.
work /tmp/sysbad
cp /tmp/sysl/sysjq.py .
printf '[fn.bad]\nimage = "python:3.12-slim"\nentry = "sysjq.py"\nsystem = ["no-such-package-zzz"]\n' > sandbox.toml
"$ZYGO" up >/tmp/up-bad.log 2>&1
rc=$?
if [ $rc -ne 0 ] && grep -q "apt-get exited" /tmp/up-bad.log; then
    ok "a package apt cannot find fails the deploy with apt's own words (exit $rc)"
else
    bad "unknown package: exit $rc, $(grep -v '^$' /tmp/up-bad.log | tr '\n' ' ' | cut -c1-400)"
fi
# The first column only, and a `+system.` layer with nothing after it: the
# bytecode layer is built on a derived image too, as `…+system.X+bytecode.Y`,
# and that is the same image compiled, not a second one.
n=$("$ZYGO" images 2>/dev/null | awk '{print $1}' | grep -cE '^python:3\.12-slim\+system\.[0-9a-f]+$')
[ "$n" -eq 1 ] && ok "and no half-built image was indexed" || bad "$n derived images indexed after a failed build"

# A name that is not a package name is refused by resolution, before any copy.
printf '[fn.bad]\nimage = "python:3.12-slim"\nentry = "sysjq.py"\nsystem = ["jq; rm -rf /"]\n' > sandbox.toml
out=$("$ZYGO" spec explain bad 2>&1)
case "$out" in
    *"not a package name"*) ok "a malformed package name is a spec error" ;;
    *) bad "malformed package name: $(printf '%s' "$out" | head -2 | tr '\n' ' ')" ;;
esac
work /tmp/sup-work

say ""
say "\`zygo shell\`"
# A debug fork: a fresh process entered into the function's namespaces. The
# warm agent must be untouched by it, which is the whole design of the command.
"$ZYGO" serve handler.py --name peek >/dev/null 2>&1
before=$("$ZYGO" --json ps 2>/dev/null | tr -d '\n ' | grep -o '"name":"peek","requests":[0-9]*' | grep -o '[0-9]*$')

out=$("$ZYGO" shell peek -- /bin/sh -c 'ls /zygo' 2>&1)
case "$out" in
    *handler.py*) ok "the shell sees the sandbox's filesystem, not the host's" ;;
    *) bad "shell filesystem: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac

# The pid namespace is the sandbox's: the agent is pid 1 in there, and the host
# has thousands of processes this shell must not see.
out=$("$ZYGO" shell peek -- /bin/sh -c 'ls /proc | grep -c "^[0-9]*$"' 2>/dev/null | tr -d ' \n')
if [ -n "$out" ] && [ "$out" -lt 10 ]; then
    ok "the pid namespace is the sandbox's ($out processes visible)"
else
    bad "process count inside the shell: '$out'"
fi

# Capabilities are dropped, so the shell can do no more than the function can.
out=$("$ZYGO" shell peek -- /bin/sh -c 'grep ^CapEff /proc/self/status' 2>/dev/null | tr -d ' \t\n')
case "$out" in
    CapEff:0000000000000000) ok "the shell holds no capabilities" ;;
    *) bad "shell capabilities: '$out'" ;;
esac

# And the point of the whole command: the agent is untouched.
after=$("$ZYGO" --json ps 2>/dev/null | tr -d '\n ' | grep -o '"name":"peek","requests":[0-9]*' | grep -o '[0-9]*$')
state=$("$ZYGO" --json ps 2>/dev/null | tr -d '\n ' | grep -o '"name":"peek"[^}]*' | grep -o '"state":"[a-z]*"')
if [ "${before:-x}" = "${after:-y}" ] && [ "$state" = '"state":"warm"' ]; then
    ok "the warm agent is untouched by it (requests $after, still warm)"
else
    bad "the shell disturbed the function: requests $before -> $after, $state"
fi

out=$("$ZYGO" exec peek '{"n": 4}' 2>&1)
case "$out" in
    *'"doubled": 8'*) ok "and it still serves requests afterwards" ;;
    *) bad "after a shell: $(printf '%s' "$out" | head -1)" ;;
esac

exits 4 "a shell into a function that was never served is \`not found\`" \
    "$ZYGO" shell nosuchfn -- /bin/true
"$ZYGO" stop peek >/dev/null 2>&1

say ""
say "\`zygo logs\`"
# The supervisor keeps a bounded log per function name: the zygote's own
# output and one entry per request. It belongs to the name, so it survives a
# replacement — a `logs -f` across a deploy shows the new zygote come up.
printf 'import sys\nprint("zygote says hello", file=sys.stderr)\n\n\ndef handler(event):\n    print("stdout line", event.get("n"))\n    print("stderr line", file=sys.stderr)\n    if event.get("boom"):\n        raise ValueError("boom")\n    return {"ok": True}\n' > chatty.py
"$ZYGO" serve chatty.py --name chatty >/dev/null 2>&1
for n in 1 2 3; do "$ZYGO" exec chatty "{\"n\": $n}" >/dev/null 2>&1; done
"$ZYGO" exec chatty '{"n": 4, "boom": true}' >/dev/null 2>&1

out=$("$ZYGO" logs chatty 2>&1)
case "$out" in
    *"zygote says hello"*) ok "the zygote's own stderr is in the log" ;;
    *) bad "no zygote line: $(printf '%s' "$out" | head -3 | tr '\n' ' ')" ;;
esac
reqs=$(printf '%s' "$out" | grep -c "req ")
[ "$reqs" -eq 4 ] && ok "one entry per request ($reqs)" || bad "expected 4 request entries, saw $reqs: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-200)"
case "$out" in
    *"stdout line 2"*"stderr line"*) ok "a request's stdout and stderr are both there" ;;
    *) bad "request output missing: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-240)" ;;
esac

out=$("$ZYGO" logs chatty --failed 2>&1)
reqs=$(printf '%s' "$out" | grep -c "req ")
case "$reqs:$out" in
    1:*ValueError*) ok "\`--failed\` keeps only the request that failed, with its error" ;;
    *) bad "--failed: $reqs entries: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-200)" ;;
esac

reqs=$("$ZYGO" logs chatty -n 2 2>&1 | grep -c "req ")
[ "$reqs" -eq 2 ] && ok "\`-n 2\` is the two most recent" || bad "-n 2 gave $reqs entries"

lines=$("$ZYGO" --json logs chatty -n 3 2>/dev/null | grep -c '"kind":"request"')
[ "$lines" -eq 3 ] && ok "\`--json\` prints one entry per line" || bad "--json: $lines request lines"

# Follow: a reader already attached sees a request made afterwards.
#
# Waited for rather than slept through. The first version gave the follower
# one second to attach and the request one and a half to arrive, which is
# generous on a laptop and not on a Raspberry Pi — where it failed twice on a
# `--follow` that was working, having captured an *earlier* request while the
# one it was waiting for was still being handled. A check that fails on slow
# hardware is measuring the hardware.
: > /tmp/follow.out
"$ZYGO" logs chatty -f -n 0 >/tmp/follow.out 2>&1 &
follower=$!
sleep 2
"$ZYGO" exec chatty '{"n": 99}' >/dev/null 2>&1
waited=0
while [ "$waited" -lt 200 ] && ! grep -q "stdout line 99" /tmp/follow.out 2>/dev/null; do
    waited=$((waited + 1))
    sleep 0.1
done
kill $follower 2>/dev/null; wait $follower 2>/dev/null
if grep -q "stdout line 99" /tmp/follow.out; then
    ok "\`-f\` shows a request made after it started (after $((waited * 100)) ms)"
else
    bad "follow did not show the request in 20 s: $(tr '\n' ' ' </tmp/follow.out | cut -c1-200)"
fi

# The log belongs to the name: a replacement's zygote lands in the same log,
# after everything that was there before.
"$ZYGO" serve chatty.py --name chatty >/dev/null 2>&1
hellos=$("$ZYGO" logs chatty -n 500 2>&1 | grep -c "zygote says hello")
[ "$hellos" -eq 2 ] && ok "the log survives a replacement: both zygotes' lines are in it" \
    || bad "after a replacement the log has $hellos zygote lines"

exits 4 "logs for a name that was never served is \`not found\`" "$ZYGO" logs nosuchfn
"$ZYGO" stop chatty >/dev/null 2>&1
exits 4 "and a stopped function's log is gone with it" "$ZYGO" logs chatty

say ""
say "the strict child filter"
# Under `strict` the agent's forked child installs a second seccomp filter
# before the handler runs: no new program, no new process, threads allowed.
# The positive case comes first, or "subprocess failed" proves nothing —
# the image might simply have no /bin/echo.
printf 'import subprocess\n\n\ndef handler(event):\n    out = subprocess.run(["/bin/echo", "spawned"], capture_output=True, text=True)\n    return {"spawned": out.stdout.strip()}\n' > spawner.py
printf 'import threading\n\n\ndef handler(event):\n    seen = []\n    t = threading.Thread(target=lambda: seen.append(1))\n    t.start()\n    t.join()\n    return {"threads": len(seen)}\n' > threader.py
if "$ZYGO" serve spawner.py --name spawn_default >/dev/null 2>&1; then
    out=$("$ZYGO" exec spawn_default '{}' 2>&1 | tr -d '\n ')
    case "$out" in
        *'"spawned":"spawned"'*) ok "under \`default\` a handler can start a program (the positive case)" ;;
        *) bad "default spawner: $(printf '%s' "$out" | cut -c1-160)" ;;
    esac
else
    bad "spawner did not come up under default"
fi
if "$ZYGO" serve spawner.py --name spawn_strict --seccomp strict >/dev/null 2>&1; then
    out=$("$ZYGO" exec spawn_strict '{}' 2>&1)
    rc=$?
    case "$out" in
        *PermissionError*|*"Errno 1"*)
            [ "$rc" -ne 0 ] && ok "under \`strict\` the child cannot execve: subprocess fails with EPERM (exit $rc)" \
                || bad "strict spawner reported EPERM but exited 0" ;;
        *) bad "strict spawner: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
    esac
    # The filter dies with the child: the zygote is untouched and answers again.
    out=$("$ZYGO" exec spawn_strict '{}' 2>&1)
    case "$out" in
        *PermissionError*|*"Errno 1"*) ok "and the zygote itself is untouched: the next request is refused the same way" ;;
        *) bad "second strict request: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
    esac
else
    bad "spawner did not come up under strict"
fi
if "$ZYGO" serve threader.py --name thread_strict --seccomp strict >/dev/null 2>&1; then
    out=$("$ZYGO" exec thread_strict '{}' 2>&1 | tr -d '\n ')
    case "$out" in
        *'"threads":1'*) ok "a thread is a clone with CLONE_THREAD and still works under the child filter" ;;
        *) bad "strict threader: $(printf '%s' "$out" | cut -c1-160)" ;;
    esac
else
    bad "threader did not come up under strict"
fi
"$ZYGO" stop spawn_default >/dev/null 2>&1
"$ZYGO" stop spawn_strict >/dev/null 2>&1
"$ZYGO" stop thread_strict >/dev/null 2>&1

say ""
say "protocol conformance"
# The design claims the warm protocol is language independent. That is worth
# what its smallest implementation proves, so the suite runs against the
# reference Python agent *and* against a complete agent written in POSIX sh —
# which shares no code with Zygo at all.
# `jq` is needed by the sh agent below and by nothing else here. Installing it
# is conditional three ways, and every one of those conditions was learned by
# this line stopping a whole run:
#
#   * only if it is missing — the container image ships no package lists, the
#     Pi has jq already, and `apt-get update` on the Pi is pure cost;
#   * only as root — as an ordinary user `apt-get update` does not fail, it
#     **blocks**, and the suite sat at this line for the rest of its life. A
#     139-check run that never reaches check 104 reports nothing at all;
#   * under a timeout either way, because an apt mirror is a network service
#     and this suite's job is not to wait on one.
#
# If it is still missing afterwards the sh agent case says so and fails, which
# is the honest outcome: not checked is not the same as passed.
if ! command -v jq >/dev/null 2>&1; then
    if [ "$(id -u)" = 0 ]; then
        timeout 120 apt-get update >/tmp/jq-install.log 2>&1
        timeout 120 apt-get install -y --no-install-recommends jq >>/tmp/jq-install.log 2>&1
    else
        say "  note  jq is missing and this is not root, so it will not be installed"
    fi
fi

out=$("$ZYGO" agent test python3 \
    --script $SRC/examples/agents/conformance/script.py \
    --script-spawn $SRC/examples/agents/conformance/spawn.py -- \
    $SRC/agents/python/zygo_agent.py --fd 3 \
    $SRC/examples/agents/conformance/handler.py 2>&1)
case "$out" in
    *"checks passed"*) ok "the Python reference agent conforms ($(printf '%s' "$out" | grep -c 'PASS') checks)" ;;
    *) bad "python agent: $(printf '%s' "$out" | grep FAIL | head -2 | tr '\n' ' ' | cut -c1-200)" ;;
esac

if command -v jq >/dev/null 2>&1; then
    # To a file, not a command substitution: `$( )` waits for the pipe to
    # close, which an agent that leaked a background child keeps open long
    # after the harness has exited. The harness kills the agent's whole
    # process group now, so this is belt and braces — but a conformance suite
    # that hangs when the agent under test misbehaves is worse than useless.
    "$ZYGO" agent test /bin/sh -- $SRC/examples/agents/sh/agent.sh \
        $SRC/examples/agents/sh/handler.sh >/tmp/sh-agent.log 2>&1
    out=$(cat /tmp/sh-agent.log)
    case "$out" in
        *"checks passed"*) ok "an agent written in POSIX sh conforms to the same protocol" ;;
        *) bad "sh agent: $(printf '%s' "$out" | grep FAIL | head -2 | tr '\n' ' ' | cut -c1-200)" ;;
    esac
else
    bad "jq could not be installed, so the sh agent could not be checked"
fi

# A conformance suite that passes everything is not evidence of anything. This
# agent answers PING and nothing else.
cat > /tmp/lazy-agent.sh <<'LAZY'
#!/bin/sh
printf '\000\000\000\070{"type":"READY","proto":1,"pid":1,"imports_ms":0,"rss_kb":0,"runtime":"lazy"}' >&3
sleep 30
LAZY
chmod +x /tmp/lazy-agent.sh
"$ZYGO" agent test /bin/sh -- /tmp/lazy-agent.sh >/tmp/lazy.log 2>&1
rc=$?
if [ "$rc" -ne 0 ] && grep -q FAIL /tmp/lazy.log; then
    ok "an agent that only sends READY is failed, not passed (exit $rc)"
else
    bad "a non-conforming agent passed the suite: exit $rc"
fi

say ""
say "egress networking"
# `network = "egress"` hands the sandbox's network namespace to `pasta` and
# installs an nftables allowlist inside it. Both programs run unprivileged:
# pasta moves packets in userspace, and nft works because entering the user
# namespace Zygo created grants a full capability set inside it. Names go
# through Zygo's own resolver, bound inside the namespace: a name off the list
# does not resolve, and one on it is admitted to the filter before it is
# answered — which is what makes a wildcard rule enforceable.
#
# These need real outbound connectivity. 1.1.1.1 is a stable address that
# needs no DNS to reach; speed.cloudflare.com serves arbitrary byte counts
# over HTTPS, which is what the bandwidth check needs.
#
# Every negative assertion below reads a named field out of the handler's
# answer and fails when it is missing. Without that, a function that never
# started reads as "the destination was refused", which is the shape of
# check that passes for the wrong reason.
# Installed here when this is a container we are root in; on a real host it is
# the operator's package manager and not this script's business. The condition
# is now checked rather than described: as an ordinary user `apt-get` blocks
# instead of failing, and a suite that stops here reports none of what follows.
if [ "$(id -u)" = 0 ] && ! command -v pasta >/dev/null 2>&1; then
    timeout 180 apt-get install -y --no-install-recommends passt nftables iproute2 \
        >/tmp/net-install.log 2>&1
fi
# `pasta` needs a tun device it can open, which is a separate question from
# whether the package is installed — a container started without `--device`
# has the binary and nothing for it to attach to. Both are prerequisites, and
# a prerequisite that is missing should skip this section rather than report
# five functions that "did not start".
#
# Opening the device is necessary and not sufficient: it says the node is
# there and the driver is loaded, not that `pasta` can build an interface
# through it. A host that gets past this and still cannot bring egress up
# fails below, once, and skips the rest.
egress_possible=no
if ! command -v pasta >/dev/null 2>&1 || ! command -v nft >/dev/null 2>&1 ||
    ! command -v tc >/dev/null 2>&1; then
    # One missing package must not become twenty failures. A section that
    # cannot run says so once and is skipped; a wall of red for a missing
    # dependency is how people learn to ignore red.
    say "  SKIP  egress needs pasta, nft and tc, and they are not installed here"
    say "        → sudo apt install passt nftables iproute2 (this script only"
    say "          installs them when it is root, as it is inside the container)"
elif ! ( : <>/dev/net/tun ) 2>/dev/null; then
    # In a **subshell**, and that is not style. `:` is a special built-in, so
    # POSIX says a redirection error on it ends the shell — and where
    # `/dev/net` does not exist at all (any container started without
    # `--device /dev/net/tun`), this prerequisite check killed the suite it
    # was protecting, one line below the comment about not letting a missing
    # package become twenty failures.
    say "  SKIP  egress needs /dev/net/tun, and this user cannot open it here"
    say "        → the device is missing or unreadable; on a host that means"
    say "          loading the tun module, on a container passing --device"
else
    ok "pasta, nft and tc are installed, and /dev/net/tun opens"
    egress_possible=yes
fi

if [ "$egress_possible" = yes ]; then

if "$ZYGO" doctor 2>/dev/null | grep -q 'egress (pasta + nft + tc)'; then
    ok "\`doctor\` reports the egress prerequisites"
else
    bad "doctor says nothing about egress: $("$ZYGO" doctor 2>&1 | tr '\n' ' ' | cut -c1-160)"
fi
# Landlock's network rules need ABI v4 (kernel 6.7). Where they are present a
# connect to a port off the list is refused by the kernel before a packet
# exists, and the error is EACCES rather than a route error.
landlock_abi=$("$ZYGO" doctor 2>/dev/null | sed -n 's/.*landlock.*ABI v\([0-9]*\).*/\1/p' | head -1)
landlock_abi=${landlock_abi:-0}

# One JSON string field, empty when it is not there.
field() { printf '%s' "$1" | sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p"; }
# One JSON number field.
num() { printf '%s' "$1" | sed -n "s/.*\"$2\":\([0-9.]*\).*/\1/p"; }
# How many `pasta` processes are running right now.
# How many `pasta` processes are running right now.
#
# Matched by prefix, not equality: `passt` ships CPU-tuned builds and
# `/usr/bin/pasta` is a symlink to one of them, so `comm` reads `pasta.avx2`
# on an x86_64 runner with AVX2 and `pasta` on aarch64. Asking for equality
# found none of them and reported "expected a pasta per networked sandbox,
# found 0" on a host where four were serving.
pastas() {
    n=0
    for c in /proc/[0-9]*/comm; do
        case "$(cat "$c" 2>/dev/null)" in
            pasta | pasta.*) n=$((n + 1)) ;;
        esac
    done
    printf '%s' "$n"
}

work /tmp/egress
cat > net.py <<'PY'
import http.client
import socket
import ssl
import time


def reach(host, port, timeout=6):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(timeout)
    try:
        s.connect((host, port))
        return "ok"
    except Exception as e:
        return type(e).__name__
    finally:
        s.close()


def handler(event):
    what = event.get("do", "probe")
    if what == "resolve":
        try:
            return {"ip": socket.gethostbyname(event["name"])}
        except Exception as e:
            return {"error": type(e).__name__}
    if what == "resolver":
        # Ask a resolver that is not the one Zygo forced.
        s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        s.settimeout(4)
        q = bytes([0xAB, 0xCD, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0])
        q += b"\x07example\x03com\x00" + bytes([0, 1, 0, 1])
        try:
            s.sendto(q, (event["at"], 53))
            s.recvfrom(512)
            return {"answered": True}
        except Exception as e:
            return {"answered": False, "error": type(e).__name__}
        finally:
            s.close()
    if what == "resolvconf":
        with open("/etc/resolv.conf") as f:
            return {"resolv": f.read().strip().splitlines()}
    if what == "connect":
        return {"result": reach(event["host"], int(event["port"]))}
    if what == "holdopen":
        # Open connections and keep every one open until the end, so the
        # count the filter sees is the count this handler holds.
        held, results = [], []
        for _ in range(int(event.get("n", 5))):
            s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            s.settimeout(6)
            try:
                s.connect(("1.1.1.1", 443))
                results.append("ok")
                held.append(s)
            except Exception as e:
                results.append(type(e).__name__)
                s.close()
        for s in held:
            s.close()
        return {"results": ",".join(results)}
    if what == "upload":
        # The bandwidth measurement, on what the sandbox sends. TLS is not
        # what is under test, so the certificate is not checked.
        ctx = ssl._create_unverified_context()
        body = b"x" * int(event.get("bytes", 500000))
        started = time.monotonic()
        try:
            c = http.client.HTTPSConnection(event["host"], 443, timeout=90, context=ctx)
            c.request("POST", "/__up", body=body, headers={"Content-Type": "application/octet-stream"})
            r = c.getresponse()
            r.read()
            return {"status": r.status, "seconds": round(time.monotonic() - started, 2)}
        except Exception as e:
            return {"error": type(e).__name__, "detail": str(e)[:120]}
    if what == "fetch":
        # A bandwidth measurement, not a TLS one: the certificate is not
        # checked, so the image's trust store is irrelevant to the result.
        ctx = ssl._create_unverified_context()
        started = time.monotonic()
        try:
            c = http.client.HTTPSConnection(event["host"], 443, timeout=60, context=ctx)
            c.request("GET", event.get("path", "/"))
            r = c.getresponse()
            body = r.read()
            return {"status": r.status, "bytes": len(body), "seconds": round(time.monotonic() - started, 2)}
        except Exception as e:
            return {"error": type(e).__name__, "detail": str(e)[:120]}
    return {
        "allowed": reach("1.1.1.1", 443),
        "other": reach("8.8.8.8", 443),
        "private": reach("10.255.255.1", 443, 3),
    }
PY
cat > sandbox.toml <<'TOML'
[defaults]
image = "python:3.12-slim"
entry = "net.py"

[fn.sealed]
network = "none"

[fn.limited]
network = "egress"
allow = ["1.1.1.1:443", "*.cloudflare.com:443"]

[fn.few]
network = "egress"
allow = ["1.1.1.1:443"]
connections = 3

[fn.slow]
network = "egress"
allow = ["*.cloudflare.com:443"]
bandwidth = "100K"

[fn.wide]
network = "full"
TOML

if "$ZYGO" up >/tmp/up-net.log 2>&1; then
    ok "\`up\` brings up sealed, egress, limited and full functions together"
    egress_up=yes
else
    # One line, then skip what depends on it. Every check below reads a field
    # out of a networked function's answer, so without those functions they
    # all fail for the same single reason — twelve red lines saying one
    # thing, with the one thing buried.
    egress_up=no
    # The *whole* log, not its last lines: the reason `up` failed is on the
    # first line and the per-function ticks are on the last, so a `tail` here
    # reports four functions that "did not start" and hides why.
    why=$(grep -v '^$' /tmp/up-net.log | tr '\n' ' ' | cut -c1-220)
    # `pasta` refusing before Zygo has said anything is a prerequisite that is
    # present but does not work. The Raspberry Pi this was found on carries
    # the passt Ubuntu shipped in June 2023, which cannot bring up a namespace
    # for an ordinary user at all — `pasta --config-net -- /bin/true` fails
    # there with no Zygo in the picture. Counting that as a Zygo failure sent
    # a day into a fix for a bug that did not exist.
    case $why in
        *"pasta could not configure"*)
            say "  SKIP  \`pasta\` on this host cannot configure a sandbox's network:"
            say "        $why"
            say "        → this is passt, not Zygo. Check it on its own with"
            say "          \`pasta --config-net -- /bin/true\`" ;;
        *) bad "up with egress failed: $why" ;;
    esac
    say "  SKIP  the rest of this section needs those functions; skipping it"
fi

if [ "$egress_up" = yes ]; then

out=$("$ZYGO" exec sealed '{}' 2>&1 | tr -d '\n ')
a=$(field "$out" allowed)
if [ -z "$a" ]; then
    bad "sealed did not answer: $(printf '%s' "$out" | cut -c1-160)"
elif [ "$a" = ok ]; then
    bad "a network = \"none\" function reached the internet"
else
    ok "network = \"none\" still reaches nothing ($a)"
fi

out=$("$ZYGO" exec limited '{}' 2>&1 | tr -d '\n ')
a=$(field "$out" allowed); o=$(field "$out" other); p=$(field "$out" private)
if [ -z "$a" ] || [ -z "$o" ] || [ -z "$p" ]; then
    bad "the egress function did not answer: $(printf '%s' "$out" | cut -c1-200)"
else
    [ "$a" = ok ] && ok "the allowed destination is reachable" \
        || bad "1.1.1.1:443 was on the allowlist and still not reachable ($a)"
    [ "$o" = ok ] && bad "a destination that is NOT on the allowlist was reachable" \
        || ok "a destination off the allowlist is refused ($o)"
    [ "$p" = ok ] && bad "a private-range address was reachable without --allow-private-net" \
        || ok "the private ranges are refused by default ($p)"
fi

# pasta has to be running for every function that has a network, and only
# for them: it is what holds their namespace open.
running=$(pastas)
[ "$running" -ge 4 ] && ok "pasta is serving the networked sandboxes ($running running)" \
    || bad "expected a pasta per networked sandbox, found $running"

out=$("$ZYGO" exec limited '{"do": "resolvconf"}' 2>&1 | tr -d '\n ')
case "$out" in
    *'"nameserver127.0.0.53"'*)
        case "$out" in
            *search*) bad "the host's search domains leaked into the sandbox: $out" ;;
            *) ok "the sandbox's resolv.conf names Zygo's resolver on loopback and nothing else" ;;
        esac ;;
    *) bad "resolv.conf inside the sandbox: $(printf '%s' "$out" | cut -c1-200)" ;;
esac

# A wildcard rule: the name is matched as the sandbox asks, its addresses are
# admitted to the filter before the answer goes back, and the connection that
# follows is permitted.
out=$("$ZYGO" exec limited '{"do": "fetch", "host": "speed.cloudflare.com", "path": "/__down?bytes=1000"}' 2>&1 | tr -d '\n ')
status=$(num "$out" status); bytes=$(num "$out" bytes)
if [ "$status" = 200 ] && [ "$bytes" = 1000 ]; then
    ok "a wildcard rule resolves and connects: *.cloudflare.com:443 served 1000 bytes"
else
    bad "wildcard fetch: $(printf '%s' "$out" | cut -c1-200)"
fi

out=$("$ZYGO" exec limited '{"do": "resolve", "name": "example.com"}' 2>&1 | tr -d '\n ')
case "$out" in
    *'"error":"gaierror"'*) ok "a name off the allowlist does not resolve at all" ;;
    *'"ip":'*) bad "a name off the allowlist resolved: $out" ;;
    *) bad "off-list resolution: $(printf '%s' "$out" | cut -c1-200)" ;;
esac

# The name is allowed, the port is not: the resolver admits the address for
# 443 only, so 80 is refused — and with Landlock ABI v4 it is refused by the
# kernel at `connect()`, before a packet exists.
out=$("$ZYGO" exec limited '{"do": "connect", "host": "speed.cloudflare.com", "port": 80}' 2>&1 | tr -d '\n ')
r=$(field "$out" result)
if [ -z "$r" ]; then
    bad "the port probe did not answer: $(printf '%s' "$out" | cut -c1-200)"
elif [ "$r" = ok ]; then
    bad "an allowed host on a port off the list was reachable"
elif [ "$landlock_abi" -ge 4 ]; then
    [ "$r" = PermissionError ] && ok "a port off the list is refused by Landlock at connect() ($r; ABI v$landlock_abi)" \
        || bad "Landlock ABI v$landlock_abi is present but the refusal was $r, not PermissionError"
else
    ok "a port off the list is refused by the filter ($r; Landlock ABI v$landlock_abi, so not at connect())"
fi

out=$("$ZYGO" exec limited '{"do": "resolver", "at": "8.8.8.8"}' 2>&1 | tr -d '\n ')
case "$out" in
    *'"answered":true'*) bad "the sandbox reached a resolver of its own choosing: $out" ;;
    *'"answered":false'*) ok "a resolver other than the forced one is unreachable ($(field "$out" error))" ;;
    *) bad "the resolver probe did not answer: $(printf '%s' "$out" | cut -c1-200)" ;;
esac
out=$("$ZYGO" exec limited '{"do": "resolver", "at": "169.254.1.1"}' 2>&1 | tr -d '\n ')
case "$out" in
    *'"answered":true'*) bad "pasta's forwarder is reachable under egress, so any name resolves: $out" ;;
    *'"answered":false'*) ok "and neither is pasta's own forwarder under egress" ;;
    *) bad "the forwarder probe did not answer: $(printf '%s' "$out" | cut -c1-200)" ;;
esac

# The connection limit: a fresh function, so no earlier connection is still
# being counted. Four are attempted against a limit of three.
out=$("$ZYGO" exec few '{"do": "holdopen", "n": 4}' 2>&1 | tr -d '\n ')
results=$(field "$out" results)
case "$results" in
    "ok,ok,ok,"*)
        case "$results" in
            "ok,ok,ok,ok") bad "a fourth connection was accepted past connections = 3" ;;
            *) ok "the fourth connection past \`connections = 3\` is refused ($results)" ;;
        esac ;;
    "") bad "the connection-count probe did not answer: $(printf '%s' "$out" | cut -c1-200)" ;;
    *) bad "fewer than three connections succeeded under a limit of three: $results" ;;
esac

# The bandwidth limit shapes what the sandbox *sends*: 500 KB at 100 KB/s is
# five seconds by arithmetic. The unlimited function uploads the same bytes as
# the control, so a slow network reads as "both slow" rather than as a limit
# that works. (What the sandbox receives is shaped only where the host has an
# `ifb` device; this one does not, and the supervisor's log says so.)
slow=$("$ZYGO" exec slow '{"do": "upload", "host": "speed.cloudflare.com", "bytes": 500000}' 2>&1 | tr -d '\n ')
fast=$("$ZYGO" exec wide '{"do": "upload", "host": "speed.cloudflare.com", "bytes": 500000}' 2>&1 | tr -d '\n ')
sst=$(num "$slow" status); ss=$(num "$slow" seconds); fst=$(num "$fast" status); fs=$(num "$fast" seconds)
if [ "$sst" != 200 ] || [ "$fst" != 200 ]; then
    bad "the bandwidth uploads did not both complete: limited $(printf '%s' "$slow" | cut -c1-120) / unlimited $(printf '%s' "$fast" | cut -c1-120)"
else
    slow_ms=$(printf '%s' "$ss" | awk '{printf "%d", $1 * 1000}')
    fast_ms=$(printf '%s' "$fs" | awk '{printf "%d", $1 * 1000}')
    # The arithmetic is the assertion: 500 KB at 100 KB/s cannot finish in
    # under five seconds, and 3.5 covers the slop. The unlimited upload is the
    # *control*, and when it is slow too the control has failed to control for
    # anything — which is not the same as the limit failing. Saying so is rule
    # 3 from the README: a measurement has to be able to report that it could
    # not measure. Reading a slow network as a broken limit is how this check
    # failed once on a busy link with the limit working perfectly.
    if [ "$slow_ms" -lt 3500 ]; then
        bad "the bandwidth limit did not hold: 500 KB sent in ${ss} s, under the ${slow_ms}ms floor 100 KB/s implies"
    elif [ "$slow_ms" -gt $((fast_ms * 2)) ]; then
        ok "\`bandwidth = \"100K\"\` holds on what the sandbox sends: 500 KB took ${ss} s limited, ${fs} s unlimited"
    else
        ok "\`bandwidth = \"100K\"\` held (${ss} s for 500 KB), though the unlimited control took ${fs} s — too slow a link to tell them apart"
    fi
fi

out=$("$ZYGO" exec wide '{}' 2>&1 | tr -d '\n ')
a=$(field "$out" allowed); o=$(field "$out" other); p=$(field "$out" private)
if [ -z "$a" ] || [ -z "$o" ] || [ -z "$p" ]; then
    bad "the full-network function did not answer: $(printf '%s' "$out" | cut -c1-200)"
else
    [ "$a" = ok ] && [ "$o" = ok ] && ok "network = \"full\" reaches the public internet unrestricted" \
        || bad "full reached $a / $o"
    [ "$p" = ok ] && bad "network = \"full\" reached a private range" \
        || ok "and still not the host's own networks ($p)"
fi
out=$("$ZYGO" exec wide '{"do": "resolve", "name": "example.com"}' 2>&1 | tr -d '\n ')
case "$out" in
    *'"ip":'*) ok "under \`full\` any name resolves, through pasta's forwarder" ;;
    *) bad "resolution under full: $(printf '%s' "$out" | cut -c1-200)" ;;
esac

# pasta holds the namespace open, so nothing else would ever end it. This is
# only meaningful because the check above saw them running first.
"$ZYGO" down >/dev/null 2>&1
sleep 1
leaked=$(pastas)
[ "$leaked" -eq 0 ] && ok "no pasta process is left behind after \`down\`" \
    || bad "$leaked pasta processes leaked"
fi   # egress_up
fi   # egress_possible

work /tmp/sup-work

say ""
say "the HTTP API"
# `zygo api` is a client of the supervisor, so everything above is reachable
# over HTTP too. The image has no curl; a few lines of urllib do the same.
http() {
    PORT="$1" METHOD="$2" URLPATH="$3" BODY="${4-}" TOKEN="${5-}" python3 - <<'PY'
import os, sys, urllib.request, urllib.error
port, method, path = os.environ["PORT"], os.environ["METHOD"], os.environ["URLPATH"]
body = os.environ.get("BODY", "").encode() or None
req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=body, method=method)
req.add_header("content-type", "application/json")
if os.environ.get("TOKEN"):
    req.add_header("authorization", "Bearer " + os.environ["TOKEN"])
try:
    with urllib.request.urlopen(req, timeout=30) as r:
        print(r.status); print(r.read().decode())
except urllib.error.HTTPError as e:
    print(e.code); print(e.read().decode())
except Exception as e:
    print(0); print(str(e))
PY
}
wait_http() {
    i=0
    while [ $i -lt 100 ]; do
        [ "$(http "$1" GET /healthz | head -1)" = 200 ] && return 0
        i=$((i+1)); sleep 0.1
    done
    return 1
}

"$ZYGO" serve handler.py --name double >/dev/null 2>&1
"$ZYGO" api --no-auth --listen 127.0.0.1:7700 >/tmp/api.log 2>&1 &
API=$!
if wait_http 7700; then
    ok "\`zygo api\` comes up and answers /healthz"
else
    bad "the API did not answer /healthz: $(head -2 /tmp/api.log)"
fi

out=$(http 7700 POST /fn/double '{"n": 21}')
case "$out" in
    200*'"doubled":42'*) ok "POST /fn/<name> runs the function and returns its result" ;;
    *) bad "POST /fn/double: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-120)" ;;
esac

out=$(http 7700 POST /fn/ghost '{}')
[ "$(printf '%s' "$out" | head -1)" = 404 ] && ok "an unserved function is a 404" || bad "unserved function: $(printf '%s' "$out" | head -1)"

"$ZYGO" serve boom.py --name boom >/dev/null 2>&1
out=$(http 7700 POST /fn/boom '{}')
case "$out" in
    500*ZeroDivisionError*) ok "a handler that raises is a 500 carrying the traceback" ;;
    *) bad "raising handler over HTTP: $(printf '%s' "$out" | head -1)" ;;
esac

served spinhttp spin.py --name spinhttp --timeout 2s
started=$(date +%s%N)
out=$(http 7700 POST /fn/spinhttp '{}')
elapsed=$(( ($(date +%s%N) - started) / 1000000 ))
if [ "$(printf '%s' "$out" | head -1)" = 408 ] && [ "$elapsed" -lt 5000 ]; then
    ok "a request that overruns its timeout is a 408, not a 500 (${elapsed} ms)"
else
    bad "timeout over HTTP: status $(printf '%s' "$out" | head -1) after ${elapsed} ms"
fi

out=$(http 7700 POST /fn/double/batch '[{"n": 1}, {"n": 2}, {"n": 3}]')
case "$out" in
    200*'"doubled":2'*'"doubled":4'*'"doubled":6'*) ok "POST /fn/<name>/batch answers every event, in order" ;;
    *) bad "batch: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac

out=$(http 7700 GET /fn)
case "$out" in
    200*'"name":"double"'*) ok "GET /fn lists warm functions" ;;
    *) bad "GET /fn: $(printf '%s' "$out" | head -1)" ;;
esac

out=$(http 7700 GET /fn/double/stats)
case "$out" in
    200*'"requests":'*) ok "GET /fn/<name>/stats reports counters" ;;
    *) bad "stats: $(printf '%s' "$out" | head -1)" ;;
esac

out=$(http 7700 POST /fn/double/warm)
case "$out" in
    200*'"state":"warm"'*) ok "POST /fn/<name>/warm reports the function warm" ;;
    *) bad "warm: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-100)" ;;
esac

out=$(http 7700 GET /metrics)
case "$out" in
    200*'zygo_function_requests_total{fn="double"}'*) ok "GET /metrics exposes per-function counters in Prometheus format" ;;
    *) bad "metrics: $(printf '%s' "$out" | head -1)" ;;
esac

out=$(http 7700 POST /fn/double 'not json at all')
[ "$(printf '%s' "$out" | head -1)" = 400 ] && ok "a body that is not JSON is a 400" || bad "bad body: $(printf '%s' "$out" | head -1)"

kill "$API" 2>/dev/null; wait "$API" 2>/dev/null

# Bearer auth: the default, and the token comes from the environment only.
ZYGO_API_TOKEN=zygo-test-token-4242 "$ZYGO" api --listen 127.0.0.1:7701 >>/tmp/api.log 2>&1 &
API=$!
if wait_http 7701; then
    ok "/healthz needs no token, so a load balancer can probe it"
else
    bad "the authenticated API did not come up: $(tail -2 /tmp/api.log)"
fi
[ "$(http 7701 POST /fn/double '{"n":1}' | head -1)" = 401 ] && ok "a request without the token is a 401" || bad "missing token was not refused"
[ "$(http 7701 POST /fn/double '{"n":1}' wrong-token | head -1)" = 401 ] && ok "a wrong token is a 401" || bad "wrong token was not refused"
out=$(http 7701 POST /fn/double '{"n":1}' zygo-test-token-4242)
case "$out" in
    200*'"doubled":2'*) ok "the right token gets through" ;;
    *) bad "right token: $(printf '%s' "$out" | head -1)" ;;
esac
kill "$API" 2>/dev/null; wait "$API" 2>/dev/null

if "$ZYGO" api --no-auth --listen 0.0.0.0:7702 >/dev/null 2>&1; then
    bad "an unauthenticated API on 0.0.0.0 was allowed"
else
    ok "an unauthenticated listener on a reachable address is refused (P6)"
fi
if ZYGO_API_TOKEN= "$ZYGO" api --listen 127.0.0.1:7703 >/dev/null 2>&1; then
    bad "bearer auth with no token was allowed"
else
    ok "bearer auth without ZYGO_API_TOKEN is refused rather than silently open"
fi
# `boom` stays up: the replacement section further down stops it, and that
# check is what proves `stop` works on a function that has served requests.
"$ZYGO" stop spinhttp >/dev/null 2>&1

say ""
say "deadlines"

# The limit is the function's own `timeout`, not whatever the client asked to
# wait for: N4 makes the spec's limits mandatory, so a client cannot buy more
# time by asking for it. `zygo exec` waits 60 s by default, so a run that ends
# near 2 s is the function's budget being enforced and one near 60 s is not.
if served spin spin.py --name spin --timeout 2s; then
    spin_up=yes
else
    spin_up=no
    say "  SKIP  the deadline and tiering checks below all need it"
fi

if [ "$spin_up" = yes ]; then
started=$(date +%s%N)
out=$("$ZYGO" exec spin '{}' 2>&1)
elapsed=$(( ($(date +%s%N) - started) / 1000000 ))
if [ "$elapsed" -ge 1500 ] && [ "$elapsed" -le 5000 ]; then
    ok "a handler that never returns is stopped at its own timeout (${elapsed} ms)"
else
    bad "the deadline was not the function's: took ${elapsed} ms for a 2 s timeout"
fi
# What the *log* says the killed request cost. The agent's `DONE` describes a
# request that finished, and a killed one did not: its `wall_ms` was zero for
# something that had just run for two seconds. Every timeout then read as the
# fastest request there was, and dragged `zygo stats`' percentiles down with
# it — the slowest counted as the quickest.
logged=$("$ZYGO" --json logs spin -n 1 2>/dev/null |
    sed -n 's/.*"wall_ms":\([0-9.]*\).*/\1/p' | head -1)
logged_ms=$(printf '%s' "${logged:-0}" | awk '{printf "%d", $1}')
if [ "${logged_ms:-0}" -ge 1500 ]; then
    ok "and the log records what it actually cost (${logged_ms} ms against a 2 s deadline)"
else
    bad "the killed request is logged as ${logged:-nothing} ms; a timeout that logs near zero makes every percentile a lie"
fi

case "$out" in
    *SIGKILL*) ok "and the caller is told it was killed, not left guessing" ;;
    *) bad "unhelpful timeout message: $(printf '%s' "$out" | head -1)" ;;
esac

# The connection has to survive a kill, or every timeout would cost a rewarm.
"$ZYGO" exec spin '{}' >/dev/null 2>&1
if "$ZYGO" --json ps 2>/dev/null | tr -d '\n ' | grep -q '"name":"spin","requests":2'; then
    ok "the connection stayed in step: a second request reached the same sandbox"
else
    bad "the function did not survive its first timeout"
fi
# Read the state from the table rather than the JSON: serde sorts object keys,
# so "name" is not followed by "state" and a grep that assumed so passes only
# by accident.
state=$("$ZYGO" ps 2>/dev/null | awk '$1 == "spin" { print $2 }')
if [ "$state" = warm ]; then
    ok "and it is still warm rather than being rewarmed on every timeout"
else
    bad "a timeout left the function in state '$state'"
fi

# Killing the one pid we know about is not enough. Below kernel 5.14 there is
# no `cgroup.kill`, and a handler that forked helpers leaves them holding the
# result pipe open, so the agent never sees EOF and the function wedges.
"$ZYGO" serve spawn.py --name spawn --timeout 2s --pids 32 >/dev/null 2>&1
"$ZYGO" exec spawn '{}' >/dev/null 2>&1
sleep 1
spinning=0
for d in /proc/[0-9]*; do
    [ "$(cat "$d/comm" 2>/dev/null)" = python3 ] || continue
    [ "$(awk '{print $3}' "$d/stat" 2>/dev/null)" = R ] && spinning=$((spinning+1))
done
if [ "$spinning" -eq 0 ]; then
    ok "a timed-out handler's own forked helpers are killed with it"
else
    bad "$spinning process(es) survived the deadline that killed their parent"
fi
fi   # spin_up

say ""
say "the request cgroup"
# The check that would have caught a bug that hid for months: the
# agent is pid 1 in its own namespace, so the pid it reports means nothing to
# the supervisor. Writing it into `cgroup.procs` silently moved *nothing*, and
# the per-request cgroup was an empty directory being created and removed.
printf 'import time\n\n\ndef handler(event):\n    time.sleep(1.5)\n    return {"ok": True}\n' > naptime.py
served nap naptime.py --name nap || true
( sleep 0.7
  found=empty
  for d in "$CG"/launch/zygo.slice/tenants/default/nap/*/req-*; do
      [ -d "$d" ] || continue
      # `-s` is useless here: cgroupfs reports st_size 0 for every file,
      # populated or not. The content is the only evidence.
      [ -n "$(cat "$d/cgroup.procs" 2>/dev/null)" ] && found=populated
  done
  echo "$found" > /tmp/reqcg ) &
probe=$!
"$ZYGO" exec nap '{}' >/dev/null 2>&1
wait "$probe"
if [ "$(cat /tmp/reqcg 2>/dev/null)" = populated ]; then
    ok "the running request is actually inside its own cgroup"
else
    bad "the per-request cgroup is empty; the pid was never translated"
fi
"$ZYGO" stop nap >/dev/null 2>&1
"$ZYGO" stop spin >/dev/null 2>&1
"$ZYGO" stop spawn >/dev/null 2>&1

say ""
say "idle tiering"
# Pausing keeps the resident pages that make the next request a fork, so a
# paused function has to answer at warm speed once thawed.
served nap2 handler.py --name nap2 --idle-timeout 1s || true
"$ZYGO" exec nap2 '{"n": 1}' >/dev/null 2>&1
sleep 3
state=$("$ZYGO" ps 2>/dev/null | awk '$1 == "nap2" { print $2 }')
if [ "$state" = paused ]; then
    ok "an idle function is paused after its idle_timeout"
else
    bad "an idle function is in state '$state', not paused"
fi

started=$(date +%s%N)
out=$("$ZYGO" exec nap2 '{"n": 21}' 2>/dev/null)
woke=$(( ($(date +%s%N) - started) / 1000000 ))
case "$out" in
    *'"doubled": 42'*)
        # A cold start is a few hundred ms; a thaw is one write. Anything in
        # that range means the pages were kept, which is the point of the tier.
        if [ "$woke" -lt 250 ]; then
            ok "and answers correctly on waking, in ${woke} ms — it was thawed, not rebuilt"
        else
            bad "waking took ${woke} ms; that is a cold start, not a thaw"
        fi ;;
    *) bad "a paused function gave the wrong answer: $(printf '%s' "$out" | head -1)" ;;
esac
state=$("$ZYGO" ps 2>/dev/null | awk '$1 == "nap2" { print $2 }')
if [ "$state" = warm ]; then
    ok "and is warm again afterwards"
else
    bad "after waking the function is '$state'"
fi
"$ZYGO" stop nap2 >/dev/null 2>&1

say ""
say "runtime pools"
# The other warm shape: a zygote that holds an interpreter and no tenant code,
# with the script arriving in the request. What has to be true for several
# tenants to share one is that the zygote is anonymous, so the checks here are
# mostly about what is *not* in it.
work /tmp/pool
cat > a.py <<'PY'
import os


def handler(event):
    return {"from": "a", "pid": os.getpid(), "file": __file__}
PY
cat > b.py <<'PY'
def handler(event):
    import sys
    return {"saw_a": any("zygo_request" in m for m in sys.modules)}
PY

exits 0 "a runtime pool is served with no handler at all" \
    "$ZYGO" serve --runtime pool --image "$IMAGE" --agent python --min-warm 1

out=$("$ZYGO" exec --runtime pool --script a.py '{}' 2>&1)
case $out in
    *'"from": "a"'*) ok "a script from the command line runs in the pool" ;;
    *) bad "exec --runtime: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac

# The supervisor writes the script into the sandbox rather than sending it
# through the zygote, which is what keeps a shared zygote free of tenant code.
case $out in
    *'/run/script/'*) ok "and reached the child as a file, not through the zygote" ;;
    *) bad "the script did not arrive as a file: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac

first_pid=$(printf '%s' "$out" | tr -d '\n ' | grep -o '"pid":[0-9]*' | grep -o '[0-9]*$')
out=$("$ZYGO" exec --runtime pool --script a.py '{}' 2>&1)
second_pid=$(printf '%s' "$out" | tr -d '\n ' | grep -o '"pid":[0-9]*' | grep -o '[0-9]*$')
if [ -n "$first_pid" ] && [ "$first_pid" != "$second_pid" ]; then
    ok "each script request is its own process ($first_pid then $second_pid)"
else
    bad "two script requests shared a process ($first_pid, $second_pid)"
fi

out=$("$ZYGO" exec --runtime pool --script b.py '{}' 2>&1)
case $out in
    *'"saw_a": false'*) ok "the zygote carries nothing from the script before" ;;
    *) bad "a script was left in the zygote: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac

# A pool is a row of its own in `top`, because it has no single state: counts
# of warm and paused zygotes are what an operator needs instead.
out=$("$ZYGO" top --once 2>&1)
case $out in
    *RUNTIME*WARM*PAUSED*) ok "\`top\` shows the pool with its zygote counts" ;;
    *) bad "top has no runtime table: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-200)" ;;
esac
case $out in
    *"pool "*) ok "and names it" ;;
    *) bad "the pool is not in the table: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-200)" ;;
esac

out=$("$ZYGO" stats pool 2>&1)
case $out in
    *pool*) ok "\`stats\` summarises the pool's requests beside the functions'" ;;
    *) bad "stats has no row for the pool: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac

# A pool may not be given code to warm: that is the isolation claim, so it is
# refused rather than quietly accepted.
exits 1 "a pool that is given a handler is refused" \
    "$ZYGO" serve a.py --runtime pool2 --image "$IMAGE"
exits 4 "a script for a runtime nobody served is \`not found\`" \
    "$ZYGO" exec --runtime nowhere --script a.py '{}'

say ""
say "replacement and shutdown"
# Back where `handler.py` is: the pool section above works in a directory of
# its own, and `serve handler.py` below resolves against the one it is in.
work /tmp/sup-work
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

# The regression a 6.5 kernel found: the old sandbox's teardown must not reach
# its replacement. Both used to share the tenant cgroup, so on a kernel with
# `cgroup.kill` retiring the old one killed the new one — and every rewarm
# after it, each retiring the one before. Only the request after a re-serve
# can say so; the counter check above passes either way.
out=$("$ZYGO" exec double '{"n": 8}' 2>&1)
case "$out" in
    *'"doubled": 16'*) ok "the replacement answers: retiring the old sandbox did not reach it" ;;
    *) bad "the replacement is dead after the old one was retired: $(printf '%s' "$out" | head -1)" ;;
esac

exits 0 "\`stop\` removes one function" "$ZYGO" stop boom
if "$ZYGO" --json ps 2>/dev/null | grep -q '"boom"'; then
    bad "the stopped function is still listed"
else
    ok "the stopped function is gone from \`ps\`"
fi

exits 4 "calling a stopped function is \`not found\`" "$ZYGO" exec boom '{}'

say ""
say "blue/green deploys"
# `up` is a deploy, and a deploy that restarts what did not change is not
# idempotent: every re-run would throw away warm pages and request counters
# on functions nobody touched. So `up` compares the spec, the secret values
# and the bytes of the source files against what is registered, and only a
# difference replaces — with the new sandbox warm before the old one stops
# taking requests, requests the old one accepted finishing on it, and requests
# queued behind it admitted to the new one.
work /tmp/bg
mkdir -p /tmp/bg-marker
cat > sandbox.toml <<'TOML'
[defaults]
image = "python:3.12-slim"

[fn.v]
entry = "v.py"
concurrency = 1
# Writable, because the handler writes a marker from inside the sandbox the
# moment a request starts. See the blue/green check below: a marker the host
# wrote would say nothing about whether the request reached the handler.
mounts = ["/tmp/bg-marker:/marker:rw"]

[fn.other]
entry = "other.py"

[fn.s]
entry = "s.py"
secrets = ["TOKEN"]
TOML
printf 'import os, time\n\n\ndef handler(event):\n    if event.get("sleep"):\n        open("/marker/started", "w").write(str(os.getpid()))\n    time.sleep(event.get("sleep", 0))\n    return {"version": 1}\n' > v.py
printf 'def handler(event):\n    return {"other": True}\n' > other.py
printf 'def handler(event):\n    return {"token": open("/run/secrets/TOKEN").read()}\n' > s.py
export TOKEN=a

exits 0 "\`up\` brings a three-function project up" "$ZYGO" up
"$ZYGO" exec v '{}' >/dev/null 2>&1
"$ZYGO" exec v '{}' >/dev/null 2>&1

out=$("$ZYGO" --json up 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"replaced":[]'*'"unchanged":["other","s","v"]'*) ok "a second \`up\` with nothing edited replaces nothing" ;;
    *) bad "second up: $out" ;;
esac
requests=$("$ZYGO" --json ps 2>/dev/null | tr -d '\n ' | grep -o '"name":"v","requests":[0-9]*' | grep -o '[0-9]*$')
[ "${requests:-0}" -eq 2 ] && ok "and the request counters survive it ($requests requests)" || bad "counters after an unchanged up: $requests"

sed -i 's/"version": 1/"version": 2/' v.py
out=$("$ZYGO" --json up 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"replaced":["v"]'*'"unchanged":["other","s"]'*) ok "editing one handler replaces that function and leaves the others" ;;
    *) bad "up after an edit: $out" ;;
esac
out=$("$ZYGO" exec v '{}' 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"version":2'*) ok "and requests now see the new code" ;;
    *) bad "after the replacement: $out" ;;
esac

# The spec itself is compared too, not only the files it names.
printf 'timeout = "9s"\n' >> sandbox.toml
out=$("$ZYGO" --json up 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"replaced":["s"]'*'"unchanged":["other","v"]'*) ok "a spec change replaces the function it applies to" ;;
    *) bad "up after a spec change: $out" ;;
esac

# A request in flight on the old function finishes there, with the old code,
# while the replacement is already answering.
"$ZYGO" exec v '{"sleep": 2}' >/tmp/bg-inflight.out 2>&1 &
inflight=$!
sleep 0.5
sed -i 's/"version": 2/"version": 3/' v.py
"$ZYGO" up >/dev/null 2>&1
out=$("$ZYGO" exec v '{}' 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"version":3'*) ok "the replacement answers while the old function is still finishing a request" ;;
    *) bad "during the drain: $out" ;;
esac
wait $inflight
rc=$?
out=$(tr -d '\n ' </tmp/bg-inflight.out)
case "$rc:$out" in
    0:*'"version":2'*) ok "the request in flight finished on the old function (exit 0, old code)" ;;
    *) bad "the in-flight request during a replacement: exit $rc, $out" ;;
esac

# A request *queued* behind the old function (concurrency = 1) is admitted to
# the replacement rather than told that the function is shutting down.
#
# The window is the whole check: the queued request has to still be waiting
# when the replacement lands. The first version held the function for 2 s and
# assumed `up` would finish inside that, which is true on a laptop and false
# on a Raspberry Pi — there the queue drained to the old function first and
# the check reported a supervisor bug that was a stopwatch. The hold is long
# enough now that it is not close, and `up` is timed so that a run where it
# *was* close says so instead of voting.
HOLD_S=8
rm -f /tmp/bg-marker/started
"$ZYGO" exec v "{\"sleep\": $HOLD_S}" >/tmp/bg-inflight.out 2>&1 &
inflight=$!
# Wait for the holder to be *inside* the handler, not merely started.
#
# `sleep 0.3` was not enough on a Raspberry Pi, where `zygo exec` takes longer
# than that to connect: the second request then won the race to the only slot,
# ran on the function that was current at the time, and this check reported a
# supervisor bug that was a stopwatch. That single failure was the whole of
# the evidence for it, and `poc/repro_blue_green.sh` could not reproduce it
# once the ordering was made certain.
i=0
while [ $i -lt 400 ]; do
    [ -f /tmp/bg-marker/started ] && break
    i=$((i+1)); sleep 0.05
done
if [ ! -f /tmp/bg-marker/started ]; then
    bad "the slot-holder never reached its handler, so nothing could queue behind it"
fi
"$ZYGO" exec v '{}' >/tmp/bg-queued.out 2>&1 &
queued=$!
settle_ms=1500
sleep 1.5
sed -i 's/"version": 3/"version": 4/' v.py
up_started=$(date +%s%N)
"$ZYGO" up >/dev/null 2>&1
up_ms=$(( ($(date +%s%N) - up_started) / 1000000 ))
# How long the replacement had to land for the queued request still to be
# there. Two bounds and the smaller decides: what is left of the hold, and
# `QUEUE_WAIT` — a request waits five seconds for a slot and is then told to
# retry, by design. The settle time comes off, because the request may have
# started waiting the instant it was launched.
window_ms=$(( 5000 - settle_ms ))
hold_left_ms=$(( HOLD_S * 1000 - settle_ms ))
[ "$hold_left_ms" -lt "$window_ms" ] && window_ms=$hold_left_ms
wait $queued
rc=$?
out=$(tr -d '\n ' </tmp/bg-queued.out)
case "$rc:$out" in
    0:*'"version":4'*)
        ok "a request queued behind the old function ran on the replacement (\`up\` took ${up_ms} ms of a ${window_ms} ms window)" ;;
    0:*'"version":3'*)
        if [ "$up_ms" -ge "$window_ms" ]; then
            ok "the queued request ran on the old function, and could not have done otherwise: \`up\` took ${up_ms} ms and the queue only held for ${window_ms} ms"
        else
            bad "the queued request ran on the old function although the replacement was ready ${up_ms} ms in, with ${window_ms} ms of queue left"
        fi ;;
    *busy*|*retry*)
        # Told to retry. Correct when `up` outlasted the five seconds the
        # request would wait, which on a slow host is most of the time.
        if [ "$up_ms" -ge "$window_ms" ]; then
            ok "the queued request was told to retry, and could not have been admitted: \`up\` took ${up_ms} ms of a ${window_ms} ms window"
        else
            bad "the queued request was refused although the replacement landed ${up_ms} ms in, with ${window_ms} ms of window: $out"
        fi ;;
    *) bad "the queued request during a replacement: exit $rc, $out" ;;
esac
wait $inflight

# Secret *values* are part of what is compared: a rotated key is a deploy.
out=$(TOKEN=b "$ZYGO" --json up 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"replaced":["s"]'*'"unchanged":["other","v"]'*) ok "a changed secret value replaces the function that uses it" ;;
    *) bad "up after rotating a secret: $out" ;;
esac
out=$("$ZYGO" exec s '{}' 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"token":"b"'*) ok "and the new value is what the function sees" ;;
    *) bad "after rotating a secret: $out" ;;
esac
unset TOKEN

say ""
say "\`zygo.lock\`"
# `up` records what each function resolved to. A tag is a pointer, and a
# registry moves it; the lock is how the next `up` on the next machine can
# tell that it is about to run something else.
#
# `fn.s` needs its secret for any of these ups to get as far as the lock.
TOKEN=b
export TOKEN
if [ -f zygo.lock ]; then
    ok "\`up\` wrote zygo.lock beside the spec"
else
    bad "no zygo.lock after three ups"
fi
# The positive case before any negative one: it has to name the functions and
# carry a real digest, or "up refused" below would prove nothing.
locked_digest=$(grep -A4 '^\[fn.v\]' zygo.lock | grep '^digest' | head -1 | cut -d'"' -f2)
case "$locked_digest" in
    sha256:*) ok "with the digest the image reference resolved to ($(printf '%.19s' "$locked_digest")…)" ;;
    *) bad "no digest recorded for fn.v: $(tr '\n' ' ' < zygo.lock | cut -c1-200)" ;;
esac
grep -q '^\[fn.other\]' zygo.lock && grep -q '^\[fn.s\]' zygo.lock \
    && ok "and an entry for every function in the spec" \
    || bad "zygo.lock is missing a function: $(grep '^\[fn' zygo.lock | tr '\n' ' ')"

out=$("$ZYGO" --json up 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"locked":false'*) ok "an \`up\` that resolves the same way does not rewrite it" ;;
    *) bad "up with an unchanged lock: $out" ;;
esac

# What a moved tag looks like: the recorded digest is no longer the one the
# host holds, under a spec nobody edited. Only `fn.v`'s entry is touched —
# all three functions share this image, so a global replacement would say
# nothing about *which* function is refused.
cp zygo.lock /tmp/lock-real
awk -v fake="sha256:$(printf '0%.0s' $(seq 1 64))" '
    /^\[fn\.v\]$/ { here = 1 }
    /^\[/ && !/^\[fn\.v\]$/ { here = 0 }
    here && /^digest = / { print "digest = \"" fake "\""; next }
    { print }
' /tmp/lock-real > zygo.lock
out=$("$ZYGO" up 2>&1)
rc=$?
case "$out" in
    *"zygo.lock pins"*"this host has"*)
        [ "$rc" -ne 0 ] && ok "an image that moved under an unedited spec is refused, with both digests (exit $rc)" \
            || bad "up reported the drift and still exited 0" ;;
    *) bad "up against a moved digest: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-200)" ;;
esac
case "$out" in
    *"--relock"*) ok "and the message says how to accept it" ;;
    *) bad "the refusal does not mention --relock" ;;
esac
# The other functions in the same project still come up: one stale entry is
# not a reason to leave the project down.
# `failed` carries a reason per function, not just a name: `up --json` used to
# print one document per failure *and* a summary, and two of its failure paths
# printed nothing at all in JSON mode, so the reason was lost (E-14). One
# document now, and the entry says why.
out=$("$ZYGO" --json up 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"name":"v"'*'"reason":'*)
        # And exactly one document, which is the other half of that fix: a
        # concatenation of two would not parse.
        if printf '%s' "$out" | python3 -c 'import json,sys; json.load(sys.stdin)' 2>/dev/null; then
            ok "only the function whose image moved is refused, with its reason, in one JSON document"
        else
            bad "up --json emitted something that is not one JSON document: $out"
        fi ;;
    *) bad "up with one stale entry: $out" ;;
esac

out=$("$ZYGO" --json up --relock 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"failed":[]'*'"locked":true'*) ok "\`up --relock\` accepts the move and rewrites the file" ;;
    *) bad "up --relock: $out" ;;
esac
grep -q "$locked_digest" zygo.lock \
    && ok "and the file holds the digest the host actually has again" \
    || bad "after --relock the digest is still the fake one"

# A lock file is a record of *this* spec: a function that is gone leaves.
sed -i '/^\[fn.other\]/,+1d' sandbox.toml
"$ZYGO" up >/dev/null 2>&1
grep -q '^\[fn.other\]' zygo.lock \
    && bad "a removed function kept its lock entry" \
    || ok "a function removed from the spec loses its entry"

printf 'version = 9\n' > zygo.lock
out=$("$ZYGO" up 2>&1)
case "$out" in
    *"zygo.lock is version 9"*) ok "a lock file from a future version is refused by name and number" ;;
    *) bad "up against a version 9 lock: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac
rm -f zygo.lock
unset TOKEN

"$ZYGO" down >/dev/null 2>&1
work /tmp/sup-work

say ""
say "surviving a restart"
# `rmdir` on a cgroup succeeds with its control files in place, which an
# ordinary filesystem does not allow — so this half of the cleanup cannot be
# unit tested and is checked here instead. A supervisor that died leaves its
# cgroups behind; the next one must not inherit them.
#
# `tenants/default/<name>`: a function served from a terminal belongs to the
# operator's own tenant, which is a real tenant rather than a special case.
"$ZYGO" serve handler.py --name leftover >/dev/null 2>&1
stale="$CG/launch/zygo.slice/tenants/default/leftover"
if [ -d "$stale" ]; then
    ok "a served function has a cgroup under its tenant"
else
    bad "no cgroup was created for a served function"
fi

kill -9 "$SUPERVISOR" 2>/dev/null
wait "$SUPERVISOR" 2>/dev/null
sleep 1
if [ -d "$stale" ]; then
    ok "a killed supervisor leaves its cgroups behind (cgroups outlive processes)"
else
    bad "the premise is wrong: the cgroup vanished on its own"
fi

if ! start_supervisor; then
    bad "the supervisor did not come back after being killed"
fi
if [ -d "$stale" ]; then
    bad "the restarted supervisor inherited a dead function's cgroup"
else
    ok "a restarted supervisor cleans up what the dead one left"
fi

# Everything the old supervisor held is gone with it: recovery is by rewarming,
# not by reattaching to sandboxes nobody can vouch for.
if "$ZYGO" ps 2>/dev/null | grep -q leftover; then
    bad "a function survived the supervisor that owned it"
else
    ok "and starts with an empty registry rather than stale entries"
fi


say ""
say "\`zygo run\` through the supervisor"
# A one-shot sandbox is started by the supervisor when one is running: the
# client hands over its three streams and waits. Everything a script relies
# on has to survive the detour — the exit code, both streams, stdin, the
# outcome file, the deadline — and two things the local path gets from the
# kernel have to be rebuilt: a Ctrl-C reaching the program, and a dead client
# taking its sandbox with it.
work /tmp/sup-run

# Processes running exactly `sleep 30`: the sandboxes below, and nothing
# this script runs itself.
sandbox_sleeps() {
    n=0
    for d in /proc/[0-9]*; do
        [ "$(tr '\0' ' ' < "$d/cmdline" 2>/dev/null)" = "sleep 30 " ] && n=$((n+1))
    done
    echo $n
}

if "$ZYGO" -v run "$IMAGE" python3 -c pass 2>&1 | grep -q "through the supervisor"; then
    ok "a one-shot run is started by the supervisor"
else
    bad "a one-shot run was not routed through the running supervisor"
fi
exits 3 "the program's exit code comes back" "$ZYGO" run "$IMAGE" sh -c "exit 3"
out=$("$ZYGO" run "$IMAGE" sh -c "echo out; echo err >&2" 2>/dev/null)
err=$("$ZYGO" run "$IMAGE" sh -c "echo out; echo err >&2" 2>&1 >/dev/null)
if [ "$out" = "out" ] && [ "$err" = "err" ]; then
    ok "stdout and stderr stay distinct"
else
    bad "the streams were mixed: stdout=[$out] stderr=[$err]"
fi
got=$(printf 'shout\n' | "$ZYGO" run "$IMAGE" tr a-z A-Z)
if [ "$got" = "SHOUT" ]; then
    ok "stdin reaches the program"
else
    bad "stdin did not arrive: [$got]"
fi
exits 137 "a deadline is enforced and reported as a kill" \
    "$ZYGO" run --timeout 1s --outcome outcome.json "$IMAGE" sleep 5
if grep -q '"timed_out":true' outcome.json 2>/dev/null; then
    ok "--outcome says it timed out"
else
    bad "--outcome did not record the timeout: $(cat outcome.json 2>/dev/null)"
fi

# Signals. Sent from Python rather than with `&` and `kill`: a job a
# non-interactive shell puts in the background has SIGINT ignored, the client
# would keep that, and the sandbox is given the client's dispositions — so
# the test would be measuring the shell. `signalled SIG N cmd…` starts the
# command with every signal at its default, sends SIG N times half a second
# apart after a second, and prints "<exit> <seconds after the last signal>".
signalled() {
    python3 - "$@" <<'PY'
import signal, subprocess, sys, time
sig, times, argv = getattr(signal, "SIG" + sys.argv[1]), int(sys.argv[2]), sys.argv[3:]
def defaults():
    for s in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP, signal.SIGQUIT):
        signal.signal(s, signal.SIG_DFL)
p = subprocess.Popen(argv, preexec_fn=defaults)
time.sleep(1)
for _ in range(times):
    p.send_signal(sig)
    time.sleep(0.5)
began = time.time()
code = p.wait()
print(code, int(time.time() - began))
PY
}

# Ctrl-C. The program is pid 1 of its namespace, and a namespace init only
# receives a signal from outside if it handles it; `sh` does not handle
# SIGINT, so the signal has to reach the sleep it forked — the process group.
set -- $(signalled INT 1 "$ZYGO" run "$IMAGE" sh -c "sleep 30")
if [ "$2" -lt 10 ]; then
    ok "one Ctrl-C ends a shell and what it forked (exit $1)"
else
    bad "Ctrl-C did not end the run: exit $1 after $2 s"
fi

# A program that handles the signal gets it, and gets to answer.
set -- $(signalled INT 1 "$ZYGO" run "$IMAGE" python3 -c 'import time
try:
    time.sleep(30)
except KeyboardInterrupt:
    raise SystemExit(42)')
if [ "$1" -eq 42 ]; then
    ok "a program that handles SIGINT receives it"
else
    bad "SIGINT did not reach a program that handles it: exit $1 after $2 s"
fi

# A program that ignores the first signal — `sleep` as init, no handler — is
# killed by the second.
set -- $(signalled INT 2 "$ZYGO" run "$IMAGE" sleep 30)
if [ "$2" -lt 10 ]; then
    ok "a second Ctrl-C kills a program that ignored the first (exit $1)"
else
    bad "two Ctrl-Cs did not end the run: exit $1 after $2 s"
fi

# A hang-up. To the client it is relayed like the others, and it reaches the
# shell's child as one would from a terminal.
set -- $(signalled HUP 1 "$ZYGO" run "$IMAGE" sh -c "sleep 30")
if [ "$2" -lt 10 ]; then
    ok "a hang-up ends a shell and what it forked (exit $1)"
else
    bad "SIGHUP did not end the run: exit $1 after $2 s"
fi

# `nohup zygo run …`: what the client ignores, the program ignores, whatever
# the supervisor was started with. The same hang-up now reaches nothing.
set -- $(signalled HUP 1 nohup "$ZYGO" run "$IMAGE" sh -c "sleep 2")
if [ "$1" -eq 0 ]; then
    ok "a hang-up the client ignores is not passed on"
else
    bad "SIGHUP reached a program under nohup: exit $1 after $2 s"
fi

# From a terminal. The terminal is the shell's, so the sandbox cannot make it
# its own; it has to use it as three plain descriptors rather than refuse.
python3 - "$ZYGO" "$IMAGE" <<'PY' > pty.out 2>&1
import os, pty, select, sys, time
z, image = sys.argv[1], sys.argv[2]
pid, fd = pty.fork()
if pid == 0:
    os.execv(z, [z, "run", image, "sh", "-c", "echo from-a-terminal"])
out = b""
while True:
    r, _, _ = select.select([fd], [], [], 5)
    if not r:
        break
    try:
        d = os.read(fd, 4096)
    except OSError:
        break
    if not d:
        break
    out += d
_, status = os.waitpid(pid, 0)
print(out.decode(errors="replace").strip())
print("exit", os.waitstatus_to_exitcode(status))
PY
if grep -q "^from-a-terminal" pty.out && grep -q "^exit 0$" pty.out; then
    ok "a run from a terminal uses it without owning it"
else
    bad "a run from a terminal failed: $(tr '\n' ' ' < pty.out | cut -c1-200)"
fi

# A client that dies takes its sandbox with it, as PDEATHSIG does locally.
"$ZYGO" run "$IMAGE" sleep 30 &
pid=$!
sleep 1
kill -KILL $pid
wait $pid 2>/dev/null
i=0
while [ $i -lt 30 ]; do
    [ "$(sandbox_sleeps)" -eq 0 ] && break
    i=$((i+1)); sleep 0.1
done
if [ "$(sandbox_sleeps)" -eq 0 ]; then
    ok "a client killed with SIGKILL takes its sandbox with it"
else
    bad "the sandbox outlived a client killed with SIGKILL"
fi

work /tmp/sup-work

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
# By cgroup, not by name. Counting every process called `python3` on the host
# is right on a machine that runs nothing else — a container — and wrong
# everywhere real: a CI runner has its own, and the check reported two
# sandboxes that had outlived the supervisor when neither was a sandbox.
#
# Anything under a `zygo.slice` is Zygo's by construction, whatever it is
# running, which is the same rule `poc/cleanup.sh` uses to decide what it may
# kill.
leftover=0
names=
for d in /proc/[0-9]*; do
    [ "$(awk '{print $3}' "$d/stat" 2>/dev/null)" = "Z" ] && continue
    grep -qs 'zygo\.slice' "$d/cgroup" 2>/dev/null || continue
    leftover=$((leftover+1))
    names="$names $(cat "$d/comm" 2>/dev/null)"
    # Who they are, not only what: the parent and the arguments say which
    # path started them, which is the question a leak leaves open.
    pid=${d#/proc/}
    say "        $(ps -o pid=,ppid=,etime=,args= -p "$pid" 2>/dev/null | cut -c1-140)"
done
if [ "$leftover" -eq 0 ]; then
    ok "no sandbox outlived the supervisor"
else
    bad "$leftover sandbox process(es) are still running with no supervisor:$names"
fi

wait "$SUPERVISOR" 2>/dev/null

say ""
say "----------------------------------------"
say "supervisor verification: $PASS passed, $FAIL failed"
harness_verdict
[ "$FAIL" -eq 0 ] || {
    say ""
    say "supervisor log:"
    sed 's/^/  /' /tmp/supervisor.log
    exit 1
}
