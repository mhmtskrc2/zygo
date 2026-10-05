#!/bin/sh
Z=/opt/zygo-bench/zygo
strace -f -b execve -tt -T -o /tmp/z.tl $Z run --quiet --mem 256M --pids 128 --net none python:3.12-slim /bin/true; echo "zygo exit $?"
f=/tmp/z.tl
echo "=== $(wc -l < $f) syscalls, pids: $(grep -oE '^[0-9]+' $f | sort -u | tr '\n' ' ')"
echo "-- cgroupfs mkdir/rmdir:"; grep -E '(mkdirat|unlinkat)\(.*/sys/fs/cgroup' $f | sed -E 's/^[0-9]+ //' | cut -c1-200
echo "-- cgroupfs writes (file: count):"; grep -E 'openat\(.*/sys/fs/cgroup.*O_WRONLY' $f | grep -oE '/sys/fs/cgroup[^"]*' | sed -E 's|.*/||' | sort | uniq -c | sort -rn
echo "-- cgroupfs reads:"; grep -E 'openat\(.*/sys/fs/cgroup.*O_RDONLY' $f | grep -oE '/sys/fs/cgroup[^"]*' | sed -E 's|.*/||' | sort | uniq -c | sort -rn | head
echo "-- mounts: $(grep -c 'mount(' $f)  landlock: $(grep -c landlock_ $f)  seccomp: $(grep -c 'seccomp(' $f)  execve: $(grep -c 'execve(' $f)"
echo "-- first and last timestamps, and the biggest gaps:"
head -1 $f | cut -c1-120; tail -1 $f | cut -c1-120
awk '{split($2,t,":"); s=t[1]*3600+t[2]*60+t[3]; if (p) { d=(s-p)*1000; if (d>0.25) printf "%.2f ms before: %s\n", d, substr($0,1,150) } p=s}' $f | sort -rn | head -12
