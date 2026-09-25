#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Run at every container start: make the container's cgroup tree one Zygo can
# build sandboxes in. The same steps poc/cgroup_harness.sh takes for the test
# suites' containers.
#
# The container starts at the root of its own cgroup namespace, with processes
# in it — and cgroup v2 hands controllers down only from a cgroup that has
# none. So every process moves to `init/` (Docker puts later `exec`s there by
# itself), memory, pids and cpu are enabled for the children, and `launch/`
# is left empty for `zygo` to start in. /usr/local/bin/zygo is a wrapper that
# steps into it.
set -u
root=/sys/fs/cgroup
[ "$(sed -n 's/^0:://p' /proc/self/cgroup | head -1)" = "/" ] || [ -d "$root/init" ] || {
    echo "cgroup.sh: not at the root of a cgroup namespace; leaving the tree alone" >&2
    exit 0
}
mkdir -p "$root/init" "$root/launch"
for p in $(cat "$root/cgroup.procs"); do
    echo "$p" > "$root/init/cgroup.procs" 2>/dev/null
done
for c in memory pids cpu; do
    echo "+$c" > "$root/cgroup.subtree_control" 2>/dev/null
done
echo "cgroup.sh: controllers for sandboxes: $(cat "$root/cgroup.subtree_control")"
