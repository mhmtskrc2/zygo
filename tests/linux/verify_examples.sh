#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# The examples, run for real: a compiled Go program as a warm-exec function.
#
# `examples/warm-exec/go/bin/parse` has to exist — `make examples-go-linux`
# builds it in a Go container first — and the alpine image it runs in is
# pulled here. The point of the check is the one the README makes: a program
# in any language is a warm function with `cmd` and nothing else.
#
# Run:  make examples-go-linux
set -u

# Where the checkout is: `/src` inside the containers `make` starts, the
# workspace in CI. Everything below is relative to it.
SRC=${SRC:-/src}

ZYGO=${ZYGO:-$SRC/tests/linux/bin/zygo-linux-musl}
PASS=0
FAIL=0
ZYGO_DATA_HOME=/tmp/zdata-ex
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

say() { printf '%s\n' "$*"; }
ok()  { PASS=$((PASS+1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); say "  FAIL  $*"; }

say "examples verification"
say "  kernel $(uname -r)"

zygo_supervisor /tmp/supervisor.log
i=0
while [ $i -lt 100 ]; do
    "$ZYGO" supervisor status >/dev/null 2>&1 && break
    i=$((i+1)); sleep 0.1
done

say ""
say "a Go program as a warm-exec function"
if [ ! -x $SRC/examples/warm-exec/go/bin/parse ]; then
    bad "examples/warm-exec/go/bin/parse is not built; run \`make examples-go-linux\`"
else
    # The spec mounts `./bin/parse`, so it has to be run from its directory —
    # but the source tree is read-only here, so the example is copied out.
    mkdir -p /tmp/go-example && cp -r $SRC/examples/warm-exec/go/. /tmp/go-example/
    cd /tmp/go-example || exit 1
    "$ZYGO" pull alpine:3 >/dev/null 2>&1

    if "$ZYGO" up >/tmp/up-go.log 2>&1; then
        ok "\`up\` brings a Go binary up as a warm-exec function on alpine"
        up_ok=1
    else
        bad "up: $(grep -v '^$' /tmp/up-go.log | tail -3 | tr '\n' ' ' | cut -c1-200)"
        up_ok=0
    fi

    # Everything below needs the function to exist. Without this gate, "a bad
    # event fails" and "ten requests were fast" both pass against a function
    # that was never there — the failure mode that fails open.
    if [ "$up_ok" -eq 1 ]; then
        runtime=$("$ZYGO" ps 2>/dev/null | awk '$1 == "parse" { print $3 }')
        [ "$runtime" = exec ] && ok "\`ps\` shows it as exec: no agent, no runtime" || bad "runtime column: '$runtime'"

        out=$("$ZYGO" exec parse '{"text": "warm exec in go", "n": 12}' 2>&1 | tr -d '\n ')
        # Two independent checks: serde sorts object keys, so a pattern that
        # assumed an order would fail for the ninth time in this project.
        case "$out" in
            *'"words":4'*) words=1 ;;
            *) words=0 ;;
        esac
        case "$out" in
            *'"squared":144'*) squared=1 ;;
            *) squared=0 ;;
        esac
        if [ "$words" -eq 1 ] && [ "$squared" -eq 1 ]; then
            ok "the event went in on stdin and the result came back from stdout"
        else
            bad "parse: $(printf '%s' "$out" | cut -c1-200)"
        fi

        # Ten requests through the CLI, each checked for a real answer, to show
        # the cost is the program's, not a runtime's.
        started=$(date +%s%N)
        good=0
        for _ in 1 2 3 4 5 6 7 8 9 10; do
            "$ZYGO" exec parse '{"text": "x"}' 2>/dev/null | grep -q '"language": "go"' && good=$((good+1))
        done
        per=$(( ($(date +%s%N) - started) / 10000000 ))
        if [ "$good" -eq 10 ] && [ "$per" -lt 60 ]; then
            ok "ten requests through the CLI all answered, averaging ${per} ms each with client start-up included"
        else
            bad "warm-exec: $good of 10 answered, ${per} ms per request"
        fi

        # Valid JSON that the *program* rejects — a string where it wants an
        # object — so the failure is the program's own, not the CLI's parser.
        out=$("$ZYGO" exec parse '"just a string"' 2>&1)
        rc=$?
        case "$rc:$out" in
            0:*) bad "an event the program rejects succeeded: $out" ;;
            *"cannot unmarshal"*) ok "an event the program rejects is a failed request carrying its own stderr (exit $rc)" ;;
            *) bad "the program's stderr did not come back: exit $rc, $(printf '%s' "$out" | head -2 | tr '\n' ' ')" ;;
        esac
    else
        bad "the exec checks were not run: the function never came up"
    fi
    "$ZYGO" down >/dev/null 2>&1
fi

say ""
say "a shell script in a warm-exec pool"
# The other warm-exec shape: `cmd` and no agent, with each
# request's script written into the sandbox and named on the command line.
# The point of the check is that a pool needs no agent at all — `sh` has no
# protocol and nothing to warm.
mkdir -p /tmp/sh-example && cp -r "$SRC/examples/warm-exec/shell/." /tmp/sh-example/
cd /tmp/sh-example || exit 1
if "$ZYGO" serve --runtime sh >/tmp/serve-sh.log 2>&1; then
    ok "\`serve --runtime\` brings up a pool whose agent is /bin/sh"
    out=$("$ZYGO" exec --runtime sh --script wordcount.sh '{"text": "warm exec in sh"}' 2>&1 | tr -d '\n ')
    case "$out" in
        *'"words":4'*) ok "a script ran with the event on stdin and answered on stdout" ;;
        *) bad "sh pool: $(printf '%s' "$out" | cut -c1-200)" ;;
    esac

    # A second script in the same pool, to show the pool holds none of the
    # first: this is the claim that makes one sandbox serve many tenants.
    cat > /tmp/sh-example/second.sh <<'SECOND'
read -r _
echo '{"second":true}'
SECOND
    out=$("$ZYGO" exec --runtime sh --script /tmp/sh-example/second.sh '{}' 2>&1 | tr -d '\n ')
    case "$out" in
        *'"second":true'*) ok "and a different script runs in the same warm sandbox" ;;
        *) bad "second script: $(printf '%s' "$out" | cut -c1-200)" ;;
    esac

    # The program's own failure, which is the whole error-handling story when
    # there is no protocol to report one.
    out=$("$ZYGO" exec --runtime sh --script wordcount.sh '{"nothing": 1}' 2>&1)
    case "$out" in
        *'needs a "text" field'*) ok "a script that exits non-zero fails the request, carrying its stderr" ;;
        *) bad "the script's stderr did not come back: $(printf '%s' "$out" | head -2 | tr '\n' ' ')" ;;
    esac
    "$ZYGO" stop sh >/dev/null 2>&1
else
    bad "serve --runtime sh: $(grep -v '^$' /tmp/serve-sh.log | tail -3 | tr '\n' ' ' | cut -c1-200)"
fi

say ""
say "the example specs"
# Every example ships a `sandbox.toml`; each has to resolve, or the README
# beside it is describing something that does not run.
for example in web-api webhook agent-tool warm-exec/go warm-exec/shell workflow-engine; do
    if "$ZYGO" spec -f "$SRC/examples/$example/sandbox.toml" validate >/dev/null 2>&1; then
        ok "examples/$example/sandbox.toml validates"
    else
        bad "examples/$example/sandbox.toml: $("$ZYGO" spec -f "$SRC/examples/$example/sandbox.toml" validate 2>&1 | head -2 | tr '\n' ' ')"
    fi
done
# The agent tool is a Python function: bring it up and call it, including the
# two inputs its README promises are refused.
mkdir -p /tmp/tool-example && cp -r "$SRC/examples/agent-tool/." /tmp/tool-example/
cd /tmp/tool-example || exit 1
"$ZYGO" pull python:3.12-slim >/dev/null 2>&1
if "$ZYGO" up >/tmp/up-tool.log 2>&1; then
    ok "the agent-tool example comes up under seccomp = \"strict\""
    out=$("$ZYGO" exec calc '{"expression": "2 ** 10 / 4"}' 2>&1 | tr -d '\n ')
    case "$out" in
        *'"result":256'*) ok "it evaluates an expression" ;;
        *) bad "calc: $(printf '%s' "$out" | cut -c1-160)" ;;
    esac
    out=$("$ZYGO" exec calc '{"expression": "__import__(\"os\").system(\"id\")"}' 2>&1 | tr -d '\n ')
    case "$out" in
        *'"error":"ValueError'*) ok "a call is refused by the parser, as the README says" ;;
        *) bad "injection attempt: $(printf '%s' "$out" | cut -c1-160)" ;;
    esac
    "$ZYGO" exec calc '{"expression": "9 ** 9 ** 9"}' >/tmp/calc-big.out 2>&1
    rc=$?
    [ "$rc" -eq 137 ] && ok "an expression that never finishes is killed at the 2 s deadline (exit 137)" \
        || bad "the runaway expression exited $rc: $(head -c 160 /tmp/calc-big.out | tr '\n' ' ')"
    "$ZYGO" down >/dev/null 2>&1
else
    bad "agent-tool up: $(grep -v '^$' /tmp/up-tool.log | tail -3 | tr '\n' ' ' | cut -c1-200)"
fi


# The workflow-engine worker, end to end: `zygo api --allow-deploy`, a queue
# of jobs against two scripts in two languages, and one fork per run. This is
# the only example whose functions do not exist until it runs, and the only
# one that exercises the Node agent through the supervisor rather than
# through `zygo agent test`.
say ""
say "a workflow engine's worker"
mkdir -p /tmp/wf-example && cp -r "$SRC/examples/workflow-engine/." /tmp/wf-example/
mkdir -p /tmp/wf-sdk && cp -r "$SRC/sdk/python/src/." /tmp/wf-sdk/
cd /tmp/wf-example || exit 1
rm -f jobs.db
"$ZYGO" pull python:3.12-slim >/dev/null 2>&1
"$ZYGO" pull node:22-slim >/dev/null 2>&1
# Through the harness wrapper, like every other long-lived process here: an
# API started straight from the shell lands in a cgroup with no delegated
# controllers, and every `PUT /fn/<name>` then fails with "the ns backend is
# not available on this host".
zygo api --allow-deploy >/tmp/wf-api.log 2>&1 &
wf_api=$!
i=0
while [ $i -lt 150 ]; do
    python3 -c "
import sys, urllib.request
try:
    urllib.request.urlopen('http://127.0.0.1:7700/health', timeout=1).read()
except Exception:
    sys.exit(1)
" >/dev/null 2>&1 && break
    i=$((i+1)); sleep 0.1
done
if ! kill -0 "$wf_api" 2>/dev/null; then
    bad "zygo api did not start: $(tail -3 /tmp/wf-api.log | tr '\n' ' ' | cut -c1-200)"
else
    if PYTHONPATH=/tmp/wf-sdk python3 worker.py --seed 10 >/tmp/wf-worker.out 2>/tmp/wf-worker.err; then
        ok "20 jobs across a Python and a Node script all completed"
    else
        bad "the worker: $(tail -4 /tmp/wf-worker.err | tr '\n' ' ' | cut -c1-240)"
    fi
    case "$(cat /tmp/wf-worker.out)" in
        *'"completed": 20'*) ok "every job reached the completed state" ;;
        *) bad "job states: $(cat /tmp/wf-worker.out | cut -c1-160)" ;;
    esac
    # Both languages really ran: the Python script normalises and the Node
    # one summarises, and each has a field only it produces.
    if grep -q '"rate"' /tmp/wf-worker.err 2>/dev/null || \
       python3 -c "
import sqlite3, sys
rows = dict(sqlite3.connect('jobs.db').execute(
    'SELECT script, count(*) FROM jobs WHERE state = \"completed\" '
    'AND result LIKE \"%rate%\" OR result LIKE \"%skus%\" GROUP BY script'))
sys.exit(0 if len(rows) == 2 else 1)
" 2>/dev/null; then
        ok "both the Python and the Node script produced their own results"
    else
        bad "one of the two scripts produced nothing"
    fi
    kill "$wf_api" 2>/dev/null
    wait "$wf_api" 2>/dev/null
fi
cd /tmp || exit 1

"$ZYGO" stop --all >/dev/null 2>&1
say ""
say "----------------------------------------"
say "examples verification: $PASS passed, $FAIL failed"
harness_verdict
[ "$FAIL" -eq 0 ]
