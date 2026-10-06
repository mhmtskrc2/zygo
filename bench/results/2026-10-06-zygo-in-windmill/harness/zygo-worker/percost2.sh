#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# percost2.sh JOBDIR N ROUNDS: the same job N times per mode, modes interleaved for ROUNDS
# rounds, wall per run and the CPU of the worker's whole cgroup per run (everything the
# run cost, children or not). Run with the worker otherwise idle.
PY=$(ls -d /tmp/windmill/cache/py_runtime/cpython-3.12*-linux-aarch64-gnu/bin/python3.12 | head -1)
cd "$1"; N=${2:-50}; R=${3:-3}
env -i PATH=/usr/bin:/bin sh -c "sed 's|^exec /usr/local/bin/zygo run|for a in; do :; done; printf \"%s\\\\n\"|' /usr/local/bin/nsjail > /tmp/argdump.sh"
ARGS=$(env -i PATH=/usr/bin:/bin sh /tmp/argdump.sh --config run.config.proto -- $PY -u -m wrapper | tr '\n' ' ')
cpu() { awk '$1 == "usage_usec" { print $2 }' /sys/fs/cgroup/cpu.stat; }
one() {
  label=$1; shift
  c0=$(cpu); s=$(date +%s%N)
  i=0; f=0
  while [ $i -lt $N ]; do sh -c "$*" >/dev/null 2>&1 || f=$((f + 1)); i=$((i + 1)); done
  e=$(date +%s%N); c1=$(cpu)
  awk -v l="$label" -v s="$s" -v e="$e" -v c0="$c0" -v c1="$c1" -v n="$N" -v f="$f" \
    'BEGIN { printf "%-22s wall %5.1f ms/run   CPU %5.1f ms/run   failed %d/%d\n", l, (e-s)/(n*1e6), (c1-c0)/(n*1e3), f, n }'
}
r=1
while [ $r -le $R ]; do
  echo "round $r"
  one "nsjail" "/usr/bin/nsjail --config run.config.proto -- $PY -u -m wrapper"
  one "stand-in, then zygo" "env -i PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin /usr/local/bin/nsjail --config run.config.proto -- $PY -u -m wrapper"
  one "zygo run, no script" "/usr/local/bin/zygo run $ARGS"
  r=$((r + 1))
done
