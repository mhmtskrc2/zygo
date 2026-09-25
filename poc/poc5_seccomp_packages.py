#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""PoC 5 — do real packages survive the `default` seccomp allowlist?

Acceptance: `requests`, `pydantic`, `numpy`, `pandas` and `Pillow`
all work. A profile that is tight but breaks numpy is not a shippable default,
and the deliverable of this PoC is the list of syscalls that had to be let back
in.

Each package gets an exercise that goes past `import` into the code paths that
actually reach the kernel: numpy's BLAS, Pillow's codecs, pandas' file I/O.

Run under the profile:

    docker run --rm --security-opt seccomp=poc/seccomp-default.json \\
        -v "$PWD/poc:/poc:ro" zygo-poc5 python3 /poc/poc5_seccomp_packages.py
"""

from __future__ import annotations

import sys
import traceback

RESULTS: list[tuple[str, bool, str]] = []


def check(name: str):
    """Run one exercise, recording the failure rather than aborting.

    A seccomp violation surfaces as `PermissionError`/`OSError` when the action
    is `SCMP_ACT_ERRNO`, but a `SIGSYS` kill would take the whole process down —
    so each exercise is kept small and the script reports progressively.
    """

    def wrap(fn):
        try:
            detail = fn()
            RESULTS.append((name, True, detail or "ok"))
            print(f"  PASS  {name:<12} {detail or ''}", flush=True)
        except BaseException as exc:  # noqa: BLE001
            line = traceback.format_exc().strip().splitlines()[-1]
            RESULTS.append((name, False, line))
            print(f"  FAIL  {name:<12} {line}", flush=True)
        return fn

    return wrap


@check("import-time")
def _imports():
    import json  # noqa: F401
    import os
    import socket  # noqa: F401
    import ssl  # noqa: F401
    import subprocess  # noqa: F401
    import threading  # noqa: F401

    return f"stdlib ok, pid {os.getpid()}"


@check("numpy")
def _numpy():
    import numpy as np

    a = np.random.rand(256, 256)
    b = np.random.rand(256, 256)
    c = a @ b                      # reaches the BLAS backend and its threads
    eig = np.linalg.eigvals(np.eye(8))
    return f"matmul {c.shape} sum={c.sum():.1f}, eig ok ({len(eig)})"


@check("pandas")
def _pandas():
    import io

    import pandas as pd

    df = pd.DataFrame({"a": range(1000), "b": [f"x{i}" for i in range(1000)]})
    buf = io.StringIO()
    df.to_csv(buf, index=False)
    back = pd.read_csv(io.StringIO(buf.getvalue()))
    grouped = back.groupby(back["a"] % 7).size()
    return f"{len(back)} rows, {len(grouped)} groups"


@check("Pillow")
def _pillow():
    import base64
    import io

    from PIL import Image

    img = Image.new("RGB", (640, 480), (120, 80, 200))
    for fmt in ("PNG", "JPEG", "WEBP"):
        out = io.BytesIO()
        img.save(out, fmt)
        reread = Image.open(io.BytesIO(out.getvalue()))
        reread.load()
    thumb = img.copy()
    thumb.thumbnail((64, 64))
    encoded = base64.b64encode(out.getvalue())
    return f"PNG/JPEG/WEBP ok, thumb {thumb.size}, {len(encoded)} b64 bytes"


@check("pydantic")
def _pydantic():
    from pydantic import BaseModel, Field

    class Item(BaseModel):
        name: str
        count: int = Field(ge=0)

    item = Item.model_validate({"name": "a", "count": 3})
    try:
        Item.model_validate({"name": "a", "count": -1})
        raise AssertionError("validation should have failed")
    except Exception as exc:  # noqa: BLE001
        if "AssertionError" in type(exc).__name__:
            raise
    return f"{item.model_dump_json()} (rust core: pydantic_core)"


@check("requests")
def _requests():
    import requests

    # No network in this sandbox by design, so exercise everything up to the
    # connect(): TLS context construction, DNS resolver setup, adapter pooling.
    session = requests.Session()
    adapter = session.get_adapter("https://example.com")
    import ssl

    ctx = ssl.create_default_context()
    return f"session ok, adapter {type(adapter).__name__}, TLS {ctx.protocol.name}"


@check("fork")
def _fork():
    import os

    # The warm path itself: the allowlist must permit fork() while refusing the
    # CLONE_NEW* flags that would create new namespaces.
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        os.write(w, b"child-ok")
        os.close(w)
        os._exit(0)
    os.close(w)
    data = os.read(r, 32)
    os.close(r)
    os.waitpid(pid, 0)
    return data.decode()


@check("unshare")
def _unshare():
    """This one must *fail*: a sandbox must not be able to nest namespaces."""
    import ctypes

    libc = ctypes.CDLL("libc.so.6", use_errno=True)
    CLONE_NEWUSER = 0x10000000
    rc = libc.unshare(CLONE_NEWUSER)
    if rc == 0:
        raise AssertionError("unshare(CLONE_NEWUSER) SUCCEEDED — the profile is too loose")
    import os

    return f"correctly refused (errno {ctypes.get_errno()} {os.strerror(ctypes.get_errno())})"


def main() -> int:
    print("PoC 5 — packages under the `default` seccomp allowlist")
    import os

    print(f"  kernel {os.uname().release}, python {sys.version.split()[0]}")
    print()

    required = {"numpy", "pandas", "Pillow", "pydantic", "requests"}
    failed = {name for name, passed, _ in RESULTS if not passed}
    missing_required = required & failed

    print()
    print("----------------------------------------")
    passed = sum(1 for _, p, _ in RESULTS if p)
    print(f"PoC 5: {passed}/{len(RESULTS)} exercises passed")
    if missing_required:
        print(f"  the five required packages are NOT all working: {sorted(missing_required)}")
        print("  → add the syscalls named above to poc/seccomp-default.json and re-run")
        return 1
    print("  all five required packages work under the allowlist")
    return 0 if not failed else 1


if __name__ == "__main__":
    sys.exit(main())
