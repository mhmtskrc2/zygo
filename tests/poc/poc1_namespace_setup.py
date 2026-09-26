#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""PoC 1 — how long does it take to build a sandbox?

The design claims a namespace set, a mount plan and a `pivot_root` cost 1–3 ms,
and that the expensive parts of `docker run` are orchestration rather than
isolation. This measures the isolation half directly.

Each iteration performs, in a forked child, the sequence of docs/book/06-how-zygo-works.md:

    unshare(USER|NS|NET|IPC|UTS|CGROUP)   →  new namespaces
    write uid_map / gid_map               →  identity inside them
    mount --make-rprivate /               →  stop propagation to the host
    bind rootfs read-only                 →  the image view
    mount proc, tmpfs /tmp                →  pseudo-filesystems
    pivot_root + umount2(MNT_DETACH)      →  commit the new root

`execve` is deliberately excluded: that cost belongs to the program being run,
not to the sandbox, and the warm path never pays it.

The calls go through ctypes rather than a launcher binary so that what is timed
is the kernel work and nothing else. ctypes adds roughly a microsecond per call
against a budget of thousands.

Run:  docker run --rm --privileged -v "$PWD/tests/poc:/poc:ro" python:3.12-slim \\
          python3 /poc/poc1_namespace_setup.py
"""

from __future__ import annotations

import ctypes
import os
import statistics
import sys
import time

libc = ctypes.CDLL("libc.so.6", use_errno=True)

CLONE_NEWNS = 0x00020000
CLONE_NEWUTS = 0x04000000
CLONE_NEWIPC = 0x08000000
CLONE_NEWUSER = 0x10000000
CLONE_NEWPID = 0x20000000
CLONE_NEWNET = 0x40000000
CLONE_NEWCGROUP = 0x02000000

MS_RDONLY = 1
MS_NOSUID = 2
MS_NODEV = 4
MS_NOEXEC = 8
MS_REC = 0x4000
MS_BIND = 0x1000
MS_PRIVATE = 0x40000
MS_REMOUNT = 0x20
MNT_DETACH = 2


def fail(what: str) -> None:
    err = ctypes.get_errno()
    raise OSError(err, f"{what}: {os.strerror(err)}")


def unshare(flags: int) -> None:
    if libc.unshare(flags) != 0:
        fail(f"unshare({flags:#x})")


def mount(source: str, target: str, fstype: str | None, flags: int, data: str | None) -> None:
    rc = libc.mount(
        source.encode(),
        target.encode(),
        fstype.encode() if fstype else None,
        ctypes.c_ulong(flags),
        data.encode() if data else None,
    )
    if rc != 0:
        fail(f"mount({source} -> {target})")


def pivot_root(new_root: str, put_old: str) -> None:
    # No libc wrapper on most platforms; go through syscall(2).
    NR = {"aarch64": 41, "x86_64": 155}.get(os.uname().machine)
    if NR is None:
        raise OSError(f"pivot_root syscall number unknown for {os.uname().machine}")
    if libc.syscall(NR, new_root.encode(), put_old.encode()) != 0:
        fail("pivot_root")


def umount2(target: str, flags: int) -> None:
    if libc.umount2(target.encode(), flags) != 0:
        fail(f"umount2({target})")


def build_rootfs(path: str) -> None:
    """A minimal image-shaped directory tree to act as the sandbox root."""
    for d in ("proc", "sys", "dev", "tmp", "run", "app", "old", "usr/bin", "etc"):
        os.makedirs(os.path.join(path, d), exist_ok=True)
    with open(os.path.join(path, "etc/hostname"), "w") as f:
        f.write("sandbox\n")


# Name of the primitive currently being attempted, so a failure names the step
# rather than just reporting EPERM. The launcher owes users the same.
CURRENT_STEP = "start"


def step(name: str) -> None:
    global CURRENT_STEP
    CURRENT_STEP = name


def setup_once(rootfs: str, phases: dict[str, float], with_netns: bool = True) -> None:
    """The one-shot sequence of docs/book/06-how-zygo-works.md, timed phase by phase."""
    # Capture the identity *before* unsharing. Inside a fresh user namespace
    # with no map yet, getuid() returns the overflow uid (65534), and writing
    # that into uid_map is rejected: a process may only map the uid it actually
    # had outside. The launcher has the same obligation.
    outer_uid, outer_gid = os.getuid(), os.getgid()

    t0 = time.perf_counter()

    # 1. Namespaces. CLONE_NEWPID is applied too, but it only takes effect for
    #    *children*, so a launcher forks once more after this; that second fork
    #    is measured separately below.
    step("unshare")
    flags = (
        CLONE_NEWUSER
        | CLONE_NEWNS
        | CLONE_NEWIPC
        | CLONE_NEWUTS
        | CLONE_NEWCGROUP
        | CLONE_NEWPID
    )
    # Optional, because it is the one that has ever cost anything here: the
    # first measurement put 94% of the total in `CLONE_NEWNET`, under nested
    # virtualisation, and creating a network namespace involves RCU
    # synchronisation that a hypervisor can make disproportionately expensive.
    # Running the same iteration with and without it, on the same machine, is
    # the only way to tell the kernel's cost from the platform's.
    if with_netns:
        flags |= CLONE_NEWNET
    unshare(flags)
    t1 = time.perf_counter()

    # 2. Identity. `setgroups=deny` is required before gid_map can be written
    #    by an unprivileged process.
    step("write /proc/self/setgroups")
    try:
        with open("/proc/self/setgroups", "w") as f:
            f.write("deny")
    except OSError:
        pass
    step("write /proc/self/uid_map")
    with open("/proc/self/uid_map", "w") as f:
        f.write(f"0 {outer_uid} 1")
    step("write /proc/self/gid_map")
    with open("/proc/self/gid_map", "w") as f:
        f.write(f"0 {outer_gid} 1")
    t2 = time.perf_counter()

    # 3. Enter the new pid namespace.
    #
    #    `unshare(CLONE_NEWPID)` does not move the caller — it only makes its
    #    *children* the first members of the new namespace. A fresh `proc` can
    #    only be mounted by a process that is itself in that namespace, so
    #    mounting it before this fork fails with EPERM. The launcher has to
    #    fork here too; the cost is measured as part of the total.
    step("fork into the new pid namespace")
    inner = os.fork()
    if inner != 0:
        # The intermediate process is not the sandbox; it waits and exits.
        os.waitpid(inner, 0)
        phases["__intermediate__"] = 1.0
        return
    t_pidns = time.perf_counter()

    # 4. Mount plan.
    step("mount --make-rprivate /")
    mount("none", "/", None, MS_REC | MS_PRIVATE, None)
    step("bind rootfs")
    mount(rootfs, rootfs, None, MS_BIND | MS_REC, None)
    step("remount rootfs read-only")
    mount(
        "none",
        rootfs,
        None,
        MS_REMOUNT | MS_BIND | MS_RDONLY | MS_NOSUID | MS_NODEV,
        None,
    )
    step("mount /proc")
    mount("proc", os.path.join(rootfs, "proc"), "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, None)
    step("mount /tmp tmpfs")
    mount(
        "tmpfs",
        os.path.join(rootfs, "tmp"),
        "tmpfs",
        MS_NOSUID | MS_NODEV,
        "size=67108864,nr_inodes=10000,mode=1777",
    )
    step("mount /run tmpfs")
    mount("tmpfs", os.path.join(rootfs, "run"), "tmpfs", MS_NOSUID | MS_NODEV | MS_NOEXEC, "size=1048576,mode=755")
    t3 = time.perf_counter()

    # 5. Commit the new root. `pivot_root` rather than `chroot`: it detaches the
    #    old root entirely instead of leaving it reachable.
    step("pivot_root")
    os.chdir(rootfs)
    pivot_root(rootfs, os.path.join(rootfs, "old"))
    os.chdir("/")
    step("umount2 old root")
    umount2("/old", MNT_DETACH)
    t4 = time.perf_counter()

    phases["namespaces"] = (t1 - t0) * 1000
    phases["id maps"] = (t2 - t1) * 1000
    phases["pid ns fork"] = (t_pidns - t2) * 1000
    phases["mounts"] = (t3 - t_pidns) * 1000
    phases["pivot_root"] = (t4 - t3) * 1000
    phases["total"] = (t4 - t0) * 1000


def one_iteration(rootfs: str, with_netns: bool = True) -> dict[str, float] | str:
    """Run the setup in a child; the namespaces die with it."""
    read_fd, write_fd = os.pipe()
    pid = os.fork()

    if pid == 0:
        os.close(read_fd)
        phases: dict[str, float] = {}
        try:
            setup_once(rootfs, phases, with_netns)
            if "__intermediate__" in phases:
                # This is the process between the two forks. The grandchild has
                # already reported; it must stay silent.
                os.close(write_fd)
                os._exit(0)
            payload = ";".join(f"{k}={v:.4f}" for k, v in phases.items())
        except BaseException as exc:  # noqa: BLE001
            payload = f"ERROR=[{CURRENT_STEP}] {type(exc).__name__}: {exc}"
        try:
            os.write(write_fd, payload.encode())
        finally:
            os.close(write_fd)
            os._exit(0)

    os.close(write_fd)
    chunks = []
    while True:
        chunk = os.read(read_fd, 512)
        if not chunk:
            break
        chunks.append(chunk)
    os.close(read_fd)
    os.waitpid(pid, 0)

    text = b"".join(chunks).decode()
    if text.startswith("ERROR="):
        return text[6:]
    return {k: float(v) for k, v in (p.split("=") for p in text.split(";"))}


def main() -> int:
    if not sys.platform.startswith("linux"):
        print(f"PoC 1 needs Linux; this is {sys.platform}")
        return 2

    iterations = int(os.environ.get("ITERATIONS", "300"))
    rootfs = "/tmp/poc1-rootfs"
    build_rootfs(rootfs)

    print("PoC 1 — sandbox setup cost")
    print(f"  kernel      {os.uname().release} {os.uname().machine}")
    print(f"  running as  uid {os.getuid()} ({'root' if os.getuid() == 0 else 'unprivileged'})")
    print(f"  iterations  {iterations}")
    print()

    def pct(values: list[float], p: float) -> float:
        s = sorted(values)
        return s[min(int(len(s) * p / 100), len(s) - 1)]

    def measure(with_netns: bool) -> dict[str, list[float]] | str:
        first = one_iteration(rootfs, with_netns)
        if isinstance(first, str):
            return first
        collected: dict[str, list[float]] = {k: [] for k in first}
        for _ in range(iterations):
            result = one_iteration(rootfs, with_netns)
            if isinstance(result, str):
                return result
            for k, v in result.items():
                collected[k].append(v)
        return collected

    samples = measure(True)
    if isinstance(samples, str):
        print(f"  setup failed: {samples}")
        print()
        print("  This is the environment refusing a primitive, not a timing result.")
        return 1

    for phase in ("namespaces", "id maps", "pid ns fork", "mounts", "pivot_root", "total"):
        v = samples[phase]
        print(
            f"  {phase:<12} p50 {statistics.median(v):6.3f} ms   "
            f"p90 {pct(v, 90):6.3f}   p99 {pct(v, 99):6.3f}   max {max(v):6.3f}"
        )

    # The same sequence again without `CLONE_NEWNET`, so the cost of a network
    # namespace is a subtraction on this machine rather than a claim carried
    # over from another one. The first measurement of this attributed 94% of
    # the total to it — under nested virtualisation, where the RCU
    # synchronisation a netns needs is exactly what a hypervisor makes
    # expensive.
    without = measure(False)
    print()
    if isinstance(without, str):
        print(f"  without CLONE_NEWNET: could not run ({without})")
    else:
        with_p50 = statistics.median(samples["total"])
        without_p50 = statistics.median(without["total"])
        cost = with_p50 - without_p50
        share = (cost / with_p50 * 100) if with_p50 > 0 else 0.0
        print(f"  without CLONE_NEWNET   p50 {without_p50:6.3f} ms")
        print(f"  the network namespace  p50 {cost:6.3f} ms — {share:.0f}% of the total")

    total_p50 = statistics.median(samples["total"])
    print()
    print(f"  acceptance: setup < 3 ms  →  "
          f"{'PASS' if total_p50 < 3.0 else 'FAIL'} (p50 {total_p50:.3f} ms)")
    print()
    print("  note: execve of the sandboxed program is excluded — that cost belongs")
    print("        to the program, and the warm path never pays it again.")
    return 0 if total_p50 < 3.0 else 1


if __name__ == "__main__":
    sys.exit(main())
