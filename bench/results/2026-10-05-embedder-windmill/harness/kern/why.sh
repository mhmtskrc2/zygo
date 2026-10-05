#!/bin/sh
# Where one `/bin/true` sandbox start goes, Zygo against kern.
Z=/opt/zygo-bench/zygo; K=/opt/zygo-bench/kern
zrun() { $Z run --quiet --mem 256M --pids 128 --net none python:3.12-slim /bin/true; }
krun() { $K box w$1 --image python:3.12-slim --pull never --rm -q --security-profile untrusted -m 256m --pids-limit 128 --net none -- /bin/true 2>/dev/null; }
for i in 1 2 3; do zrun; krun $i; done
echo "== 50 runs each: user s, sys s, wall s, max RSS of any one process (KB)"
for rt in zygo kern; do
  /usr/bin/time -f "$rt: %U user  %S sys  %e wall  maxrss %M KB" sh -c "
    for i in \$(seq 50); do
      if [ $rt = zygo ]; then $Z run --quiet --mem 256M --pids 128 --net none python:3.12-slim /bin/true
      else $K box x\$i --image python:3.12-slim --pull never --rm -q --security-profile untrusted -m 256m --pids-limit 128 --net none -- /bin/true 2>/dev/null; fi
    done"
done
echo "== one run under strace -f -c"
strace -f -c -o /tmp/z.st $Z run --quiet --mem 256M --pids 128 --net none python:3.12-slim /bin/true
strace -f -c -o /tmp/k.st $K box s1 --image python:3.12-slim --pull never --rm -q --security-profile untrusted -m 256m --pids-limit 128 --net none -- /bin/true 2>/dev/null
for f in z k; do echo "-- $f"; tail -1 /tmp/$f.st; grep -E " (clone3?|execve|mount|umount2|openat|mkdirat|write|read|mmap|munmap|landlock_[a-z_]+|pivot_root|wait4|unshare|setns|futex|newfstatat|statx)$" /tmp/$f.st | awk '{print $NF": "$4" calls, "$2" s"}'; done
echo "== processes and threads started"
strace -f -e trace=clone,clone3,execve,vfork -o /tmp/z.pr $Z run --quiet --mem 256M --pids 128 --net none python:3.12-slim /bin/true
strace -f -e trace=clone,clone3,execve,vfork -o /tmp/k.pr $K box s2 --image python:3.12-slim --pull never --rm -q --security-profile untrusted -m 256m --pids-limit 128 --net none -- /bin/true 2>/dev/null
for f in z k; do echo "-- $f: $(grep -cE 'clone3?\(' /tmp/$f.pr) clones, $(grep -c 'execve(' /tmp/$f.pr) execs: $(grep -o 'execve("[^"]*"' /tmp/$f.pr | cut -d'"' -f2 | tr '\n' ' ')"; done
ls -la $Z $K | awk '{print $5, $9}'
