#!/bin/sh
# The `gvisor` backend against a real `runsc` (todo.md, phase 4).
#
# Requirement N8 is that the same spec means the same thing on every backend.
# The only way to check that is to run the same command on two of them and
# compare, which is what the last section here does — the rest establishes
# that the backend works at all, and that the things it cannot do are refused
# rather than quietly weakened.
#
# `runsc` is ~114 MB and is downloaded on first use into $ZYGO_DATA_HOME; the
# make target keeps that in a volume so a second run is fast.
#
# Run:  make gvisor-linux
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux}
IMAGE=alpine:3
PASS=0
FAIL=0

# Two environments need opposite cgroup preparation; the shared prelude picks.
. "$(dirname "$0")/cgroup_harness.sh"

say() { printf '%s\n' "$*"; }
ok()  { PASS=$((PASS+1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); say "  FAIL  $*"; }

say "gvisor backend"
say "  host kernel $(uname -r)"

zygo pull "$IMAGE" >/dev/null 2>&1

# The `ns` side of the comparison is captured *first*, before anything else
# has run. Two reasons: it is the baseline, and a failing baseline makes the
# comparison vacuous; and `ns` is what builds the cgroup hierarchy the
# harness's wrapper then places every later invocation in.
say ""
say "the ns baseline, before anything else"
probe_1='/bin/echo same-on-both'
probe_2='/bin/sh -c "id -u"'
probe_3='/bin/sh -c "echo x > /tmp/f && cat /tmp/f"'
ns_1=$(eval zygo run --isolation ns "$IMAGE" $probe_1 2>/dev/null); ns_rc_1=$?
ns_2=$(eval zygo run --isolation ns "$IMAGE" $probe_2 2>/dev/null); ns_rc_2=$?
ns_3=$(eval zygo run --isolation ns "$IMAGE" $probe_3 2>/dev/null); ns_rc_3=$?
ns_kernel=$(zygo run --isolation ns "$IMAGE" /bin/uname -r 2>/dev/null)
if [ "$ns_rc_1" -eq 0 ] && [ "$ns_1" = "same-on-both" ] && [ -n "$ns_kernel" ]; then
    ok "the ns backend runs here, so it is a baseline worth comparing against"
else
    bad "the ns baseline does not work (exit $ns_rc_1, '$ns_1'); every comparison below would be vacuous"
fi

say ""
say "installing the runtime"
if zygo backend list 2>/dev/null | grep -q '^gvisor  *available'; then
    ok "runsc is already installed"
else
    if zygo backend install gvisor >/tmp/gvisor-install.log 2>&1; then
        ok "\`backend install gvisor\` downloaded and verified runsc"
    else
        bad "install: $(tail -2 /tmp/gvisor-install.log | tr '\n' ' ' | cut -c1-200)"
        say ""
        say "gvisor verification: $PASS passed, $FAIL failed"
        exit 1
    fi
fi

# Split on `{` so each line is one object, then ask *that* object about its
# status. Matching `"backend":"gvisor","status":"available"` as one string
# would be asserting serde's key order, which is alphabetical and puts
# `detail` between them — the mistake this suite has now made three times
# (docs/poc-report.md, the inventory of bugs found in the tests themselves).
available=$(zygo --json backend list 2>/dev/null | tr -d '\n ' | tr '{' '\n' \
    | grep '"backend":"gvisor"' | grep -o '"status":"available"')
if [ -n "$available" ]; then
    ok "\`backend list\` reports gvisor available"
else
    bad "backend list: $(zygo backend list 2>&1 | tr '\n' ' ' | cut -c1-160)"
fi

say ""
say "running something"
out=$(zygo run --isolation gvisor "$IMAGE" /bin/echo hello-from-gvisor 2>/dev/null)
rc=$?
if [ "$out" = "hello-from-gvisor" ] && [ "$rc" -eq 0 ]; then
    ok "a one-shot sandbox runs and its stdout comes back (exit 0)"
else
    bad "one-shot run: exit $rc, output '$out'"
fi

# The whole point of the backend. `ns` shares the host's kernel and reports
# its version; gVisor's Sentry is a different kernel and says so. If this
# reported the host's, the sandbox would be running somewhere unintended.
guest=$(zygo run --isolation gvisor "$IMAGE" /bin/uname -r 2>/dev/null)
host=$(uname -r)
case "$guest" in
    *gvisor*)
        [ "$guest" != "$host" ] \
            && ok "the guest kernel is gVisor's Sentry, not the host's ($guest vs $host)" \
            || bad "the guest reports the host's kernel" ;;
    *) bad "guest kernel is '$guest', expected a gvisor one (host is $host)" ;;
esac

# Through `sh`, because alpine keeps `id` at /usr/bin and resolves it on PATH.
uid=$(zygo run --isolation gvisor "$IMAGE" /bin/sh -c 'id -u' 2>/dev/null)
[ "$uid" = "1000" ] \
    && ok "the sandbox runs as the spec's uid, not root (uid $uid)" \
    || bad "uid inside the sandbox is '$uid', expected 1000"

# A program's own exit code is what a script branches on, and it has to
# survive two process boundaries: the program, runsc, and zygo.
zygo run --isolation gvisor "$IMAGE" /bin/sh -c 'exit 7' >/dev/null 2>&1
rc=$?
[ "$rc" -eq 7 ] \
    && ok "the program's exit code passes through runsc and zygo (exit 7)" \
    || bad "exit code passthrough: got $rc, wanted 7"

echo "on stdin" | zygo run --isolation gvisor "$IMAGE" /bin/cat >/tmp/gvisor-stdin.out 2>/dev/null
if [ "$(cat /tmp/gvisor-stdin.out)" = "on stdin" ]; then
    ok "stdin reaches the sandboxed program"
else
    bad "stdin: got '$(cat /tmp/gvisor-stdin.out)'"
fi

say ""
say "the sandbox's shape"
# The mount plan reached runsc: these are the plan's own masked paths and its
# pid namespace, checked from inside rather than read off the bundle.
out=$(zygo run --isolation gvisor "$IMAGE" /bin/sh -c 'cat /proc/kcore >/dev/null 2>&1 && echo READABLE || echo absent' 2>/dev/null)
[ "$out" = "absent" ] \
    && ok "/proc/kcore is not readable (the plan's masked paths were applied)" \
    || bad "/proc/kcore: $out"

out=$(zygo run --isolation gvisor "$IMAGE" /bin/sh -c 'cat /proc/1/comm' 2>/dev/null)
case "$out" in
    sh|echo|cat) ok "pid 1 is the sandboxed program: its own pid namespace ($out)" ;;
    *) bad "pid 1 inside the sandbox is '$out'" ;;
esac

out=$(zygo run --isolation gvisor "$IMAGE" /bin/sh -c 'ls /proc | grep -c "^[0-9]*$"' 2>/dev/null)
if [ "${out:-99}" -lt 10 ] 2>/dev/null; then
    ok "the sandbox sees only its own processes ($out in /proc)"
else
    bad "the sandbox can see $out processes"
fi

# Writing to the root must fail: `root.readonly` is the plan's guarantee.
out=$(zygo run --isolation gvisor "$IMAGE" /bin/sh -c 'echo x > /rootwrite 2>/dev/null && echo WROTE || echo refused' 2>/dev/null)
[ "$out" = "refused" ] \
    && ok "the root filesystem is read-only" \
    || bad "the sandbox wrote to its root: $out"

# ...but /tmp is the plan's tmpfs and must work, or "read-only" above would
# just mean "nothing works".
out=$(zygo run --isolation gvisor "$IMAGE" /bin/sh -c 'echo x > /tmp/f && cat /tmp/f' 2>/dev/null)
[ "$out" = "x" ] \
    && ok "and /tmp is writable, so the root being read-only is a boundary and not a breakage" \
    || bad "/tmp is not writable: $out"

say ""
say "what the backend refuses"
# Each of these is something the backend cannot do. Refusing with a reason is
# the contract; running something weaker without saying so is not.
out=$(zygo run --isolation gvisor --net egress "$IMAGE" /bin/true 2>&1)
case "$out" in
    *network*none*) ok "a networked sandbox is refused by name, not silently sealed" ;;
    *) bad "networked run: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac

# A warm-exec function holds its sandbox open, which is the `hold` the backend
# cannot do. Checked through `up` on an alpine `cmd` so this needs no second
# image and no agent.
mkdir -p /tmp/gvisor-work && cd /tmp/gvisor-work || exit 1
cat > sandbox.toml <<'TOML'
[fn.held]
image     = "alpine:3"
isolation = "gvisor"
cmd       = ["/bin/cat"]
TOML
out=$(zygo up 2>&1)
case "$out" in
    *"runsc exec"*) ok "a warm function on gvisor is refused, pointing at what it would need" ;;
    *) bad "warm up: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-200)" ;;
esac
zygo stop --all >/dev/null 2>&1
cd /tmp || exit 1

say ""
say "the same spec on two backends (requirement N8)"
# The acceptance criterion for this phase: the same command, the same answer,
# whichever backend drew the boundary. The `ns` side was captured at the top.
compare() {
    probe=$1; ns_out=$2; ns_rc=$3
    gv_out=$(eval zygo run --isolation gvisor "$IMAGE" $probe 2>/dev/null)
    gv_rc=$?
    if [ "$ns_rc" -ne 0 ]; then
        bad "the ns baseline for \`$probe\` failed (exit $ns_rc); this comparison is vacuous"
    elif [ "$ns_out" = "$gv_out" ] && [ "$ns_rc" -eq "$gv_rc" ]; then
        ok "\`$probe\` gives the same answer on ns and gvisor ('$gv_out')"
    else
        bad "\`$probe\`: ns gave '$ns_out' (exit $ns_rc), gvisor gave '$gv_out' (exit $gv_rc)"
    fi
}
compare "$probe_1" "$ns_1" "$ns_rc_1"
compare "$probe_2" "$ns_2" "$ns_rc_2"
compare "$probe_3" "$ns_3" "$ns_rc_3"

# And one thing that must differ, or the comparison above proves only that
# both backends ran the same binary rather than that either isolated it.
gv_kernel=$(zygo run --isolation gvisor "$IMAGE" /bin/uname -r 2>/dev/null)
[ "$ns_kernel" != "$gv_kernel" ] \
    && ok "and the kernel differs, which is the whole point of the backend ($ns_kernel vs $gv_kernel)" \
    || bad "both backends report the same kernel '$ns_kernel'"

say ""
say "----------------------------------------"
say "gvisor verification: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ] || exit 1
