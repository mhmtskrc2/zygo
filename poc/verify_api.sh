#!/bin/sh
# The HTTP API end to end, driven by the client that ships with it.
#
# Three runs of the same driver against three listeners:
#
#   1. `--allow-deploy`, no auth — the whole API, working.
#   2. call-only, no auth — the same calls, refused, because "it is refused"
#      is only worth asserting beside "it works when allowed".
#   3. bearer auth with a bootstrap token — scoped tokens, which the other two
#      cannot test: with no token there is nothing to resolve a tenant from.
#
# The unix socket is the transport a local caller actually uses — no port,
# permissions the kernel enforces — and it is the one the client implements
# differently, so it is the one checked here.
#
# Run:  make verify-api-linux
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
IMAGE=${IMAGE:-python:3.12-slim}

ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-api}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

if [ ! -x "$ZYGO" ]; then
    echo "  no binary at $ZYGO — run \`make poc/zygo-linux-musl\` first" >&2
    exit 1
fi

# The client, straight from the tree. It has no dependencies, so a path is all
# it takes — which is also the claim being checked.
PYTHONPATH=$SRC/sdk/python/src
export PYTHONPATH

WORK=$(mktemp -d)
SOCK=$WORK/api.sock
SOCK_CALL_ONLY=$WORK/api-call-only.sock
SOCK_TOKENS=$WORK/api-tokens.sock

cleanup() {
    [ -n "${API_PID:-}" ] && kill "$API_PID" 2>/dev/null
    [ -n "${API_CALL_ONLY_PID:-}" ] && kill "$API_CALL_ONLY_PID" 2>/dev/null
    [ -n "${API_TOKENS_PID:-}" ] && kill "$API_TOKENS_PID" 2>/dev/null
    "$ZYGO" stop --all >/dev/null 2>&1
    rm -rf "$WORK"
}
trap cleanup EXIT

"$ZYGO" pull "$IMAGE" >/dev/null 2>&1

# Every listener runs in the harness's cgroup, like every other long-lived
# process here: the API spawns `zygo run` children and they need somewhere to
# build a slice.
#
# $1 is the socket, $2 the auth mode (`--no-auth` or empty, in which case
# ZYGO_API_TOKEN must be set), and the rest are the API's own flags.
start_api() {
    socket=$1
    auth=$2
    shift 2
    sh -c '
        root=$1; socket=$2; auth=$3; shift 3
        if [ -d "$root/launch/zygo.slice/system" ]; then
            echo $$ > "$root/launch/zygo.slice/system/cgroup.procs" 2>/dev/null
        else
            echo $$ > "$root/launch/cgroup.procs" 2>/dev/null
        fi
        # `$auth` is unquoted on purpose: empty means "bearer", which is the
        # default, and an empty quoted argument would be an unparseable flag.
        exec "$@" --listen "unix://$socket" $auth
    ' _ "$ZYGO_HARNESS_ROOT" "$socket" "$auth" "$ZYGO" api "$@" >>"$WORK/api.log" 2>&1 &
    echo $!
}

wait_for() {
    i=0
    while [ $i -lt 100 ]; do
        [ -S "$1" ] && return 0
        i=$((i + 1))
        sleep 0.1
    done
    return 1
}

API_PID=$(start_api "$SOCK" --no-auth --allow-deploy)
if ! wait_for "$SOCK"; then
    echo "  the API did not come up:" >&2
    tail -10 "$WORK/api.log" >&2
    exit 1
fi

python3 "$SRC/poc/api_driver.py" "$SOCK" "$WORK" "$IMAGE"
status=$?

kill "$API_PID" 2>/dev/null
API_PID=

API_CALL_ONLY_PID=$(start_api "$SOCK_CALL_ONLY" --no-auth)
if wait_for "$SOCK_CALL_ONLY"; then
    python3 "$SRC/poc/api_driver.py" --call-only "$SOCK_CALL_ONLY" || status=1
else
    echo "  FAIL  the call-only API did not come up" >&2
    tail -10 "$WORK/api.log" >&2
    status=1
fi
kill "$API_CALL_ONLY_PID" 2>/dev/null
API_CALL_ONLY_PID=

# The bootstrap operator token: the same variable an existing deployment
# already sets, which is what keeps one working across this change.
ZYGO_API_TOKEN=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
export ZYGO_API_TOKEN

API_TOKENS_PID=$(start_api "$SOCK_TOKENS" "" --allow-deploy)
if wait_for "$SOCK_TOKENS"; then
    python3 "$SRC/poc/api_driver.py" --tokens "$SOCK_TOKENS" "$IMAGE" || status=1
else
    echo "  FAIL  the token API did not come up" >&2
    tail -10 "$WORK/api.log" >&2
    status=1
fi

harness_verdict
exit $status
