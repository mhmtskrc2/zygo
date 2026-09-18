#!/usr/bin/env python3
"""PoC 9 — is a pre-created network namespace reusable? (todo.md, phase 1.3)

PoC 1 measured `CLONE_NEWNET` at 2.47 ms, 94% of the whole sandbox setup cost.
The obvious fix is a pool: `network = "none"` namespaces are all identical, so
create them ahead of time and `setns` into one instead of paying for a new one.

Before building that, the question this answers: **can a sandbox enter a
namespace it did not create?** `setns(CLONE_NEWNET)` requires `CAP_SYS_ADMIN`
*in the user namespace that owns the target namespace* — and every Zygo sandbox
gets its own user namespace, so a pooled namespace is owned by somebody else's.

Three cases, in the order they matter:

1. **privileged** — the pool is created in the initial user namespace and the
   sandbox has capabilities there.
2. **rootless, shared owner** — the pool and the sandbox live in the *same* user
   namespace.
3. **rootless, per-tenant userns** — what Zygo actually does: the pool is owned
   by one user namespace and the sandbox is in another.

Also measures `setns` against `unshare`, so the saving is a number rather than
an assumption.

Run:  docker run --rm --privileged -v "$PWD/poc:/poc:ro" python:3.12-slim \\
          python3 /poc/poc9_netns_pool.py
"""

from __future__ import annotations

import ctypes
import os
import statistics
import sys
import time

libc = ctypes.CDLL("libc.so.6", use_errno=True)

CLONE_NEWNET = 0x4000_0000
CLONE_NEWUSER = 0x1000_0000


def unshare(flags: int) -> None:
    if libc.unshare(flags) != 0:
        err = ctypes.get_errno()
        raise OSError(err, f"unshare({flags:#x}): {os.strerror(err)}")


def setns(fd: int, nstype: int) -> None:
    if libc.setns(fd, nstype) != 0:
        err = ctypes.get_errno()
        raise OSError(err, f"setns: {os.strerror(err)}")


def write_id_maps(uid: int, gid: int, pid: int | str = "self") -> None:
    """Map an identity into a freshly unshared user namespace.

    `uid`/`gid` must be read *before* the unshare: inside a user namespace with
    no map yet, `getuid()` returns the overflow uid and mapping that is
    rejected. The launcher has the same obligation; PoC 1 found it first.
    """
    try:
        with open(f"/proc/{pid}/setgroups", "w") as f:
            f.write("deny")
    except OSError:
        pass
    with open(f"/proc/{pid}/uid_map", "w") as f:
        f.write(f"0 {uid} 1")
    with open(f"/proc/{pid}/gid_map", "w") as f:
        f.write(f"0 {gid} 1")


def spawn_netns_holder(own_userns: bool) -> tuple[int, int]:
    """Create a network namespace and keep it alive.

    Returns `(pid, fd)`; the fd refers to `/proc/<pid>/ns/net`, which is how a
    pool would hand namespaces out.
    """
    uid, gid = os.getuid(), os.getgid()
    ready_r, ready_w = os.pipe()
    hold_r, hold_w = os.pipe()

    # `os._exit` in the child skips flushing, but the child inherits a *copy*
    # of whatever is still buffered here and writes it out itself — which is
    # why the first run of this PoC printed its header three times.
    sys.stdout.flush()
    pid = os.fork()
    if pid == 0:
        os.close(ready_r)
        os.close(hold_w)
        try:
            # A network namespace can only be created with CAP_SYS_ADMIN, which
            # an unprivileged process gets by first entering a user namespace.
            if own_userns:
                unshare(CLONE_NEWUSER)
                write_id_maps(uid, gid)
            unshare(CLONE_NEWNET)
            os.write(ready_w, b"1")
        except OSError:
            os.write(ready_w, b"0")
            os._exit(1)
        os.close(ready_w)
        os.read(hold_r, 1)  # stay alive until the parent is done
        os._exit(0)

    os.close(ready_w)
    os.close(hold_r)
    ok = os.read(ready_r, 1)
    os.close(ready_r)
    if ok != b"1":
        os.waitpid(pid, 0)
        raise OSError("the holder could not create a network namespace")

    fd = os.open(f"/proc/{pid}/ns/net", os.O_RDONLY)
    return pid, hold_w


def try_enter(netns_fd: int, own_userns: bool) -> tuple[bool, str]:
    """Attempt `setns` into `netns_fd` from a child, optionally in its own userns."""
    uid, gid = os.getuid(), os.getgid()
    r, w = os.pipe()
    sys.stdout.flush()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        try:
            if own_userns:
                unshare(CLONE_NEWUSER)
                write_id_maps(uid, gid)
            setns(netns_fd, CLONE_NEWNET)
            os.write(w, b"ok")
        except OSError as exc:
            os.write(w, f"{exc.errno}:{os.strerror(exc.errno)}".encode())
        finally:
            os.close(w)
            sys.stdout.flush()
            os._exit(0)

    os.close(w)
    out = b""
    while chunk := os.read(r, 128):
        out += chunk
    os.close(r)
    os.waitpid(pid, 0)
    text = out.decode()
    return text == "ok", text


def measure(fn, iterations: int) -> list[float]:
    samples = []
    for _ in range(iterations):
        start = time.perf_counter()
        fn()
        samples.append((time.perf_counter() - start) * 1000)
    return samples


def measure_in_child(body) -> float:
    """Run `body` in a forked child and return what it timed, in ms."""
    r, w = os.pipe()
    sys.stdout.flush()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        try:
            ms = body()
            os.write(w, f"{ms:.5f}".encode())
        except OSError:
            os.write(w, b"-1")
        finally:
            os.close(w)
            os._exit(0)
    os.close(w)
    out = b""
    while chunk := os.read(r, 64):
        out += chunk
    os.close(r)
    os.waitpid(pid, 0)
    return float(out or -1)


def main() -> int:
    if not sys.platform.startswith("linux"):
        print(f"PoC 9 needs Linux; this is {sys.platform}")
        return 2

    print("PoC 9 — can a network namespace be pooled and reused?")
    print(f"  kernel  {os.uname().release}")
    print(f"  uid     {os.getuid()} ({'root' if os.getuid() == 0 else 'unprivileged'})")
    print()

    results: dict[str, tuple[bool, str]] = {}

    # --- case 1: the pool lives in the initial user namespace ---------------
    print("case 1 — privileged: pool in the initial user namespace")
    try:
        pid, hold = spawn_netns_holder(own_userns=False)
        fd = os.open(f"/proc/{pid}/ns/net", os.O_RDONLY)
        ok, detail = try_enter(fd, own_userns=False)
        results["privileged"] = (ok, detail)
        print(f"  setns from the same user namespace: {'OK' if ok else detail}")

        # And the case Zygo actually needs: the sandbox has its own userns.
        ok2, detail2 = try_enter(fd, own_userns=True)
        results["privileged + own userns"] = (ok2, detail2)
        print(f"  setns after unsharing a user namespace: {'OK' if ok2 else detail2}")
        os.close(fd)
        os.write(hold, b"x")
        os.close(hold)
        os.waitpid(pid, 0)
    except OSError as exc:
        print(f"  could not set up: {exc}")
        results["privileged"] = (False, str(exc))

    print()

    # --- case 2 and 3: the pool lives in its own user namespace -------------
    print("case 2/3 — rootless: pool inside its own user namespace")
    try:
        pid, hold = spawn_netns_holder(own_userns=True)
        fd = os.open(f"/proc/{pid}/ns/net", os.O_RDONLY)

        ok, detail = try_enter(fd, own_userns=False)
        results["rootless, caller in the parent userns"] = (ok, detail)
        print(f"  setns from the launcher's user namespace: {'OK' if ok else detail}")

        ok2, detail2 = try_enter(fd, own_userns=True)
        results["rootless, caller in its own userns"] = (ok2, detail2)
        print(f"  setns from a *different* user namespace:  {'OK' if ok2 else detail2}")

        os.close(fd)
        os.write(hold, b"x")
        os.close(hold)
        os.waitpid(pid, 0)
    except OSError as exc:
        print(f"  could not set up: {exc}")

    print()

    # --- the saving, if it is reachable at all ------------------------------
    print("cost of creating versus entering a network namespace")

    def create_cost() -> float:
        start = time.perf_counter()
        unshare(CLONE_NEWNET)
        return (time.perf_counter() - start) * 1000

    creates = [measure_in_child(create_cost) for _ in range(50)]
    creates = [c for c in creates if c >= 0]
    if creates:
        print(f"  unshare(CLONE_NEWNET)  p50 {statistics.median(creates):6.3f} ms")

    try:
        pid, hold = spawn_netns_holder(own_userns=False)
        fd = os.open(f"/proc/{pid}/ns/net", os.O_RDONLY)

        def enter_cost() -> float:
            start = time.perf_counter()
            setns(fd, CLONE_NEWNET)
            return (time.perf_counter() - start) * 1000

        enters = [measure_in_child(enter_cost) for _ in range(50)]
        enters = [e for e in enters if e >= 0]
        if enters:
            print(f"  setns(CLONE_NEWNET)    p50 {statistics.median(enters):6.3f} ms")
            if creates:
                saving = statistics.median(creates) - statistics.median(enters)
                print(f"  saving                 {saving:6.3f} ms per sandbox")
        os.close(fd)
        os.write(hold, b"x")
        os.close(hold)
        os.waitpid(pid, 0)
    except OSError as exc:
        print(f"  could not measure setns: {exc}")

    print()
    print("----------------------------------------")
    print("verdict")
    for label, (ok, detail) in results.items():
        print(f"  {label:42} {'usable' if ok else 'refused: ' + detail}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
