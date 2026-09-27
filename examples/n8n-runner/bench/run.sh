#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# The whole comparison: every stack measured twice, the second round in the
# opposite order so that drift on the machine does not favour either side;
# then first-run latency, then what a Code node can reach. About two hours on
# two cores. Results in work/; tables with `python3 summarize.py`.
#
#   sh run.sh                 stock and zygo
#   STACKS="stock zygo box" sh run.sh
set -e
cd "$(dirname "$0")"
STACKS=${STACKS:-stock zygo}
REVERSED=$(printf '%s\n' $STACKS | awk '{a[NR]=$0} END {for (i=NR; i>0; i--) print a[i]}')
mkdir -p work
post() { curl -s -m 90 -o /dev/null -X POST -H "content-type: application/json" "$@"; }
rows() { python3 -c 'import json; print(json.dumps({"rows": [{"id": i, "name": "item-%d" % i, "qty": i % 7 + 1, "price": round(1.5 + i % 13, 2)} for i in range(1000)]}))'; }

matrix() { # stack round
    sh stack.sh "$1"
    rows > work/rows.json
    # Warm-up, not measured: ten of each.
    for i in $(seq 10); do
        for w in noop trivial-js trivial-py cpu-js cpu-py deps-js deps-py; do post -d '{"a":1}' localhost:5678/webhook/$w; done
        post --data-binary @work/rows.json localhost:5678/webhook/items-js
        post --data-binary @work/rows.json localhost:5678/webhook/items-py
    done
    sleep 20
    python3 bench.py "$1#$2" - idle
    python3 bench.py "$1#$2" noop seq 100
    for w in trivial-js trivial-py cpu-js cpu-py items-js items-py deps-js deps-py; do
        python3 bench.py "$1#$2" $w seq 100
    done
    for w in trivial-js trivial-py cpu-js cpu-py items-js items-py deps-js deps-py; do
        python3 bench.py "$1#$2" $w burst 200 32; sleep 5
    done
    for w in trivial-js trivial-py; do python3 bench.py "$1#$2" $w rate 5 30; sleep 5; done
}

for s in $STACKS; do matrix "$s" 1; done
for s in $REVERSED; do matrix "$s" 2; done
rm -f work/cold.jsonl
for s in $STACKS; do sh stack.sh "$s" >/dev/null; sh coldstart.sh "$s"; done
for s in $STACKS; do
    sh stack.sh "$s" >/dev/null; sh probes.sh "$s" "$s" > "work/probes-$s.txt" 2>&1
done
RUNNERS_JSON=n8n-task-runners-open.json sh stack.sh stock >/dev/null
sh probes.sh stock-open stock > work/probes-stock-open.txt 2>&1
echo "done: python3 summarize.py"
