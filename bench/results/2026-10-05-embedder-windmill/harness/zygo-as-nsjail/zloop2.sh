#!/bin/sh
# zloop2.sh JOBDIR N MODE   (MODE zygo | nsjail): N runs, as Windmill starts them
PY=/tmp/windmill/cache/py_runtime/cpython-3.12.13-linux-aarch64-gnu/bin/python3.12
cd "$1"
if [ "$3" = nsjail ]; then B=/usr/bin/nsjail; else B=/usr/local/bin/nsjail; fi
s=$(date +%s%N)
out=$(sh -c "for i in \$(seq $2); do $B --config run.config.proto -- $PY -u -m wrapper >/dev/null 2>&1 || echo fail; done; times" | tail -1)
e=$(date +%s%N)
echo "$3: $(( (e - s) / ($2 * 1000000) )) ms wall per run; children CPU for $2 runs (user sys): $out"
