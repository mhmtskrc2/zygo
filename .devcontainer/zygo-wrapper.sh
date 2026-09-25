#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# /usr/local/bin/zygo in the dev container: step into the empty cgroup that
# .devcontainer/cgroup.sh left for it, then run the real binary. Once zygo has
# built its slice there, later runs go straight to that slice's `system`
# cgroup, as they would on a host.
root=/sys/fs/cgroup
if [ -d "$root/launch/zygo.slice/system" ]; then
    echo $$ > "$root/launch/zygo.slice/system/cgroup.procs" 2>/dev/null
elif [ -d "$root/launch" ]; then
    echo $$ > "$root/launch/cgroup.procs" 2>/dev/null
fi
exec /usr/local/lib/zygo/zygo "$@"
