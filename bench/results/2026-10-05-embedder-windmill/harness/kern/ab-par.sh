#!/bin/sh
busy() { awk '/^cpu /{print $2+$3+$4+$7+$8}' /proc/stat; }
for r in 1 2 3; do for Z in /opt/zygo-bench/zygo-before /opt/zygo-bench/zygo; do
  b0=$(busy); s=$(date +%s%N)
  seq 400 | xargs -P 8 -I{} $Z run --quiet --mem 256M --pids 128 --net none python:3.12-slim /bin/true
  e=$(date +%s%N); b1=$(busy)
  echo "$Z: $(( 400 * 1000000000 / (e - s) )) runs/s, $(echo "($b1-$b0)*10/400" | bc -l | cut -c1-5) ms VM CPU per run"
done; done
