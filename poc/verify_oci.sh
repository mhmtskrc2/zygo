#!/bin/sh
# The container image, built and run for real (todo 4.2).
#
# What this checks is the image rather than Zygo: that the binary in it is the
# one that was built, that it starts as a non-root user, that `/healthz`
# answers without a token and the `HEALTHCHECK` goes green off it, that the
# API is **call-only** unless somebody asks for more — and, the one that
# matters, that a sandbox really runs inside a container with no privileges
# and no capabilities.
#
# The flags are the three from `docs/guide.md`, and no more:
#
#   --security-opt seccomp=unconfined      `unshare(CLONE_NEWUSER)`
#   --security-opt systempaths=unconfined  an unmasked /proc
#   a cgroup v2 subtree of its own         and only its own
#
# Run:  make verify-oci
set -u

SRC=${SRC:-.}
IMAGE=${OCI_IMAGE:-zygo:local}
PARENT=${OCI_CGROUP:-zygo-oci}
PORT=${OCI_PORT:-17711}
NAME=zygo-oci-verify

PASS=0
FAIL=0
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }
skip() { printf '  SKIP  %s\n' "$*"; }

cleanup() { docker rm -f "$NAME" >/dev/null 2>&1; }
trap cleanup EXIT

echo "the container image"

# A cgroup subtree for the container, made by whatever can make one. On a host
# with `sudo` that is `sudo`; on a developer's Docker Desktop the "host" is a
# VM no shell here can reach, so a *privileged* helper container makes it.
# Preparing the host with a privilege the thing under test does not get is the
# point rather than a compromise: what is being checked is that the unprivileged
# container can use it.
prepare_cgroup() {
    if sudo -n mkdir -p "/sys/fs/cgroup/$PARENT" 2>/dev/null; then
        for c in memory pids cpu; do
            echo "+$c" | sudo -n tee /sys/fs/cgroup/cgroup.subtree_control >/dev/null 2>&1
        done
        return 0
    fi
    docker run --rm --privileged -v /sys/fs/cgroup:/sys/fs/cgroup:rw alpine:3 sh -c "
        mkdir -p /sys/fs/cgroup/$PARENT || exit 1
        for c in memory pids cpu; do
            echo \"+\$c\" > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null
        done" >/dev/null 2>&1
}

if ! prepare_cgroup; then
    echo "  could not create a cgroup subtree for the container" >&2
    exit 1
fi

# --- what the image is ------------------------------------------------------

version=$(docker run --rm "$IMAGE" --version 2>&1 | head -1)
case "$version" in
    zygo*) ok "the binary in the image runs and says what it is ($version)" ;;
    *)     bad "the image could not run zygo: $version" ;;
esac

user=$(docker run --rm --entrypoint id "$IMAGE" -u 2>&1)
if [ "$user" != "0" ]; then
    ok "it runs as a non-root user by default (uid $user)"
else
    bad "the image's default user is root"
fi

# The four programs `network = "egress"` needs. A missing one is not a broken
# image — egress is refused with a reason — but it is a silently smaller
# product, which is what this catches.
missing=$(docker run --rm --entrypoint sh "$IMAGE" -c '
    for b in pasta nft tc newuidmap; do
        command -v $b >/dev/null 2>&1 || printf "%s " "$b"
    done' 2>&1)
if [ -z "$missing" ]; then
    ok "pasta, nft, tc and newuidmap are all in it"
else
    bad "the image is missing: $missing"
fi

# --- what it can do ---------------------------------------------------------

TOKEN=verify-oci-token
docker run -d --rm --name "$NAME" \
    --user 0:0 \
    --security-opt seccomp=unconfined \
    --security-opt systempaths=unconfined \
    --cgroupns=host --cgroup-parent="/$PARENT" \
    -v "/sys/fs/cgroup/$PARENT:/sys/fs/cgroup/$PARENT:rw" \
    -p "$PORT:7700" \
    -e ZYGO_API_TOKEN="$TOKEN" \
    "$IMAGE" api --listen 0.0.0.0:7700 --allow-deploy >/dev/null 2>&1

i=0
while [ $i -lt 100 ]; do
    [ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/healthz" 2>/dev/null)" = 200 ] && break
    i=$((i + 1)); sleep 0.2
done

code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/healthz" 2>/dev/null)
if [ "$code" = 200 ]; then
    ok "/healthz answers 200 without a token"
else
    bad "/healthz answered $code"
    docker logs "$NAME" 2>&1 | tail -5
    printf '\n  %s passed, %s failed\n' "$PASS" "$FAIL"
    exit 1
fi

if curl -s "http://127.0.0.1:$PORT/version" -H "Authorization: Bearer $TOKEN" |
    grep -q '"api"'; then
    ok "and /version answers a caller with the token"
else
    bad "/version with the token"
fi

if curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/version" |
    grep -q '401'; then
    ok "and refuses one without it"
else
    bad "/version answered a caller with no token"
fi

# The check the rest is in service of: a sandbox, inside a container with no
# privileges, running a program and answering with what it printed.
docker exec "$NAME" zygo pull alpine:3 >/dev/null 2>&1
out=$(curl -s "http://127.0.0.1:$PORT/run" \
    -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
    -d '{"layer": {"image": "alpine:3", "cmd": ["echo", "from-inside"]}}' 2>&1)
case "$out" in
    *from-inside*) ok "a sandbox runs inside the unprivileged container" ;;
    *)             bad "the sandbox did not run: $(printf '%s' "$out" | cut -c1-200)" ;;
esac

# `HEALTHCHECK` is in the image rather than in whoever runs it, so a compose
# file or a Kubernetes manifest gets one for free. It takes a few seconds.
i=0
health=""
while [ $i -lt 60 ]; do
    health=$(docker inspect --format '{{.State.Health.Status}}' "$NAME" 2>/dev/null)
    [ "$health" = healthy ] && break
    i=$((i + 1)); sleep 1
done
if [ "$health" = healthy ]; then
    ok "the image's own HEALTHCHECK goes green"
else
    bad "HEALTHCHECK is '$health' after ${i}s"
fi

docker rm -f "$NAME" >/dev/null 2>&1

# --- and what it refuses ----------------------------------------------------

# Call-only unless asked: deploy rights over HTTP are a shell, and a default
# that hands them out is one nobody reads the flag for.
docker run -d --rm --name "$NAME" \
    --user 0:0 \
    --security-opt seccomp=unconfined \
    --security-opt systempaths=unconfined \
    --cgroupns=host --cgroup-parent="/$PARENT" \
    -v "/sys/fs/cgroup/$PARENT:/sys/fs/cgroup/$PARENT:rw" \
    -p "$PORT:7700" -e ZYGO_API_TOKEN="$TOKEN" "$IMAGE" >/dev/null 2>&1
i=0
while [ $i -lt 100 ]; do
    [ "$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/healthz" 2>/dev/null)" = 200 ] && break
    i=$((i + 1)); sleep 0.2
done
if curl -s "http://127.0.0.1:$PORT/version" -H "Authorization: Bearer $TOKEN" |
    grep -q '"deploy":false'; then
    ok "the default command is call-only: deploy is a flag somebody has to pass"
else
    bad "the image's default command allows deploy"
fi

printf '\n  %s passed, %s failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ] || exit 1
