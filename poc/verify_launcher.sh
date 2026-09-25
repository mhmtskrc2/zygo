#!/bin/sh
# Integration check for the `ns` launcher.
#
# Running the program is the easy half. This asserts the boundary is actually
# there: that the sandbox cannot see the host's processes, cannot write to its
# own root, holds no capabilities, and is stopped by every limit it was given.
#
# Run:  docker run --rm --privileged -v "$PWD:/src:ro" -v /tmp/zygorun.sh:/r.sh:ro \
#           -e ZYGO_DATA_HOME=/tmp/zdata alpine:3 sh /src/poc/verify_launcher.sh
set -u

# Where the checkout is: `/src` inside the containers `make` starts, the
# workspace in CI. Everything below is relative to it.
SRC=${SRC:-/src}

ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
IMAGE=alpine:3
# Checks that need an allocator or an interpreter use this one.
PYIMAGE=python:3.12-slim
PASS=0
FAIL=0

# Two environments need opposite cgroup preparation; the shared prelude picks.
. "$(dirname "$0")/cgroup_harness.sh"

say() { printf '%s\n' "$*"; }
ok()  { PASS=$((PASS+1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); say "  FAIL  $*"; }

# Run a shell snippet inside a sandbox and echo its stdout.
sb() {
    zygo run "$IMAGE" /bin/sh -c "$1" 2>/tmp/sb.err
}

# What the last sandbox said on stderr, if anything — appended to a failure so
# an empty answer comes with its reason.
#
# `sb` discarded stderr, and in CI that turned ten distinct failures into ten
# blanks: `the sandbox sees  processes`, `CapEff = `, `seccomp mode is ''`.
# Every one of them was the same unread error. `sb` runs in a command
# substitution, so it cannot print a diagnostic itself — whatever it printed
# would be captured as the answer — but the file it writes outlives the
# subshell, and this reads it.
why() {
    said=$(grep -v '^$' /tmp/sb.err 2>/dev/null | tail -1 | cut -c1-160)
    [ -n "$said" ] && printf ' — the sandbox said: %s' "$said"
}

say "ns launcher verification"
say "  kernel $(uname -r)"
zygo pull "$IMAGE" >/dev/null 2>&1
say ""

# --- it runs at all ---------------------------------------------------------

out=$(sb 'echo alive')
[ "$out" = "alive" ] && ok "a program runs and its stdout reaches the caller" \
                     || bad "stdout passthrough: got '$out'"

zygo run "$IMAGE" /bin/sh -c 'exit 42' >/dev/null 2>&1
[ $? -eq 42 ] && ok "the exit code passes through" || bad "exit code passthrough"

out=$(echo "piped in" | zygo run "$IMAGE" /bin/cat 2>/dev/null)
[ "$out" = "piped in" ] && ok "stdin passes through" || bad "stdin passthrough: got '$out'"

say ""

# --- the boundary -----------------------------------------------------------

out=$(sb 'ls /proc | grep -c "^[0-9]*$"')
# Only the shell itself, and possibly its child, exist in a fresh pid namespace.
if [ "${out:-99}" -le 3 ] 2>/dev/null; then
    ok "pid namespace: the sandbox sees $out processes, not the host's"
else
    bad "pid namespace: the sandbox sees $out processes$(why)"
fi

out=$(sb 'echo $$')
[ "$out" = "1" ] && ok "the sandbox process is pid 1 in its own namespace" \
                 || bad "expected pid 1, got '$out'"

out=$(sb 'hostname')
[ "$out" = "default" ] && ok "uts namespace: hostname is the tenant name" \
                   || bad "uts namespace: hostname is '$out'"

out=$(sb 'cat /proc/self/status | grep ^CapEff | awk "{print \$2}"')
case "$out" in
    0000000000000000|0000000000000000*) ok "no capabilities: CapEff = $out" ;;
    *) bad "capabilities remain: CapEff = $out$(why)" ;;
esac

out=$(sb 'cat /proc/self/status | grep ^NoNewPrivs | awk "{print \$2}"')
[ "$out" = "1" ] && ok "no_new_privs is set" || bad "no_new_privs = '$out'$(why)"

# A fresh network namespace is not empty: the kernel auto-creates `tunl0` and
# `ip6tnl0` in every one when the tunnel modules are loaded. What matters is
# that the host's real interfaces are gone.
host_ifaces=$(awk -F: '/:/ {gsub(/ /,"",$1); print $1}' /proc/net/dev | sort)
sb_ifaces=$(sb 'awk -F: "/:/ {gsub(/ /,\"\",\$1); print \$1}" /proc/net/dev' | sort)
hidden=$(echo "$host_ifaces" | grep -vxF "$(echo "$sb_ifaces")" 2>/dev/null | tr '\n' ' ')
leaked=$(echo "$sb_ifaces" | grep -vxF "$(echo "$host_ifaces")" 2>/dev/null | tr '\n' ' ')

say "  note  host: $(echo "$host_ifaces" | tr '\n' ' ')"
say "  note  sandbox: $(echo "$sb_ifaces" | tr '\n' ' ')"
if [ -n "$hidden" ] && [ -z "$leaked" ]; then
    ok "network namespace: the host's $hidden is not reachable"
else
    bad "network namespace: hidden='$hidden' unexpected='$leaked'$(why)"
fi

out=$(sb 'ping -c1 -W1 127.0.0.1 >/dev/null 2>&1 && echo up || echo down')
[ "$out" = "up" ] && ok "loopback is up inside the sandbox" \
                  || say "  note  loopback check inconclusive ('$out'); ping needs a capability"

say ""

# --- the filesystem ---------------------------------------------------------

out=$(sb 'touch /newfile 2>&1 >/dev/null; echo done')
if sb 'touch /newfile 2>/dev/null && echo WRITABLE' | grep -q WRITABLE; then
    bad "the sandbox root is writable$(why)"
else
    ok "the sandbox root is read-only"
fi

sb 'touch /tmp/x && echo ok' | grep -q ok \
    && ok "/tmp is writable" || bad "/tmp is not writable$(why)"

# Masking replaces the path with /dev/null, so it stays *readable* and yields
# nothing. The wrong assertion here — "it must be unreadable" — passes on an
# unmasked kernel that simply denies the read, and fails on a correct mask.
bytes=$(sb 'head -c 64 /proc/kcore 2>/dev/null | wc -c')
node=$(sb 'ls -l /proc/kcore 2>/dev/null | grep -c "^c"')
if [ "${bytes:-1}" = "0" ] && [ "${node:-0}" = "1" ]; then
    ok "/proc/kcore is masked (a character device yielding 0 bytes)"
else
    bad "/proc/kcore leaked $bytes bytes (char device: $node)$(why)"
fi

sb 'echo 1 > /proc/sys/kernel/hostname 2>/dev/null && echo WRITABLE' | grep -q WRITABLE \
    && bad "/proc/sys is writable" || ok "/proc/sys is read-only"

out=$(sb 'ls /dev | tr "\n" " "')
say "  note  /dev contains: $out"
case "$out" in *null*) ok "/dev/null exists" ;; *) bad "/dev/null missing$(why)" ;; esac

# A bind mount needs its target to be the same kind of object as its source.
# Mount points used to be created as directories unconditionally, so a file
# mount failed with ENOTDIR — `./config.json:/app/config.json` did not work.
mkdir -p /tmp/bindsrc/dir
echo 'file-mount-works' > /tmp/bindsrc/file.txt
echo 'dir-mount-works' > /tmp/bindsrc/dir/note.txt

zygo run --mount /tmp/bindsrc/file.txt:/app/config.json:ro "$IMAGE" \
    /bin/cat /app/config.json 2>/dev/null | grep -q file-mount-works \
    && ok "a file bind mount lands on a file" \
    || bad "a file bind mount failed"

zygo run --mount /tmp/bindsrc/dir:/data:ro "$IMAGE" \
    /bin/cat /data/note.txt 2>/dev/null | grep -q dir-mount-works \
    && ok "a directory bind mount still works" \
    || bad "a directory bind mount broke"

out=$(zygo run --mount /tmp/bindsrc/file.txt:/app/config.json:ro "$IMAGE" \
      /bin/sh -c 'echo x > /app/config.json 2>/dev/null && echo WRITABLE || echo readonly' 2>/dev/null)
[ "$out" = "readonly" ] && ok "a read-only file mount cannot be written" \
                        || bad "a ro file mount was writable: $out"

# The host's filesystem must not be reachable at all.
sb 'ls /src 2>/dev/null && echo LEAKED' | grep -q LEAKED \
    && bad "the host's /src is visible inside the sandbox" \
    || ok "the host filesystem is not reachable"

say ""

# --- seccomp ----------------------------------------------------------------

say "seccomp and the terminal"

out=$(sb 'grep ^Seccomp: /proc/self/status | awk "{print \$2}"')
[ "$out" = "2" ] && ok "a filter is installed (Seccomp: 2 = filter mode)" \
                 || bad "seccomp mode is '$out', expected 2$(why)"

# The warm path is a fork. A filter that gates `clone` on its flags but gets the
# jump arithmetic wrong denies every fork — which is how this was first caught.
sb '(echo forked) 2>/dev/null' | grep -q forked \
    && ok "fork still works under the default filter" \
    || bad "the default filter denies fork — the warm path would be dead"

out=$(sb 'unshare -U true 2>&1 | head -1')
case "$out" in
    *"Operation not permitted"*) ok "unshare(CLONE_NEWUSER) is refused" ;;
    *) bad "unshare was not refused: $out$(why)" ;;
esac

# strict must still be able to *start* — the filter is installed immediately
# before execve, so a profile denying execve produces a sandbox that never runs.
zygo run --seccomp strict "$IMAGE" /bin/sh -c 'echo strict-runs' 2>/dev/null | grep -q strict-runs \
    && ok "the strict profile still starts a sandbox" \
    || bad "the strict profile cannot start a sandbox"

out=$(zygo run --seccomp strict "$IMAGE" /bin/sh -c \
      'wget -T1 -q -O- http://127.0.0.1 2>&1 | head -1' 2>/dev/null)
case "$out" in
    *"Operation not permitted"*) ok "the strict profile denies socket()" ;;
    *) bad "strict did not deny socket(): $out$(why)" ;;
esac

out=$(zygo run --seccomp permissive "$IMAGE" /bin/sh -c \
      'unshare -U true && echo allowed || echo refused' 2>/dev/null)
[ "$out" = "allowed" ] && ok "the permissive profile allows what default denies" \
                       || bad "permissive behaved like default: $out$(why)"

# Zygo is daemonless, so the sandbox inherits the caller's terminal rather than
# getting one of its own. A writable fd to that terminal is a way into the
# user's shell: TIOCSTI pushes characters into its *input* queue, which the
# shell reads as typed after Zygo exits. Measured working before the filter.
if [ -t 1 ]; then
    zygo pull "$PYIMAGE" >/dev/null 2>&1
    out=$(zygo run "$PYIMAGE" python3 -c '
import fcntl, os, sys
try:
    fcntl.ioctl(1, 0x5412, b"X")
    print("INJECTED")
except OSError as e:
    print("refused:%d" % e.errno)' 2>/dev/null)
    case "$out" in
        refused:1) ok "TIOCSTI terminal injection is refused (EPERM)" ;;
        INJECTED)  bad "TIOCSTI SUCCEEDED — the sandbox can type into your shell" ;;
        *)         bad "TIOCSTI check inconclusive: $out" ;;
    esac

    # stdout is deliberately left alone: piping or redirecting it would make
    # `isatty` false and the check would be testing the pipe, not the sandbox.
    # The verdict comes back as an exit code instead.
    if zygo run "$PYIMAGE" python3 -c \
        'import os, sys; sys.exit(0 if os.isatty(1) else 1)' 2>/dev/null; then
        ok "the terminal still works inside the sandbox (isatty)"
    else
        bad "blocking TIOCSTI broke ordinary terminal use$(why)"
    fi
    # `--tty` is the structural fix: the sandbox gets a pty of its own, so the
    # caller's terminal is not merely un-injectable but absent.
    #
    # The terminal's minor number comes back as an *exit code*. Capturing it
    # any other way means redirecting fd 1, which is the very thing being
    # measured — three earlier versions of this check reported `/dev/null`.
    minor='import os, sys; sys.exit(os.minor(os.fstat(1).st_rdev))'

    python3 -c "$minor"; caller_minor=$?
    zygo run "$PYIMAGE" python3 -c "$minor" 2>/dev/null; default_minor=$?
    zygo run --tty "$PYIMAGE" python3 -c "$minor" 2>/dev/null; own_minor=$?

    say "  note  terminal minor: caller $caller_minor · default $default_minor · --tty $own_minor"
    [ "$default_minor" = "$caller_minor" ] \
        && ok "by default the sandbox shares the caller's terminal (daemonless passthrough)" \
        || bad "default passthrough broken: sandbox $default_minor vs caller $caller_minor"
    [ "$own_minor" != "$caller_minor" ] \
        && ok "--tty gives the sandbox a terminal of its own (minor $own_minor)" \
        || bad "--tty did not isolate the terminal"

    zygo run --tty "$PYIMAGE" python3 -c 'print("relayed")' 2>/dev/null | grep -q relayed \
        && ok "--tty relays the sandbox's output back to the caller" \
        || bad "--tty lost the sandbox's output"
    zygo run --tty "$PYIMAGE" python3 -c 'raise SystemExit(7)' >/dev/null 2>&1
    [ $? -eq 7 ] && ok "--tty preserves the exit code" || bad "--tty lost the exit code"
else
    say "  skip  the terminal checks need a tty; run with 'docker run -t'"
fi

say ""

# --- Landlock ---------------------------------------------------------------
#
# Landlock needs Linux 5.13. On an older kernel the only thing that *can* be
# verified is that the launcher notices and carries on — a sandbox that refused
# to start, or one that silently claimed a restriction it does not have, would
# both be wrong.

say "landlock"

lsm=$(cat /sys/kernel/security/lsm 2>/dev/null || echo "unavailable")
say "  note  kernel LSMs: $lsm"

abi=$(zygo doctor --json 2>/dev/null | grep -A3 '"name": "landlock"' | grep '"detail"' | cut -d'"' -f4)
say "  note  zygo doctor reports: ${abi:-<no landlock line>}"

case "$lsm" in
    *landlock*)
        # 5.13+: the restriction itself is testable.
        sb 'touch /newfile-landlock 2>/dev/null && echo WRITABLE' | grep -q WRITABLE \
            && bad "landlock is active but the root is still writable" \
            || ok "landlock is active and the root is not writable"
        ;;
    *)
        # Below 5.13. Assert the degradation, which is what this kernel can show.
        case "$abi" in
            *unavailable*) ok "landlock is correctly reported as unavailable" ;;
            *) bad "landlock reported '$abi' on a kernel without the LSM" ;;
        esac
        sb 'echo still-runs' | grep -q still-runs \
            && ok "the sandbox starts anyway; landlock is defence in depth, not the boundary" \
            || bad "the sandbox failed to start without landlock"
        say "  skip  the restriction itself needs Linux 5.13+; this kernel is $(uname -r)"
        ;;
esac

say ""

# --- the limits -------------------------------------------------------------

say "limits"

# pids.max: a fork bomb must be capped, and the host must be unaffected.
before=$(ls /proc | grep -c '^[0-9]*$')
zygo run --pids 8 "$IMAGE" /bin/sh -c \
    'i=0; while [ $i -lt 200 ]; do sleep 5 & i=$((i+1)); done; echo spawned=$i' \
    >/tmp/pids.out 2>/tmp/pids.err
after=$(ls /proc | grep -c '^[0-9]*$')
if [ "$after" -le $((before + 5)) ]; then
    ok "pids.max: the fork bomb did not leak into the host ($before → $after)"
else
    bad "pids.max: host processes went $before → $after$(why)"
fi

# memory.max: a runaway allocation must be OOM-killed inside its own cgroup,
# and *promptly*. "Non-zero exit" is not enough of an assertion: with
# `memory.high` set and no swap the allocation is merely throttled, never
# killed, and what eventually stops it is the wall-clock timeout — which also
# exits non-zero, so a loose check passes while the limit does nothing useful.
if zygo pull "$PYIMAGE" >/dev/null 2>&1; then
    MEMIMG="$PYIMAGE"
    start=$(date +%s)
    zygo run --mem 128M --scratch 16M --timeout 20s "$MEMIMG" \
        python3 -c 'x = bytearray(400*1024*1024); print("ALLOCATED")' >/dev/null 2>&1
    status=$?
    elapsed=$(( $(date +%s) - start ))
    if [ "$status" = "137" ] && [ "$elapsed" -lt 10 ]; then
        ok "memory.max: OOM-killed in ${elapsed}s with SIGKILL (exit 137)"
    elif [ "$status" -ne 0 ]; then
        bad "memory.max: stopped after ${elapsed}s with exit $status, expected a prompt 137$(why)"
    else
        bad "memory.max: the allocation ran to completion$(why)"
    fi

    zygo run --mem 128M --scratch 16M --timeout 20s "$MEMIMG" \
        python3 -c 'x = bytearray(32*1024*1024)' >/dev/null 2>&1 \
        && ok "memory.max: a sandbox inside its limit is untouched" \
        || bad "memory.max: a sandbox inside its limit was killed"
else
    say "  skip  memory.max needs an image with an allocator; python:3.12-slim unavailable"
fi

# timeout: a program that never exits must be killed.
start=$(date +%s)
zygo run --timeout 2s "$IMAGE" /bin/sh -c 'sleep 60' >/dev/null 2>&1
status=$?
elapsed=$(( $(date +%s) - start ))
if [ "$status" -ne 0 ] && [ "$elapsed" -lt 15 ]; then
    ok "timeout: killed after ${elapsed}s (budget 2s), exit $status"
else
    bad "timeout: exit $status after ${elapsed}s$(why)"
fi

# The limits must actually be present in the cgroup, not merely requested.
say ""
say "cgroup state"
tenant=$(find "$CG" -type d -name run -path '*zygo.slice*' 2>/dev/null | head -1)
if [ -n "$tenant" ]; then
    say "  tenant cgroup: $tenant"
    for f in memory.max pids.max cpu.max; do
        say "    $f = $(cat "$tenant/$f" 2>/dev/null || echo MISSING)"
    done
else
    say "  note  the tenant cgroup was already cleaned up"
fi

say ""
say "----------------------------------------"
say "launcher verification: $PASS passed, $FAIL failed"
harness_verdict
[ "$FAIL" -eq 0 ] || exit 1
