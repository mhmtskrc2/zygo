#!/usr/bin/env python3
"""PoC 4 — copy-on-write pollution with and without ``gc.freeze()``.

The density claim (N3: 800+ warm tenants on 64 GB) rests on forked children
sharing the parent's pages. In a refcounting runtime they do not, by default:
the first GC pass in the child touches the refcount of every reachable object,
which dirties the page it lives on and forces a copy. ``gc.freeze()`` moves
everything already allocated into a permanent generation the collector does not
walk.

Measures the child's private (copied) memory from ``/proc/self/smaps_rollup``,
which is the only place the kernel reports it honestly.

Acceptance (todo.md): under 2 MB copied per request.

Run:  docker run --rm -v "$PWD/poc:/poc" python:3.12-slim python3 /poc/poc4_cow_pollution.py
"""

from __future__ import annotations

import gc
import os
import statistics
import sys


def smaps_private_kb() -> int | None:
    """Private (already copied) memory of this process, in kB.

    ``Private_Dirty`` is the page count the child has written to since the fork
    — exactly the copy-on-write cost. Linux only.
    """
    try:
        with open("/proc/self/smaps_rollup") as f:
            text = f.read()
    except OSError:
        return None
    dirty = 0
    for line in text.splitlines():
        if line.startswith("Private_Dirty:"):
            dirty += int(line.split()[1])
    return dirty


def build_workload() -> list:
    """Stand in for a real handler's imports.

    A handler that imports `requests` and `pydantic` ends up with tens of
    thousands of live objects — classes, functions, docstrings, type
    annotations. What matters for CoW is the object *count*, since every object
    header carries a refcount the collector will touch.
    """
    import base64  # noqa: F401
    import datetime  # noqa: F401
    import decimal  # noqa: F401
    import email.parser  # noqa: F401
    import http.client  # noqa: F401
    import json  # noqa: F401
    import logging  # noqa: F401
    import sqlite3  # noqa: F401
    import unittest  # noqa: F401
    import xml.etree.ElementTree  # noqa: F401

    # Plus a chunk of application-shaped data.
    return [{"id": i, "name": f"item-{i}", "tags": ["a", "b", "c"]} for i in range(50_000)]


def child_cost(freeze: bool, collect_in_child: bool) -> int:
    """Fork once and report how much memory the child had to copy."""
    read_fd, write_fd = os.pipe()
    pid = os.fork()

    if pid == 0:
        os.close(read_fd)
        try:
            if collect_in_child:
                # What a real handler triggers sooner or later: any allocation
                # pressure sets off a generational collection.
                gc.collect()
            # Touch nothing else; measure immediately.
            kb = smaps_private_kb() or 0
            os.write(write_fd, str(kb).encode())
        finally:
            os.close(write_fd)
            os._exit(0)

    os.close(write_fd)
    raw = b""
    while True:
        chunk = os.read(read_fd, 64)
        if not chunk:
            break
        raw += chunk
    os.close(read_fd)
    os.waitpid(pid, 0)
    return int(raw or 0)


def run(freeze: bool, collect_in_child: bool, samples: int) -> list[int]:
    return [child_cost(freeze, collect_in_child) for _ in range(samples)]


def main() -> int:
    if not sys.platform.startswith("linux"):
        print(f"PoC 4 needs Linux: /proc/self/smaps_rollup does not exist on {sys.platform}")
        return 2
    if smaps_private_kb() is None:
        print("this kernel has no /proc/self/smaps_rollup (needs Linux 4.14+)")
        return 2

    samples = int(os.environ.get("SAMPLES", "20"))

    print("PoC 4 — copy-on-write pollution after fork()")
    print(f"  python  {sys.version.split()[0]}")
    print(f"  kernel  {os.uname().release}")
    print(f"  samples {samples}")
    print()

    data = build_workload()
    print(f"  workload: {len(gc.get_objects()):,} tracked objects, "
          f"parent private {smaps_private_kb() / 1024:.1f} MB")
    print()

    # Baseline: no freeze, and the child triggers a collection.
    unfrozen = run(freeze=False, collect_in_child=True, samples=samples)

    # Now freeze and repeat. Everything allocated so far moves to the permanent
    # generation, so the child's collection no longer walks it.
    gc.freeze()
    frozen_count = gc.get_freeze_count()
    frozen = run(freeze=True, collect_in_child=True, samples=samples)

    # And the floor: no collection in the child at all, which is the best case
    # any strategy could reach.
    floor = run(freeze=True, collect_in_child=False, samples=samples)

    def show(label: str, values: list[int]) -> float:
        mb = statistics.median(values) / 1024
        print(f"  {label:<34} median {mb:6.2f} MB   "
              f"min {min(values) / 1024:5.2f}   max {max(values) / 1024:5.2f}")
        return mb

    print(f"  gc.freeze() moved {frozen_count:,} objects to the permanent generation")
    print()
    without = show("without gc.freeze(), gc in child", unfrozen)
    with_freeze = show("with gc.freeze(), gc in child", frozen)
    no_gc = show("with gc.freeze(), no gc in child", floor)

    print()
    if without > 0:
        saved = without - with_freeze
        print(f"  gc.freeze() saves {saved:.2f} MB per request "
              f"({saved / without * 100:.0f}% less copied)")

    # `data` must stay alive to the end or the workload is not what was measured.
    assert len(data) == 50_000

    budget = 2.0
    print()
    print(f"  acceptance: < {budget} MB copied per request  →  "
          f"{'PASS' if with_freeze < budget else 'FAIL'} ({with_freeze:.2f} MB)")
    print(f"  (floor without any child collection: {no_gc:.2f} MB)")
    return 0 if with_freeze < budget else 1


if __name__ == "__main__":
    sys.exit(main())
