#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Before/after: cgroup work in one run, then 3 x 100 runs of each build, interleaved.
for Z in /opt/zygo-bench/zygo-before /opt/zygo-bench/zygo; do
  strace -f -o /tmp/ab.tl $Z run --quiet --mem 256M --pids 128 --net none python:3.12-slim /bin/true 2>/dev/null
  echo "$Z: probe cgroups $(grep -c 'mkdirat(.*zygo-doctor' /tmp/ab.tl), subtree_control writes $(grep -E 'openat\(.*subtree_control.*O_WRONLY' /tmp/ab.tl | wc -l), cgroup mkdirs $(grep -cE 'mkdirat\(.*/sys/fs/cgroup.*= 0' /tmp/ab.tl)"
done
rm -f /tmp/ab.tl
for r in 1 2 3; do for Z in /opt/zygo-bench/zygo-before /opt/zygo-bench/zygo; do
  /usr/bin/time -f "$Z: %U user %S sys %e wall (100 runs)" sh -c "for i in \$(seq 100); do $Z run --quiet --mem 256M --pids 128 --net none python:3.12-slim /bin/true; done"
done; done
