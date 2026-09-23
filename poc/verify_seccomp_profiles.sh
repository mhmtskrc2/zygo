#!/bin/sh
# What the first adoption report found about seccomp, attempted on a kernel.
#
# Three things, each of which was a session to diagnose from the outside and
# is a few seconds to check from here:
#
#   1. Z-1: `shutil.copy2` across a mount boundary works under every profile.
#      `copy2` calls `listxattr`, which the allowlist did not name, so it
#      answered `EPERM` — reported by Python as `[Errno 1] Operation not
#      permitted` on a file that plainly exists. `pip install --target` is a
#      `copy2` per file, and it is the shape every "install this tenant's
#      packages" feature has.
#   2. Z-3: the three profiles are observably different from *inside* a
#      sandbox, not merely as lists. `unshare(2)` succeeds under `permissive`
#      and is refused under `default`; `socket(2)` succeeds under `default`
#      and is refused under `strict`. The unit tests prove the lists differ;
#      this proves the sandbox does.
#   3. `--seccomp` is visible in `--dry-run`, with where it came from, so the
#      flag can be seen to take effect without running anything.
#
# Run:  make verify-seccomp-profiles-linux
#   or, inside the Lima VM on a Mac:
#       SRC=$PWD sh poc/verify_seccomp_profiles.sh
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
PYIMAGE=python:3.12-slim
PASS=0
FAIL=0

. "$(dirname "$0")/cgroup_harness.sh"

say() { printf '%s\n' "$*"; }
ok()  { PASS=$((PASS+1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); say "  FAIL  $*"; }

# A directory the sandbox can write to through a mount: the boundary
# `copy2` has to cross. Under `$HOME` so that, on a Mac, the VM can see it.
OUT=${ZYGO_VERIFY_OUT:-${HOME:-/tmp}/.zygo-verify-seccomp}
rm -rf "$OUT"
mkdir -p "$OUT" || exit 1

say "seccomp profiles verification"
say "  kernel $(uname -r), $(uname -m)"
"$ZYGO" pull "$PYIMAGE" >/dev/null 2>&1

# ---------------------------------------------------------------------------
say ""
say "Z-1: shutil.copy2 across a mount boundary, under every profile"

COPY2='
import shutil, os
open("/tmp/x", "w").write("payload")
os.setxattr("/tmp/x", "user.zygo", b"1") if hasattr(os, "setxattr") else None
shutil.copy2("/tmp/x", "/out/x")
print(open("/out/x").read(), sorted(os.listxattr("/out/x")))
'
for profile in permissive default strict; do
    rm -f "$OUT/x"
    out=$(zygo run --seccomp "$profile" --mount "$OUT:/out:rw" "$PYIMAGE" \
        python3 -c "$COPY2" 2>&1)
    case $out in
        payload*) ok "copy2 under \`$profile\`: $out" ;;
        *"Operation not permitted"*) bad "copy2 under \`$profile\` hit EPERM — the xattr family is refused: $(printf '%s' "$out" | tail -1)" ;;
        *) bad "copy2 under \`$profile\`: $(printf '%s' "$out" | tail -1)" ;;
    esac
done

# Reading, listing, setting and removing, by every spelling Python reaches.
XATTR='
import os
p = "/tmp/a"; open(p, "w").close()
os.setxattr(p, "user.k", b"v")
assert os.getxattr(p, "user.k") == b"v"
assert "user.k" in os.listxattr(p)
fd = os.open(p, os.O_RDONLY)
assert "user.k" in os.listxattr(fd)
assert os.getxattr(fd, "user.k") == b"v"
os.removexattr(p, "user.k")
assert "user.k" not in os.listxattr(p)
print("xattr ok")
'
for profile in permissive default strict; do
    out=$(zygo run --seccomp "$profile" "$PYIMAGE" python3 -c "$XATTR" 2>&1)
    case $out in
        "xattr ok") ok "set, get, list and remove an xattr on /tmp under \`$profile\`" ;;
        *) bad "xattr calls under \`$profile\`: $(printf '%s' "$out" | tail -1)" ;;
    esac
done

# ---------------------------------------------------------------------------
say ""
say "Z-1: pip install --target into a mount"

# The exact shape the report ran: a networked one-shot, packages installed
# into a mounted directory, and then imported from it in a second sandbox
# that has no network at all.
rm -rf "$OUT/pkgs"
mkdir -p "$OUT/pkgs"
started=$(date +%s)
if out=$(zygo run --net full --timeout 120s --mount "$OUT/pkgs:/pkgs:rw" "$PYIMAGE" \
    python3 -m pip install --quiet --no-cache-dir --disable-pip-version-check \
    --target /pkgs python-dateutil 2>&1); then
    took=$(( $(date +%s) - started ))
    ok "pip install --target /pkgs python-dateutil (${took}s)"
else
    case $out in
        *"Operation not permitted"*) bad "pip install --target hit EPERM: $(printf '%s' "$out" | grep -m1 'Errno 1')" ;;
        *"No matching distribution"*|*"Could not find a version"*|*"ConnectionError"*)
            say "  SKIP  no route to PyPI from here: $(printf '%s' "$out" | tail -1 | cut -c1-100)" ;;
        *) bad "pip install --target failed: $(printf '%s' "$out" | tail -2 | tr '\n' ' ')" ;;
    esac
fi
if [ -d "$OUT/pkgs/dateutil" ]; then
    out=$(zygo run --mount "$OUT/pkgs:/pkgs:ro" --env PYTHONPATH=/pkgs "$PYIMAGE" \
        python3 -c 'import six, dateutil; print("imports", six.__version__, dateutil.__version__)' 2>&1)
    case $out in
        imports*) ok "six and dateutil import from the mount in a second, networkless sandbox: $out" ;;
        *) bad "import from /pkgs: $(printf '%s' "$out" | tail -1)" ;;
    esac
fi

# ---------------------------------------------------------------------------
say ""
say "Z-3: the three profiles are observably different from inside a sandbox"

# `unshare(CLONE_NEWUSER)`: in NEVER_ALLOWED, granted by `permissive` alone.
UNSHARE='
import ctypes, os
libc = ctypes.CDLL(None, use_errno=True)
r = libc.unshare(0x10000000)  # CLONE_NEWUSER
print("ok" if r == 0 else os.strerror(ctypes.get_errno()))
'
p=$(zygo run --seccomp permissive "$PYIMAGE" python3 -c "$UNSHARE" 2>&1)
d=$(zygo run --seccomp default "$PYIMAGE" python3 -c "$UNSHARE" 2>&1)
if [ "$p" = ok ] && [ "$d" = "Operation not permitted" ]; then
    ok "unshare(CLONE_NEWUSER): \`permissive\` allows, \`default\` refuses with EPERM"
else
    bad "unshare: permissive said '$p', default said '$d'"
fi

# `socket(2)`: in the base list, removed by `strict`.
SOCKET='
import socket
try:
    socket.socket(socket.AF_INET, socket.SOCK_STREAM).close(); print("ok")
except PermissionError as e:
    print(e.strerror)
'
d=$(zygo run --seccomp default "$PYIMAGE" python3 -c "$SOCKET" 2>&1)
s=$(zygo run --seccomp strict "$PYIMAGE" python3 -c "$SOCKET" 2>&1)
if [ "$d" = ok ] && [ "$s" = "Operation not permitted" ]; then
    ok "socket(AF_INET): \`default\` allows, \`strict\` refuses with EPERM"
else
    bad "socket: default said '$d', strict said '$s'"
fi

# ---------------------------------------------------------------------------
say ""
say "Z-3: --seccomp is visible in --dry-run"

plan=$(zygo run --dry-run --seccomp strict "$PYIMAGE" true 2>&1)
case $plan in
    *"strict"*"the --seccomp flag"*) ok "the text plan names the profile and where it came from" ;;
    *) bad "the text plan does not show the flag: $(printf '%s' "$plan" | grep -A1 seccomp | tr '\n' ' ')" ;;
esac
json=$(zygo --json run --dry-run --seccomp permissive "$PYIMAGE" true 2>&1)
profile=$(printf '%s' "$json" | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["seccomp"]["profile"], d["seccomp"]["source"], d["seccomp"]["allowed_syscalls"])' 2>&1)
case $profile in
    "permissive override "[0-9]*) ok "the JSON plan carries profile, source and the allowed-syscall count: $profile" ;;
    *) bad "the JSON plan: $profile" ;;
esac
a=$(zygo --json run --dry-run --seccomp strict "$PYIMAGE" true 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["seccomp"]["allowed_syscalls"])')
b=$(zygo --json run --dry-run --seccomp default "$PYIMAGE" true 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin)["seccomp"]["allowed_syscalls"])')
if [ "$a" -lt "$b" ] 2>/dev/null; then
    ok "two plans differ where the profiles do: strict allows $a syscalls, default $b"
else
    bad "the counts do not order: strict $a, default $b"
fi

rm -rf "$OUT"
say ""
say "----------------------------------------"
say "seccomp profiles verification: $PASS passed, $FAIL failed"
harness_verdict
[ "$FAIL" -eq 0 ]
