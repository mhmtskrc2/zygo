# Give this shell a cgroup of its own. Sourced, not run.
#
# `poc/cgroup_harness.sh` needs to start in a cgroup it can write, and it has
# two ways to get one: the whole hierarchy, when it is root in a privileged
# container, or a delegated systemd scope, which it enters by itself on a host
# with a user session.
#
# A CI runner is neither. The job runs as an ordinary user under a system
# service, so the cgroup it starts in belongs to root, and there is no user
# bus for `systemd-run --user` to reach. Without this the suites run with
# `ZYGO_HARNESS=unusable` and report every cgroup assertion as a failure.
#
# `sudo` is the thing a runner does have, so delegation is done the plain way:
# make a cgroup, hand it to this user, and move this shell into it as root.
# The move has to be root's doing — cgroup v2 only lets an unprivileged
# process migrate within a subtree it already owns, and the ancestor here is
# root's.
#
# Every `sudo` is `sudo -n`, so sourcing this on a workstation whose sudo
# wants a password says it cannot rather than stopping to ask for one.
#
# Must be **sourced**, because `$$` has to be the shell that goes on to run
# the suite:
#
#     . poc/ci_cgroup.sh
#     sh poc/verify_launcher.sh
#
# Does nothing when the current cgroup is already writable, so it is harmless
# in a container and on a developer's machine.

# Nothing below may end on a non-zero status. GitHub runs each step under
# `bash -e`, and this file is *sourced* into that shell, so one failing
# command here would end the step before the suite it is preparing for ever
# starts. Every step is therefore guarded and reports rather than exits.

# One per invocation. The first step that used a shared `zygo-ci` left it
# delegating controllers to its children, and a cgroup that delegates may no
# longer hold processes — so every *later* step failed to move into it and ran
# with no cgroup at all, silently. A fresh name each time costs nothing and
# the kernel reclaims an empty cgroup when it is removed.
CI_CGROUP=zygo-ci-$$

if [ -w "/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup 2>/dev/null | head -1)" ]; then
    echo "ci_cgroup: the current cgroup is already writable; nothing to do"
else
    if sudo -n mkdir -p /sys/fs/cgroup/$CI_CGROUP 2>/dev/null &&
        sudo -n chown -R "$(id -u):$(id -g)" /sys/fs/cgroup/$CI_CGROUP 2>/dev/null; then
        # Move first, delegate second — the order is not a preference.
        # cgroup v2 forbids a cgroup from holding processes *and* delegating
        # controllers, and a container's `/sys/fs/cgroup` is its own cgroup
        # namespace root rather than the real one, so it does not get the real
        # root's exemption. Delegating first and moving afterwards fails the
        # move with `EIO`, which is the kernel saying exactly that.
        #
        # Root has to do the move: cgroup v2 only lets an unprivileged process
        # migrate within a subtree it already owns, and this one's ancestor is
        # root's.
        if ! echo $$ | sudo -n tee /sys/fs/cgroup/$CI_CGROUP/cgroup.procs >/dev/null 2>&1; then
            echo "ci_cgroup: could not move this shell into zygo-ci"
        fi
        # One controller at a time: the write is atomic, so a kernel that
        # refuses any one of them refuses the whole line, and a host that
        # could have delegated two of the three would delegate none.
        for c in memory pids cpu; do
            echo "+$c" | sudo -n tee /sys/fs/cgroup/cgroup.subtree_control \
                >/dev/null 2>&1 ||
                echo "ci_cgroup: this kernel will not delegate \`$c\` from the root"
        done
    else
        echo "ci_cgroup: could not create /sys/fs/cgroup/$CI_CGROUP"
    fi

    now=$(sed -n 's/^0:://p' /proc/self/cgroup 2>/dev/null | head -1)
    if [ "$now" = "/$CI_CGROUP" ]; then
        # Say what arrived, not what was asked for. Without the controllers
        # the move is worth nothing: `zygo` gets as far as creating its slice
        # and then finds no `memory.max` there.
        echo "ci_cgroup: this shell is now in /sys/fs/cgroup/$CI_CGROUP," \
            "with controllers: [$(cat /sys/fs/cgroup/$CI_CGROUP/cgroup.controllers 2>/dev/null)]"
    else
        # Not fatal: the harness says the same thing in its own words, and a
        # suite that runs and reports honestly beats a step that never starts.
        echo "::warning::ci_cgroup: no delegated cgroup (still ${now:-unknown});" \
            "the suite will say which of its checks that invalidates"
    fi
fi
