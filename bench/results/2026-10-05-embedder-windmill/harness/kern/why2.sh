#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Each start-up piece on its own, 50 runs each; then `perf` by program over 60 runs of Zygo and of kern.
Z=/opt/zygo-bench/zygo; K=/opt/zygo-bench/kern
t() { label=$1; shift; /usr/bin/time -f "$label: %U user %S sys %e wall (50 runs)" sh -c "for i in \$(seq 50); do $* >/dev/null 2>&1; done"; }
t "zygo default user     " $Z run --quiet --mem 256M --pids 128 --net none python:3.12-slim /bin/true
t "zygo --user 0         " $Z run --quiet --user 0 --mem 256M --pids 128 --net none python:3.12-slim /bin/true
t "zygo --user 0, no mem " $Z run --quiet --user 0 --net none python:3.12-slim /bin/true
t "zygo --version        " $Z --version
t "kern --version        " $K --version
t "newuidmap (bad args)  " /usr/bin/newuidmap 1
echo "== perf, 60 runs of each, samples by program"
for rt in zygo kern; do
  if [ $rt = zygo ]; then C="$Z run --quiet --mem 256M --pids 128 --net none python:3.12-slim /bin/true"
  else C="$K box p\$i --image python:3.12-slim --pull never --rm -q --security-profile untrusted -m 256m --pids-limit 128 --net none -- /bin/true"; fi
  sudo perf record -q -a -g -o /tmp/why-$rt.data -- sh -c "for i in \$(seq 60); do $C >/dev/null 2>&1; done" >/dev/null 2>&1
  echo "-- $rt"; sudo perf report -i /tmp/why-$rt.data --no-children --sort comm -g none -q 2>/dev/null | grep -v swapper | head -8
done
