#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Setup for the warm-script density benchmark; the measuring is in
# `bench_density.py`.
#
# Run:  make bench-density
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
export ZYGO
ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-density}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

zygo pull python:3.12-slim >/dev/null 2>&1

zygo_supervisor /tmp/supervisor-density.log
i=0
while [ $i -lt 100 ]; do
    "$ZYGO" supervisor status >/dev/null 2>&1 && break
    i=$((i+1)); sleep 0.1
done

# The pool mode calls over the API, because that is the path an embedder uses
# and because a `zygo exec` per call would measure process start-up rather than
# the request. One API for the whole run, on a unix socket in this container.
case " $* " in
    *" --pool "*)
        API_SOCK=/tmp/bench-density-api.sock
        rm -f "$API_SOCK"
        zygo api --listen "unix://$API_SOCK" --no-auth --allow-deploy \
            >/tmp/api-density.log 2>&1 &
        API_PID=$!
        i=0
        while [ $i -lt 100 ] && [ ! -S "$API_SOCK" ]; do
            i=$((i+1)); sleep 0.1
        done
        set -- "$@" --api "unix://$API_SOCK"
        ;;
esac

python3 "$SRC/poc/bench_density.py" "$@"
status=$?
[ -n "${API_PID:-}" ] && kill "$API_PID" 2>/dev/null

echo
harness_verdict
exit "$status"
