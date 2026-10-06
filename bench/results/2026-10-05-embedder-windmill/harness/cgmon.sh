#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Peak anonymous memory per stack (memory.stat "anon": what processes hold, not page
# cache), sampled every 20 ms. `touch /tmp/reset_<g>` starts a window.
while true; do
  for g in zygo windmill; do
    m=$(awk '$1=="anon"{print $2}' /cg/$g/memory.stat 2>/dev/null); m=${m:-0}
    if [ -f /tmp/reset_$g ]; then rm -f /tmp/reset_$g; echo $m > /tmp/base_$g; echo $m > /tmp/max_$g; fi
    max=$(cat /tmp/max_$g 2>/dev/null || echo 0); [ "$m" -gt "$max" ] && echo $m > /tmp/max_$g
  done
  sleep 0.02
done
