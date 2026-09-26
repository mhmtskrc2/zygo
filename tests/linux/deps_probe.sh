#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# One dependency set, built for real, over the API (todo 3.3).
#
# Separate from `verify_api.sh` because it needs what nothing else there does:
# `passt` and `nftables` on the host, and a working route to the package
# registries. A dependency build reaches the network — that is what it is for —
# and it reaches it through `network = "egress"` with an allowlist, because the
# lockfile came from whoever holds a token and installing a package runs that
# package's code. A build with host networking would put that code on the Zygo
# host's own network.
#
# So this is the check that the restriction is real rather than intended: a
# build that succeeds against the registry, and a package name that does not
# exist failing with the resolver's own words in the log.
#
# Run:  make verify-deps-linux
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/tests/linux/bin/zygo-linux-musl}
IMAGE=${IMAGE:-python:3.12-slim}
NODE_IMAGE=${NODE_IMAGE:-node:22-slim}

ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-deps}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

PASS=0
FAIL=0
ok()  { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad() { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }

PYTHONPATH=$SRC/sdk/python/src
export PYTHONPATH

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

echo "dependency sets over the API"
# In the harness's cgroup, like every other long-lived process here: the API
# starts a supervisor, the supervisor builds sandboxes, and a sandbox needs a
# cgroup with delegated controllers to be built under.
sh -c '
    root=$1; socket=$2; shift 2
    if [ -d "$root/launch/zygo.slice/system" ]; then
        echo $$ > "$root/launch/zygo.slice/system/cgroup.procs" 2>/dev/null
    else
        echo $$ > "$root/launch/cgroup.procs" 2>/dev/null
    fi
    exec "$@" --listen "unix://$socket" --no-auth --allow-deploy
' _ "$ZYGO_HARNESS_ROOT" "$SOCK" "$ZYGO" api >"$WORK/api.log" 2>&1 &
API_PID=$!
i=0
while [ ! -S "$SOCK" ] && [ $i -lt 100 ]; do i=$((i+1)); sleep 0.1; done
if [ ! -S "$SOCK" ]; then
    echo "  the API never listened:"; sed 's/^/    /' "$WORK/api.log"; exit 1
fi

SOCK=$SOCK IMAGE=$IMAGE NODE_IMAGE=$NODE_IMAGE python3 "$SRC/tests/linux/deps_driver.py"
STATUS=$?

exit $STATUS
