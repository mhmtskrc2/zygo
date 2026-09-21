#!/bin/sh
# Escape suite for the `ns` backend (design doc §3.10).
#
# Each case is an actual attempt to get out, not an inspection of settings: a
# test that only reads a flag passes on a kernel that ignores the flag.
#
# Each case also reports what it observed, so "blocked" can be checked rather
# than trusted. That rule was added after this suite reported a successful
# rewrite of `/proc/self/uid_map` that the kernel had in fact refused — see
# `attempt_write` below.
#
# Run:  make escape-linux
set -u

# Where the checkout is: `/src` inside the containers `make` starts, the
# workspace in CI. Everything below is relative to it.
SRC=${SRC:-/src}

ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
IMAGE=python:3.12-slim
PASS=0
FAIL=0
SKIP=0

# Two environments need opposite cgroup preparation; the shared prelude picks.
. "$(dirname "$0")/cgroup_harness.sh"

say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); say "  BLOCKED  $*"; }
bad()  { FAIL=$((FAIL+1)); say "  ESCAPED  $*"; }
skip() { SKIP=$((SKIP+1)); say "  skip     $*"; }

# Prelude for the cases that attempt a write.
#
# `open(path, "w").write(data)` is *not* a write test: the bytes go into
# Python's buffer and the flush happens in the file object's `__del__`, where
# an OSError is printed and swallowed rather than raised. That is why this
# suite once reported the sandbox rewriting its own `uid_map` when the kernel
# had refused it with EPERM. `os.write` on a raw descriptor reports the
# kernel's answer directly.
HELPER=$(cat <<'PYEOF'
import os

def attempt_write(path, data=b"x"):
    """Return "WROTE" or "refused:<errno>" — never a lie."""
    try:
        fd = os.open(path, os.O_WRONLY)
    except OSError as e:
        return "refused:%d" % e.errno
    try:
        os.write(fd, data)
        return "WROTE"
    except OSError as e:
        return "refused:%d" % e.errno
    finally:
        os.close(fd)
PYEOF
)

# Run Python inside a sandbox and echo its stdout.
py() { zygo run "$IMAGE" python3 -c "$1" 2>/tmp/escape.err; }

say "escape suite — every case is an attempt, not an inspection"
say "  kernel $(uname -r)"
zygo pull "$IMAGE" >/dev/null 2>&1

# Nothing below is a result unless a sandbox runs. Every case here reads the
# attempt's *output*, and an empty answer looks exactly like a refusal that
# printed nothing — so a suite that cannot start a sandbox at all reported
# **1 blocked, 16 escaped**, which is the most alarming way to say "the data
# directory was not writable". It was a CI step running unprivileged against
# a tree an earlier `sudo` step had created.
#
# Rule 4 from the README, at the top of the file it is about: a negative check
# must first prove the thing ran.
if [ "$(py 'print("alive")')" != alive ]; then
    say ""
    say "  the suite cannot start a sandbox, so none of its cases would mean"
    say "  anything. Nothing below has been attempted."
    say "  $(grep -v '^$' /tmp/escape.err 2>/dev/null | tr '\n' ' ' | cut -c1-300)"
    exit 1
fi
say ""

# --- 1. the runtime binary (CVE-2019-5736 shape) ----------------------------

say "1. overwrite the runtime binary through /proc/self/exe"
out=$(py "$HELPER
print(attempt_write('/proc/self/exe'))")
case "$out" in
    refused:*) ok "/proc/self/exe is not writable ($out)" ;;
    WROTE)     bad "the sandbox overwrote its own executable" ;;
    *)         bad "inconclusive: '$out'" ;;
esac

# --- 2. cgroup release_agent ------------------------------------------------

say "2. escape via cgroup release_agent"
out=$(py "$HELPER
paths = ['/sys/fs/cgroup/release_agent', '/sys/fs/cgroup/notify_on_release']
found = [p for p in paths if os.path.exists(p)]
print('absent' if not found else attempt_write(found[0], b'/tmp/x'))")
case "$out" in
    absent)    ok "cgroupfs is not mounted in the sandbox at all" ;;
    refused:*) ok "release_agent is not writable ($out)" ;;
    WROTE)     bad "release_agent was writable" ;;
    *)         bad "inconclusive: '$out'" ;;
esac

# --- 3. mounting anything ---------------------------------------------------

say "3. mount a filesystem to reach the host"
out=$(py "
import ctypes
libc = ctypes.CDLL('libc.so.6', use_errno=True)
rc = libc.mount(b'proc', b'/tmp', b'proc', 0, None)
print('MOUNTED' if rc == 0 else 'refused:%d' % ctypes.get_errno())")
case "$out" in
    refused:*) ok "mount() is refused ($out)" ;;
    MOUNTED)   bad "the sandbox mounted a filesystem" ;;
    *)         bad "inconclusive: '$out'" ;;
esac

# --- 4. joining another namespace -------------------------------------------

say "4. setns into the host's namespaces"
out=$(py "
import ctypes, os
libc = ctypes.CDLL('libc.so.6', use_errno=True)
try:
    fd = os.open('/proc/1/ns/net', os.O_RDONLY)
except OSError as e:
    print('no-such-namespace:%d' % e.errno)
else:
    rc = libc.setns(fd, 0)
    print('JOINED' if rc == 0 else 'refused:%d' % ctypes.get_errno())")
case "$out" in
    refused:*|no-such-namespace:*) ok "setns is refused ($out)" ;;
    JOINED) bad "the sandbox joined another namespace" ;;
    *)      bad "inconclusive: '$out'" ;;
esac

# --- 5. new namespaces (nesting out of the confinement) ---------------------

say "5. create a new user namespace to regain capabilities"
out=$(py "
import ctypes
libc = ctypes.CDLL('libc.so.6', use_errno=True)
rc = libc.unshare(0x10000000)
print('UNSHARED' if rc == 0 else 'refused:%d' % ctypes.get_errno())")
case "$out" in
    refused:*) ok "unshare(CLONE_NEWUSER) is refused ($out)" ;;
    UNSHARED)  bad "the sandbox created a user namespace" ;;
    *)         bad "inconclusive: '$out'" ;;
esac

# --- 6. remapping identity --------------------------------------------------

say "6. rewrite uid_map to become another uid"
out=$(py "$HELPER
before = open('/proc/self/uid_map').read().split()
result = attempt_write('/proc/self/uid_map', b'0 0 1')
after = open('/proc/self/uid_map').read().split()
# The map is reported either way: a write that 'succeeds' without changing
# anything is not an escape, and a silent change would be the worst outcome.
print('%s map=%s->%s' % (result, ','.join(before), ','.join(after)))")
case "$out" in
    refused:*) ok "uid_map cannot be rewritten ($out)" ;;
    WROTE*)    bad "the sandbox remapped its own identity ($out)" ;;
    *)         bad "inconclusive: '$out'" ;;
esac

# --- 7. device nodes --------------------------------------------------------

say "7. create a block device node and read the host's disk"
out=$(py "
import os
try:
    os.mknod('/tmp/disk', 0o600 | 0o60000, os.makedev(8, 0))
    print('CREATED')
except OSError as e:
    print('refused:%d' % e.errno)")
case "$out" in
    refused:*) ok "mknod is refused ($out)" ;;
    CREATED)   bad "the sandbox created a block device" ;;
    *)         bad "inconclusive: '$out'" ;;
esac

say "   and: are any host block devices already visible?"
out=$(py "
import os, stat
found = []
for entry in os.scandir('/dev'):
    try:
        if stat.S_ISBLK(os.lstat(entry.path).st_mode):
            found.append(entry.name)
    except OSError:
        pass
print(','.join(found) if found else 'none')")
[ "$out" = "none" ] && ok "no block devices in /dev" || bad "block devices visible: $out"

# --- 8. kernel memory and logs ----------------------------------------------

say "8. read kernel memory through /dev/mem, /dev/kmem, /proc/kcore"
out=$(py "
import os
reachable = []
for path in ['/dev/mem', '/dev/kmem', '/dev/kmsg', '/proc/kcore']:
    try:
        fd = os.open(path, os.O_RDONLY)
        data = os.read(fd, 16)
        os.close(fd)
        if data:
            reachable.append(path)
    except OSError:
        pass
print(','.join(reachable) if reachable else 'none')")
[ "$out" = "none" ] && ok "kernel memory is not readable" || bad "readable: $out"

# --- 9. the host's processes ------------------------------------------------

say "9. see or signal a host process"
out=$(py "
import os
pids = sorted(int(p) for p in os.listdir('/proc') if p.isdigit())
print('%d:%s' % (len(pids), ','.join(str(p) for p in pids[:5])))")
count=${out%%:*}
if [ "${count:-999}" -le 3 ] 2>/dev/null; then
    ok "only the sandbox's own processes are visible ($out)"
else
    bad "the sandbox sees $count processes ($out)"
fi

# --- 10. privileged syscalls ------------------------------------------------

say "10. reach the syscalls that bypass or undo confinement"
out=$(py "
import ctypes, platform
libc = ctypes.CDLL('libc.so.6', use_errno=True)
arm = platform.machine() == 'aarch64'
calls = {
    'bpf':             280 if arm else 321,
    'perf_event_open': 241 if arm else 298,
    'keyctl':          219 if arm else 250,
    'userfaultfd':     282 if arm else 323,
    'io_uring_setup':  425,
    'ptrace':          117 if arm else 101,
    'process_vm_readv': 270 if arm else 310,
    'kcmp':            272 if arm else 312,
}
reached = []
for name, nr in sorted(calls.items()):
    rc = libc.syscall(nr, 0, 0, 0, 0, 0, 0)
    # EPERM is the seccomp filter. Anything else means the call was reached.
    if not (rc == -1 and ctypes.get_errno() == 1):
        reached.append('%s(rc=%d,errno=%d)' % (name, rc, ctypes.get_errno()))
print(';'.join(reached) if reached else 'all-refused')")
[ "$out" = "all-refused" ] && ok "every privileged syscall returns EPERM" \
                           || bad "reached: $out"

# --- 11. writing through a read-only mount ----------------------------------

say "11. write through a read-only bind mount"
mkdir -p /tmp/escape-ro
echo original > /tmp/escape-ro/host.txt
out=$(zygo run --mount /tmp/escape-ro:/ro:ro "$IMAGE" python3 -c "$HELPER
print(attempt_write('/ro/host.txt', b'tampered'))" 2>/dev/null)
case "$out" in
    refused:*) ok "a read-only mount cannot be written ($out)" ;;
    WROTE)     bad "a read-only mount was written" ;;
    *)         bad "inconclusive: '$out'" ;;
esac
[ "$(cat /tmp/escape-ro/host.txt)" = "original" ] \
    && ok "the host file is unchanged" || bad "the host file was modified"

# --- 12. escaping through a writable mount with a symlink -------------------

say "12. follow a symlink out of a writable mount"
# Per uid, so a root run and a rootless run on the same host do not share the
# directory: a rootless sandbox handed a root-owned `/rw` cannot even create
# the symlink, and the case comes out empty rather than blocked — the harness
# disturbing what it measures.
RW=/tmp/escape-rw-$(id -u)
MARKER=/tmp/escape-host-marker-$(id -u)
mkdir -p "$RW"
rm -f "$RW/root-link"
# Marker on the host, outside anything mounted into the sandbox. If the symlink
# escaped, it would show up.
echo marker > "$MARKER"

# The comparison happens *inside* one sandbox. Comparing against a baseline
# taken from a second, differently-mounted sandbox is not the same measurement:
# the first version of this check compared 21 entries against 20 and called it
# an escape, when the difference was simply the `/rw` mount point itself.
out=$(zygo run --mount "$RW:/rw:rw" "$IMAGE" python3 -c "
import os
os.symlink('/', '/rw/root-link')
try:
    through = sorted(os.listdir('/rw/root-link'))
except OSError as e:
    print('refused:%d' % e.errno)
else:
    direct = sorted(os.listdir('/'))
    same = 'same' if through == direct else 'different:%s' % (set(through) ^ set(direct))
    leaked = os.path.exists('/rw/root-link$MARKER')
    print('%s leaked=%s' % (same, leaked))" 2>/dev/null)
case "$out" in
    "same leaked=False")
        ok "the symlink resolves to the sandbox root, not the host's" ;;
    *leaked=True*)
        bad "the symlink reached the host filesystem ($out)" ;;
    refused:*)
        ok "following the symlink was refused ($out)" ;;
    *)
        bad "inconclusive: '$out'" ;;
esac
rm -f "$RW/root-link" "$MARKER"

# --- 13. regaining privilege through a setuid binary ------------------------

say "13. regain privilege through a setuid binary"
out=$(py "
import os, stat
found = []
for d in ('/usr/bin', '/bin', '/usr/sbin', '/sbin'):
    try:
        for name in os.listdir(d):
            p = os.path.join(d, name)
            try:
                if os.lstat(p).st_mode & stat.S_ISUID:
                    found.append(p)
            except OSError:
                pass
    except OSError:
        pass
print(','.join(found[:5]) if found else 'none')")
if [ "$out" = "none" ]; then
    skip "the image ships no setuid binary to try"
else
    say "   image has setuid binaries: $out"
    nnp=$(py "print(open('/proc/self/status').read().split('NoNewPrivs:')[1].split()[0])")
    [ "$nnp" = "1" ] && ok "no_new_privs is set, so setuid cannot raise privilege" \
                     || bad "no_new_privs is $nnp"
fi

# --- 14. the host's filesystem ----------------------------------------------

say "14. reach the host's filesystem"
out=$(py "
import os
reachable = [p for p in ('/src', '/work', '/host', '/var/lib/docker') if os.path.exists(p)]
print(','.join(reachable) if reachable else 'none')")
[ "$out" = "none" ] && ok "no host path is reachable" || bad "reachable: $out"

# --- 15. the image layers on the host ---------------------------------------

say "15. tamper with the image layers other tenants share"
out=$(py "
import os
# The store's own paths, if they leaked into the sandbox, would let one tenant
# rewrite a layer every other tenant runs from.
reachable = [p for p in ('/tmp/zdata', '/root/.local/share/zygo') if os.path.exists(p)]
print(','.join(reachable) if reachable else 'none')")
[ "$out" = "none" ] && ok "the image store is not reachable from inside" \
                    || bad "the store is reachable: $out"

# --- 16. descriptors inherited into a warm-exec request ---------------------
#
# A warm-exec request is forked inside the held sandbox and its helper
# renumbers thirteen descriptors — seven namespace descriptors among them —
# before `execve`. `F_DUPFD` and `dup2` both *clear* close-on-exec, so until
# the code review (B-03) every one of them was inherited by the tenant
# program: the namespace descriptors it would need to `setns` back out, and a
# second copy of the helper's error pipe.
#
# `setns` is denied by the seccomp filter, so this was a leak rather than an
# escape. It is checked here because it is the kind that stops being a leak the
# moment a profile changes.

say "16. inherit descriptors into a warm-exec request"
WORK=$(mktemp -d)
cat > "$WORK/fds.sh" <<'FDEOF'
#!/bin/sh
# Everything open, by name, so a stray descriptor can be identified rather
# than merely counted.
for fd in /proc/self/fd/*; do
    printf '%s->%s ' "${fd##*/}" "$(readlink "$fd" 2>/dev/null)"
done
printf '\n'
FDEOF
cat > "$WORK/sandbox.toml" <<TOMLEOF
[fn.fdcheck]
image = "$IMAGE"
cmd   = ["/bin/sh", "/w/fds.sh"]
mounts = ["$WORK:/w:ro"]
TOMLEOF

if zygo up -f "$WORK/sandbox.toml" >/tmp/escape-fd.err 2>&1; then
    out=$(zygo exec fdcheck '{}' 2>>/tmp/escape-fd.err)
    zygo stop fdcheck >/dev/null 2>&1
    # The program's own stdio is 0, 1 and 2; `sh` opens the script it runs and
    # the glob above opens the directory, so a small number of extra
    # descriptors belonging to the shell is expected. What must not be there
    # is anything pointing into a namespace.
    leaked=$(printf '%s' "$out" | tr ' ' '\n' | grep -c 'ns/' || true)
    if [ "${leaked:-0}" -eq 0 ]; then
        ok "no namespace descriptor reached the tenant program"
    else
        bad "the program inherited $leaked namespace descriptor(s): $out"
    fi
else
    skip "16. warm-exec could not be started: $(tail -2 /tmp/escape-fd.err | tr '\n' ' ')"
fi
rm -rf "$WORK"

say ""
say "----------------------------------------"
say "escape suite: $PASS blocked, $FAIL escaped, $SKIP skipped"
harness_verdict
[ "$FAIL" -eq 0 ] || exit 1
