"""Zygo — warm sandboxes for function-shaped code, from Python.

    import zygo

    client = zygo.connect()                      # `zygo api` on loopback
    resize = client.fn("resize")                 # a function from sandbox.toml
    out = resize({"url": "..."})                 # ~2 ms, a fresh process
    print(out.result)

A warm function costs about a millisecond and gets a clean process per request.
A one-shot sandbox costs tens of milliseconds and needs nothing declared in
advance:

    r = client.run("python:3.12-slim", ["python3", "-c", "print(6*7)"])
    print(r.stdout)

For an event loop, ``zygo.aio`` is the same API with every call a coroutine.

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
    HandlerError,
    NotFound,
    SpecError,
    Timeout,
    TransportError,
    ZygoError,
)
from ._models import (
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

__version__ = "0.1.0"

__all__ = [
    "Client",
    "FunctionHandle",
    "connect",
    "DEFAULT_URL",
    "Endpoint",
    # results
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
    "HandlerError",
    "NotFound",
    "SpecError",
    "Timeout",
    "TransportError",
    "ZygoError",
]
