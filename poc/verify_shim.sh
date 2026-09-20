#!/bin/sh
# The macOS shim (todo.md, phase 5), end to end.
#
# Runs **on macOS**, against a real Lima VM. Nothing here inspects the shim's
# decisions — those are unit tests in `crates/zygo-cli/src/shim.rs` and they
# run on every platform. What this suite is for is the part unit tests cannot
# reach: whether a command typed on a Mac reaches a Linux kernel, comes back
# with the right bytes and the right exit status, and finds the same files the
# user is looking at.
#
# The VM is created on first use and left running afterwards. That is the
# product's own behaviour and the suite does not work around it: a run that
# started its own VM some other way would not be measuring the shim.
#
# Run:  make verify-shim
set -u

ZYGO=${ZYGO:-./target/release/zygo}
PASS=0
FAIL=0

say() { printf '%s\n' "$*"; }

# Milliseconds since the epoch. BSD `date` has no `%N` — on macOS
# `date +%s%N` yields a literal `N`, which `$(( ))` then rejects with "value
# too great for base". Python is on every Mac and is the portable answer.
now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }
ok() { PASS=$((PASS + 1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL + 1)); say "  FAIL  $*"; }

say "macOS shim verification"

if [ "$(uname -s)" != Darwin ]; then
    say "  this suite is about macOS; nothing to check on $(uname -s)"
    exit 0
fi
if ! command -v limactl >/dev/null 2>&1; then
    # A missing prerequisite skips, and says so once. It is not a result.
    say "  SKIP  \`limactl\` is not installed, so there is no VM to forward to"
    say "        → brew install lima"
    exit 0
fi
if [ ! -x "$ZYGO" ]; then
    say "  FAIL  no binary at $ZYGO — cargo build --release first"
    exit 1
fi
ZYGO=$(cd "$(dirname "$ZYGO")" && pwd)/$(basename "$ZYGO")

say "  $(sw_vers -productName) $(sw_vers -productVersion), $(limactl --version 2>&1 | head -1)"

# Start from a running VM, because the section below asks `doctor` what the VM
# says about itself and a stopped one says nothing. The suite's *last* act is
# `stop --all`, which now stops the VM too — so running this twice in a row
# failed the second time, on a check about `doctor` rather than about
# anything the run had done. One ordinary command is what a user's first
# command does anyway.
"$ZYGO" ps >/dev/null 2>&1
say ""

# ---------------------------------------------------------------------------
say "what stays on this side"

# `doctor` must answer here, because the useful answer is about *both* sides.
# Checked by its content rather than its speed: a fast wrong answer passes a
# timing test.
out=$("$ZYGO" doctor 2>&1)
case $out in
    *"has no kernel to build a sandbox in"*) ok "\`doctor\` reports the host honestly: macOS has no kernel to sandbox in" ;;
    *) bad "doctor did not report the platform: $(printf '%s' "$out" | head -1)" ;;
esac
case $out in
    *"what that VM says about itself"*) ok "and appends what the Linux VM says about itself" ;;
    *) bad "doctor said nothing about the VM: $(printf '%s' "$out" | tail -3 | tr '\n' ' ')" ;;
esac

# The protocol suite tests an agent on *this* machine's interpreter. Forwarding
# it would test the VM's, which is the opposite of what was asked.
if out=$("$ZYGO" agent test /bin/sh -- examples/agents/sh/agent.sh \
    examples/agents/sh/handler.sh 2>&1); then
    case $out in
        *"conforms to protocol"*) ok "\`agent test\` runs here, against this machine's own interpreter" ;;
        *) bad "agent test gave an unexpected answer: $(printf '%s' "$out" | tail -1)" ;;
    esac
else
    bad "agent test failed on the host: $(printf '%s' "$out" | tail -1)"
fi

say ""
# ---------------------------------------------------------------------------
say "the path contract"

# Only `$HOME` is shared, and a command from outside it must be refused rather
# than run somewhere that merely exists on both sides. Attempted, not read: the
# check runs `zygo` from `/tmp` and reads what it says.
outside=$(mktemp -d /tmp/zygo-shim-XXXXXX)
out=$(cd "$outside" && "$ZYGO" ps 2>&1)
rc=$?
rmdir "$outside" 2>/dev/null
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q "$HOME"; then
    ok "a command from outside \$HOME is refused, and the message names both directories"
else
    bad "running from $outside was not refused (exit $rc): $(printf '%s' "$out" | head -1)"
fi

say ""
# ---------------------------------------------------------------------------
say "reaching a Linux kernel"

work=$HOME/.zygo-shim-check
rm -rf "$work"
mkdir -p "$work" || exit 1
cd "$work" || exit 1

# Whether the VM was already up decides what this number means, so it is
# read before the command rather than guessed at afterwards.
was=$(limactl list --format '{{.Status}}' zygo 2>/dev/null | head -1)
[ "$was" = Running ] && how="the VM was already running" || how="including starting the VM"
started=$(now_ms)
out=$("$ZYGO" run python:3.12-slim python -c 'import platform; print(platform.system(), platform.release())' 2>&1)
elapsed=$((($(now_ms) - started) / 1000))
case $out in
    Linux\ *) ok "\`run\` on a Mac produced output from a Linux kernel: $out (${elapsed}s, $how)" ;;
    *) bad "run did not reach Linux: $(printf '%s' "$out" | tail -2 | tr '\n' ' ')" ;;
esac

# An exit code is not a message. A program that fails inside the VM has to
# fail here with the same number, or a script on the Mac cannot branch on it.
"$ZYGO" run python:3.12-slim python -c 'raise SystemExit(7)' >/dev/null 2>&1
rc=$?
[ "$rc" -eq 7 ] && ok "a program's exit status comes back across the VM (7)" ||
    bad "exit status was lost: wanted 7, got $rc"

# stdin has to cross too, or every pipeline on the host breaks at the VM.
out=$(printf 'over the wall\n' | "$ZYGO" run python:3.12-slim cat 2>&1)
[ "$out" = "over the wall" ] && ok "stdin crosses into the sandbox and back out" ||
    bad "stdin did not cross: got '$out'"

say ""
# ---------------------------------------------------------------------------
say "a warm function, from here"

cat > handler.py <<'PY'
import platform


def handler(event):
    return {"echo": event, "kernel": platform.release()}
PY

if "$ZYGO" serve handler.py --name shimcheck >/tmp/shim-serve.log 2>&1; then
    ok "\`serve\` warms a function whose handler is a file in *this* directory"
else
    bad "could not serve: $(grep -v '^$' /tmp/shim-serve.log | tail -2 | tr '\n' ' ')"
fi

out=$("$ZYGO" exec shimcheck '{"from":"macOS"}' 2>&1 | tr -d '\n ')
case $out in
    *'"from":"macOS"'*) ok "\`exec\` carries the event in and the result back" ;;
    *) bad "exec did not round-trip the event: $out" ;;
esac

# The design's number for a second command is 100 ms. Measured with the VM
# already up, which is what the number is about — and reported either way,
# because a latency check that only prints when it wins is not a measurement.
started=$(now_ms)
"$ZYGO" exec shimcheck '{}' >/dev/null 2>&1
ms=$(($(now_ms) - started))
if [ "$ms" -lt 250 ]; then
    ok "a warm request from the Mac took ${ms} ms, inside the 250 ms this check allows"
else
    bad "a warm request took ${ms} ms, over the 250 ms allowed (design target: 100 ms)"
fi

out=$("$ZYGO" ps 2>&1)
case $out in
    *shimcheck*warm*) ok "\`ps\` on the Mac lists the function running in the VM" ;;
    *) bad "ps did not show the function: $(printf '%s' "$out" | tail -1)" ;;
esac

say ""
# ---------------------------------------------------------------------------
say "the VM keeps up with this binary"

# A `brew upgrade` replaces the binary here and must replace the one in there.
# Attempted: the guest's copy is deleted and a command is run.
limactl shell zygo -- sudo rm -f /usr/local/bin/zygo >/dev/null 2>&1
rm -f "${XDG_CACHE_HOME:-$HOME/.cache}/zygo/vm-zygo.stamp"
if "$ZYGO" ps >/dev/null 2>&1; then
    ok "a missing Linux binary in the VM is replaced by the next command"
else
    bad "the shim did not reinstall the Linux binary: $("$ZYGO" ps 2>&1 | head -1)"
fi

say ""
# ---------------------------------------------------------------------------
say "cleaning up"

"$ZYGO" stop --all >/dev/null 2>&1

# "Stop everything" includes the machine it was all running in. Read from
# `limactl` rather than from Zygo, because Zygo saying it stopped the VM is
# the claim under test.
state=$(limactl list --format '{{.Status}}' zygo 2>/dev/null | head -1)
if [ "$state" = Running ]; then
    bad "the VM is still running after \`stop --all\`; an idle Mac is holding one"
else
    ok "\`stop --all\` stops the VM too — it is $state"
fi

# And now the strong version of "nothing survived": the next command brings
# the VM back from a full stop, and the function must not be in it.
started=$(now_ms)
out=$("$ZYGO" ps 2>&1)
back=$((($(now_ms) - started) / 1000))
case $out in
    *shimcheck*) bad "the function came back with the VM" ;;
    *) ok "the next command restarts the VM (${back}s) and the function is not in it" ;;
esac

cd / || exit 1
rm -rf "$work"

say ""
say "----------------------------------------"
say "macOS shim verification: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
