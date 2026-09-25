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

# What an empty answer means, which is never "escaped".
#
# Every case here reads the attempt's stdout, and a sandbox that did not run
# prints nothing — which is indistinguishable from a refusal that printed
# nothing. The suite has a positive control at the top for the case where *no*
# sandbox can start; this is for the case where one fails in the middle of the
# run, which used to be reported as an escape with an empty list of what was
# reached. An escape is a claim about the kernel; "the sandbox did not start"
# is a claim about this machine, and they must not share a verdict.
#
# Rule 4 again: a negative check must first prove the thing ran. Emptiness is
# the proof that it did not.
nothing_ran() {
    case_name=$1
    printf '%s' "        $(grep -v '^$' /tmp/escape.err 2>/dev/null | tr '\n' ' ' | cut -c1-200)\n"
    skip "$case_name: the sandbox produced no output, so nothing was attempted"
}

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
    *)         if [ -z "$out" ]; then nothing_ran "this case"; else bad "inconclusive: '$out'"; fi ;;
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
    *)         if [ -z "$out" ]; then nothing_ran "this case"; else bad "inconclusive: '$out'"; fi ;;
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
    *)         if [ -z "$out" ]; then nothing_ran "this case"; else bad "inconclusive: '$out'"; fi ;;
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
    *)      if [ -z "$out" ]; then nothing_ran "this case"; else bad "inconclusive: '$out'"; fi ;;
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
    *)         if [ -z "$out" ]; then nothing_ran "this case"; else bad "inconclusive: '$out'"; fi ;;
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
    *)         if [ -z "$out" ]; then nothing_ran "this case"; else bad "inconclusive: '$out'"; fi ;;
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
    *)         if [ -z "$out" ]; then nothing_ran "this case"; else bad "inconclusive: '$out'"; fi ;;
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

# --- 10. privileged syscalls, under every profile ---------------------------
#
# The threat model names four vectors by name — `io_uring`, `userfaultfd`,
# `bpf` and `ptrace` — and this used to attempt them under whatever profile
# `zygo run` happened to default to, with nothing proving the attempt reached
# the kernel at all. A syscall number that is wrong for this architecture, and
# a `libc.syscall` that never ran, both produce "refused" and both look like a
# result.
#
# So: all three profiles, and two controls.
#
#   * `getpid` through the *same* `libc.syscall` path. It is on every
#     allowlist, so a non-positive answer means the mechanism is broken and
#     nothing else in the probe is evidence.
#   * `ptrace` under `permissive`, which is the one profile that grants it. A
#     `ptrace` refused everywhere looks identical whether the filter is
#     working or the number is wrong; one that is *reachable* under
#     `permissive` and `EPERM` under the other two proves the number is right
#     and the filter is what refuses it.

say "10. reach the syscalls that bypass or undo confinement"

# The probe, written once and run under each profile in turn.
SYSCALL_PROBE=$(cat <<'PYEOF'
import ctypes, platform
libc = ctypes.CDLL('libc.so.6', use_errno=True)
arm = platform.machine() == 'aarch64'

# Generated from each architecture's own headers; see
# crates/zygo-core/src/backend/ns/syscalls.rs, which `make syscall-tables`
# regenerates. A name with the wrong number here would read as "refused".
calls = {
    'bpf':                280 if arm else 321,
    'io_uring_enter':     426,
    'io_uring_register':  427,
    'io_uring_setup':     425,
    'kcmp':               272 if arm else 312,
    'keyctl':             219 if arm else 250,
    'perf_event_open':    241 if arm else 298,
    'process_vm_readv':   270 if arm else 310,
    'ptrace':             117 if arm else 101,
    'userfaultfd':        282 if arm else 323,
}
GETPID = 172 if arm else 39

# Control: the same path, on a syscall every profile allows.
if libc.syscall(GETPID) <= 0:
    print('CONTROL-FAILED')
    raise SystemExit

reached = []
for name, nr in sorted(calls.items()):
    ctypes.set_errno(0)
    rc = libc.syscall(nr, 0, 0, 0, 0, 0, 0)
    # EPERM is the seccomp filter. Anything else means the call was reached
    # and the kernel itself answered — EFAULT or EINVAL for the null
    # arguments above, ENOSYS where the kernel has no such call built in.
    if not (rc == -1 and ctypes.get_errno() == 1):
        reached.append('%s(errno=%d)' % (name, ctypes.get_errno()))
print(';'.join(reached) if reached else 'all-refused')
PYEOF
)

for profile in permissive default strict; do
    out=$(zygo run --seccomp "$profile" "$IMAGE" python3 -c "$SYSCALL_PROBE" 2>/tmp/escape.err)
    if [ -z "$out" ]; then
        nothing_ran "10. privileged syscalls under --seccomp $profile"
        continue
    fi
    if [ "$out" = CONTROL-FAILED ]; then
        skip "10. under --seccomp $profile: getpid failed through the same path, so nothing here is evidence"
        continue
    fi

    if [ "$profile" = permissive ]; then
        # `permissive` adds `ptrace` and nothing else from this list. That it
        # is reachable *here* is the proof the numbers are right.
        case "$out" in
            *ptrace*) ok "under --seccomp permissive, ptrace is reachable — the probe can tell reached from refused" ;;
            all-refused) bad "ptrace was refused under permissive, which grants it: either the number is wrong or the probe never reached the kernel" ;;
            *) bad "under permissive, ptrace was refused but $out was reached" ;;
        esac
        # `permissive` is the debugging profile and grants exactly two names
        # from this probe: `ptrace` and `process_vm_readv`, both of which it
        # is documented to. Everything else stays refused even here —
        # `io_uring` and `userfaultfd` were reachable until this check
        # existed, which is what removed them from `PERMISSIVE_EXTRA`.
        rest=$(printf '%s' "$out" | tr ';' '\n' \
            | grep -v '^ptrace' | grep -v '^process_vm_readv' | tr '\n' ' ')
        if [ -z "$rest" ]; then
            ok "and bpf, io_uring, userfaultfd, keyctl, perf_event_open and kcmp are refused even under permissive"
        else
            bad "under permissive, these were reached as well: $rest"
        fi
    else
        if [ "$out" = all-refused ]; then
            ok "under --seccomp $profile, all ten return EPERM — including io_uring, userfaultfd, bpf and ptrace"
        else
            bad "under --seccomp $profile, reached: $out"
        fi
    fi
done

# --- 11. writing through a read-only mount ----------------------------------

say "11. write through a read-only bind mount"
mkdir -p /tmp/escape-ro
echo original > /tmp/escape-ro/host.txt
out=$(zygo run --mount /tmp/escape-ro:/ro:ro "$IMAGE" python3 -c "$HELPER
print(attempt_write('/ro/host.txt', b'tampered'))" 2>/dev/null)
case "$out" in
    refused:*) ok "a read-only mount cannot be written ($out)" ;;
    WROTE)     bad "a read-only mount was written" ;;
    *)         if [ -z "$out" ]; then nothing_ran "this case"; else bad "inconclusive: '$out'"; fi ;;
esac
[ "$(cat /tmp/escape-ro/host.txt)" = "original" ] \
    && ok "the host file is unchanged" || bad "the host file was modified"

# A mount *below* the read-only source. `MS_REC` carries it into the sandbox,
# and a remount of the top mount alone leaves it writable — Landlock hid that
# on 5.13+, nothing did below. Needs a host that can mount a tmpfs.
mkdir -p /tmp/escape-ro/sub
if mount -t tmpfs tmpfs /tmp/escape-ro/sub 2>/dev/null; then
    echo original > /tmp/escape-ro/sub/inner.txt
    out=$(zygo run --mount /tmp/escape-ro:/ro:ro "$IMAGE" python3 -c "$HELPER
print(attempt_write('/ro/sub/inner.txt', b'tampered'))" 2>/dev/null)
    case "$out" in
        refused:*) ok "a mount below a read-only mount cannot be written ($out)" ;;
        WROTE)     bad "a mount below a read-only mount was written" ;;
        *)         if [ -z "$out" ]; then nothing_ran "this case"; else bad "inconclusive: '$out'"; fi ;;
    esac
    [ "$(cat /tmp/escape-ro/sub/inner.txt)" = "original" ] \
        && ok "the file on the submount is unchanged" || bad "the file on the submount was modified"
    umount /tmp/escape-ro/sub
else
    skip "a mount below a read-only mount (this host cannot mount a tmpfs)"
fi

# A writable bind is still `nosuid,nodev`: a device node or a setuid binary
# the host left in a shared directory gives the sandbox nothing.
mkdir -p /tmp/escape-rwflags
out=$(zygo run --mount /tmp/escape-rwflags:/rwflags:rw "$IMAGE" \
      sh -c "grep ' /rwflags ' /proc/self/mountinfo" 2>/dev/null)
case "$out" in
    *nosuid*nodev*|*nodev*nosuid*) ok "a writable bind is nosuid,nodev" ;;
    '')                            nothing_ran "this case" ;;
    *)                             bad "a writable bind keeps setuid or devices: $out" ;;
esac

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
        if [ -z "$out" ]; then nothing_ran "this case"; else bad "inconclusive: '$out'"; fi ;;
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
#
# `/work` used to be on this list and is not any more. Phase 2.6 mounted a
# tmpfs there for per-request workspaces, so the path now exists in every
# sandbox and this case began reporting an escape on a directory Zygo itself
# had created. Existence is the wrong question for it; 14b asks the right one.

say "14. reach the host's filesystem"
out=$(py "
import os
reachable = [p for p in ('/src', '/host', '/var/lib/docker') if os.path.exists(p)]
print(','.join(reachable) if reachable else 'none')")
if [ -z "$out" ]; then
    nothing_ran "this case"
elif [ "$out" = "none" ]; then
    ok "no host path is reachable"
else
    bad "reachable: $out"
fi

# --- 14b. the other requests' workspaces ------------------------------------
#
# One sandbox serves several requests and, in a pool, several tenants. Each
# request's files live under `/work/<128 random bits>` and the directory above
# them is mode 0311, so a handler cannot enumerate its neighbours — that mode
# is the whole boundary, because a mount namespace per request is not
# available to a forked child (6.12). Two answers would break it: `/work`
# turning out to be a directory of the host's rather than a tmpfs of Zygo's,
# and `/work` turning out to be listable.

say "14b. list the other requests' workspaces"
out=$(py "
import os
# mountinfo: '<id> <parent> <maj:min> <root> <mountpoint> <opts…> - <fstype> …'
try:
    kind = [l.split(' - ')[1].split()[0] for l in open('/proc/self/mountinfo')
            if l.split(' - ')[0].split()[4] == '/work'][-1]
except (IndexError, IOError):
    kind = 'not-a-mount'
try:
    names = os.listdir('/work')
    listed = 'LISTED:' + (','.join(names[:5]) or 'empty')
except OSError as e:
    listed = 'refused:%d' % e.errno
print('%s %s' % (kind, listed))")
case "$out" in
    "tmpfs refused:13")
        ok "/work is a tmpfs of Zygo's own and cannot be listed ($out)" ;;
    "")
        nothing_ran "this case" ;;
    *)
        bad "the workspace directory: $out" ;;
esac

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

# --- 17. replacing the script another request is about to load --------------

say "17. a script in a pool rewrites its own file"
# The one attack `path` delivery would open, if the delivery were only a
# directory. A pool's zygotes are shared and every process in a sandbox runs
# as one uid, so mode bits cannot keep a script out of a directory its own uid
# owns — and Landlock cannot either, because a nested rule grants rather than
# removes. What does it is the mount: `/run/script` is a read-only bind of a
# directory the supervisor owns on the host, so the answer here is `EROFS` on
# every kernel rather than only on one with Landlock compiled in.
WORK=/tmp/escape-pool
mkdir -p "$WORK"
cat > "$WORK/probe.py" <<'PY'
import os


def handler(event):
    # Its own file, which it is about to be asked to run again.
    try:
        os.unlink(__file__)
        return {"unlinked": True}
    except OSError as e:
        return {"unlinked": False, "errno": e.errno}
PY

if zygo serve --runtime escape-pool --image "$IMAGE" --agent python >/tmp/escape-pool.err 2>&1; then
    out=$(zygo exec --runtime escape-pool --script "$WORK/probe.py" '{}' 2>>/tmp/escape-pool.err | tr -d '\n ')
    # `stop <name>` stops functions, not runtime pools, and `serve` started a
    # supervisor for this one. Leaving either running hands the next suite a
    # sandbox it did not start.
    zygo stop --all >/dev/null 2>&1
    case "$out" in
        *'"unlinked":false'*)
            ok "a script cannot remove the file it was loaded from ($out)" ;;
        *'"unlinked":true'*)
            bad "a script removed the file it was loaded from: $out" ;;
        *) skip "17. the probe said nothing usable: '$out'" ;;
    esac
else
    skip "17. the pool would not start: $(tail -2 /tmp/escape-pool.err | tr '\n' ' ')"
fi
rm -rf "$WORK"

say ""
say "----------------------------------------"
say "escape suite: $PASS blocked, $FAIL escaped, $SKIP skipped"
harness_verdict
[ "$FAIL" -eq 0 ] || exit 1
