# Shared cgroup preparation for the verification suites. Sourced, not run.
#
# Every suite needs the same thing, and it is not obvious: each `zygo`
# invocation must start in a cgroup that holds **nothing else**. cgroup v2
# forbids a cgroup from holding processes *and* delegating controllers to its
# children ("no internal processes"), so a `zygo` started from the same cgroup
# as the shell that called it cannot enable the controllers its tenants need.
# It gets as far as creating `zygo.slice/tenants/<name>` and then finds no
# `memory.max` there.
#
# So the harness does two things, relative to whatever cgroup the suite itself
# was started in:
#
#   * steps aside into `harness/`, leaving the starting cgroup empty;
#   * gives each `zygo` its own `launch/` to build a slice underneath.
#
# The only difference between a privileged container and a real host is where
# that starting cgroup is. In a container it is the root of the hierarchy and
# `/proc/self/cgroup` says `0::/`. On a real host it is the systemd scope the
# suite was started in, which has to be a *delegated* one:
#
#     systemd-run --user --scope -p Delegate=yes -- sh poc/verify_launcher.sh
#
# Reading it from `/proc/self/cgroup` rather than assuming `/sys/fs/cgroup` is
# what makes the same suite run in both. Before that, every suite hard-coded
# the container's answer, and `verify_launcher.sh` on a working host reported
# 24 failures that were all this file's absence.
#
# `$ZYGO_HARNESS_ROOT` is left set so a suite can print where it built, and
# `$ZYGO_HARNESS` is `ready` or `unusable` — a run that silently prepared
# nothing is a run whose failures mean nothing.
#
# Requires `$ZYGO`. Defines `zygo()` and `zygo_supervisor()`, the latter
# setting `$SUPERVISOR_PID`.

ZYGO_HARNESS_ROOT=$(
    rel=$(sed -n 's/^0:://p' /proc/self/cgroup 2>/dev/null | head -1)
    printf '%s' "/sys/fs/cgroup${rel%/}"
)

# The suites reach into the hierarchy to read `cgroup.procs` and check what a
# sandbox actually put where. They spell that root `$CG`, and under `set -u` an
# unset one is fatal — the first version of this file left it undefined and
# `verify_supervisor.sh` died mid-run with `CG: parameter not set`.
CG=$ZYGO_HARNESS_ROOT

if mkdir -p "$ZYGO_HARNESS_ROOT/harness" "$ZYGO_HARNESS_ROOT/launch" 2>/dev/null; then
    ZYGO_HARNESS=ready

    # Vacate the starting cgroup: until it is empty its controllers cannot be
    # delegated downwards. In a container this moves every process there; on a
    # host in its own scope it moves this shell.
    for p in $(cat "$ZYGO_HARNESS_ROOT/cgroup.procs" 2>/dev/null); do
        echo "$p" > "$ZYGO_HARNESS_ROOT/harness/cgroup.procs" 2>/dev/null
    done
    for c in memory pids cpu; do
        echo "+$c" > "$ZYGO_HARNESS_ROOT/cgroup.subtree_control" 2>/dev/null
    done
else
    # Neither a writable hierarchy nor a delegated scope. Nothing below will
    # work, and saying so here beats twenty failures that look like the
    # launcher's.
    ZYGO_HARNESS=unusable
    echo "  harness: cannot create a cgroup under $ZYGO_HARNESS_ROOT" >&2
    echo "  → in a container, run privileged; on a host, start this suite with" >&2
    echo "    systemd-run --user --scope -p Delegate=yes -- sh \$0" >&2
fi

# Run zygo in `launch/`, which holds nothing else, so it can build its slice
# underneath. `exec` matters: the shell must not survive inside that cgroup.
# Once the first run has built `launch/zygo.slice`, later runs go straight
# into its `system` cgroup — zygo recognises the enclosing slice and reuses
# it. Before that, `launch/` itself is the starting point.
zygo() {
    sh -c '
        root=$1; shift
        if [ -d "$root/launch/zygo.slice/system" ]; then
            echo $$ > "$root/launch/zygo.slice/system/cgroup.procs" 2>/dev/null
        else
            echo $$ > "$root/launch/cgroup.procs" 2>/dev/null
        fi
        exec "$@"' _ "$ZYGO_HARNESS_ROOT" "$ZYGO" "$@"
}

# The supervisor is long-lived, so it takes `launch/` for itself; later
# clients run from wherever they are, because a client only talks to a socket.
zygo_supervisor() {
    sh -c '
        root=$1; shift
        if [ -d "$root/launch/zygo.slice/system" ]; then
            echo $$ > "$root/launch/zygo.slice/system/cgroup.procs" 2>/dev/null
        else
            echo $$ > "$root/launch/cgroup.procs" 2>/dev/null
        fi
        exec "$@" supervisor run' _ "$ZYGO_HARNESS_ROOT" "$ZYGO" \
        >>"${1:-/tmp/supervisor.log}" 2>&1 &
    SUPERVISOR_PID=$!
}
