# SPDX-License-Identifier: Apache-2.0
"""Zygo — warm sandboxes for function-shaped code, from Python.

    import zygo_sdk as zygo

    client = zygo.connect()                      # `zygo api` on loopback
    resize = client.fn("resize")                 # a function from sandbox.toml
    out = resize({"url": "..."})                 # ~2 ms, a fresh process
    print(out.result)

A warm function costs about a millisecond and gets a clean process per request.
A one-shot sandbox costs tens of milliseconds and needs nothing declared in
advance:

    r = client.run("python:3.12-slim", ["python3", "-c", "print(6*7)"])
    print(r.stdout)

For an event loop, ``zygo.aio`` is the same API with every call a coroutine:

    async with zygo.aio.connect() as client:
        out = await client.call("resize", {"url": "..."})

A refused request — :class:`Busy`, or :class:`Unavailable` while the host is
still building something — can be retried for you: ``zygo.connect(retries=3)``
waits the server's ``Retry-After`` and sends it again. Off by default.

This package talks to ``zygo api`` over HTTP — on a unix socket when the API is
on this machine, which is the usual case and needs no token. It has no
dependencies.

What it is *not* is a second implementation of Zygo. Every boundary a sandbox
has is built by the Zygo binary and enforced by the kernel; nothing here can
widen one, and an API started without ``--allow-deploy`` will not let this
package create a sandbox at all.
"""

from ._endpoint import DEFAULT_URL, Endpoint
from ._errors import (
    AuthError,
    Busy,
    Cancelled,
    HandlerError,
    NotFound,
    SpecError,
    Stuck,
    Timeout,
    TransportError,
    Unavailable,
    ZygoError,
)
from ._models import (
    Deps,
    Event,
    Function,
    LogEntry,
    LogPage,
    Metrics,
    Minted,
    Result,
    Run,
    Runtime,
    Script,
    Served,
    Tenant,
    Token,
)
from ._sync import Client, FunctionHandle, connect

__version__ = "0.1.2"

__all__ = [
    "aio",
    "Client",
    "FunctionHandle",
    "connect",
    "DEFAULT_URL",
    "Endpoint",
    # results
    "Deps",
    "Event",
    "Function",
    "LogEntry",
    "LogPage",
    "Metrics",
    "Minted",
    "Result",
    "Run",
    "Runtime",
    "Script",
    "Served",
    "Tenant",
    "Token",
    # failures
    "AuthError",
    "Busy",
    "Cancelled",
    "HandlerError",
    "NotFound",
    "SpecError",
    "Stuck",
    "Timeout",
    "TransportError",
    "Unavailable",
    "ZygoError",
]


def __getattr__(name: str):  # noqa: ANN202 - PEP 562, a module attribute
    """``zygo_sdk.aio`` without a second import line.

    Loaded on first use rather than here, so that ``import zygo_sdk`` does not
    pull in :mod:`asyncio` for a script that never opens an event loop. Both
    ``import zygo_sdk.aio`` and ``zygo_sdk.aio`` reach the same module.
    """
    if name == "aio":
        from importlib import import_module

        return import_module(".aio", __name__)
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
