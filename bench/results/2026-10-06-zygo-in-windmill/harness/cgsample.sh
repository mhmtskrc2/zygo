#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# In the VM, beside a load run: the CPU of every container under windmill.slice, every two
# seconds, so that a set that runs slow can be read container by container afterwards
# (server, database, each worker). Output: time, container id prefix, cumulative CPU in µs.
#   nohup sh cgsample.sh > cgsample.txt &
# To read it as per-tick deltas:  awk '{k=$2; if (k in p) printf "%s %s %6.0f ms\n", $1, k, ($3-p[k])/1000; p[k]=$3}' cgsample.txt
while sleep 2; do
  t=$(date +%T)
  for c in /sys/fs/cgroup/windmill.slice/docker-*.scope; do
    awk -v c="$(basename "$c" | cut -c8-19)" -v t="$t" '$1 == "usage_usec" { print t, c, $2 }' "$c/cpu.stat"
  done
done
