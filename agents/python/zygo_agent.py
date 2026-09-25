#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""zygo-agent — the reference runtime agent (design doc §3.4.1).

Loads a handler once, then serves each request from a ``fork()`` of the warmed
interpreter. The child starts with copy-on-write memory, so there is nothing to
copy, and it still sees clean state: no monkeypatch, cache or open file left by
the previous request.

Contract, in four rules:

1. The zygote never runs user request code itself, so its memory stays in the
   "just after a clean import" state.
2. Every request runs in its own process.
3. The child reports its pid and then waits for ``GO`` — the supervisor needs
   that window to move it into its own cgroup, or the request's limits would
   not apply to it.
4. The child exits with ``os._exit``, bypassing atexit handlers and interpreter
   teardown. One request cannot corrupt the zygote.

Pure standard library, on purpose: it is imported into every Python sandbox,
and its own import cost is paid by every tenant.
"""

from __future__ import annotations

import base64
import binascii
import gc
import hashlib
import hmac
import importlib.util
import json
import os
import random
import resource
import select
import shutil
import signal
import socket
import struct
import sys
import time
import traceback
import types

PROTOCOL_VERSION = 1

# Everything on the per-request path is imported *here*, in the zygote, so the
# child inherits it through copy-on-write. An import deferred into the child is
# paid on every single request: `inspect` alone costs ~10 ms and `random` ~2 ms,
# which is five times the entire warm-path budget.
#
# The exception is `asyncio` (~50 ms): it is only needed for async handlers, so
# it is imported at load time when the handler turns out to be one.

# Must match zygo_core::protocol::frame.
HEADER = struct.Struct(">I")
MAX_FRAME_BYTES = 32 * 1024 * 1024

# Per-request stdout/stderr capture, truncated past this (design doc §3.12).
RING_BUFFER_BYTES = 256 * 1024

# How often the agent says a request in flight is still alive (proto 1.4).
#
# Well inside the supervisor's grace so that one late wake-up is not a killed
# request, and long enough that an idle agent is not a process that wakes
# constantly for nothing.
HEARTBEAT_SECONDS = 2.0


# --------------------------------------------------------------------------
# Framing
# --------------------------------------------------------------------------


class InFlight:
    """One forked request the agent is still carrying.

    `go_fd` becomes `None` once `GO` has released the child, which is also how
    the loop knows whether a request that is finishing was ever released.
    """

    __slots__ = (
        "request_id", "pid", "result_fd", "go_fd", "chunks", "cancelled", "go_payload", "proc",
        "tmpdir",
    )

    def __init__(
        self,
        request_id: str,
        pid: int,
        result_fd: int,
        go_fd: int,
        *,
        go_payload: bytes = b"\0",
        proc=None,
        tmpdir: "str | None" = None,
    ) -> None:
        self.request_id = request_id
        self.pid = pid
        self.result_fd = result_fd
        self.go_fd: int | None = go_fd
        self.chunks: list[bytes] = []
        #: What `GO` writes to `go_fd`: one byte for a fork, which is blocked
        #: on it, and the whole request for a spawned worker, which is blocked
        #: reading its stdin.
        self.go_payload = go_payload
        #: The `Popen` of a spawned worker, which reaps it; `None` for a fork.
        self.proc = proc
        #: A `CANCEL` arrived for this request (proto 1.2).
        #:
        #: Only ever read on the way out, to set `cancelled` on the `DONE`.
        #: The agent does not stop the child itself: the supervisor kills the
        #: request's cgroup from outside the sandbox, which reaches
        #: grandchildren an agent cannot see and does not depend on tenant
        #: code cooperating.
        self.cancelled = False
        #: The request's private temporary directory, which the agent removes
        #: if the child could not. See `_private_tmp_name`.
        self.tmpdir = tmpdir


class BadFrame(Exception):
    """A frame arrived whole, but its body is not a message.

    Recoverable on purpose. The announced length was honoured, so the stream is
    still sitting at a frame boundary and the agent can report the problem and
    carry on serving — which is what `spec/protocol.md` asks for: a protocol
    failure is an `ERROR`, not silence and not a dead agent that loses every
    request in flight with it.

    An announced length past the cap is deliberately *not* this: nothing was
    consumed, the stream cannot be resynchronised, and the connection has to go.
    """


class Framing:
    """Length-prefixed JSON over a byte stream."""

    def __init__(self, sock: socket.socket) -> None:
        self._sock = sock

    def fileno(self) -> int:
        return self._sock.fileno()

    def send(self, message: dict) -> None:
        body = json.dumps(message, separators=(",", ":")).encode()
        if len(body) > MAX_FRAME_BYTES:
            raise ValueError(f"frame of {len(body)} bytes exceeds the limit")
        self._sock.sendall(HEADER.pack(len(body)) + body)

    def recv(self) -> dict | None:
        header = self._read_exactly(HEADER.size)
        if header is None:
            return None
        (size,) = HEADER.unpack(header)
        if size > MAX_FRAME_BYTES:
            raise ValueError(f"peer announced a {size} byte frame")
        body = self._read_exactly(size)
        if body is None:
            raise ConnectionError("connection closed mid-frame")
        try:
            message = json.loads(body)
        except ValueError as e:
            raise BadFrame(f"body is not valid JSON: {e}") from None
        if not isinstance(message, dict):
            raise BadFrame(f"body is {type(message).__name__}, not a JSON object")
        return message

    def _read_exactly(self, count: int) -> bytes | None:
        chunks: list[bytes] = []
        remaining = count
        while remaining:
            chunk = self._sock.recv(remaining)
            if not chunk:
                # A clean end of stream only at a frame boundary.
                return None if remaining == count else b""
            chunks.append(chunk)
            remaining -= len(chunk)
        return b"".join(chunks)


# --------------------------------------------------------------------------
# Handler loading
# --------------------------------------------------------------------------


def load_handler(path: str, mode: str):
    """Import the user's module and return its entry point.

    Everything expensive — imports, model loading, schema compilation — happens
    here, once, and is then shared by every forked child through
    copy-on-write.
    """
    spec = importlib.util.spec_from_file_location("zygo_handler", path)
    if spec is None or spec.loader is None:
        raise ImportError(f"cannot load a Python module from {path}")

    module = importlib.util.module_from_spec(spec)
    sys.modules["zygo_handler"] = module
    spec.loader.exec_module(module)

    if mode == "stdin":
        # Windmill-style scripts: the module body is the program. Re-executing
        # it per request is the contract, so the "handler" re-runs the file.
        return lambda event: _run_stdin_script(path, event)

    handler = getattr(module, "handler", None)
    if handler is None:
        raise AttributeError(
            f"{path} defines no `handler`; expected `def handler(event: dict)` "
            "— or, for a script whose module body is the program, "
            "serve it with `--mode stdin`"
        )
    return handler


def _run_stdin_script(path: str, event) -> object:
    """`mode = "stdin"`: feed the event on stdin, read the result from stdout."""
    import subprocess

    proc = subprocess.run(
        [sys.executable, path],
        input=json.dumps(event).encode(),
        capture_output=True,
        check=False,
    )
    sys.stderr.write(proc.stderr.decode(errors="replace"))
    if proc.returncode != 0:
        raise RuntimeError(f"script exited with {proc.returncode}")
    out = proc.stdout.decode(errors="replace").strip()
    return json.loads(out) if out else None


# --------------------------------------------------------------------------
# A script that arrives with the request (protocol 1.1)
# --------------------------------------------------------------------------


class ScriptError(Exception):
    """The request named a script the child could not load."""


class ScriptDigestMismatch(ScriptError):
    """The bytes are not the ones the supervisor named.

    Reported as `ERROR` / `handler_load` rather than as a failed request: a
    handler that raised is the tenant's problem, and this is a disagreement
    about what the request *is*.
    """


def load_request_script(script: dict):
    """Load the script an `EXEC` carried, and return its entry point.

    **This runs in the child, after `GO`.** Never in the zygote: a zygote that
    imported a tenant's script would hold that tenant's code, and a runtime
    pool is shared — the next request could be somebody else's. Keeping the
    load here is what lets one warm interpreter serve ten thousand scripts
    without any of them being able to reach each other through it.

    The cost is that the import is paid per request rather than once. That is
    the trade a runtime pool makes, and it is why `entry` still exists: a hot
    function should be warmed with its handler and forked, and only the long
    tail should arrive this way.
    """
    name = "zygo_request_script"
    entry_point = script.get("entry_point") or "handler"

    source = script.get("source")
    path = script.get("path")
    if source is None and path is None:
        raise ScriptError("the request's script carries neither `source` nor `path`")

    if source is not None:
        raw = source.encode()
    else:
        # Binary, and hashed as read. Text mode would translate newlines and
        # decode by the locale's encoding, so the bytes compiled here would not
        # be the bytes the supervisor wrote — and the digest below would be
        # checking something other than what runs.
        try:
            with open(path, "rb") as f:
                raw = f.read()
        except OSError as e:
            raise ScriptError(f"cannot read {path}: {e}") from None

    _check_digest(raw, script.get("digest"), path)

    module = types.ModuleType(name)
    module.__file__ = path or "<script>"
    # Not in `sys.modules`: the child is about to exit, and a module that is
    # registered can be found by anything else the handler imports. Nothing
    # here should outlive the request, and this is the only copy of it.
    try:
        exec(compile(raw, module.__file__, "exec"), module.__dict__)
    except BaseException as exc:
        raise ScriptError(f"the script did not load: {_handler_traceback(exc)}") from None

    handler = getattr(module, entry_point, None)
    if handler is None:
        raise ScriptError(
            f"{module.__file__} defines no `{entry_point}`; "
            f"expected `def {entry_point}(event: dict)`"
        )
    return handler


def _check_digest(raw: bytes, digest: "str | None", path: "str | None") -> None:
    """Refuse bytes that are not the ones the supervisor named.

    Not a formality. `/run/script/<hash>` is written by the supervisor but
    lives in a sandbox with one uid, so the child about to load it can unlink
    it and put its own there instead — for itself, or for another request in
    flight on the same pool zygote. The digest arrives on the supervisor's
    connection, which nothing inside can reach, so it is the part of this that
    a tenant cannot forge.

    Hashed over what was *read*, never by opening the file a second time: two
    reads are two different files if somebody is trying.
    """
    if not digest:
        return
    actual = "sha256:" + hashlib.sha256(raw).hexdigest()
    if not hmac.compare_digest(actual, digest):
        where = path or "the script in this EXEC"
        raise ScriptDigestMismatch(
            f"{where} is {actual}, and the request asked for {digest}; "
            "refusing to run it"
        )


def has_extra_threads() -> bool:
    """Whether the handler started threads at import time that a fork cannot
    survive.

    ``fork()`` in a threaded process only carries the calling thread across, so
    a lock another thread held stays locked forever in the child. That is risk
    R1; the caller falls back to spawning instead of forking.

    Python threads are not the only kind. ``import duckdb`` starts four native
    ones, which ``threading`` cannot see: forked anyway, a child died in glibc
    with "The futex facility returned an unexpected error code" on about half
    of all requests and took the zygote with it. So the kernel's count is read
    too. Native threads are not all a hazard, though: OpenBLAS starts its pool
    at import and stops it in a ``pthread_atfork`` handler, so it is gone by
    the time the fork happens. The test is therefore the one CPython applies
    itself: fork once, and count what is still running in the parent. A pool
    that stops for a fork is fine; one that does not is what deadlocks.
    """
    import threading

    if threading.active_count() > 1:
        return True
    try:
        if len(os.listdir("/proc/self/task")) <= 1:
            return False
    except OSError:
        return False
    import warnings

    with warnings.catch_warnings():
        # The warning this would print is the verdict being taken here.
        warnings.simplefilter("ignore", DeprecationWarning)
        pid = os.fork()
    if pid == 0:
        os._exit(0)
    try:
        survivors = len(os.listdir("/proc/self/task"))
    finally:
        os.waitpid(pid, 0)
    return survivors > 1


# --------------------------------------------------------------------------
# Child: one request
# --------------------------------------------------------------------------


def _handler_traceback(exc: BaseException) -> str:
    """The traceback as the handler author's own, without Zygo's own frame.

    ``format_exc()`` starts where the exception was caught, which is the
    ``try:`` in `run_request` — so the first line a developer sees when their
    handler raises is ``File "/zygo/agent.py", line N, in run_request``, the
    runtime explaining itself before it explains their bug. That frame is the
    same for every failure and there is nothing to do about it from a handler,
    so it is noise on the one output that has to be clear.

    Only the *leading* run of frames in this file is dropped, and only when
    something is left underneath: a handler that calls back into the agent
    keeps its frames, and an error raised by the harness itself still prints
    in full, because there the harness *is* the answer.

    Chained causes are unaffected: `format_exception` walks ``__cause__`` and
    ``__context__`` from the exception, not from the traceback given here.
    """
    tb = exc.__traceback__
    while tb is not None and tb.tb_frame.f_globals.get("__file__") == __file__:
        tb = tb.tb_next
    if tb is None:
        return traceback.format_exc()
    return "".join(traceback.format_exception(type(exc), exc, tb))


# A child that could not run the request, by reason. Distinct from any exit
# code a handler can produce, so the supervisor's log says which happened.
_EXIT_NO_GO = 121
"""The supervisor closed the GO pipe without sending GO."""

_EXIT_CHILD_ESCAPED = 122
"""`run_request` returned instead of exiting; the child left rather than
unwind into the parent's serve loop."""


#: The request key that carries a private temporary directory's name from the
#: zygote to its child. The agent's own, never sent by a supervisor.
PRIVATE_TMP_KEY = "_zygo_tmp"

#: Where those directories go: the workspace tmpfs. Its root is `0311`, so a
#: request can create a directory there but cannot list its neighbours'.
#: `ZYGO_AGENT_TMP_PARENT` moves it, for the agent's own tests outside a
#: sandbox; nothing in Zygo sets it.
PRIVATE_TMP_PARENT = os.environ.get("ZYGO_AGENT_TMP_PARENT", "/work")


def _private_tmp_name() -> "str | None":
    """A name for the next request's temporary directory, or `None` where the
    sandbox has no workspace tmpfs.

    `/tmp` is one tmpfs for the whole sandbox, so a file one request leaves
    there is still there for the next — for the next tenant, in a runtime pool.
    A mount namespace per request would fix that and a forked child cannot make
    one (see the `workspace` module), so each request gets a directory of its
    own instead: 128 random bits under a parent that cannot be listed, and
    every temp-file API pointed at it. Chosen here, in the zygote, so the
    zygote can remove it if the child dies before it does.
    """
    if not os.path.isdir(PRIVATE_TMP_PARENT):
        return None
    return os.path.join(PRIVATE_TMP_PARENT, "tmp-" + os.urandom(16).hex())


def _enter_private_tmp(path: str, create: bool) -> bool:
    """Point `TMPDIR`, `TMP`, `TEMP` and `tempfile` at `path`.

    `tempfile.tempdir` as well as the environment: `tempfile` caches the
    directory the first time anybody asks, and a handler module that asked at
    import time left the zygote's answer — `/tmp` — in every child.
    """
    if create:
        try:
            os.mkdir(path, 0o700)
        except OSError:
            return False
    for key in ("TMPDIR", "TMP", "TEMP"):
        os.environ[key] = path
    cached = sys.modules.get("tempfile")
    if cached is not None:
        cached.tempdir = path
    return True


def _remove_private_tmp(path: "str | None") -> None:
    if path:
        shutil.rmtree(path, ignore_errors=True)


def run_request(
    handler,
    request: dict,
    result_fd: int,
    go_fd: int,
    child_filter: "ChildFilter | None" = None,
) -> None:
    # `handler` may be `None` when this agent is a runtime pool: the zygote
    # holds an interpreter and its dependencies and no tenant code at all, and
    # the script arrives in the request. It is loaded below, after `GO` and
    # after the child filter, because both of those are the supervisor's
    # guarantees about this process and the script is the first thing that is
    # not.
    """Run one request in the freshly forked child. Never returns."""
    exit_code = 0
    error = None
    value = None
    wall_ms = 0.0
    # Set when the request was refused rather than run: the answer is then an
    # `ERROR` frame rather than a `RESULT`. See `_check_digest`.
    refused = None
    # The private temporary directory this child made, removed on the way out.
    own_tmp = None

    # Proto 1.3: the request asked to watch its own output. The frames go on
    # the same pipe the `RESULT` does, ahead of it, and the agent forwards
    # them as they arrive.
    streaming = bool(request.get("stream"))

    def chunk(stream: str, text: str) -> None:
        _write_frame(result_fd, {
            "type": "CHUNK",
            "id": request.get("id", ""),
            "stream": stream,
            "data": text,
        })

    out = _Ring(tee=(lambda t: chunk("stdout", t)) if streaming else None)
    err = _Ring(tee=(lambda t: chunk("stderr", t)) if streaming else None)
    started = time.monotonic()

    try:
        # Wait for the supervisor to place this pid in its own cgroup. Until
        # `GO` arrives the child has the *zygote's* limits, so a runaway
        # allocation here would be billed to the wrong place.
        #
        # End of file is not `GO`. A zero-length read means the supervisor
        # closed the pipe without sending anything — it gave up, or it died —
        # and running the handler then is running it with the zygote's limits,
        # which is the one thing this barrier exists to prevent. The child
        # leaves instead, and the supervisor sees the result pipe close.
        if len(os.read(go_fd, 1)) != 1:
            os._exit(_EXIT_NO_GO)
        os.close(go_fd)

        # The supervisor's tightening for this child, before any handler code:
        # no new program, no new process. A filter that cannot be installed
        # is a failed request, never a request that ran without it.
        if child_filter is not None:
            child_filter.install()

        # Each request gets its own entropy. Without this, forks share the
        # parent's seeded state and every child produces identical "random"
        # values — tokens, temp names, jitter.
        _reseed_random()

        # A temporary directory of this request's own, before any request code
        # runs — the script's module body included. The workspace when the
        # caller sent one, which is private already and which the supervisor
        # removes; otherwise the directory the zygote named for this child.
        if request.get("workspace"):
            _enter_private_tmp(request["workspace"], create=False)
        elif request.get(PRIVATE_TMP_KEY):
            if _enter_private_tmp(request[PRIVATE_TMP_KEY], create=True):
                own_tmp = request[PRIVATE_TMP_KEY]

        # The script, if the supervisor sent one. After the filter on purpose:
        # under `strict` a script that tries to start a program is refused by
        # the kernel while it is loading, not after.
        #
        # Captured, because a module body is request code: what a script
        # prints while it loads belongs to the request that sent it, not to
        # the zygote's own log where the next tenant's `zygo logs` would find
        # it. The same ring the handler writes into, so the two arrive in
        # order.
        script = request.get("script")
        if script:
            with _captured(out, err):
                handler = load_request_script(script)
        elif handler is None:
            raise ScriptError(
                "this agent was started without a handler, so every request must "
                "carry a `script`"
            )

        # The handler's own progress reports, which are not its output. A
        # long request has two things to say — what it printed, and how far it
        # has got — and a caller that had to parse the first to find the
        # second would be parsing a handler's log messages.
        #
        # Attached **whether or not anybody is listening**. A handler that
        # calls `event.progress(...)` must not break because this particular
        # caller did not ask for a stream: whether there is a reader is not
        # the handler's business, and a method that exists only sometimes is a
        # method every handler has to guard. Without a stream it is a no-op.
        #
        # The cost is one `dict` copy per request, for an event that is a
        # dict. Measured against a 1.9 ms p50 that is a fraction of a percent,
        # and the alternative is handlers that work on one call and not the
        # next.
        event = _StreamingEvent(
            request.get("event"),
            (lambda value: chunk("progress", value)) if streaming else None,
        )

        # This request's own directory, if it has one (proto 1.5). The
        # handler is told where it is and started *in* it, so a handler that
        # writes `out.txt` writes it somewhere the caller will collect rather
        # than somewhere the next request will find.
        #
        # `chdir` and not a mount: making one path mean a different directory
        # to each request needs a mount namespace per request, and a forked
        # child has no capability to create one — measured, see the Rust
        # `workspace` module. The path is unguessable instead.
        workspace = request.get("workspace")
        if workspace:
            os.environ["ZYGO_WORKSPACE"] = workspace
            try:
                os.chdir(workspace)
            except OSError as exc:
                raise ScriptError(
                    f"the supervisor said this request's workspace is {workspace}, "
                    f"and it cannot be entered: {exc}"
                ) from exc

        os.environ["ZYGO_REQUEST_ID"] = request.get("id", "")
        os.environ["ZYGO_DEADLINE_MS"] = str(request.get("timeout_ms", 0))
        for key, val in (request.get("env_overrides") or {}).items():
            os.environ[key] = val

        started = time.monotonic()
        with _captured(out, err):
            value = handler(event)
            if _is_awaitable(value):
                import asyncio

                value = asyncio.new_event_loop().run_until_complete(value)
        wall_ms = (time.monotonic() - started) * 1000.0

        try:
            json.dumps(value, default=_json_default)
        except (TypeError, ValueError) as exc:
            raise TypeError(f"handler returned a value that is not JSON: {exc}") from exc

    except ScriptDigestMismatch as exc:
        # Not a failed handler — nothing of the script has run. The supervisor
        # and this child disagree about what the request is, which §2 makes an
        # `ERROR` so that a caller can tell "your code raised" from "your code
        # was not what was asked for".
        refused = str(exc)
    except BaseException as exc:  # noqa: BLE001 - the child reports everything upwards
        error = _handler_traceback(exc)
        exit_code = 1
        value = None
        if wall_ms == 0.0:
            wall_ms = (time.monotonic() - started) * 1000.0

    usage = resource.getrusage(resource.RUSAGE_SELF)
    if refused is not None:
        message = {
            "type": "ERROR",
            "id": request.get("id", ""),
            "code": "handler_load",
            "message": refused,
        }
    else:
        message = {
            "type": "RESULT",
            "id": request.get("id", ""),
            "exit_code": exit_code,
            "result": value,
            "stdout": _text(out),
            "stderr": _text(err),
            "peak_rss_kb": _peak_rss_kb(usage),
            "wall_ms": wall_ms,
            "cpu_ms": (usage.ru_utime + usage.ru_stime) * 1000.0,
        }
        if error is not None:
            message["error"] = error

    _write_frame(result_fd, message)
    os.close(result_fd)
    # After the answer, so it is off the request's clock. The zygote removes
    # it too if this child dies first.
    _remove_private_tmp(own_tmp)

    # Straight out: no atexit handlers, no interpreter teardown, no flushing of
    # buffers the parent also owns.
    os._exit(0)


CHILD_SECCOMP_ENV = "ZYGO_CHILD_SECCOMP"


class ChildFilter:
    """The seccomp program the supervisor asks every forked child to install.

    It arrives in ``ZYGO_CHILD_SECCOMP`` as base64 of the raw ``struct
    sock_filter`` array — the supervisor knows the host's syscall numbers and
    this agent does not need to. Decoded and laid out once, in the zygote, so
    the child's share of the work is one ``prctl``. Filters stack: the
    sandbox's own filter stays in force underneath this one.
    """

    PR_SET_SECCOMP = 22
    PR_SET_NO_NEW_PRIVS = 38
    SECCOMP_MODE_FILTER = 2

    def __init__(self, raw: bytes) -> None:
        import ctypes

        if not raw or len(raw) % 8:
            raise ValueError(
                f"{CHILD_SECCOMP_ENV} is {len(raw)} bytes, not a whole number of "
                "8-byte BPF instructions"
            )

        class SockFprog(ctypes.Structure):
            _fields_ = [("len", ctypes.c_ushort), ("filter", ctypes.c_void_p)]

        # Kept on the instance: the kernel reads the instructions through the
        # pointer at install time, so the buffer must outlive the struct.
        self._buffer = ctypes.create_string_buffer(raw, len(raw))
        self._prog = SockFprog(len(raw) // 8, ctypes.cast(self._buffer, ctypes.c_void_p))
        self._libc = ctypes.CDLL(None, use_errno=True)
        self._libc.prctl.restype = ctypes.c_int
        self._libc.prctl.argtypes = [ctypes.c_int] + [ctypes.c_ulong] * 4
        self._ctypes = ctypes
        self.instructions = len(raw) // 8

    @classmethod
    def from_environment(cls) -> "ChildFilter | None":
        encoded = os.environ.get(CHILD_SECCOMP_ENV)
        if not encoded:
            return None
        try:
            raw = base64.b64decode(encoded, validate=True)
        except (ValueError, binascii.Error) as exc:
            raise ValueError(f"{CHILD_SECCOMP_ENV} is not base64: {exc}") from exc
        return cls(raw)

    def install(self) -> None:
        """Install in the calling process. Irreversible, by design."""
        # Already set by the launcher; harmless to repeat, and it is what lets
        # an unprivileged process install a filter at all.
        if self._libc.prctl(self.PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0:
            errno_ = self._ctypes.get_errno()
            raise OSError(errno_, f"PR_SET_NO_NEW_PRIVS: {os.strerror(errno_)}")
        rc = self._libc.prctl(
            self.PR_SET_SECCOMP,
            self.SECCOMP_MODE_FILTER,
            self._ctypes.addressof(self._prog),
            0,
            0,
        )
        if rc != 0:
            errno_ = self._ctypes.get_errno()
            raise OSError(
                errno_,
                f"cannot install the child seccomp filter "
                f"({self.instructions} instructions): {os.strerror(errno_)}",
            )


def _reseed_random() -> None:
    random.seed(os.urandom(16))
    # The process-wide generators of the libraries that have one, when the
    # handler imported them — never imported here. numpy's legacy global and
    # torch's default generator are seeded once, in the zygote, and every
    # child would otherwise draw the same sequence from them.
    np_random = sys.modules.get("numpy.random")
    if np_random is not None:
        try:
            np_random.seed()
        except Exception:  # noqa: BLE001
            pass
    torch = sys.modules.get("torch")
    if torch is not None:
        try:
            torch.seed()
        except Exception:  # noqa: BLE001
            pass


def shared_generators(handler) -> list[str]:
    """Module-level generators in the handler's module, by name.

    A generator the handler built at import time lives in the zygote, and every
    child starts from a copy of its state: ``RNG = random.Random()`` or
    ``np.random.default_rng()`` hands every request the same "random" numbers.
    The agent cannot reseed an object it does not know about, so it says so
    at warm-up instead.
    """
    names = []
    for name, value in getattr(handler, "__globals__", {}).items():
        cls = type(value)
        if isinstance(value, random.Random) and value is not getattr(random, "_inst", None):
            names.append(name)
        elif cls.__module__.startswith("numpy.random") and cls.__name__ in (
            "Generator",
            "RandomState",
        ):
            names.append(name)
        elif cls.__module__.startswith("torch") and cls.__name__ == "Generator":
            names.append(name)
    return names


def _is_awaitable(value) -> bool:
    """`inspect.isawaitable` without importing `inspect`.

    `inspect` costs ~10 ms to import and this runs in the child on every
    request. The two checks below cover what a handler can actually return: a
    native coroutine, or an object implementing the awaitable protocol.
    """
    return isinstance(value, types.CoroutineType) or hasattr(type(value), "__await__")


# `co_flags` bit for a coroutine function (CPython `Include/cpython/code.h`).
CO_COROUTINE = 0x80


def is_async_handler(fn) -> bool:
    """Whether calling `fn` will return a coroutine, decided without `inspect`."""
    code = getattr(fn, "__code__", None)
    return bool(code and code.co_flags & CO_COROUTINE)


def _json_default(value):
    """`bytes` become base64, as the handler contract promises."""
    if isinstance(value, (bytes, bytearray)):
        return base64.b64encode(bytes(value)).decode()
    raise TypeError(f"{type(value).__name__} is not JSON serialisable")


def _peak_rss_kb(usage) -> int:
    # Linux reports kilobytes; macOS reports bytes.
    peak = usage.ru_maxrss
    return peak // 1024 if sys.platform == "darwin" else peak


class _Ring:
    """Capture that keeps the first `limit` bytes and counts the rest.

    With a `tee`, every write is *also* handed on as it happens — which is how
    a streaming request (proto 1.3) gets its output to the caller before it
    finishes. The capture still happens: `RESULT` carries the whole of stdout
    and stderr either way, so a caller that streamed and one that did not see
    the same text, and neither has to reassemble anything.

    Without a `tee` this is exactly what it was, which is the point: a `CHUNK`
    per `print()` is a syscall per `print()`, and a request that did not ask
    for a stream must not pay for one.
    """

    def __init__(self, limit: int = RING_BUFFER_BYTES, tee=None) -> None:
        self._parts: list[str] = []
        self._size = 0
        self._dropped = 0
        self._limit = limit
        self._tee = tee

    def write(self, text: str) -> int:
        if self._tee is not None and text:
            self._tee(text)
        room = self._limit - self._size
        if room > 0:
            piece = text[:room]
            self._parts.append(piece)
            self._size += len(piece)
            self._dropped += len(text) - len(piece)
        else:
            self._dropped += len(text)
        return len(text)

    def flush(self) -> None:
        pass

    def isatty(self) -> bool:
        return False

    def value(self) -> str:
        text = "".join(self._parts)
        if self._dropped:
            text += f"\n… {self._dropped} bytes truncated"
        return text


class _captured:
    """Redirect stdout and stderr into the given ring buffers."""

    def __init__(self, out: _Ring, err: _Ring) -> None:
        self._out, self._err = out, err

    def __enter__(self) -> tuple[_Ring, _Ring]:
        self._saved = (sys.stdout, sys.stderr)
        sys.stdout, sys.stderr = self._out, self._err
        return self._out, self._err

    def __exit__(self, *_exc) -> bool:
        sys.stdout, sys.stderr = self._saved
        return False


def _text(buffer) -> str:
    return buffer.value() if isinstance(buffer, _Ring) else str(buffer)


# --------------------------------------------------------------------------
# Zygote
# --------------------------------------------------------------------------


class Agent:
    """The zygote loop.

    Requests are served one at a time. The ``concurrency`` field in the spec is
    enforced by the supervisor, which is what owns the queue and the
    backpressure; an agent that also multiplexed would
    have to duplicate that logic and could not apply per-request limits without
    the supervisor's cooperation anyway.
    """

    def __init__(
        self,
        sock: socket.socket,
        handler,
        *,
        can_fork: bool,
        handler_path: str,
        mode: str,
        child_filter: "ChildFilter | None" = None,
    ) -> None:
        self._wire = Framing(sock)
        self._handler = handler
        self._can_fork = can_fork
        self._handler_path = handler_path
        self._mode = mode
        self._child_filter = child_filter
        # Children whose result has already been forwarded, still to be reaped.
        # Reaping is process teardown, and teardown of a forked interpreter is
        # not fast: measured at p99 32 ms on the request path, against a 2 ms
        # budget. Once the RESULT is in hand the answer owes nothing to the
        # corpse, so it is collected between requests instead.
        self._unreaped: list[tuple[int, str | None]] = []
        # Requests that have been forked and not yet answered, keyed by id and
        # by the descriptor their result will arrive on.
        self._inflight: dict[str, InFlight] = {}
        self._by_result_fd: dict[int, InFlight] = {}

    def serve(self) -> None:
        """Handle requests until the supervisor goes away.

        Several at once. The loop waits on the control socket *and* on every
        in-flight request's result pipe together, so a request that is still
        running does not stop the next `EXEC` being accepted.

        Doing one request to completion before reading the next was the
        original shape, and it was measured to be the ceiling on throughput:
        with the CPU quota lifted, four concurrent callers got the same ~600
        requests/s as one, using 1.5 of 4 cores, and one of them waited 3.7 s
        for its turn. `concurrency` in the spec bounds what the supervisor
        admits; this is what lets the agent actually overlap them.
        """
        wire_fd = self._wire.fileno()
        accepting = True
        beat_at = time.monotonic() + HEARTBEAT_SECONDS

        while accepting or self._inflight:
            watch = list(self._by_result_fd)
            if accepting:
                watch.append(wire_fd)
            if not watch:
                break

            try:
                # A bounded wait rather than an indefinite one, so the loop
                # wakes often enough to send heartbeats for the requests it is
                # carrying (proto 1.4). With nothing in flight the timeout
                # costs one wake-up every few seconds and nothing else.
                #
                # The bound is the time left until the *next beat is due*, not
                # a fresh two seconds. Waiting a fixed period and beating only
                # when the wait expired is what this did, and it meant the
                # heartbeat was starved by traffic: measured with
                # `poc/agent_stall.py`, one `PING` from the supervisor a
                # second into a three-second request pushed the beat past the
                # end of the request, and an agent busy enough to be woken
                # every couple of seconds would never beat at all. That is
                # exactly the agent whose requests most need saying alive —
                # and after `HEARTBEAT_GRACE` the supervisor kills a long one
                # as stuck.
                ready, _, _ = select.select(
                    watch, [], [], max(0.0, beat_at - time.monotonic())
                )
            except InterruptedError:
                continue
            except OSError:
                return

            if time.monotonic() >= beat_at:
                self._heartbeat()
                beat_at = time.monotonic() + HEARTBEAT_SECONDS
            if not ready:
                continue

            for fd in ready:
                if fd == wire_fd:
                    if not self._handle_message():
                        # End of stream, or a `SHUTDOWN`: stop taking new work
                        # but finish what is already forked, so a request in
                        # flight still gets its answer.
                        accepting = False
                else:
                    self._collect(self._by_result_fd[fd])

        self._reap_all()

    def _heartbeat(self) -> None:
        """Say that every request still in flight is still alive (proto 1.4).

        A `PING` carrying a request id, which is how a supervisor tells a
        request that is *working* from one that is wedged. It exists because a
        long timeout is a poor backstop on its own: a request stuck in the
        first minute of a six-hour budget would hold its slot for the rest of
        it, and the deadline is the only thing that would ever notice.

        Deliberately not conditional on the child looking busy. The agent
        cannot tell a child that is computing from one that is blocked, and a
        heartbeat that tried to would be reporting a guess. What it can say
        honestly is that the child exists and this agent is still scheduling,
        which is exactly what going quiet would deny.
        """
        for request in list(self._inflight.values()):
            if request.go_fd is None:
                self._wire.send({"type": "PING", "seq": 0, "id": request.request_id})

    def _handle_message(self) -> bool:
        """Read and act on one control message. `False` means stop accepting."""
        try:
            message = self._wire.recv()
        except BadFrame as e:
            # The stream is still aligned, so this is reportable rather than
            # fatal. Found by `zygo agent test`: before this, one malformed
            # frame killed the agent and every request in flight with it.
            self._wire.send(
                {"type": "ERROR", "id": None, "code": "bad_message", "message": str(e)}
            )
            return True
        except (ConnectionError, OSError):
            return False
        if message is None:
            return False

        kind = message.get("type")
        if kind == "EXEC":
            self._exec(message)
        elif kind == "GO":
            self._release(message.get("id", ""))
        elif kind == "CANCEL":
            self._cancel(message.get("id", ""))
        elif kind == "PING":
            self._wire.send({"type": "PONG", "seq": message.get("seq", 0)})
        elif kind == "SHUTDOWN":
            return False
        else:
            self._wire.send(
                {
                    "type": "ERROR",
                    "id": message.get("id"),
                    "code": "bad_message",
                    "message": f"unexpected message `{kind}`",
                }
            )
        return True

    def _release(self, request_id: str) -> None:
        """`GO`: the supervisor has the child in its cgroup; let it run."""
        request = self._inflight.get(request_id)
        if request is None or request.go_fd is None:
            return
        # A loop, because a spawned worker's payload is a whole request and
        # can be larger than a pipe holds; the worker reads all of it.
        payload = memoryview(request.go_payload)
        try:
            while payload:
                payload = payload[os.write(request.go_fd, payload):]
        except OSError:
            pass
        os.close(request.go_fd)
        request.go_fd = None

    def _cancel(self, request_id: str) -> None:
        """`CANCEL`: remember why this request is about to die (proto 1.2).

        Deliberately not a kill. The supervisor writes `cgroup.kill` on the
        request's own cgroup from outside the sandbox — which takes the child
        and everything it spawned, and does not require the handler to be in a
        state where a signal helps. All that is left for the agent is to say
        *why* on the way out, so the caller reads `cancelled` instead of
        guessing between a deadline and an out-of-memory kill at exit 137.

        An unknown id is ignored: the request finished between the supervisor
        deciding to cancel it and this arriving, which is a race with no
        wrong outcome.
        """
        request = self._inflight.get(request_id)
        if request is not None:
            request.cancelled = True

    def _collect(self, request: InFlight) -> None:
        """A result pipe is readable: take what is there, answer at end of file.

        A streaming request's pipe carries several frames — a `CHUNK` per piece
        of output, then the `RESULT` — so whole frames are decoded and
        forwarded **as they arrive**. That is the whole value of a stream: an
        agent that accumulated would deliver the same bytes at the same moment
        `RESULT` does, which is what it already did before this existed.
        """
        try:
            chunk = os.read(request.result_fd, 64 * 1024)
        except OSError:
            chunk = b""
        if chunk:
            request.chunks.append(chunk)
            self._forward_chunks(request)
            return

        self._finish(request)

    def _forward_chunks(self, request: InFlight) -> None:
        """Send on every complete `CHUNK` the child has written so far.

        Anything that is not a `CHUNK` — the `RESULT`, or an `ERROR` — is left
        where it is for `_finish`, which is the one place that decides what a
        request's answer was. So this only ever moves output, never outcomes.
        """
        buffer = b"".join(request.chunks)
        at = 0
        while True:
            if len(buffer) - at < HEADER.size:
                break
            (size,) = HEADER.unpack(buffer[at : at + HEADER.size])
            end = at + HEADER.size + size
            if len(buffer) < end or size > MAX_FRAME_BYTES:
                break
            try:
                message = json.loads(buffer[at + HEADER.size : end])
            except (ValueError, UnicodeDecodeError):
                break
            if not isinstance(message, dict) or message.get("type") != "CHUNK":
                break
            self._wire.send(message)
            at = end
        request.chunks = [buffer[at:]] if at else [buffer]

    def _finish(self, request: InFlight) -> None:
        os.close(request.result_fd)
        del self._by_result_fd[request.result_fd]
        del self._inflight[request.request_id]
        if request.go_fd is not None:
            # The supervisor never said `GO`, so the child is still blocked.
            os.close(request.go_fd)

        result = _decode_result_frame(b"".join(request.chunks), request.request_id)
        if result is None:
            # No result means the child died on the way — an OOM kill, a
            # deadline kill, or a segfault in a C extension. Here the exit
            # status *is* the answer, so it is worth waiting for.
            if request.proc is not None:
                code = request.proc.wait()
                exit_code = 128 - code if code < 0 else code
                error = (
                    _death_reason(-code) if code < 0
                    else f"spawned worker exited with {code} without returning a result"
                )
            else:
                _, status = os.waitpid(request.pid, 0)
                exit_code, error = _exit_code(status), _death_reason(status)
            # The child is gone and did not clean up after itself.
            _remove_private_tmp(request.tmpdir)
            result = {
                "id": request.request_id,
                "exit_code": exit_code,
                "result": None,
                "stdout": "",
                "stderr": "",
                "error": error,
                "peak_rss_kb": 0,
                "wall_ms": 0.0,
                "cpu_ms": 0.0,
            }
        else:
            # The child has already reported and called `_exit`; what remains
            # is the kernel tearing its address space down. Answering now and
            # reaping later keeps that off the request's clock. A spawned
            # worker closed its stdout by exiting, so it is reaped at once.
            if request.proc is not None:
                request.proc.wait()
                _remove_private_tmp(request.tmpdir)
            else:
                self._unreaped.append((request.pid, request.tmpdir))

        # An `ERROR` from the child goes up as an `ERROR`: it is the answer to
        # this `EXEC` either way (§3.5), and a request that was refused is not
        # a request that produced a result.
        if result.get("type") != "ERROR":
            result["type"] = "DONE"
            if request.cancelled:
                result["cancelled"] = True
        self._send_result(result)
        self._reap_finished()

    def _send_result(self, result: dict) -> None:
        """Send a `DONE`, or a `DONE` saying why the real one could not go.

        A handler that returns more than the frame limit allows used to raise
        out of the serve loop and take the agent with it: one oversize return
        value, and every later request to that function failed until it was
        rewarmed (B-24). The request is the thing that failed, not the agent,
        so it is reported as a failed request and the loop carries on.

        The replacement is built from scratch rather than by trimming the
        original, because whatever made it oversize is in there.
        """
        try:
            self._wire.send(result)
            return
        except ValueError as exc:
            reason = str(exc)

        self._wire.send(
            {
                "type": "DONE",
                "id": result.get("id"),
                "exit_code": 1,
                "error": (
                    f"the handler's result does not fit in one frame ({reason}); "
                    "return a reference to it — a path in a writable mount, an "
                    "object key — rather than the bytes"
                ),
                "stdout": "",
                "stderr": "",
                "peak_rss_kb": result.get("peak_rss_kb", 0),
                "wall_ms": result.get("wall_ms", 0.0),
                "cpu_ms": result.get("cpu_ms", 0.0),
            }
        )

    def _open_fds(self) -> list[int]:
        """Descriptors a newly forked child must not inherit.

        Every in-flight request's pipes. Without this the new child holds the
        write end of *another* request's result pipe, so that request's reader
        never sees end of file and its answer waits for an unrelated handler to
        finish — the failure that turns overlapping requests into a deadlock.
        """
        fds = []
        for other in self._inflight.values():
            fds.append(other.result_fd)
            if other.go_fd is not None:
                fds.append(other.go_fd)
        return fds

    def _exec(self, request: dict) -> None:
        """Fork a child for `request` and return; the loop takes it from here.

        Nothing is waited for: `FORKED` goes out and the request joins the
        in-flight table. `GO` arrives as an ordinary message and the result
        arrives on the pipe, both handled by `serve`.
        """
        if not self._can_fork:
            self._exec_spawned(request)
            return

        request_id = request.get("id", "")
        tmpdir = None if request.get("workspace") else _private_tmp_name()
        if tmpdir:
            request[PRIVATE_TMP_KEY] = tmpdir
        result_r, result_w = os.pipe()
        go_r, go_w = os.pipe()
        # Collected before the fork: afterwards the child cannot ask the parent
        # what else was open, and it must close every one of them.
        inherited = self._open_fds()

        try:
            pid = os.fork()
        except OSError as exc:
            for fd in (result_r, result_w, go_r, go_w):
                os.close(fd)
            self._wire.send(
                {
                    "type": "ERROR",
                    "id": request_id,
                    "code": "spawn_failed",
                    "message": str(exc),
                }
            )
            return

        if pid == 0:
            os.close(result_r)
            os.close(go_w)
            # Another request's pipes, inherited by accident. Holding the write
            # end of someone else's result pipe would stop them ever reaching
            # end of file.
            for fd in inherited:
                try:
                    os.close(fd)
                except OSError:
                    pass
            # `os._exit` in a `finally`, because the child returning here is
            # the worst outcome available: it would unwind into the parent's
            # `serve()` loop and there would be two agents on one socket,
            # answering each other's requests. `run_request` exits on every
            # path it knows about; this covers the ones it does not — an
            # exception before its own try block, a `SystemExit` from handler
            # code, a bug in this file.
            try:
                run_request(self._handler, request, result_w, go_r, self._child_filter)
            finally:
                os._exit(_EXIT_CHILD_ESCAPED)

        os.close(result_w)
        os.close(go_r)

        request_state = InFlight(request_id, pid, result_r, go_w, tmpdir=tmpdir)
        self._inflight[request_id] = request_state
        self._by_result_fd[result_r] = request_state

        # The supervisor needs the pid to build the request cgroup; the child
        # is blocked on its `go` pipe until `GO` comes back.
        self._wire.send({"type": "FORKED", "id": request_id, "pid": pid})

    def _exec_spawned(self, request: dict) -> None:
        """Fallback for handlers that cannot be forked (risk R1).

        A fresh interpreter per request, imports and all, instead of a 1-2 ms
        fork — but it cannot deadlock on a lock some import-time thread was
        holding. The
        handshake is unchanged, so the supervisor still gets its cgroup window
        — the worker blocks reading stdin until `GO` writes the request to it.

        It joins the same in-flight table a fork does, with its stdin as the
        `go` pipe and its stdout as the result pipe, so the serve loop carries
        several at once and keeps sending heartbeats. It used to run each one
        to completion inside this method and answer every `EXEC` that arrived
        meanwhile with `overloaded`: a function served with `concurrency = 8`
        whose import started a thread failed 40 of 49 requests in the fork
        sweep (`poc/fork_sweep.py`).
        """
        import subprocess

        request_id = request.get("id", "")
        tmpdir = None if request.get("workspace") else _private_tmp_name()
        if tmpdir:
            request[PRIVATE_TMP_KEY] = tmpdir
        go_r, go_w = os.pipe()
        result_r, result_w = os.pipe()
        try:
            proc = subprocess.Popen(
                [sys.executable, os.path.abspath(__file__), "--oneshot",
                 self._handler_path, self._mode],
                stdin=go_r,
                stdout=result_w,
            )
        except OSError as exc:
            for fd in (go_r, go_w, result_r, result_w):
                os.close(fd)
            self._wire.send(
                {
                    "type": "ERROR",
                    "id": request_id,
                    "code": "spawn_failed",
                    "message": str(exc),
                }
            )
            return
        os.close(go_r)
        os.close(result_w)

        body = json.dumps(request, separators=(",", ":")).encode()
        request_state = InFlight(
            request_id, proc.pid, result_r, go_w,
            go_payload=HEADER.pack(len(body)) + body, proc=proc, tmpdir=tmpdir,
        )
        self._inflight[request_id] = request_state
        self._by_result_fd[result_r] = request_state
        self._wire.send({"type": "FORKED", "id": request_id, "pid": proc.pid})

    def _reap_finished(self) -> None:
        """Collect children that have finished exiting, without waiting.

        Called after the answer has gone out, so a slow teardown costs nothing.
        At most one child is ever outstanding, because requests are serialised.
        """
        still_running = []
        for pid, tmpdir in self._unreaped:
            try:
                reaped, _ = os.waitpid(pid, os.WNOHANG)
            except ChildProcessError:
                reaped = pid  # already gone
            if reaped == 0:
                still_running.append((pid, tmpdir))
            elif tmpdir and os.path.lexists(tmpdir):
                # It answered and then died before removing its directory.
                _remove_private_tmp(tmpdir)
        self._unreaped = still_running

    def _reap_all(self) -> None:
        """Block until every child is gone. Used on shutdown."""
        for pid, tmpdir in self._unreaped:
            try:
                os.waitpid(pid, 0)
            except ChildProcessError:
                pass
            _remove_private_tmp(tmpdir)
        self._unreaped = []

    @staticmethod
    def _read_result(fd: int, request_id: str) -> dict | None:
        chunks: list[bytes] = []
        while True:
            chunk = os.read(fd, 64 * 1024)
            if not chunk:
                break
            chunks.append(chunk)
        return _decode_result_frame(b"".join(chunks), request_id)


def _write_frame(fd: int, message: dict) -> None:
    """One framed message to the agent, header first.

    The same framing the control socket uses, so the agent can decode the
    child's pipe with the same reader — which is what lets a `CHUNK` and a
    `RESULT` share it.
    """
    body = json.dumps(message, default=_json_default, separators=(",", ":")).encode()
    os.write(fd, HEADER.pack(len(body)) + body)


class _StreamingEvent(dict):
    """The event, with a `progress()` the handler can call (proto 1.3).

    A `dict` subclass rather than a wrapper: handlers do `event["x"]` and
    `isinstance(event, dict)`, and a wrapper would break both to add one
    method. A non-dict event — a list, a string, `None` — is passed through
    unchanged, because there is nothing to attach to.

    `send` is `None` when nobody is streaming, and `progress()` is then a
    no-op. That is deliberate: a handler reports progress, and whether anyone
    is listening is not its problem.
    """

    __slots__ = ("_send",)

    def __new__(cls, event, send):
        if not isinstance(event, dict):
            return event
        return super().__new__(cls, event)

    def __init__(self, event, send) -> None:
        super().__init__(event)
        self._send = send

    def progress(self, message) -> None:
        """Report progress. Delivered before the result, if anyone is reading."""
        if self._send is None:
            return
        self._send(
            message
            if isinstance(message, str)
            else json.dumps(message, default=_json_default)
        )


def _decode_result_frame(raw: bytes, request_id: str) -> dict | None:
    """Parse the child's single frame, or `None` if it never sent one.

    Usually a `RESULT`. A child that refused the request answers `ERROR`
    instead, and the `type` is kept so the caller can tell which — it is the
    difference between a `DONE` and an `ERROR` going up to the supervisor.
    """
    if len(raw) < HEADER.size:
        return None
    (size,) = HEADER.unpack(raw[: HEADER.size])
    body = raw[HEADER.size : HEADER.size + size]
    if len(body) < size:
        return None
    try:
        message = json.loads(body)
    except json.JSONDecodeError:
        return None
    message["id"] = request_id
    return message


def _exit_code(status: int) -> int:
    if os.WIFSIGNALED(status):
        return 128 + os.WTERMSIG(status)
    return os.WEXITSTATUS(status)


def _death_reason(status: int) -> str:
    if os.WIFSIGNALED(status):
        signum = os.WTERMSIG(status)
        name = signal.Signals(signum).name if signum in set(signal.Signals) else str(signum)
        if signum == signal.SIGKILL:
            return "killed by SIGKILL (out of memory, or the deadline expired)"
        return f"killed by {name}"
    return f"exited with {os.WEXITSTATUS(status)} without returning a result"


def oneshot(handler_path: str, mode: str) -> int:
    """`--oneshot`: read one request from stdin, write one result to stdout.

    The worker half of the spawn fallback. The blocking read on stdin is also
    the `GO` barrier: the parent writes the request only after the supervisor
    has confirmed the cgroup.
    """
    raw = sys.stdin.buffer.read()
    if len(raw) < HEADER.size:
        return 2
    (size,) = HEADER.unpack(raw[: HEADER.size])
    request = json.loads(raw[HEADER.size : HEADER.size + size])

    handler = load_handler(handler_path, mode)
    # The spawned interpreter is the "child" here, and inherits the variable.
    child_filter = ChildFilter.from_environment()

    # `run_request` waits for a byte on `go_fd`; with the spawn path the
    # barrier has already been crossed, so hand it one that is ready.
    go_r, go_w = os.pipe()
    os.write(go_w, b"\0")
    os.close(go_w)
    run_request(handler, request, sys.stdout.fileno(), go_r, child_filter)
    return 0  # unreachable: run_request calls os._exit


def main(argv: list[str]) -> int:
    # A runtime pool announces itself the same way; what differs is that it
    # was started with no handler, which `READY` does not need to say because
    # the supervisor is the one that started it.
    if len(argv) >= 2 and argv[1] == "--oneshot":
        if len(argv) < 3:
            sys.stderr.write("usage: zygo_agent.py --oneshot <handler.py> [mode]\n")
            return 2
        return oneshot(argv[2], argv[3] if len(argv) > 3 else "function")

    # Two ways to reach the supervisor:
    #
    #   --fd N          an already-connected socket, inherited across the clone
    #   <socket path>   connect to a unix socket
    #
    # The launcher uses `--fd`. It needs no socket file inside the sandbox — so
    # no bind mount, no path that has to exist in the image, and nothing on the
    # filesystem for a second sandbox to find.
    if len(argv) >= 3 and argv[1] == "--fd":
        sock = socket.socket(fileno=int(argv[2]))
        # No handler is the *runtime pool* shape: this zygote is an
        # interpreter and its dependency set, holding no tenant code, and
        # every request carries its own script. See `load_request_script`.
        handler_path = argv[3] if len(argv) > 3 else None
        mode = argv[4] if len(argv) > 4 else "function"
    elif len(argv) >= 2:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(argv[1])
        # As above: no handler is a runtime pool, and every request brings
        # its own script.
        handler_path = argv[2] if len(argv) > 2 else None
        mode = argv[3] if len(argv) > 3 else "function"
    else:
        sys.stderr.write(
            "usage: zygo_agent.py (--fd <n> | <socket>) [handler.py] [mode]\n"
            "  with no handler, every request must carry its own `script`\n"
        )
        return 2

    wire = Framing(sock)

    started = time.monotonic()
    try:
        handler = load_handler(handler_path, mode) if handler_path else None
        # Decoded here, once, so a bad value is a start-up failure the
        # supervisor sees, not a per-request one — and so no child pays for
        # the `ctypes` import.
        child_filter = ChildFilter.from_environment()
    except BaseException:  # noqa: BLE001
        wire.send(
            {
                "type": "ERROR",
                "code": "handler_load",
                "message": traceback.format_exc(),
            }
        )
        return 1
    # An async handler needs an event loop in every child. Importing asyncio
    # here means the ~50 ms is paid once, at warm-up, not per request.
    if handler is not None and is_async_handler(handler):
        import asyncio  # noqa: F401
    elif handler is None:
        # A runtime pool cannot know whether the scripts it will be sent are
        # async, and an `import asyncio` in a child costs ~50 ms — five times
        # the whole warm budget. So it is paid here, once, by every pool.
        import asyncio  # noqa: F401
    imports_ms = (time.monotonic() - started) * 1000.0

    # Move everything imported so far into the permanent generation. Without
    # this, the first GC pass in a child touches the refcount of every shared
    # object and copies the pages it lives on — the whole point of forking is
    # that those pages stay shared.
    gc.freeze()

    can_fork = not has_extra_threads()
    generators = shared_generators(handler) if handler is not None else []
    if generators:
        sys.stderr.write(
            "zygo: %s %s a random generator created at import time; every "
            "request starts from the same copy of its state and draws the same "
            "numbers. Create it inside the handler, or seed it from "
            "os.urandom there.\n"
            % (", ".join(generators), "is" if len(generators) == 1 else "are")
        )
    if not can_fork:
        sys.stderr.write(
            "zygo: the handler started threads at import time; falling back to "
            "spawn per request — a fresh interpreter, and every import again, "
            "instead of a 1-2 ms fork\n"
        )

    wire.send(
        {
            "type": "READY",
            "proto": PROTOCOL_VERSION,
            "pid": os.getpid(),
            "imports_ms": imports_ms,
            "rss_kb": _peak_rss_kb(resource.getrusage(resource.RUSAGE_SELF)),
            "runtime": f"python/{sys.version_info.major}.{sys.version_info.minor}"
            f".{sys.version_info.micro}",
        }
    )

    Agent(
        sock,
        handler,
        can_fork=can_fork,
        handler_path=handler_path,
        mode=mode,
        child_filter=child_filter,
    ).serve()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
