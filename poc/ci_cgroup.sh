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

if [ -w "/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup 2>/dev/null | head -1)" ]; then
    echo "ci_cgroup: the current cgroup is already writable; nothing to do"
else
    if sudo -n mkdir -p /sys/fs/cgroup/zygo-ci 2>/dev/null &&
        sudo -n chown -R "$(id -u):$(id -g)" /sys/fs/cgroup/zygo-ci 2>/dev/null; then
        # The controllers have to be available *below* the root before a child
        # can use them; the root cgroup is exempt from "no internal processes"
        # so enabling them there is allowed even though it holds everything.
        if ! echo '+memory +pids +cpu' |
            sudo -n tee /sys/fs/cgroup/cgroup.subtree_control >/dev/null 2>&1; then
            echo "ci_cgroup: could not enable controllers at the root"
        fi
        # Root has to do the move: cgroup v2 only lets an unprivileged process
        # migrate within a subtree it already owns, and this one's ancestor is
        # root's.
        if ! echo $$ | sudo -n tee /sys/fs/cgroup/zygo-ci/cgroup.procs >/dev/null 2>&1; then
            echo "ci_cgroup: could not move this shell into zygo-ci"
        fi
    else
        echo "ci_cgroup: could not create /sys/fs/cgroup/zygo-ci"
    fi

    now=$(sed -n 's/^0:://p' /proc/self/cgroup 2>/dev/null | head -1)
    if [ "$now" = /zygo-ci ]; then
        echo "ci_cgroup: this shell is now in /sys/fs/cgroup/zygo-ci"
    else
        # Not fatal: the harness says the same thing in its own words, and a
        # suite that runs and reports honestly beats a step that never starts.
        echo "::warning::ci_cgroup: no delegated cgroup (still ${now:-unknown});" \
            "the suite will say which of its checks that invalidates"
    fi
fi
