#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# The embedder API's exit criterion (roadmap 2.12), run for real.
#
# A plugin host built on the HTTP API alone: no `sandbox.toml`, no file written
# on the Zygo host, no shelling out to `zygo`. If it needed one of those, the
# API was missing a route and the phase was not done.
#
# The check that it really is API-only is not a promise in a comment: this
# script starts the API from a directory with no spec file, and afterwards
# asserts that nothing appeared in the data directory but what the API itself
# stores.
#
# Run:  make verify-plugin-host
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
IMAGE=${IMAGE:-python:3.12-slim}
# Phase 3's exit criterion adds the second language: one Python plugin and one
# JavaScript plugin, through the same API and under the same customer's limits.
NODE_IMAGE=${NODE_IMAGE:-node:22-slim}

ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-plugin}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

if [ ! -x "$ZYGO" ]; then
    echo "  no binary at $ZYGO — run \`make poc/zygo-linux-musl\` first" >&2
    exit 1
fi

WORK=$(mktemp -d)
SOCK=$WORK/api.sock

cleanup() {
    [ -n "${API_PID:-}" ] && kill "$API_PID" 2>/dev/null
    "$ZYGO" stop --all >/dev/null 2>&1
    rm -rf "$WORK"
}
trap cleanup EXIT

"$ZYGO" pull "$IMAGE" >/dev/null 2>&1
"$ZYGO" pull "$NODE_IMAGE" >/dev/null 2>&1

ZYGO_API_TOKEN=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
export ZYGO_API_TOKEN
ZYGO_SECRETS_KEY=$("$ZYGO" secrets keygen 2>/dev/null)
export ZYGO_SECRETS_KEY

# Started from an empty directory on purpose: a spec file anywhere above it
# would be found and read, and the claim here is that none is needed.
EMPTY=$WORK/empty
mkdir -p "$EMPTY"
sh -c '
    root=$1; socket=$2; cwd=$3; shift 3
    cd "$cwd" || exit 1
    if [ -d "$root/launch/zygo.slice/system" ]; then
        echo $$ > "$root/launch/zygo.slice/system/cgroup.procs" 2>/dev/null
    else
        echo $$ > "$root/launch/cgroup.procs" 2>/dev/null
    fi
    exec "$@" --listen "unix://$socket" --allow-deploy
' _ "$ZYGO_HARNESS_ROOT" "$SOCK" "$EMPTY" "$ZYGO" api >"$WORK/api.log" 2>&1 &
API_PID=$!

i=0
while [ $i -lt 100 ] && [ ! -S "$SOCK" ]; do i=$((i + 1)); sleep 0.1; done
if [ ! -S "$SOCK" ]; then
    echo "  the API did not come up:" >&2
    tail -10 "$WORK/api.log" >&2
    exit 1
fi

ZYGO_SDK=$SRC/sdk/python/src ZYGO_IMAGE=$IMAGE ZYGO_NODE_IMAGE=$NODE_IMAGE \
    python3 "$SRC/examples/plugin-host/demo.py" "unix://$SOCK"
status=$?

# The claim, checked rather than asserted: the host left nothing on the Zygo
# machine but what the API's own stores hold. A `sandbox.toml`, a handler on
# disk, a working directory it reached into — any of those would show up.
echo
strays=$(find "$ZYGO_DATA_HOME" -maxdepth 1 -mindepth 1 -printf '%f\n' 2>/dev/null |
    grep -vE '^(images|cache|scripts|blobs|tenants|secrets|tokens\.json|run|tmp|agents|backends|etc)$' |
    tr '\n' ' ')
if [ -z "$strays" ]; then
    echo "  PASS  the host left nothing on the Zygo machine but the API's own stores"
else
    echo "  FAIL  unexpected in the data directory: $strays"
    status=1
fi
if [ -f "$EMPTY/sandbox.toml" ]; then
    echo "  FAIL  a sandbox.toml appeared where the API was started"
    status=1
else
    echo "  PASS  and no spec file was needed or written"
fi

harness_verdict
exit $status
