#!/bin/sh
# Setup for the embedder's benchmark; the measuring is in `bench_embed.py`.
#
# The comparison only means something if every runner is on one host. That is
# awkward to arrange: Zygo runs inside a container here, and `docker run`
# cannot. The answer is the host's own docker socket — a container started
# from inside this one is a *sibling*, on the same kernel and the same VM as
# the Zygo sandboxes beside it. Same host, one run.
#
# Run:  make bench-embed
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
export ZYGO
ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-embed}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

# A docker CLI, if there is a socket to talk to. Not fatal: the harness
# reports a missing column rather than refusing to run.
if [ -S /var/run/docker.sock ] && ! command -v docker >/dev/null 2>&1; then
    echo "installing a docker client to talk to the mounted socket…"
    apt-get -qq update >/dev/null 2>&1
    apt-get -qq install -y docker.io >/dev/null 2>&1 || \
        echo "  could not install one; that column will be missing"
fi

echo "pulling the image, before anything is timed"
zygo pull python:3.12-slim >/dev/null 2>&1
command -v docker >/dev/null 2>&1 && docker pull python:3.12-slim >/dev/null 2>&1
# kern keeps its own store; a measurement that pulled would be a network test.
# Loud on failure: `--pull never` below turns a missing image into sixty
# identical failures, which is a column of zeroes and not a result.
if [ -n "${ZYGO_BENCH_KERN:-}" ] && [ -x "$ZYGO_BENCH_KERN" ]; then
    # kern shells out for pulls — `curl`, GNU `tar`, `gzip`, `sha256sum` — and
    # a slim Python image has none of them. Installed before anything is
    # timed, like the image itself.
    command -v curl >/dev/null 2>&1 || {
        apt-get -qq update >/dev/null 2>&1
        apt-get -qq install -y curl tar gzip coreutils >/dev/null 2>&1
    }
    if ! "$ZYGO_BENCH_KERN" image pull python:3.12-slim >/tmp/kern-pull.log 2>&1; then
        echo "kern could not pull the image, so its column would be empty:" >&2
        tail -3 /tmp/kern-pull.log >&2
        exit 1
    fi
fi

zygo_supervisor /tmp/supervisor-embed.log
i=0
while [ $i -lt 100 ]; do
    "$ZYGO" supervisor status >/dev/null 2>&1 && break
    i=$((i+1)); sleep 0.1
done

# One throwaway sandbox, through the harness wrapper, before anything is
# measured or moved. It is what makes `zygo.slice/system` exist: the supervisor
# builds the cgroup layout on its first sandbox, not at start-up, and moving
# the driver in before that put it in `launch/` itself — where it then blocked
# the supervisor from delegating controllers at all (`EBUSY`), and every
# measurement failed with "its controllers were not delegated".
zygo run --quiet python:3.12-slim python3 -c pass >/dev/null 2>&1

# The measuring process has to start in a cgroup Zygo can build under, because
# it spawns `zygo run` directly rather than through the harness's `zygo()`
# wrapper — a benchmark that timed a wrapper would be timing the wrapper. This
# is the same move `zygo()` makes, done once for the whole run.
run_in_harness() {
    sh -c '
        root=$1; shift
        if [ -d "$root/launch/zygo.slice/system" ]; then
            echo $$ > "$root/launch/zygo.slice/system/cgroup.procs" 2>/dev/null
        else
            echo "bench: no zygo.slice/system to move into; the numbers below" >&2
            echo "bench: would be about a cgroup failure, not about Zygo." >&2
            exit 1
        fi
        exec "$@"' _ "$ZYGO_HARNESS_ROOT" "$@"
}

run_in_harness python3 "$SRC/poc/bench_embed.py" --work-dir "${WORK_DIR:-/tmp/bench-embed}" "$@"
status=$?

echo
harness_verdict
exit "$status"
