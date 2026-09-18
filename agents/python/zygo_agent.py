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
import gc
import importlib.util
import json
import os
import random
import resource
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


class Framing:
    """Length-prefixed JSON over a byte stream."""

    def __init__(self, sock: socket.socket) -> None:
        self._sock = sock

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
        return json.loads(body)

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


def run_request(handler, request: dict, result_fd: int, go_fd: int) -> None:
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
        os.read(go_fd, 1)
        os.close(go_fd)

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

    except BaseException:  # noqa: BLE001 - the child reports everything upwards
        error = traceback.format_exc()
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
    backpressure (todo.md, phase 2.2); an agent that also multiplexed would
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
    ) -> None:
        self._wire = Framing(sock)
        self._handler = handler
        self._can_fork = can_fork
        self._handler_path = handler_path
        self._mode = mode
        # Children whose result has already been forwarded, still to be reaped.
        # Reaping is process teardown, and teardown of a forked interpreter is
        # not fast: measured at p99 32 ms on the request path, against a 2 ms
        # budget. Once the RESULT is in hand the answer owes nothing to the
        # corpse, so it is collected between requests instead.
        self._unreaped: list[int] = []

    def serve(self) -> None:
        while True:
            try:
                message = self._wire.recv()
            except (ConnectionError, OSError):
                return
            if message is None:
                return

            kind = message.get("type")
            if kind == "EXEC":
                self._exec(message)
            elif kind == "PING":
                self._wire.send({"type": "PONG", "seq": message.get("seq", 0)})
            elif kind == "SHUTDOWN":
                self._reap_all()
                return
            else:
                self._wire.send(
                    {
                        "type": "ERROR",
                        "id": message.get("id"),
                        "code": "bad_message",
                        "message": f"unexpected message `{kind}`",
                    }
                )

    def _exec(self, request: dict) -> None:
        if not self._can_fork:
            self._exec_spawned(request)
            return

        request_id = request.get("id", "")
        result_r, result_w = os.pipe()
        go_r, go_w = os.pipe()

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
            run_request(self._handler, request, result_w, go_r)
            return  # unreachable: run_request calls os._exit

        os.close(result_w)
        os.close(go_r)

        # Tell the supervisor the pid, wait for it to say the cgroup is ready,
        # then release the child.
        self._wire.send({"type": "FORKED", "id": request_id, "pid": pid})
        reply = self._wire.recv()
        if reply is None or reply.get("type") != "GO":
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
            os.close(go_w)
            os.close(result_r)
            return
        os.write(go_w, b"\0")
        os.close(go_w)

        result = self._read_result(result_r, request_id)
        os.close(result_r)

        if result is None:
            # No result means the child died on the way — an OOM kill, a
            # deadline kill, or a segfault in a C extension. Here the exit
            # status *is* the answer, so it is worth waiting for.
            _, status = os.waitpid(pid, 0)
            result = {
                "id": request_id,
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
            self._unreaped.append(pid)

        result["type"] = "DONE"
        self._wire.send(result)
        self._reap_finished()

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
        reply = self._wire.recv()
        if reply is None or reply.get("type") != "GO":
            proc.kill()
            proc.wait()
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

    # `run_request` waits for a byte on `go_fd`; with the spawn path the
    # barrier has already been crossed, so hand it one that is ready.
    go_r, go_w = os.pipe()
    os.write(go_w, b"\0")
    os.close(go_w)
    run_request(handler, request, sys.stdout.fileno(), go_r)
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
    ).serve()
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
