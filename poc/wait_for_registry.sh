#!/bin/sh
# Wait for the test registry to answer, rather than sleeping and hoping.
#
# `sleep 3` was right on a warm machine and wrong on a cold one, where the
# suite then failed with a connection error that looked like a bug in `zygo
# login`. Polling the endpoint the suite is about to use says what it is
# waiting for, and gives up with a reason rather than a timeout somewhere else.
set -u

DEADLINE=$(( $(date +%s) + 60 ))
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    if docker run --rm --network container:zygo-reg curlimages/curl:latest \
        -fsS -o /dev/null http://127.0.0.1:5000/v2/ 2>/dev/null; then
        exit 0
    fi
    # A 401 is the registry answering: it wants credentials, which is the
    # whole point of this fixture. `curl -f` calls that a failure, so the
    # status code is what decides.
    code=$(docker run --rm --network container:zygo-reg curlimages/curl:latest \
        -s -o /dev/null -w '%{http_code}' http://127.0.0.1:5000/v2/ 2>/dev/null)
    case "$code" in
        200|401) exit 0 ;;
    esac
    sleep 1
done

echo "the test registry did not answer on 127.0.0.1:5000 within 60s" >&2
docker logs zygo-reg 2>&1 | tail -20 >&2
exit 1
