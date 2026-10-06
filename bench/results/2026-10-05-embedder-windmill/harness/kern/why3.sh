#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# One run of each under strace: syscalls, files written outside /proc and /sys, cgroup writes, mounts, processes.
Z=/opt/zygo-bench/zygo; K=/opt/zygo-bench/kern
strace -f -tt -T -o /tmp/z.tl $Z run --quiet --user 0 --mem 256M --pids 128 --net none python:3.12-slim /bin/true; echo "zygo exit $?"
strace -f -tt -T -o /tmp/k.tl $K box t1 --image python:3.12-slim --pull never --rm -q --security-profile untrusted -m 256m --pids-limit 128 --net none -- /bin/true; echo "kern exit $?"
for f in z k; do
  echo "=== $f: $(wc -l < /tmp/$f.tl) syscalls, $(grep -c 'clone' /tmp/$f.tl) clone lines"
  echo "-- file-creating or writing syscalls outside /proc and /sys/fs/cgroup:"
  grep -E '(openat|mkdirat|renameat2?|unlinkat|symlinkat|linkat)\(' /tmp/$f.tl | grep -E 'O_CREAT|mkdirat|rename|unlink|symlink|link' | grep -vE '"/(proc|sys)/' | sed -E 's/^[0-9]+ [0-9:.]+ //' | cut -c1-150 | head -40
  echo "-- fsync/fdatasync/sync: $(grep -cE 'f?(data)?sync\(' /tmp/$f.tl)"
  echo "-- cgroup writes: $(grep -E 'openat\(.*/sys/fs/cgroup' /tmp/$f.tl | grep -c O_WRONLY), mkdir in cgroupfs: $(grep -E 'mkdirat\(.*/sys/fs/cgroup' /tmp/$f.tl | wc -l)"
  echo "-- mounts: $(grep -c ' mount(' /tmp/$f.tl), umounts: $(grep -c 'umount2(' /tmp/$f.tl), threads/processes: $(grep -oE '^[0-9]+' /tmp/$f.tl | sort -u | wc -l)"
done
