#!/bin/sh
# zcost.sh JOBDIR ZYGO  -> per-run wall and children CPU for 20 runs of zygo run directly
PY=/tmp/windmill/cache/py_runtime/cpython-3.12.13-linux-aarch64-gnu/bin/python3.12
cd "$1"
ARGS=$(env -i PATH=/usr/bin:/bin sh /tmp/argdump.sh --config run.config.proto -- $PY -u -m wrapper | tr '\n' ' ')
s=$(date +%s%N)
out=$(sh -c "for i in \$(seq 20); do $2 run $ARGS >/dev/null 2>&1; done; times" | tail -1)
e=$(date +%s%N)
echo "$2: $(( (e - s) / 20000000 )) ms wall per run; children CPU for 20 runs (user sys): $out"
