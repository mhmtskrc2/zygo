#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Inside a worker, in a kept job directory: wall and children's CPU per run for nsjail,
# the stand-in with Zygo, and Zygo called directly with the stand-in's arguments.
PY=/tmp/windmill/cache/py_runtime/cpython-3.12.13-linux-aarch64-gnu/bin/python3.12
cd "$1"
t() {
  label=$1; shift
  s=$(date +%s%N)
  out=$(sh -c "for i in \$(seq 20); do $* >/dev/null 2>&1; done; times" | tail -1)
  e=$(date +%s%N)
  echo "$label: $(( (e - s) / 20000000 )) ms wall per run; children CPU for 20 runs (user sys): $out"
}
t "nsjail           " "/usr/bin/nsjail --config run.config.proto -- $PY -u -m wrapper"
t "stand-in + zygo   " "env -i PATH=/usr/bin:/bin /usr/local/bin/nsjail --config run.config.proto -- $PY -u -m wrapper"
env -i PATH=/usr/bin:/bin sh -c "sed 's|^exec /usr/local/bin/zygo run|for a in; do :; done; printf \"%s\\\\n\"|' /usr/local/bin/nsjail > /tmp/argdump.sh"
ARGS=$(env -i PATH=/usr/bin:/bin sh /tmp/argdump.sh --config run.config.proto -- $PY -u -m wrapper | tr '\n' ' ')
t "zygo run directly " "/usr/local/bin/zygo run $ARGS"
