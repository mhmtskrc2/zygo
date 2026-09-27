#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# The first run after the runner side restarts, and after 30 s idle. n8n's
# stock launcher stops an idle runner after 15 s, so the second is what a
# workflow triggered now and then sees.
#   sh coldstart.sh stock|zygo|box      (the stack must be up)
S=$1; cd "$(dirname "$0")"
ZYGO=${ZYGO:-zygo}; DOCKER=${DOCKER:-docker}
mkdir -p work
t() { curl -s -m 90 -o /dev/null -w "%{time_total} %{http_code}" -X POST -H "content-type: application/json" -d '{"a":1}' localhost:5678/webhook/$1; }
restart() {
    case $S in
        stock) $DOCKER restart n8n-runners >/dev/null ;;
        zygo)  systemctl --user restart n8n-zygo-runner
               (cd .. && "$ZYGO" stop py313 >/dev/null && "$ZYGO" stop node26 >/dev/null &&
                "$ZYGO" serve --runtime py313 >/dev/null && "$ZYGO" serve --runtime node26 >/dev/null) ;;
        box)   systemctl --user restart n8n-runner-box ;;
    esac
}
for i in 1 2 3 4 5; do
    restart; sleep 10
    echo "{\"stack\":\"$S\",\"cold\":\"restart\",\"js\":\"$(t trivial-js)\",\"py\":\"$(t trivial-py)\"}" | tee -a work/cold.jsonl
done
for i in 1 2 3; do
    t trivial-js >/dev/null; t trivial-py >/dev/null; sleep 30
    echo "{\"stack\":\"$S\",\"cold\":\"idle30\",\"js\":\"$(t trivial-js)\",\"py\":\"$(t trivial-py)\"}" | tee -a work/cold.jsonl
done
