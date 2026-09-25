#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Requirement N6: one static binary, no runtime dependencies, small enough to
# `curl | sh`.
#
# The budget lives here rather than in three copies — `make dist-linux`, the
# `static-binary` CI job and the release workflow all run this — because a
# budget that is written down three times is a budget that is enforced at
# whichever number was edited last.
#
#   sh poc/check_dist.sh target/x86_64-unknown-linux-musl/release/zygo
#
# `ZYGO_SIZE_BUDGET_MB` overrides the limit, for measuring rather than for
# passing.
set -u

BIN=${1:-}
if [ -z "$BIN" ]; then
    echo "usage: check_dist.sh <binary>" >&2
    exit 2
fi
if [ ! -f "$BIN" ]; then
    echo "no such binary: $BIN" >&2
    exit 2
fi

BUDGET_MB=${ZYGO_SIZE_BUDGET_MB:-15}
BUDGET=$((BUDGET_MB * 1048576))

# `wc -c`, not `stat`: the flag for a file's size is `-c %s` on GNU and
# `-f %z` on BSD, and this runs on both.
SIZE=$(wc -c <"$BIN" | tr -d ' ')

DESCRIPTION=$(file "$BIN")
echo "$DESCRIPTION"
echo "size: $((SIZE / 1048576)) MB ($SIZE bytes), budget ${BUDGET_MB} MB"

FAIL=0

# `static-pie linked` and `statically linked` are both static: what N6 asks is
# whether there is a dynamic interpreter to satisfy at run time, and neither
# has one. x86_64 musl produces the PIE form and aarch64 does not, so matching
# only the second phrase fails on the architecture it was not written on.
if ! echo "$DESCRIPTION" | grep -qE "static(ally|-pie) linked"; then
    echo "::error::the binary is not statically linked"
    FAIL=1
fi
if echo "$DESCRIPTION" | grep -q "dynamically linked"; then
    echo "::error::the binary has a dynamic interpreter"
    FAIL=1
fi
if [ "$SIZE" -ge "$BUDGET" ]; then
    echo "::error::binary is $((SIZE / 1048576)) MB, over the ${BUDGET_MB} MB budget"
    FAIL=1
fi

exit "$FAIL"
