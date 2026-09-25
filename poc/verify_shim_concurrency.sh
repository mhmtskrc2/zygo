#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Concurrent `zygo run` from a Mac, which is Z-2 of the first adoption report.
#
# Every forwarded command is a session on one multiplexed SSH connection, and
# the guest's sshd caps how many one connection may carry (`MaxSessions`, ten
# by default). Past sixteen at once some fraction failed with SSH's
# `Session open refused by peer`, exit 255, before the guest ran anything.
# An adopter measured 7 failures in 144 at width 24 — about one in twenty.
#
# This fires 24 at once, six rounds, and wants 144 of 144. Two things fixed
# it and both are in the product: the template raises `MaxSessions` in the
# guest, and the shim retries a session the peer refused. On a VM created
# before the template change the retry alone should still get to 144 — that
# is what it is for — and `zygo doctor` says the VM is stale.
#
# macOS only: there is no shim on Linux. Run:  make verify-shim-concurrency
set -u

ZYGO=${ZYGO:-./target/release/zygo}
WIDTH=${WIDTH:-24}
ROUNDS=${ROUNDS:-6}

say() { printf '%s\n' "$*"; }

if [ "$(uname -s)" != Darwin ]; then
    say "  this suite is about macOS; nothing to check on $(uname -s)"
    exit 0
fi
if ! command -v limactl >/dev/null 2>&1; then
    say "  SKIP  \`limactl\` is not installed, so there is no VM to forward to"
    exit 0
fi
if [ ! -x "$ZYGO" ]; then
    say "  FAIL  no binary at $ZYGO — cargo build --release first"
    exit 1
fi
ZYGO=$(cd "$(dirname "$ZYGO")" && pwd)/$(basename "$ZYGO")

say "macOS shim concurrency: $WIDTH at once, $ROUNDS rounds"

# One ordinary command first: starts the VM if it is down, installs the
# binary if it changed, pulls the image — none of which is what is timed.
"$ZYGO" run alpine:3 true >/dev/null 2>&1

work=$(mktemp -d "${TMPDIR:-/tmp}/zygo-conc-XXXXXX")
failures=0
transport=0
total=0
round=0
while [ "$round" -lt "$ROUNDS" ]; do
    round=$((round + 1))
    i=0
    while [ "$i" -lt "$WIDTH" ]; do
        i=$((i + 1))
        (
            "$ZYGO" run alpine:3 true >"$work/$round.$i.out" 2>"$work/$round.$i.err"
            echo $? >"$work/$round.$i.rc"
        ) &
    done
    wait
    i=0
    bad_this_round=0
    while [ "$i" -lt "$WIDTH" ]; do
        i=$((i + 1))
        total=$((total + 1))
        rc=$(cat "$work/$round.$i.rc")
        if [ "$rc" != 0 ]; then
            failures=$((failures + 1))
            bad_this_round=$((bad_this_round + 1))
            # 111 is the shim's own "could not reach the VM"; 255 is SSH's,
            # which the shim should have turned into 111 or retried away.
            if [ "$rc" = 111 ] || grep -q "refused by peer\|mux_client_request_session" "$work/$round.$i.err"; then
                transport=$((transport + 1))
            fi
        fi
    done
    say "  round $round: $((WIDTH - bad_this_round))/$WIDTH"
done

say ""
if [ "$failures" -eq 0 ]; then
    say "  PASS  $total/$total at width $WIDTH"
else
    say "  FAIL  $failures of $total failed ($transport of them the SSH session cap)"
    say "        the adopter's baseline before the fix was about 7 in 144"
    grep -h . "$work"/*.err 2>/dev/null | sort | uniq -c | sort -rn | head -5 | sed 's/^/        /'
fi
rm -rf "$work"
[ "$failures" -eq 0 ]
