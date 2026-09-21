#!/usr/bin/env python3
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
import importlib.util
import json
import os
import random
import resource
import select
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


# --------------------------------------------------------------------------
# Framing
# --------------------------------------------------------------------------


class InFlight:
    """One forked request the agent is still carrying.

    `go_fd` becomes `None` once `GO` has released the child, which is also how
    the loop knows whether a request that is finishing was ever released.
    """

    __slots__ = ("request_id", "pid", "result_fd", "go_fd", "chunks")

    def __init__(self, request_id: str, pid: int, result_fd: int, go_fd: int) -> None:
        self.request_id = request_id
        self.pid = pid
        self.result_fd = result_fd
        self.go_fd: int | None = go_fd
        self.chunks: list[bytes] = []


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
            f"{path} defines no `handler`; expected `def handler(event: dict)`"
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


def has_extra_threads() -> bool:
    """Whether the handler started threads at import time.

    ``fork()`` in a threaded process only carries the calling thread across, so
    a lock another thread held stays locked forever in the child. That is risk
    R1; the caller falls back to spawning instead of forking.
    """
    import threading

    return threading.active_count() > 1


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


def run_request(
    handler,
    request: dict,
    result_fd: int,
    go_fd: int,
    child_filter: "ChildFilter | None" = None,
) -> None:
    """Run one request in the freshly forked child. Never returns."""
    exit_code = 0
    error = None
    value = None
    wall_ms = 0.0
    out, err = _Ring(), _Ring()
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

        os.environ["ZYGO_REQUEST_ID"] = request.get("id", "")
        os.environ["ZYGO_DEADLINE_MS"] = str(request.get("timeout_ms", 0))
        for key, val in (request.get("env_overrides") or {}).items():
            os.environ[key] = val

        started = time.monotonic()
        with _captured(out, err):
            value = handler(request.get("event"))
            if _is_awaitable(value):
                import asyncio

                value = asyncio.new_event_loop().run_until_complete(value)
        wall_ms = (time.monotonic() - started) * 1000.0

        try:
            json.dumps(value, default=_json_default)
        except (TypeError, ValueError) as exc:
            raise TypeError(f"handler returned a value that is not JSON: {exc}") from exc

    except BaseException as exc:  # noqa: BLE001 - the child reports everything upwards
        error = _handler_traceback(exc)
        exit_code = 1
        value = None
        if wall_ms == 0.0:
            wall_ms = (time.monotonic() - started) * 1000.0

    usage = resource.getrusage(resource.RUSAGE_SELF)
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

    body = json.dumps(message, default=_json_default, separators=(",", ":")).encode()
    os.write(result_fd, HEADER.pack(len(body)) + body)
    os.close(result_fd)

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
    """Capture that keeps the first `limit` bytes and counts the rest."""

    def __init__(self, limit: int = RING_BUFFER_BYTES) -> None:
        self._parts: list[str] = []
        self._size = 0
        self._dropped = 0
        self._limit = limit

    def write(self, text: str) -> int:
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
        self._unreaped: list[int] = []
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

        while accepting or self._inflight:
            watch = list(self._by_result_fd)
            if accepting:
                watch.append(wire_fd)
            if not watch:
                break

            try:
                ready, _, _ = select.select(watch, [], [])
            except InterruptedError:
                continue
            except OSError:
                return

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
        try:
            os.write(request.go_fd, b"\0")
        except OSError:
            pass
        os.close(request.go_fd)
        request.go_fd = None

    def _collect(self, request: InFlight) -> None:
        """A result pipe is readable: take what is there, answer at end of file."""
        try:
            chunk = os.read(request.result_fd, 64 * 1024)
        except OSError:
            chunk = b""
        if chunk:
            request.chunks.append(chunk)
            return

        self._finish(request)

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
            _, status = os.waitpid(request.pid, 0)
            result = {
                "id": request.request_id,
                "exit_code": _exit_code(status),
                "result": None,
                "stdout": "",
                "stderr": "",
                "error": _death_reason(status),
                "peak_rss_kb": 0,
                "wall_ms": 0.0,
                "cpu_ms": 0.0,
            }
        else:
            # The child has already reported and called `_exit`; what remains
            # is the kernel tearing its address space down. Answering now and
            # reaping later keeps that off the request's clock.
            self._unreaped.append(request.pid)

        result["type"] = "DONE"
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

        request_state = InFlight(request_id, pid, result_r, go_w)
        self._inflight[request_id] = request_state
        self._by_result_fd[result_r] = request_state

        # The supervisor needs the pid to build the request cgroup; the child
        # is blocked on its `go` pipe until `GO` comes back.
        self._wire.send({"type": "FORKED", "id": request_id, "pid": pid})

    def _exec_spawned(self, request: dict) -> None:
        """Fallback for handlers that cannot be forked (risk R1).

        A fresh interpreter per request: 20-40 ms instead of 1-2 ms, but it
        cannot deadlock on a lock some import-time thread was holding. The
        handshake is unchanged, so the supervisor still gets its cgroup window
        — the child blocks reading stdin until `GO` has been acknowledged.
        """
        import subprocess

        request_id = request.get("id", "")
        try:
            proc = subprocess.Popen(
                [sys.executable, os.path.abspath(__file__), "--oneshot",
                 self._handler_path, self._mode],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
            )
        except OSError as exc:
            self._wire.send(
                {
                    "type": "ERROR",
                    "id": request_id,
                    "code": "spawn_failed",
                    "message": str(exc),
                }
            )
            return

        self._wire.send({"type": "FORKED", "id": request_id, "pid": proc.pid})
        if not self._await_go(request_id, proc):
            return

        body = json.dumps(request, separators=(",", ":")).encode()
        try:
            stdout, _ = proc.communicate(HEADER.pack(len(body)) + body)
        except BrokenPipeError:
            stdout = b""
        # `communicate` already waited; a spawned worker is a fresh interpreter
        # whose teardown cannot be deferred the way a fork's can.
        proc.wait()

        result = _decode_result_frame(stdout, request_id)
        if result is None:
            result = {
                "id": request_id,
                "exit_code": proc.returncode or 1,
                "result": None,
                "stdout": "",
                "stderr": "",
                "error": f"spawned worker exited with {proc.returncode} "
                         "without returning a result",
                "peak_rss_kb": 0,
                "wall_ms": 0.0,
                "cpu_ms": 0.0,
            }
        result["type"] = "DONE"
        self._wire.send(result)

    def _await_go(self, request_id: str, proc) -> bool:
        """Wait for *this* request's `GO`, answering anything else on the way.

        The spawn fallback used to take the next frame and require it to be
        `GO`: a second `EXEC`, a `PING`, or a `GO` for another request killed
        the worker and answered **nothing** — the supervisor then waited out
        the whole deadline for a reply that was never coming. `zygo agent test`
        sends two `EXEC`s before any `GO`, so every handler that made this
        agent fall back to spawning failed conformance for this reason
        (B-09, 2026-09-21 review).

        Returns `True` when this request may run.
        """
        while True:
            try:
                reply = self._wire.recv()
            except BadFrame as e:
                # Reportable, not fatal: the stream is still aligned.
                self._wire.send(
                    {"type": "ERROR", "id": None, "code": "bad_message", "message": str(e)}
                )
                continue
            except (ConnectionError, OSError):
                reply = None

            if reply is None:
                self._kill(proc)
                return False

            kind = reply.get("type")
            if kind == "GO" and reply.get("id") == request_id:
                return True
            if kind == "PING":
                self._wire.send({"type": "PONG", "seq": reply.get("seq", 0)})
                continue
            if kind == "EXEC":
                # One at a time on this path: the fallback exists because
                # forking is unavailable, and a second interpreter would not
                # share the warmed one's memory anyway.
                self._wire.send(
                    {
                        "type": "ERROR",
                        "id": reply.get("id"),
                        "code": "overloaded",
                        "message": "this agent runs one spawned request at a time",
                    }
                )
                continue
            if kind == "SHUTDOWN":
                self._kill(proc)
                return False
            if kind == "GO":
                # A `GO` for a request this agent is not running. Reported
                # rather than ignored: it means the two sides disagree about
                # what is in flight.
                self._wire.send(
                    {
                        "type": "ERROR",
                        "id": reply.get("id"),
                        "code": "bad_message",
                        "message": "no such request is waiting to be released",
                    }
                )
                continue
            self._wire.send(
                {
                    "type": "ERROR",
                    "id": reply.get("id"),
                    "code": "bad_message",
                    "message": f"unexpected message `{kind}` while waiting for GO",
                }
            )

    @staticmethod
    def _kill(proc) -> None:
        proc.kill()
        proc.wait()

    def _reap_finished(self) -> None:
        """Collect children that have finished exiting, without waiting.

        Called after the answer has gone out, so a slow teardown costs nothing.
        At most one child is ever outstanding, because requests are serialised.
        """
        still_running = []
        for pid in self._unreaped:
            try:
                reaped, _ = os.waitpid(pid, os.WNOHANG)
            except ChildProcessError:
                continue  # already gone
            if reaped == 0:
                still_running.append(pid)
        self._unreaped = still_running

    def _reap_all(self) -> None:
        """Block until every child is gone. Used on shutdown."""
        for pid in self._unreaped:
            try:
                os.waitpid(pid, 0)
            except ChildProcessError:
                pass
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


def _decode_result_frame(raw: bytes, request_id: str) -> dict | None:
    """Parse the child's single `RESULT` frame, or `None` if it never sent one."""
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
    message.pop("type", None)
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
        if len(argv) < 4:
            sys.stderr.write("usage: zygo_agent.py --fd <n> <handler.py> [mode]\n")
            return 2
        sock = socket.socket(fileno=int(argv[2]))
        handler_path = argv[3]
        mode = argv[4] if len(argv) > 4 else "function"
    elif len(argv) >= 3:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(argv[1])
        handler_path = argv[2]
        mode = argv[3] if len(argv) > 3 else "function"
    else:
        sys.stderr.write(
            "usage: zygo_agent.py (--fd <n> | <socket>) <handler.py> [mode]\n"
        )
        return 2

    wire = Framing(sock)

    started = time.monotonic()
    try:
        handler = load_handler(handler_path, mode)
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
    if is_async_handler(handler):
        import asyncio  # noqa: F401
    imports_ms = (time.monotonic() - started) * 1000.0

    # Move everything imported so far into the permanent generation. Without
    # this, the first GC pass in a child touches the refcount of every shared
    # object and copies the pages it lives on — the whole point of forking is
    # that those pages stay shared.
    gc.freeze()

    can_fork = not has_extra_threads()
    if not can_fork:
        sys.stderr.write(
            "zygo: the handler started threads at import time; falling back to "
            "spawn per request (20-40 ms instead of 1-2 ms)\n"
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
