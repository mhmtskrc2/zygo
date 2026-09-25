#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# `zygo bench all` with everything it needs already in place.
#
# The benchmark itself is in the binary — `zygo bench all` — because the
# numbers have to be reproducible by someone who installed Zygo and never
# cloned this repository. This script is only the setup a container needs: a
# cgroup it can write, and the image already in the store, because pulling is
# a network measurement and belongs nowhere near these numbers.
#
# Run:  make bench
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-bench}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

echo "pulling the image the benchmark runs in, before anything is timed"
zygo pull python:3.12-slim >/dev/null 2>&1 || {
    echo "could not pull python:3.12-slim; the cold-start number needs it" >&2
    exit 1
}

zygo bench all "$@"
status=$?

echo
harness_verdict
exit "$status"
