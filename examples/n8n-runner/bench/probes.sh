#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# What a Code node can reach and what hostile code does to its neighbours.
#   sh probes.sh LABEL STACK      (the stack must already be up)
L=$1; S=$2; cd "$(dirname "$0")"; DOCKER=${DOCKER:-docker}
p() { curl -s -m 75 -w " |%{http_code}|%{time_total}s" -X POST -H "content-type: application/json" -d '{"a":1}' localhost:5678/webhook/$1; }
revive() {
  case $S in
    stock) $DOCKER restart n8n-runners >/dev/null ;;
    box) systemctl --user restart n8n-runner-box ;;
    zygo) systemctl --user restart n8n-zygo-runner ;;
  esac
  sleep 10
}
alive() {
  a="trivial-js $(p trivial-js | tail -c 30)  trivial-py $(p trivial-py | tail -c 30)"
  echo "   after: $a  n8n $(curl -s -m 5 localhost:5678/healthz)"
  case "$a" in *500*|*000*) echo "   !! runner side broken; restarting it by hand"; revive
    echo "   after restart: trivial-js $(p trivial-js | tail -c 30)  trivial-py $(p trivial-py | tail -c 30)";; esac
}
echo "=== $L"
echo "--- probe-js"; p probe-js; echo
echo "--- probe-py"; p probe-py; echo
for w in mem-js mem-py disk-js disk-py; do echo "--- $w"; p $w; echo; alive; done
for l in js py; do
  echo "--- mem-$l while 3 slow-$l tasks (3 s each) are running beside it"
  (p slow-$l > /tmp/s1) & (p slow-$l > /tmp/s2) & (p slow-$l > /tmp/s3) &
  sleep 1; echo "   mem-$l: $(p mem-$l | tail -c 40)"; wait
  echo "   neighbours: $(tail -c 30 /tmp/s1) | $(tail -c 30 /tmp/s2) | $(tail -c 30 /tmp/s3)"; alive
done
for w in loop-js loop-py; do
  echo "--- $w (while it runs: a trivial task in each language)"
  (p $w > /tmp/loop.out) &
  sleep 3
  echo "   during: trivial-js $(p trivial-js | tail -c 30)  trivial-py $(p trivial-py | tail -c 30)"
  wait; echo "   loop answer: $(cat /tmp/loop.out)"; alive
done
