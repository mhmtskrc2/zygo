#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Bring one stack up behind a fresh n8n: its database emptied, the
# comparison's workflows imported and published.
#
#   sh stack.sh stock   n8n's own runner sidecar, n8nio/runners
#   sh stack.sh zygo    the Zygo runner in ../zygo_runner.py, over two pools
#   sh stack.sh box     n8nio/runners unchanged, in one long-lived Zygo sandbox
#
# Needs: Linux, docker with compose, a systemd user session (the runner parts
# run as user units, each in a cgroup of its own, so bench.py can read them),
# `zygo` on PATH or in $ZYGO, and for `zygo` Python's `websockets`.
#   ZYGO=/path/to/zygo  DOCKER="sudo docker"  RUNNERS_JSON=n8n-task-runners-open.json
# ZYGO_RUNTIME_DIR and ZYGO_DATA_HOME, when set, are passed to the units too, so
# the comparison can run beside another Zygo on the same machine without
# touching its supervisor.
set -e
HERE=$(cd "$(dirname "$0")" && pwd)
EXAMPLE=$(dirname "$HERE")
cd "$HERE"
ZYGO=${ZYGO:-zygo}
DOCKER=${DOCKER:-docker}
SDK=${ZYGO_SDK:-$EXAMPLE/../../sdk/python/src}
STACK=${1:?usage: sh stack.sh stock|zygo|box}

HOST_IP=$(ip -4 route get 1.1.1.1 2>/dev/null | sed -n 's/.* src \([0-9.]*\).*/\1/p')
# An address the host has *besides* the one pasta copies into a sandbox: a
# sandbox that dials the host's main address reaches itself. Docker's bridge
# is on every host this runs on.
BRIDGE_IP=$(ip -4 addr show docker0 2>/dev/null | sed -n 's/.*inet \([0-9.]*\).*/\1/p' | head -1)
GATEWAY=$(ip -4 route show default 2>/dev/null | sed -n 's/^default via \([0-9.]*\).*/\1/p' | head -1)

# The broker's secret is made once and kept; the rest of .env is this run's.
TOKEN=$(sed -n 's/^N8N_RUNNERS_AUTH_TOKEN=//p' .env 2>/dev/null || true)
[ -n "$TOKEN" ] || TOKEN=$(od -An -tx1 -N24 /dev/urandom | tr -d ' \n')
# The box reaches the broker on the bridge address: its own loopback leads
# nowhere else, and the host's main address is its own too (chapter 14, "The
# host's own loopback").
BROKER_BIND=127.0.0.1
if [ "$STACK" = box ]; then
    [ -n "$BRIDGE_IP" ] || { echo "box needs docker0's address to reach the broker" >&2; exit 2; }
    BROKER_BIND=$BRIDGE_IP
fi
umask 077
printf 'N8N_RUNNERS_AUTH_TOKEN=%s\nBROKER_BIND=%s\nRUNNERS_JSON=%s\n' \
    "$TOKEN" "$BROKER_BIND" "${RUNNERS_JSON:-n8n-task-runners.json}" > .env
umask 022

# Everything off.
units="n8n-zygo-runner n8n-zygo-api n8n-zygo-sup n8n-runner-box"
ZENV=""
[ -n "${ZYGO_RUNTIME_DIR:-}" ] && ZENV="$ZENV --setenv=ZYGO_RUNTIME_DIR=$ZYGO_RUNTIME_DIR"
[ -n "${ZYGO_DATA_HOME:-}" ] && ZENV="$ZENV --setenv=ZYGO_DATA_HOME=$ZYGO_DATA_HOME"
systemctl --user stop $units 2>/dev/null || true
systemctl --user reset-failed $units 2>/dev/null || true
$DOCKER compose --profile stock down -v >/dev/null 2>&1 || true

# A fresh n8n with the workflows.
rm -rf work/workflows
python3 workflows.py work/workflows "$HOST_IP" "$GATEWAY" >/dev/null
$DOCKER compose up -d n8n >/dev/null 2>&1
until curl -sf localhost:5678/healthz >/dev/null; do sleep 1; done
$DOCKER cp work/workflows n8n:/tmp/wf
$DOCKER exec n8n n8n import:workflow --separate --input=/tmp/wf >/dev/null 2>&1
for id in $($DOCKER exec -e N8N_LOG_LEVEL=info n8n n8n list:workflow 2>/dev/null | cut -d'|' -f1); do
    $DOCKER exec n8n n8n publish:workflow --id="$id" >/dev/null 2>&1
done
$DOCKER restart n8n >/dev/null
until curl -sf localhost:5678/healthz >/dev/null; do sleep 1; done

case $STACK in
stock)
    $DOCKER compose --profile stock up -d runners >/dev/null 2>&1
    ;;
zygo)
    # The supervisor gets a unit of its own, with OOMPolicy=continue: one
    # task over its memory limit is killed alone, and must not take the
    # unit — the API and every pool — with it (chapter 16).
    systemd-run --user --unit=n8n-zygo-sup -p OOMPolicy=continue $ZENV \
        "$(command -v "$ZYGO")" supervisor run >/dev/null
    until "$ZYGO" supervisor status >/dev/null 2>&1; do sleep 0.2; done
    systemd-run --user --unit=n8n-zygo-api -p OOMPolicy=continue $ZENV \
        --working-directory="$EXAMPLE" "$(command -v "$ZYGO")" api >/dev/null
    (cd "$EXAMPLE" && "$ZYGO" serve --runtime py313 >/dev/null && "$ZYGO" serve --runtime node26 >/dev/null)
    systemd-run --user --unit=n8n-zygo-runner $ZENV \
        --setenv=N8N_RUNNERS_AUTH_TOKEN="$TOKEN" \
        --setenv=N8N_RUNNERS_TASK_BROKER_URI=http://127.0.0.1:5679 \
        --setenv=N8N_RUNNERS_MAX_CONCURRENCY=5 --setenv=N8N_RUNNERS_TASK_TIMEOUT=60 \
        --setenv=PYTHONPATH="$SDK${RUNNER_PYTHONPATH:+:$RUNNER_PYTHONPATH}" \
        --setenv=LOG_LEVEL="${LOG_LEVEL:-WARNING}" \
        "$(command -v python3)" "$EXAMPLE/zygo_runner.py" >/dev/null
    ;;
box)
    # n8n's own image and its own entrypoint — the Go launcher and both
    # runners — in one sandbox. Its only network is the broker. The launcher
    # listens for health checks on 5680 and has each runner listen on 5681 and
    # 5682, all on the sandbox's own loopback (ADR 0008); it also connects to
    # them, and under `egress` a connect meets the allowlist's port rule even
    # on loopback, so the three ports are named.
    "$ZYGO" pull "n8nio/runners:${N8N_VERSION:-2.38.7}" >/dev/null
    systemd-run --user --unit=n8n-runner-box -p OOMPolicy=continue $ZENV \
        --working-directory="$HERE" \
        -p StandardOutput=truncate:"$HERE/work/box.log" -p StandardError=file:"$HERE/work/box.log" \
        "$(command -v "$ZYGO")" run \
        --mem 1G --cpu 2 --pids 256 --timeout 24h \
        --net egress --allow "$BRIDGE_IP/32:5679" --allow-private-net \
        --allow 127.0.0.1/32:5680 --allow 127.0.0.1/32:5681 --allow 127.0.0.1/32:5682 \
        --mount "$HERE/${RUNNERS_JSON:-n8n-task-runners.json}:/etc/n8n-task-runners.json" \
        --env N8N_RUNNERS_TASK_BROKER_URI="http://$BRIDGE_IP:5679" \
        --env N8N_RUNNERS_AUTH_TOKEN="$TOKEN" \
        --env N8N_RUNNERS_MAX_CONCURRENCY=5 --env N8N_RUNNERS_TASK_TIMEOUT=60 \
        "n8nio/runners:${N8N_VERSION:-2.38.7}" >/dev/null
    ;;
*)
    echo "unknown stack: $STACK" >&2
    exit 2
    ;;
esac
sleep 8
echo "stack $STACK up"
