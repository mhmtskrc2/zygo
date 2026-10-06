#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Cost of each start-up step on its own: CPU of the whole VM per iteration.
N=200
busy() { awk '/^cpu /{print $2+$3+$4+$7+$8}' /proc/stat; }
hz=$(getconf CLK_TCK)
m() { label=$1; shift; b0=$(busy); s=$(date +%s%N); i=0; while [ $i -lt $N ]; do eval "$*"; i=$((i+1)); done; e=$(date +%s%N); b1=$(busy); sleep 1; b2=$(busy)
  printf "%-44s %6.3f ms CPU (+%5.3f ms after), %6.3f ms wall\n" "$label" $(echo "($b1-$b0)*1000/$hz/$N; ($b2-$b1)*1000/$hz/$N; ($e-$s)/1000000/$N" | bc -l | tr '\n' ' '); }
S=/sys/fs/cgroup/user.slice/user-$(id -u).slice/user@$(id -u).service/zygo.slice
T=$S/tenants/default
cat $T/cgroup.subtree_control
m "one cgroup mkdir+rmdir (probe)"            'mkdir $S/p$$; rmdir $S/p$$'
m "three nested cgroups mkdir+rmdir"          'mkdir -p $T/f$$/g/z; rmdir $T/f$$/g/z $T/f$$/g $T/f$$'
m "one leaf cgroup mkdir+rmdir"               'mkdir $T/f$$; rmdir $T/f$$'
m "32 no-op writes to subtree_control"        'for k in 1 2 3 4 5 6 7 8; do for c in cpu memory pids io; do echo +$c > $T/cgroup.subtree_control 2>/dev/null; done; done'
m "read subtree_control once"                 'read x < $T/cgroup.subtree_control'
m "staging dir mkdir+rmdir on ext4"           'mkdir ~/.local/share/zygo/tmp/x$$; rmdir ~/.local/share/zygo/tmp/x$$'
m "staging dir mkdir+rmdir on tmpfs"          'mkdir /run/user/$(id -u)/x$$; rmdir /run/user/$(id -u)/x$$'
m "unshare, one uid mapped directly"          'unshare --user --map-root-user /bin/true'
m "unshare, uid range via newuidmap/gidmap"   'unshare --user --map-auto --map-root-user /bin/true'
m "exec zygo --version"                        '/opt/zygo-bench/zygo --version >/dev/null'
m "exec kern --version"                        '/opt/zygo-bench/kern --version >/dev/null'
m "exec /bin/true"                             '/bin/true'
