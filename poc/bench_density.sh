#!/bin/sh
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

python3 "$SRC/poc/bench_density.py" "$@"
status=$?

echo
harness_verdict
exit "$status"
