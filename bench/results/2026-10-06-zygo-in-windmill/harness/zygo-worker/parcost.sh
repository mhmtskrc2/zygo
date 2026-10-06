#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# parcost.sh JOBDIR N P: P loops of N runs at once, per mode — what three workers running
# jobs side by side ask of the sandbox. Wall for the whole batch, CPU of the worker cgroup.
PY=$(ls -d /tmp/windmill/cache/py_runtime/cpython-3.12*-linux-aarch64-gnu/bin/python3.12 | head -1)
cd "$1"; N=${2:-30}; P=${3:-3}
ARGS=$(env -i PATH=/usr/bin:/bin sh /tmp/argdump.sh --config run.config.proto -- $PY -u -m wrapper | tr '\n' ' ')
cpu() { awk '$1 == "usage_usec" { print $2 }' /sys/fs/cgroup/cpu.stat; }
batch() {
  label=$1; shift
  c0=$(cpu); s=$(date +%s%N)
  j=0; while [ $j -lt $P ]; do
    ( i=0; while [ $i -lt $N ]; do sh -c "$*" >/dev/null 2>&1 || echo FAIL; i=$((i + 1)); done ) > /tmp/par.$j &
    j=$((j + 1))
  done
  wait
  e=$(date +%s%N); c1=$(cpu); f=$(cat /tmp/par.* | grep -c FAIL)
  awk -v l="$label" -v s="$s" -v e="$e" -v c0="$c0" -v c1="$c1" -v n="$((N * P))" -v f="$f" \
    'BEGIN { printf "%-22s %5.1f runs/s   CPU %5.1f ms/run   failed %d/%d\n", l, n/((e-s)/1e9), (c1-c0)/(n*1e3), f, n }'
}
for r in 1 2; do
  echo "round $r, $P at once"
  batch "nsjail" "/usr/bin/nsjail --config run.config.proto -- $PY -u -m wrapper"
  batch "stand-in, then zygo" "env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin /usr/local/bin/nsjail --config run.config.proto -- $PY -u -m wrapper"
  batch "zygo run, no script" "/usr/local/bin/zygo run $ARGS"
done
