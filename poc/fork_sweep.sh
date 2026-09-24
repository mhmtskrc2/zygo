#!/bin/sh
# Fork-safety sweep across popular PyPI packages. See poc/fork_sweep.py.
#
# Run:  make fork-sweep-linux
#       PACKAGES=numpy,pandas make fork-sweep-linux
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
export ZYGO
ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/zdata}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
[ "$ZYGO_HARNESS" = ready ] || exit 1

# The venvs are the slow part; a data home on a volume keeps them across runs,
# so only what is left over from a crashed run is cleared.
"$ZYGO" stop --all >/dev/null 2>&1
zygo_supervisor /tmp/supervisor.log
i=0
while ! "$ZYGO" ps >/dev/null 2>&1 && [ $i -lt 100 ]; do i=$((i+1)); sleep 0.1; done

python3 "$SRC/poc/fork_sweep.py"
STATUS=$?
"$ZYGO" stop --all >/dev/null 2>&1
kill "$SUPERVISOR_PID" 2>/dev/null
exit $STATUS
