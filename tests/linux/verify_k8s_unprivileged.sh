#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Zygo in a Kubernetes pod with no privileges, checked for real.
#
# `examples/kubernetes/unprivileged.yaml`, applied to a cluster whose nodes
# have the `zygo` runtime handler (`examples/kubernetes/node/`), and then the
# claims that manifest makes, one by one:
#
#   * the pod is not privileged, has a user namespace of its own, and its root
#     is not the node's;
#   * `zygo doctor` finds the cgroup delegated and egress possible;
#   * a sandbox runs, as the CI `kubernetes` job checks for the privileged one;
#   * a request over its memory limit is killed and the next one is served —
#     the limits are real, not written and ignored;
#   * an egress allowlist lets through what it names and nothing else;
#   * the pod cannot raise its own memory limit.
#
# It does not install the handler or the cluster; the CI job does that, and so
# does whoever runs this against their own.
#
#   KUBECTL="sudo k3s kubectl" IMAGE=zygo:local sh tests/linux/verify_k8s_unprivileged.sh
#
# Needs `curl`, and a pullable or preloaded image (IMAGE, PULL=Never for a
# preloaded one). The egress check reaches example.com:80 from the pod, so the
# cluster needs a way out; EGRESS=0 skips it.
set -u

SRC=${SRC:-.}
KUBECTL=${KUBECTL:-kubectl}
IMAGE=${IMAGE:-zygo:local}
PULL=${PULL:-Never}
PORT=${PORT:-17712}
EGRESS=${EGRESS:-1}
K=$KUBECTL

PASS=0
FAIL=0
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }
skip() { printf '  SKIP  %s\n' "$*"; }
# `verdict $? "passed" "failed"`: ok or bad by the status of what ran just before.
verdict() { if [ "$1" -eq 0 ]; then ok "$2"; else bad "$3"; fi; }

forward=""
cleanup() { [ -n "$forward" ] && kill "$forward" 2>/dev/null; }
trap cleanup EXIT

echo "Zygo in an unprivileged pod"

token=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
# `kubectl run -i` hands back stdout and stderr as one stream, and keygen puts
# its advice on stderr, in whichever order the log arrives: the key is the
# line that looks like one.
key=$($K run zygo-keygen -q --rm -i --restart=Never --image="$IMAGE" \
    --image-pull-policy="$PULL" -- secrets keygen 2>/dev/null | grep -E '^[0-9a-f]{64}$' | head -1)
if [ -z "$key" ]; then
    echo "  could not run $IMAGE to make a secrets key" >&2
    exit 1
fi

# One replica: the check is about one pod, and a single-node test cluster may
# not fit the example's two.
$K delete namespace zygo --ignore-not-found --wait=true >/dev/null 2>&1
sed -e "s|ghcr.io/mhmtskrc2/zygo:latest|$IMAGE|" \
    -e "s|          image: $IMAGE|          image: $IMAGE\n          imagePullPolicy: $PULL|" \
    -e 's|replicas: 2|replicas: 1|' \
    -e "s|replace-me-with-zygo-secrets-keygen|$key|" \
    -e "s|replace-me-with-32-random-bytes|$token|" \
    "$SRC/examples/kubernetes/unprivileged.yaml" | $K apply -f - >/dev/null

if $K -n zygo rollout status deployment/zygo --timeout=180s >/dev/null 2>&1; then
    ok "the deployment rolls out"
else
    bad "the deployment rolls out"
    $K -n zygo describe pods | tail -25
    $K -n zygo logs deploy/zygo --tail=40 2>/dev/null
    printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
    exit 1
fi

pod=$($K -n zygo get pods -l app=zygo -o jsonpath='{.items[0].metadata.name}')
in_pod() { $K -n zygo exec "$pod" -- sh -c "$1" 2>&1; }

# ---- the pod itself ----------------------------------------------------------

privileged=$($K -n zygo get pod "$pod" -o jsonpath='{.spec.containers[0].securityContext.privileged}')
[ "$privileged" = "false" ]
verdict $? "the pod is not privileged" "the pod is privileged ($privileged)"

uid_map=$(in_pod 'cat /proc/self/uid_map')
case $(echo "$uid_map" | awk '{print $1, $2}') in
    "0 0") bad "the pod has a user namespace of its own: $uid_map" ;;
    *) ok "the pod's root is uid $(echo "$uid_map" | awk '{print $2}') on the node" ;;
esac

caps=$(in_pod 'grep CapEff /proc/self/status' | awk '{print $2}')
# CAP_SYS_ADMIN is bit 21: not in the default set, and not to be in this one.
if [ -n "$caps" ] && [ $(( 0x$caps >> 21 & 1 )) -eq 0 ]; then
    ok "no CAP_SYS_ADMIN, even in the pod's own namespace (CapEff $caps)"
else
    bad "the pod has CAP_SYS_ADMIN (CapEff $caps)"
fi

doctor=$(in_pod 'zygo doctor')
echo "$doctor" | grep -q '^cgroup v2 .*delegated'
verdict $? "zygo doctor: the cgroup is delegated" \
    "zygo doctor: the cgroup is not delegated: $(echo "$doctor" | grep -A1 '^cgroup v2')"
echo "$doctor" | grep -q '^procfs (fully visible) .* ok'
verdict $? "zygo doctor: a fresh /proc can be mounted" \
    "zygo doctor: /proc: $(echo "$doctor" | grep -A1 '^procfs')"
if [ "$EGRESS" = 1 ]; then
    echo "$doctor" | grep -q '^egress .* ok'
    verdict $? "zygo doctor: egress is possible" \
        "zygo doctor: egress: $(echo "$doctor" | grep -A1 '^egress')"
fi

if in_pod 'echo max > /sys/fs/cgroup/memory.max' >/dev/null; then
    bad "the pod raised its own memory.max"
else
    ok "the pod cannot raise its own memory.max"
fi

# ---- sandboxes, through the API ----------------------------------------------

in_pod 'zygo pull alpine:3' >/dev/null || true
$K -n zygo port-forward "pod/$pod" "$PORT:7700" >/dev/null 2>&1 &
forward=$!
for _ in $(seq 40); do
    if curl -fsS "http://127.0.0.1:$PORT/healthz" >/dev/null 2>&1; then break; fi
    sleep 0.5
done

api() { curl -sS -X POST "http://127.0.0.1:$PORT$1" -H "authorization: Bearer $token" \
    -H 'content-type: application/json' -d "$2"; }

# The JSON is written to files in quoted heredocs: each request body is a JSON
# string holding a shell script holding quotes.
tmp=$(mktemp -d)
cat > "$tmp/hello.json" <<'JSON'
{"script": {"source": "read -r event\nprintf '{\"from\": \"the-pod\", \"uid\": \"%s\"}\\n' \"$(id -u)\"\n"}, "event": {}}
JSON
cat > "$tmp/big.json" <<'JSON'
{"script": {"source": "read -r event\ndd if=/dev/zero of=/dev/null bs=200M count=1 2>/dev/null\nprintf '{\"survived\": true}\\n'\n"}, "event": {}}
JSON
cat > "$tmp/net.json" <<'JSON'
{"script": {"source": "read -r event\na=$(wget -q -T 5 -O- http://example.com/ 2>/dev/null | grep -c -i 'example domain')\nb=$(wget -q -T 5 -O- http://neverssl.com/ 2>/dev/null | wc -c)\nprintf '{\"allowed\": %s, \"other\": %s}\\n' \"$a\" \"$b\"\n"}, "event": {}}
JSON

api /runtimes '{"name":"sh","layer":{"image":"alpine:3","cmd":["/bin/sh"],"timeout":"20s","mem":"64M"}}' >/dev/null
out=$(api /runtimes/sh/call "@$tmp/hello.json")
echo "$out" | grep -q '"from":"the-pod"'
verdict $? "a sandbox runs: $out" "a sandbox runs: $out"

out=$(api /runtimes/sh/call "@$tmp/big.json")
echo "$out" | grep -q '"exit_code":137'
verdict $? "200 MB in a 64 MB request is killed" "200 MB in a 64 MB request is killed: $out"
out=$(api /runtimes/sh/call "@$tmp/hello.json")
echo "$out" | grep -q '"from":"the-pod"'
verdict $? "and the next request is served" "and the next request is served: $out"

if [ "$EGRESS" = 1 ]; then
    api /runtimes '{"name":"net","layer":{"image":"alpine:3","cmd":["/bin/sh"],"timeout":"20s","network":"egress","seccomp":"default","allow":["example.com:80"]}}' >/dev/null
    out=$(api /runtimes/net/call "@$tmp/net.json")
    echo "$out" | grep -q '"allowed": *[1-9]'
    verdict $? "egress reaches what the allowlist names" "egress reaches what the allowlist names: $out"
    echo "$out" | grep -q '"other": *0'
    verdict $? "and nothing else" "and nothing else: $out"
else
    skip "egress (EGRESS=0)"
fi
rm -rf "$tmp"

# The liveness probe is an `exec` into the container, and the container's cgroup
# is no longer a leaf once Zygo has passed controllers down from it. A probe
# that could not get in would restart the pod every ninety seconds.
if in_pod 'zygo doctor --json' >/dev/null; then
    ok "an exec still gets into the pod once sandboxes have run"
else
    bad "an exec still gets into the pod once sandboxes have run"
fi
restarts=$($K -n zygo get pod "$pod" -o jsonpath='{.status.containerStatuses[0].restartCount}')
[ "$restarts" = "0" ]
verdict $? "no restarts" "the pod restarted $restarts times"

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
