#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# zloop.sh JOBDIR N MODE   (MODE zygo | nsjail)
PY=/tmp/windmill/cache/py_runtime/cpython-3.12.13-linux-aarch64-gnu/bin/python3.12
cd "$1"
if [ "$3" = nsjail ]; then
  for i in $(seq $2); do /usr/bin/nsjail --config run.config.proto -- $PY -u -m wrapper >/dev/null 2>&1; done
else
  ARGS=$(env -i PATH=/usr/bin:/bin sh /tmp/argdump.sh --config run.config.proto -- $PY -u -m wrapper | tr '\n' ' ')
  for i in $(seq $2); do /usr/local/bin/zygo run $ARGS >/dev/null 2>&1; done
fi
