#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# In the Lima VM: peak anonymous memory per slice, every 20 ms. touch /tmp/cgm/reset_<g> to start a window.
mkdir -p /tmp/cgm
while true; do
  for g in bench.slice windmill.slice; do
    m=$(awk '$1=="anon"{print $2}' /sys/fs/cgroup/$g/memory.stat 2>/dev/null); m=${m:-0}
    if [ -f /tmp/cgm/reset_$g ]; then rm -f /tmp/cgm/reset_$g; echo $m > /tmp/cgm/base_$g; echo $m > /tmp/cgm/max_$g; fi
    max=$(cat /tmp/cgm/max_$g 2>/dev/null || echo 0); [ "$m" -gt "$max" ] && echo $m > /tmp/cgm/max_$g
  done
  sleep 0.02
done
