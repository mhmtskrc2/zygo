#!/bin/bash
# A sweep of the whole syscall space against each seccomp profile
# (todo.md, phase 4 — the fuzz-based extension of the escape suite).
#
# The escape suite attempts the vectors somebody thought of. This attempts
# *every* syscall number the architecture has, which is the only way to catch
# the failure the filter's shape makes possible: the branch offsets are
# computed, and one wrong offset silently allows a syscall nobody tested. The
# unit tests run a BPF interpreter over the program; this runs the kernel.
#
# Every call is made in a forked child with all-zero arguments, so a syscall
# that blocks, exits or changes process state affects one child and nothing
# else, and "the filter killed the process" is distinguishable from "the
# filter returned an errno". Zero arguments are almost always invalid, which
# is the point: what is asserted is *which answer came from the filter*, not
# what the syscall would have done.
#
# What it asserts, none of which needs a copy of the allowlist — a check that
# compared against a list would only be testing that list against itself:
#
#   1. the profiles are ordered by what they permit, on a real kernel:
#      permissive ⊋ default ⊋ strict;
#   2. no syscall kills the process — the filter's default action is EPERM,
#      and a kill would take a whole sandbox down instead of failing a call;
#   3. syscalls that must never be reachable are refused under every profile;
#   4. `clone3` answers ENOSYS and not EPERM, which is what glibc needs to
#      fall back to `clone` (the bug the compatibility matrix found).
#
# Run:  make fuzz-linux
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux}
IMAGE=python:3.12-slim
PASS=0
FAIL=0

# Two environments need opposite cgroup preparation; the shared prelude picks.
. "$(dirname "$0")/cgroup_harness.sh"

say() { printf '%s\n' "$*"; }
ok()  { PASS=$((PASS+1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); say "  FAIL  $*"; }

say "syscall sweep"
say "  kernel $(uname -r)  $(uname -m)"

mkdir -p /tmp/fuzz
cat > /tmp/fuzz/sweep.py <<'PYEOF'
"""Call every syscall number once, in a forked child, and report the answer.

Output is one line per number: `<nr> <verdict>`, where verdict is `ok`,
`errno:<n>`, `killed:<signal>` or `timeout`. Nothing is interpreted here —
the shell decides what the answers mean, so that this half stays a
measurement.
"""

import ctypes
import os
import platform
import signal
import struct
import sys

# The highest syscall number worth trying. Both tables are far below this;
# the tail is numbers the kernel does not implement, which return ENOSYS and
# are informative in their own right (they must not come back as EPERM).
HIGHEST = 470

# Numbers to leave alone: a syscall that would take the *sweeping* process
# down rather than the child, or wait forever in a way the timeout below
# cannot see. Everything else, including the dangerous-sounding ones, is
# attempted — zero arguments make them harmless and a fork contains them.
SKIP = {
    "aarch64": {
        128,  # restart_syscall — resumes whatever this child was not doing
        139,  # rt_sigreturn — returns onto a signal frame that is not there
    },
    "x86_64": {
        15,   # rt_sigreturn
        219,  # restart_syscall
    },
}.get(platform.machine(), set())

libc = ctypes.CDLL(None, use_errno=True)
libc.syscall.restype = ctypes.c_long


class Timeout(Exception):
    """Raised from the SIGALRM handler.

    It has to *raise*. A handler that returns leaves PEP 475 to restart the
    interrupted read, so the alarm fires, nothing happens, and the sweep
    blocks for ever on the first syscall that waits — which is exactly what
    the first version of this script did.
    """


def _on_alarm(*_):
    raise Timeout


def attempt(nr):
    """Call syscall `nr` in a fresh child. Never raises."""
    read_fd, write_fd = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(read_fd)
        try:
            ctypes.set_errno(0)
            rc = libc.syscall(nr, 0, 0, 0, 0, 0, 0)
            err = ctypes.get_errno() if rc == -1 else 0
            os.write(write_fd, struct.pack("<i", err))
        except BaseException:
            pass
        os._exit(0)

    os.close(write_fd)
    # The child either answers or is stopped; a syscall that blocks with zero
    # arguments (a few do) is cut short rather than hanging the sweep.
    signal.setitimer(signal.ITIMER_REAL, 1.0)
    answer = b""
    try:
        while len(answer) < 4:
            chunk = os.read(read_fd, 4 - len(answer))
            if not chunk:
                break
            answer += chunk
    except (Timeout, InterruptedError, OSError):
        pass
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        os.close(read_fd)

    if len(answer) < 4:
        os.kill(pid, signal.SIGKILL)
    _, status = os.waitpid(pid, 0)
    if len(answer) == 4:
        (err,) = struct.unpack("<i", answer)
        return "ok" if err == 0 else "errno:%d" % err
    if os.WIFSIGNALED(status) and os.WTERMSIG(status) != signal.SIGKILL:
        return "killed:%d" % os.WTERMSIG(status)
    return "timeout"


def main():
    signal.signal(signal.SIGALRM, _on_alarm)
    for nr in range(HIGHEST + 1):
        if nr in SKIP:
            continue
        sys.stdout.write("%d %s\n" % (nr, attempt(nr)))
    sys.stdout.flush()


main()
PYEOF

# One sweep per profile. `pids` has to be generous: the sweep forks once per
# syscall, and a `pids.max` of four would be measuring the cgroup instead.
sweep() {
    profile=$1
    zygo run --seccomp "$profile" --pids 64 --mem 256M --timeout 300s \
        --mount /tmp/fuzz/sweep.py:/sweep.py:ro \
        "$IMAGE" python3 /sweep.py 2>/tmp/fuzz/$profile.err
}

for profile in permissive default strict; do
    sweep "$profile" > "/tmp/fuzz/$profile.out"
    lines=$(wc -l < "/tmp/fuzz/$profile.out")
    if [ "$lines" -gt 400 ]; then
        ok "the sweep ran to the end under \`$profile\` ($lines syscalls attempted)"
    else
        bad "the sweep under \`$profile\` produced $lines lines: $(tail -2 /tmp/fuzz/$profile.err | tr '\n' ' ' | cut -c1-160)"
        continue
    fi
done

[ "$FAIL" -eq 0 ] || {
    say ""
    say "syscall sweep: $PASS passed, $FAIL failed"
    exit 1
}

# EPERM is what this filter answers for a syscall it does not allow. ENOSYS is
# the kernel saying the number is not a syscall at all, and is the deliberate
# exception for `clone3`.
denied() { grep -c ' errno:1$' "/tmp/fuzz/$1.out"; }
# `sort` and not `sort -n`: `comm` compares lines as strings and quietly
# produces nonsense on numerically sorted input, warning to stderr where a
# test harness never looks. The numbers are only ever set members here, so
# lexicographic order is the correct order.
allowed_set() { grep -v ' errno:1$' "/tmp/fuzz/$1.out" | cut -d' ' -f1 | sort; }

say ""
say "the profiles, ordered by what the kernel actually permitted"
for profile in permissive default strict; do
    say "  $profile: $(denied "$profile") of $(wc -l < "/tmp/fuzz/$profile.out") refused with EPERM"
done

# The ordering, on a real kernel rather than in the constants. A syscall that
# `strict` permits and `default` does not is a filter whose branches have
# moved — exactly the failure that produced a profile denying every `fork`.
for pair in "strict default" "default permissive"; do
    tighter=${pair% *}
    looser=${pair#* }
    extra=$(comm -23 <(allowed_set "$tighter") <(allowed_set "$looser") 2>/tmp/fuzz/comm.err | tr '\n' ' ')
    if [ -s /tmp/fuzz/comm.err ]; then
        bad "the set comparison is unsorted and its answer cannot be trusted: $(cat /tmp/fuzz/comm.err | tr '\n' ' ')"
        continue
    fi
    if [ -z "$extra" ]; then
        ok "everything \`$tighter\` permits, \`$looser\` permits too"
    else
        bad "\`$tighter\` permits syscalls \`$looser\` does not: $extra"
    fi
    fewer=$(comm -13 <(allowed_set "$tighter") <(allowed_set "$looser") | wc -l)
    if [ "$fewer" -gt 0 ]; then
        ok "and \`$tighter\` refuses $fewer that \`$looser\` allows"
    else
        bad "\`$tighter\` and \`$looser\` permit exactly the same set"
    fi
done

say ""
say "what the filter must never do"
# A filter whose default action is a kill turns one bad call into a dead
# sandbox, and takes every request in flight with it.
killed=$(grep -c ' killed:' /tmp/fuzz/default.out || true)
if [ "${killed:-0}" -eq 0 ]; then
    ok "no syscall killed the process: the default action is an errno, not SIGSYS"
else
    bad "$killed syscalls killed the process under \`default\`: $(grep ' killed:' /tmp/fuzz/default.out | head -3 | tr '\n' ' ')"
fi

# `clone3` is the one syscall the filter answers with ENOSYS rather than
# EPERM: glibc's pthread_create tries it first and falls back to `clone` only
# on ENOSYS. On EPERM every threaded program dies with "can't start new
# thread", which is what the compatibility matrix found.
case "$(uname -m)" in
    aarch64) clone3=435 ;;
    x86_64)  clone3=435 ;;
    *)       clone3="" ;;
esac
if [ -n "$clone3" ]; then
    # The positive case first: the number has to be a syscall this kernel
    # knows, or "it answered ENOSYS" would prove nothing at all.
    under_permissive=$(awk -v n="$clone3" '$1 == n {print $2}' /tmp/fuzz/permissive.out)
    case "$under_permissive" in
        errno:1|"")
            bad "clone3 is not reachable even under \`permissive\` ($under_permissive); the check below would be vacuous" ;;
        *)
            ok "clone3 is a syscall this kernel has (\`permissive\` answered $under_permissive)"
            for profile in default strict; do
                got=$(awk -v n="$clone3" '$1 == n {print $2}' "/tmp/fuzz/$profile.out")
                case "$got" in
                    errno:38) ok "and under \`$profile\` it answers ENOSYS, which is what glibc falls back from" ;;
                    errno:1)  bad "under \`$profile\` clone3 answers EPERM: every threaded program dies" ;;
                    *)        bad "under \`$profile\` clone3 answered $got" ;;
                esac
            done ;;
    esac
fi

# The syscalls the threat model names. Numbers are per architecture; a name
# with no number here is not checked rather than silently passing.
case "$(uname -m)" in
    aarch64) must_deny="ptrace:117 unshare:97 setns:268 mount:40 umount2:39 keyctl:219 perf_event_open:241 bpf:280 pivot_root:41 reboot:142 kexec_load:104 init_module:105 delete_module:106" ;;
    x86_64)  must_deny="ptrace:101 unshare:272 setns:308 mount:165 umount2:166 keyctl:250 perf_event_open:298 bpf:321 pivot_root:155 reboot:169 kexec_load:246 init_module:175 delete_module:176" ;;
    *)       must_deny="" ;;
esac
missed=""
for entry in $must_deny; do
    name=${entry%:*}
    nr=${entry#*:}
    for profile in default strict; do
        got=$(awk -v n="$nr" '$1 == n {print $2}' "/tmp/fuzz/$profile.out")
        [ "$got" = "errno:1" ] || missed="$missed $name($profile:$got)"
    done
done
if [ -z "$must_deny" ]; then
    say "  (no syscall numbers for $(uname -m); the must-deny list is not checked)"
elif [ -z "$missed" ]; then
    ok "every syscall the threat model names is refused with EPERM under \`default\` and \`strict\`"
else
    bad "reachable where the threat model says they are not:$missed"
fi

# And the counterpart: the sandbox is not merely broken. If nothing worked at
# all, every assertion above would pass for the wrong reason.
worked=$(grep -vc ' errno:1$' /tmp/fuzz/default.out || true)
if [ "${worked:-0}" -gt 50 ]; then
    ok "the sandbox was working throughout: $worked syscalls reached the kernel under \`default\`"
else
    bad "only $worked syscalls reached the kernel; the sweep proves nothing"
fi

say ""
say "----------------------------------------"
say "syscall sweep: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
