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
elif [ -z "${ZYGO_HARNESS_SCOPE:-}" ] && command -v systemd-run >/dev/null 2>&1 &&
    systemd-run --user --scope -q -- true >/dev/null 2>&1; then
    # A systemd session that has not delegated anything. Telling the caller to
    # re-run under `systemd-run` is a step they will forget, and forgetting it
    # does not look like a missing scope — it looks like a dozen ordinary
    # failures, which is how a Raspberry Pi run reported the launcher broken
    # when the launcher was fine. `zygo` itself steps into a delegated scope
    # rather than asking (crates/zygo-cli/src/scope.rs); the suites do the
    # same here, once, guarded so a scope that still cannot delegate falls
    # through to `unusable` below instead of re-execing forever.
    case $0 in
        /*) harness_suite=$0 ;;
        *) harness_suite=$PWD/$0 ;;
    esac
    # Re-exec through the interpreter already running, not through `$0`'s
    # executable bit: the suites are started as `sh poc/<name>.sh`, and one of
    # them is bash.
    if [ -n "${BASH_VERSION:-}" ]; then harness_sh=bash; else harness_sh=sh; fi

    ZYGO_HARNESS_SCOPE=1
    export ZYGO_HARNESS_SCOPE
    echo "  harness: no delegated cgroup here; re-running inside a scope of our own" >&2
    exec systemd-run --user --scope -p Delegate=yes -q -- \
        "$harness_sh" "$harness_suite" "$@"
else
    # Neither a writable hierarchy nor a delegated scope, and no way to make
    # one. Nothing below will work, and saying so here beats twenty failures
    # that look like the launcher's.
    ZYGO_HARNESS=unusable
    echo "  harness: cannot create a cgroup under $ZYGO_HARNESS_ROOT" >&2
    echo "  → in a container, run privileged; on a host, start this suite with" >&2
    echo "    systemd-run --user --scope -p Delegate=yes -- sh \$0" >&2
fi

# Printed under a suite's summary line.
#
# A failure count collected without a usable cgroup is not a count of bugs,
# and a summary that does not say so is read as one: an early Raspberry Pi run
# reported "121 passed, 13 failed" when the whole difference was this file
# having nowhere to build. The banner above scrolls past; this is at the
# bottom, next to the number somebody will quote.
harness_verdict() {
    [ "$ZYGO_HARNESS" = ready ] && return 0
    # On stdout, unlike the harness's other messages: this one has to land
    # directly under the summary, and a suite's summary goes to stdout.
    printf '%s\n' "" \
        "  NOTE  this run had no cgroup it could write, so anything resting on" \
        "        limits, or on reading a sandbox's processes, failed for that" \
        "        reason rather than its own. The count above is not a verdict."
}

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
