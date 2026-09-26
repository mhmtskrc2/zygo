# SPDX-License-Identifier: Apache-2.0
"""A stand-in for ``zygo api``, so the client can be tested without a kernel.

It answers the routes the client calls, records what it received, and can be
told to answer with a particular status. What it is *not* is a model of Zygo:
nothing here runs a sandbox, and a test that needs one belongs in the Rust
suites, against a real kernel.

Both transports are covered, because they are the ones the client implements
differently — a unix socket and a TCP port — and a bug in either is invisible
from the other.
"""

from __future__ import annotations

import json
import os
import socketserver
import tempfile
import threading
from http.server import BaseHTTPRequestHandler
from typing import Any, Dict, List, Optional, Tuple


class _Ndjson:
    """An answer that is a stream of lines rather than one body."""

    def __init__(self, lines: List[Any], gap: float) -> None:
        self.lines = lines
        self.gap = gap


class Recorder:
    """What the server was asked, and what it should answer."""

    def __init__(self) -> None:
        self.requests: List[Dict[str, Any]] = []
        self.connections = 0
        self.answers: Dict[Tuple[str, str], Any] = {}
        self.delay = 0.0
        # Close each connection after answering, without saying so — what a
        # server that drops idle keep-alive connections looks like to a client.
        self.hang_up = False
        self.lock = threading.Lock()

    def answer(
        self,
        method: str,
        path: str,
        status: int,
        body: Any,
        retry_after: Optional[float] = None,
    ) -> None:
        """Answer this route the same way every time.

        ``retry_after`` sets the header; left out, it is what the real API
        sends — one second on a 429, five while dependencies build.
        """
        self.answers[(method, path)] = (status, body, retry_after)

    def answer_then(
        self,
        method: str,
        path: str,
        answers: List[Tuple[int, Any]],
        retry_after: Optional[float] = None,
    ) -> None:
        """Answer this route with each of ``answers`` in turn, then keep
        giving the last one — a host that refuses twice and then accepts."""
        self.answers[(method, path)] = [(status, body, retry_after) for status, body in answers]

    def next_answer(self, key: Tuple[str, str]) -> Tuple[int, Any, Optional[float]]:
        with self.lock:
            planned = self.answers.get(key)
            if planned is None:
                return 404, {"error": f"no route {key[1]}"}, None
            if isinstance(planned, list):
                return planned.pop(0) if len(planned) > 1 else planned[0]
            return planned

    def stream(self, method: str, path: str, lines: List[Any], gap: float = 0.0) -> None:
        """Answer this route with NDJSON, one object per line.

        `gap` is the pause between lines: with one, a test can tell a client
        that yields as lines arrive from one that waits for the last.
        """
        self.answers[(method, path)] = (200, _Ndjson(lines, gap), None)


def _handler(recorder: Recorder):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        # `client_address` is a string for a unix socket, and the base class
        # assumes a (host, port) pair when it logs.
        def address_string(self) -> str:  # noqa: D102
            return "test"

        def log_message(self, *_: Any) -> None:  # noqa: D102
            pass

        def _serve(self, method: str) -> None:
            length = int(self.headers.get("content-length", 0) or 0)
            raw = self.rfile.read(length) if length else b""
            # Not every body is JSON: `PUT /scripts` sends the script as
            # itself, so this parses by content type rather than by hope.
            is_json = (self.headers.get("content-type") or "").startswith("application/json")
            with recorder.lock:
                recorder.requests.append(
                    {
                        "method": method,
                        "path": self.path,
                        "headers": {k.lower(): v for k, v in self.headers.items()},
                        "body": json.loads(raw) if raw and is_json else None,
                        "raw": raw.decode("utf-8", "replace"),
                    }
                )
            if recorder.delay:
                import time

                time.sleep(recorder.delay)

            key = (method, self.path.split("?", 1)[0])
            status, body, retry_after = recorder.next_answer(key)

            # A stream is chunked and written a line at a time, like the real
            # API's: a test that received one whole buffer could not tell a
            # client that yields as lines arrive from one that waits.
            if isinstance(body, _Ndjson):
                import time as _time

                self.send_response(status)
                self.send_header("content-type", "application/x-ndjson")
                self.send_header("transfer-encoding", "chunked")
                self.end_headers()
                for i, item in enumerate(body.lines):
                    if i and body.gap:
                        _time.sleep(body.gap)
                    piece = (json.dumps(item) + "\n").encode()
                    self.wfile.write(b"%x\r\n" % len(piece) + piece + b"\r\n")
                    self.wfile.flush()
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
                return

            payload = json.dumps(body).encode()
            self.send_response(status)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(payload)))
            # What the real API sends: a second on backpressure, five while a
            # dependency set builds, and nothing on any other refusal.
            if retry_after is None and status == 429:
                retry_after = 3
            if retry_after is None and isinstance(body, dict) and body.get("code") == "deps_building":
                retry_after = 5
            if retry_after is not None:
                self.send_header("retry-after", f"{retry_after:g}")
            self.end_headers()
            self.wfile.write(payload)
            if recorder.hang_up:
                self.close_connection = True

        def do_GET(self) -> None:  # noqa: N802, D102
            self._serve("GET")

        def do_POST(self) -> None:  # noqa: N802, D102
            self._serve("POST")

        def do_PUT(self) -> None:  # noqa: N802, D102
            self._serve("PUT")

        def do_DELETE(self) -> None:  # noqa: N802, D102
            self._serve("DELETE")

        def do_PATCH(self) -> None:  # noqa: N802, D102
            self._serve("PATCH")

    return Handler


class _CountingMixin:
    def get_request(self):  # type: ignore[override]
        result = super().get_request()  # type: ignore[misc]
        with self.recorder.lock:  # type: ignore[attr-defined]
            self.recorder.connections += 1  # type: ignore[attr-defined]
        return result


# `socketserver` listens with a backlog of five by default, and the
# concurrency tests open eight connections at once — so the ninth was refused
# and the test failed as though the client had a bug. A real `zygo api` is
# hyper on tokio, whose backlog is three orders of magnitude larger.
BACKLOG = 128


class _TcpServer(_CountingMixin, socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
    request_queue_size = BACKLOG


class _UnixServer(_CountingMixin, socketserver.ThreadingUnixStreamServer):
    daemon_threads = True
    request_queue_size = BACKLOG


class FakeApi:
    """A running stand-in. Use as a context manager; ``url`` is where it is."""

    def __init__(self, unix: bool = False) -> None:
        self.recorder = Recorder()
        self._dir: Optional[tempfile.TemporaryDirectory] = None
        handler = _handler(self.recorder)
        if unix:
            self._dir = tempfile.TemporaryDirectory(prefix="zygo-fake-")
            path = os.path.join(self._dir.name, "api.sock")
            self._server: socketserver.BaseServer = _UnixServer(path, handler)
            self.url = f"unix://{path}"
        else:
            self._server = _TcpServer(("127.0.0.1", 0), handler)
            host, port = self._server.server_address  # type: ignore[misc]
            self.url = f"http://{host}:{port}"
        self._server.recorder = self.recorder  # type: ignore[attr-defined]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()

    def answer(
        self,
        method: str,
        path: str,
        status: int,
        body: Any,
        retry_after: Optional[float] = None,
    ) -> None:
        self.recorder.answer(method, path, status, body, retry_after)

    def answer_then(
        self,
        method: str,
        path: str,
        answers: List[Tuple[int, Any]],
        retry_after: Optional[float] = None,
    ) -> None:
        self.recorder.answer_then(method, path, answers, retry_after)

    def stream(self, method: str, path: str, lines: List[Any], gap: float = 0.0) -> None:
        self.recorder.stream(method, path, lines, gap)

    @property
    def requests(self) -> List[Dict[str, Any]]:
        return self.recorder.requests

    @property
    def connections(self) -> int:
        return self.recorder.connections

    def close(self) -> None:
        self._server.shutdown()
        self._server.server_close()
        if self._dir is not None:
            self._dir.cleanup()

    def __enter__(self) -> "FakeApi":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()
