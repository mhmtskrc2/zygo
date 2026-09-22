"""The asynchronous client: ``from zygo.aio import connect``.

Agent frameworks are asynchronous, and a tool that blocks the event loop for
the length of a sandbox request is not usable inside one. This is the same API
as :mod:`zygo`, with every call a coroutine.

The HTTP/1.1 here is written out rather than taken from a library, for the same
reason the synchronous client uses :mod:`http.client`: no dependencies. That is
affordable because both ends are known — the server is hyper, answering with a
content length — and the parser still handles a chunked body rather than
assuming it will never see one.
"""

from __future__ import annotations

import asyncio
import base64
import json
import os
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple, Union

from ._endpoint import Endpoint, resolve
from ._errors import TransportError, ZygoError, from_response
from ._models import Deps, Event, Function, LogPage, Result, Run, Runtime, Script, Served
from ._sync import (
    MAX_BODY,
    _batch_element,
    _body,
    _decode,
    _escape,
    _escape_digest,
    _request_key,
    _retry_after,
    _timeout_header,
)

_Connection = Tuple[asyncio.StreamReader, asyncio.StreamWriter]


class AsyncClient:
    """A connection to a Zygo API, for an event loop.

    Safe to share between tasks: connections are pooled and one is held for the
    duration of each request, so concurrent callers become concurrent sandbox
    requests rather than a queue behind one socket.
    """

    def __init__(
        self,
        url: Optional[str] = None,
        *,
        token: Optional[str] = None,
        timeout: float = 300.0,
    ) -> None:
        self.endpoint: Endpoint = resolve(url)
        self.token = token if token is not None else os.environ.get("ZYGO_API_TOKEN")
        self.timeout = timeout
        self._idle: List[_Connection] = []
        self._lock = asyncio.Lock()
        self._closed = False

    # ---- lifecycle ----------------------------------------------------

    async def close(self) -> None:
        async with self._lock:
            self._closed = True
            idle, self._idle = self._idle, []
        for _, writer in idle:
            await _shut(writer)

    async def __aenter__(self) -> "AsyncClient":
        return self

    async def __aexit__(self, *_: object) -> None:
        await self.close()

    # ---- the API ------------------------------------------------------

    async def version(self) -> Dict[str, Any]:
        return await self._request("GET", "/version")

    async def health(self) -> Dict[str, Any]:
        return await self._request("GET", "/healthz", authenticated=False)

    async def functions(self) -> List[Function]:
        body = await self._request("GET", "/fn")
        return [Function.parse(f) for f in body.get("functions", [])]

    async def stats(self, name: str) -> Function:
        return Function.parse(await self._request("GET", f"/fn/{_escape(name)}/stats"))

    async def warm(self, name: str) -> Dict[str, Any]:
        return await self._request("POST", f"/fn/{_escape(name)}/warm")

    async def call(
        self,
        name: str,
        event: Any = None,
        *,
        timeout: Optional[float] = None,
        key: Optional[str] = None,
    ) -> Result:
        """Call a warm function.

        **Cancelling the task cancels the request.** ``asyncio.CancelledError`` —
        from ``task.cancel()``, from a ``timeout`` block, from the loop
        shutting down — sends ``DELETE /requests/<key>`` before it propagates,
        so the sandbox stops rather than running on unread. That is the whole
        reason the request carries a key: the server's own id only arrives
        *with the answer*, which is too late to stop the call it belongs to.

        Pass ``key`` to choose the name yourself; otherwise one is generated.
        """
        key = key or _request_key()
        headers = _timeout_header(timeout)
        headers["x-zygo-request-key"] = key
        try:
            body = await self._request(
                "POST", f"/fn/{_escape(name)}", body=event, headers=headers
            )
        except asyncio.CancelledError:
            await self._cancel_quietly(key)
            raise
        return Result.parse(body)

    async def cancel(self, request_id: str) -> Dict[str, Any]:
        """Stop a request that is running. See :meth:`zygo.Client.cancel`."""
        return await self._request("DELETE", f"/requests/{_escape(request_id)}")

    async def stream(
        self,
        name: str,
        event: Any = None,
        *,
        timeout: Optional[float] = None,
        key: Optional[str] = None,
    ):
        """Call a function and yield its output as it is produced.

        An async iterator of :class:`~zygo._models.Event`; see
        :meth:`zygo.Client.stream` for what the items are.

        Cancelling the task cancels the request, like :meth:`call` — the key
        is generated for you when you do not pass one.
        """
        key = key or _request_key()
        headers = _timeout_header(timeout)
        headers["x-zygo-request-key"] = key
        path = f"/fn/{_escape(name)}?stream=1"
        async for item in self._stream(path, event, headers, key):
            yield item

    async def stream_script(
        self,
        runtime: str,
        script: str,
        event: Any = None,
        *,
        entry_point: Optional[str] = None,
        timeout: Optional[float] = None,
        key: Optional[str] = None,
    ):
        """Run a script in a pool, yielding its output. See :meth:`stream`."""
        payload: Dict[str, Any] = {
            "script": script if script.startswith("sha256:") else {"source": script},
            "event": event,
        }
        if entry_point is not None:
            payload["entry_point"] = entry_point
        key = key or _request_key()
        headers = _timeout_header(timeout)
        headers["x-zygo-request-key"] = key
        path = f"/runtimes/{_escape(runtime)}/call?stream=1"
        async for item in self._stream(path, payload, headers, key):
            yield item

    async def _stream(self, path: str, body: Any, headers: Dict[str, str], key: str):
        """One request whose answer arrives a line at a time.

        Outside the connection pool, for the reason the synchronous client
        gives: this connection is in use for as long as the handler runs, and
        a pooled one is a connection that is finished with.
        """
        if self._closed:
            raise ZygoError("this client has been closed")

        payload = json.dumps(body).encode()
        sent = {
            "host": "localhost"
            if self.endpoint.is_unix
            else f"{self.endpoint.host}:{self.endpoint.port}",
            "accept": "application/x-ndjson",
            "content-type": "application/json",
            "content-length": str(len(payload)),
        }
        if self.token:
            sent["authorization"] = f"Bearer {self.token}"
        sent.update(headers)
        head = f"POST {path} HTTP/1.1\r\n"
        head += "".join(f"{k}: {v}\r\n" for k, v in sent.items())
        head += "\r\n"

        reader, writer = await self._open()
        try:
            writer.write(head.encode("latin-1") + payload)
            await writer.drain()
            status, response_headers = await _read_head(reader)
            if status >= 400:
                raise from_response(status, _body(await _read_body(reader, response_headers)))
            async for line in _ndjson(reader, response_headers):
                event = Event.parse(_body(line))
                yield event
                if event.is_result:
                    event.raise_for_status()
                    return
        except asyncio.CancelledError:
            await self._cancel_quietly(key)
            raise
        finally:
            await _shut(writer)

    async def _cancel_quietly(self, key: str) -> None:
        """Send a cancel on the way out of a cancelled task.

        Shielded, because the task is *already* being cancelled: an ordinary
        await here would be cancelled too and the sandbox would keep running,
        which is the failure this exists to prevent.

        Every failure is swallowed. The caller is unwinding with
        ``CancelledError`` and that is the exception they must see; a request
        that had already finished, or a connection that has gone, are both
        "there is nothing left to stop" rather than something to report over
        the top of it.
        """
        try:
            await asyncio.shield(self.cancel(key))
        except BaseException:  # noqa: BLE001 - see the docstring
            pass

    async def batch(
        self,
        name: str,
        events: Sequence[Any],
        *,
        timeout: Optional[float] = None,
    ) -> List[Any]:
        answers = await self._request(
            "POST",
            f"/fn/{_escape(name)}/batch",
            body=list(events),
            headers=_timeout_header(timeout),
        )
        return [_batch_element(a) for a in answers]

    async def logs(
        self,
        name: str,
        *,
        after: int = 0,
        limit: int = 50,
        failed: bool = False,
    ) -> LogPage:
        query = f"?after={int(after)}&limit={int(limit)}&failed={'true' if failed else 'false'}"
        return LogPage.parse(await self._request("GET", f"/fn/{_escape(name)}/logs{query}"))

    async def serve(
        self,
        name: str,
        layer: Mapping[str, Any],
        *,
        base_dir: Optional[str] = None,
        secrets: Optional[Mapping[str, str]] = None,
        if_changed: bool = False,
    ) -> Served:
        payload = {
            "layer": dict(layer),
            "base_dir": os.path.abspath(base_dir or os.getcwd()),
            "secrets": dict(secrets or {}),
            "if_changed": bool(if_changed),
        }
        return Served.parse(await self._request("PUT", f"/fn/{_escape(name)}", body=payload))

    async def stop(self, name: str) -> List[str]:
        body = await self._request("DELETE", f"/fn/{_escape(name)}")
        return list(body.get("stopped", []))

    async def run(
        self,
        image: str,
        cmd: Optional[Sequence[str]] = None,
        *,
        stdin: str = "",
        **layer: Any,
    ) -> Run:
        described: Dict[str, Any] = dict(layer)
        described["image"] = image
        if cmd is not None:
            described["cmd"] = list(cmd)
        body = await self._request("POST", "/run", body={"layer": described, "stdin": stdin})
        return Run.parse(body)

    async def serve_runtime(
        self,
        name: str,
        layer: Mapping[str, Any],
        *,
        base_dir: Optional[str] = None,
        deps: Optional[str] = None,
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {"name": name, "layer": dict(layer)}
        if base_dir is not None:
            payload["base_dir"] = os.path.abspath(base_dir)
        if deps is not None:
            payload["deps"] = deps
        return await self._request("POST", "/runtimes", body=payload)

    async def runtimes(self) -> List[Runtime]:
        body = await self._request("GET", "/runtimes")
        return [Runtime.parse(r) for r in body.get("runtimes", [])]

    async def stop_runtime(self, name: str) -> List[str]:
        body = await self._request("DELETE", f"/runtimes/{_escape(name)}")
        return list(body.get("stopped", []))

    async def run_script(
        self,
        runtime: str,
        script: str,
        event: Any = None,
        *,
        entry_point: Optional[str] = None,
        timeout: Optional[float] = None,
        key: Optional[str] = None,
    ) -> Result:
        """Run one script in a pool. Cancelling the task cancels the request;
        see :meth:`call`."""
        payload: Dict[str, Any] = {
            "script": script if script.startswith("sha256:") else {"source": script},
            "event": event,
        }
        if entry_point is not None:
            payload["entry_point"] = entry_point
        key = key or _request_key()
        headers = _timeout_header(timeout)
        headers["x-zygo-request-key"] = key
        try:
            body = await self._request(
                "POST",
                f"/runtimes/{_escape(runtime)}/call",
                body=payload,
                headers=headers,
            )
        except asyncio.CancelledError:
            await self._cancel_quietly(key)
            raise
        return Result.parse(body)

    async def put_script(self, source: str) -> Script:
        return Script.parse(await self._request("PUT", "/scripts", raw_body=source.encode()))

    async def put_deps(
        self,
        image: str,
        files: Mapping[str, Union[str, bytes]],
    ) -> Deps:
        encoded = {
            name: base64.b64encode(
                content.encode() if isinstance(content, str) else content
            ).decode()
            for name, content in files.items()
        }
        return Deps.parse(
            await self._request("POST", "/deps", body={"image": image, "files": encoded})
        )

    async def deps(self, id: Optional[str] = None) -> Union[Deps, List[Deps]]:
        if id is None:
            body = await self._request("GET", "/deps")
            return [Deps.parse(d) for d in body.get("deps", [])]
        return Deps.parse(await self._request("GET", f"/deps/{_escape(id)}"))

    async def delete_deps(self, id: str) -> bool:
        body = await self._request("DELETE", f"/deps/{_escape(id)}")
        return bool(body.get("deleted", False))

    async def script(self, digest: str) -> Script:
        return Script.parse(await self._request("GET", f"/scripts/{_escape_digest(digest)}"))

    async def delete_script(self, digest: str) -> bool:
        body = await self._request("DELETE", f"/scripts/{_escape_digest(digest)}")
        return bool(body.get("deleted", False))

    def fn(self, name: str) -> "AsyncFunctionHandle":
        return AsyncFunctionHandle(self, name)

    # ---- transport ----------------------------------------------------

    async def _request(
        self,
        method: str,
        path: str,
        *,
        body: Any = None,
        headers: Optional[Dict[str, str]] = None,
        authenticated: bool = True,
        raw_body: Optional[bytes] = None,
    ) -> Any:
        if self._closed:
            raise ZygoError("this client has been closed")

        # `raw_body` is for the one route whose body is not JSON: see the
        # synchronous client, which says why a script travels as itself.
        payload = raw_body if raw_body is not None else (
            b"" if body is None else json.dumps(body).encode()
        )
        sent = {
            "host": "localhost" if self.endpoint.is_unix else f"{self.endpoint.host}:{self.endpoint.port}",
            "accept": "application/json",
            "content-length": str(len(payload)),
        }
        if raw_body is not None:
            sent["content-type"] = "text/plain; charset=utf-8"
        elif body is not None:
            sent["content-type"] = "application/json"
        if authenticated and self.token:
            sent["authorization"] = f"Bearer {self.token}"
        sent.update(headers or {})

        head = f"{method} {path} HTTP/1.1\r\n"
        head += "".join(f"{k}: {v}\r\n" for k, v in sent.items())
        head += "\r\n"

        reader, writer = await self._take()
        try:
            writer.write(head.encode("latin-1") + payload)
            await writer.drain()
            status, response_headers, raw = await asyncio.wait_for(
                _read_response(reader), timeout=self.timeout
            )
        except asyncio.CancelledError:
            # The exchange is half-finished: the request went out and the
            # answer is still coming. The connection can neither be reused —
            # the next request would read this one's response — nor left, which
            # is what used to happen: a cancelled task leaked its socket, and
            # the warning only appeared once cancelling became an ordinary
            # thing to do. Close it and let the cancel propagate.
            await _shut(writer)
            raise
        except (OSError, asyncio.IncompleteReadError, asyncio.TimeoutError, ValueError) as e:
            await _shut(writer)
            raise TransportError(f"{method} {path} failed against {self.endpoint}: {e}") from e

        # Only a connection that completed an exchange cleanly goes back: one
        # that raised may be mid-message, and the next request must not inherit
        # that.
        if response_headers.get("connection", "").lower() == "close":
            await _shut(writer)
        else:
            await self._give_back((reader, writer))

        return _decode(status, raw, _retry_after(response_headers.get("retry-after")))

    async def _take(self) -> _Connection:
        async with self._lock:
            if self._idle:
                return self._idle.pop()
        return await self._open()

    async def _give_back(self, connection: _Connection) -> None:
        async with self._lock:
            if self._closed:
                await _shut(connection[1])
                return
            self._idle.append(connection)

    async def _open(self) -> _Connection:
        endpoint = self.endpoint
        try:
            if endpoint.is_unix:
                assert endpoint.socket_path is not None
                return await asyncio.open_unix_connection(endpoint.socket_path)
            ssl = None
            if endpoint.tls:
                import ssl as ssl_module

                ssl = ssl_module.create_default_context()
            return await asyncio.open_connection(endpoint.host, endpoint.port, ssl=ssl)
        except OSError as e:
            raise TransportError(
                f"no Zygo API at {endpoint}: {e}\n  -> start one with `zygo api`"
            ) from e


class AsyncFunctionHandle:
    """One function, bound to a client. Await it like a coroutine function."""

    __slots__ = ("_client", "name")

    def __init__(self, client: AsyncClient, name: str) -> None:
        self._client = client
        self.name = name

    async def __call__(self, event: Any = None, *, timeout: Optional[float] = None) -> Result:
        return await self._client.call(self.name, event, timeout=timeout)

    async def batch(self, events: Iterable[Any], *, timeout: Optional[float] = None) -> List[Any]:
        return await self._client.batch(self.name, list(events), timeout=timeout)

    async def stats(self) -> Function:
        return await self._client.stats(self.name)

    async def logs(self, *, after: int = 0, limit: int = 50, failed: bool = False) -> LogPage:
        return await self._client.logs(self.name, after=after, limit=limit, failed=failed)

    async def warm(self) -> Dict[str, Any]:
        return await self._client.warm(self.name)

    async def stop(self) -> List[str]:
        return await self._client.stop(self.name)

    def __repr__(self) -> str:
        return f"<zygo async function {self.name!r}>"


def connect(url: Optional[str] = None, *, token: Optional[str] = None, timeout: float = 300.0) -> AsyncClient:
    """Open an asynchronous client.

    Not a coroutine: nothing is connected until the first call, so there is
    nothing to await here and ``async with zygo.aio.connect() as c`` works.
    """
    return AsyncClient(url, token=token, timeout=timeout)


# ---- a small HTTP/1.1 reader -----------------------------------------


async def _read_head(reader: asyncio.StreamReader) -> Tuple[int, Dict[str, str]]:
    """The status line and headers, stopping before the body.

    Split out from reading the whole answer because a stream's body has no end
    to wait for: the point is to look at the status, decide, and then read the
    body a piece at a time.
    """
    status_line = await reader.readline()
    if not status_line:
        raise TransportError("the API closed the connection without answering")
    parts = status_line.decode("latin-1").split(None, 2)
    if len(parts) < 2 or not parts[0].startswith("HTTP/"):
        raise TransportError(f"not an HTTP answer: {status_line!r}")
    status = int(parts[1])

    headers: Dict[str, str] = {}
    while True:
        line = await reader.readline()
        if line in (b"\r\n", b"\n", b""):
            break
        name, _, value = line.decode("latin-1").partition(":")
        headers[name.strip().lower()] = value.strip()
    return status, headers


async def _read_response(reader: asyncio.StreamReader) -> Tuple[int, Dict[str, str], bytes]:
    status, headers = await _read_head(reader)
    return status, headers, await _read_body(reader, headers)


async def _read_body(reader: asyncio.StreamReader, headers: Dict[str, str]) -> bytes:
    if "chunked" in headers.get("transfer-encoding", "").lower():
        return await _read_chunked(reader)
    length = int(headers.get("content-length", "0") or 0)
    if length > MAX_BODY:
        raise TransportError(f"the API answered with {length} bytes, over this client's limit")
    return await reader.readexactly(length) if length else b""


async def _ndjson(reader: asyncio.StreamReader, headers: Dict[str, str]):
    """Yield newline-delimited JSON lines **as they arrive**.

    A stream's body is chunked, and the ordinary reader waits for the last
    chunk — which on a request that runs for a minute is the whole minute.
    This decodes the chunked framing a piece at a time and yields every
    complete line it uncovers, which is what makes a stream a stream.
    """
    chunked = "chunked" in headers.get("transfer-encoding", "").lower()
    buffer = bytearray()
    while True:
        if chunked:
            header = (await reader.readline()).strip()
            if not header:
                piece, ended = b"", True
            else:
                size = int(header.split(b";", 1)[0] or b"0", 16)
                if size == 0:
                    while True:
                        trailer = await reader.readline()
                        if trailer in (b"\r\n", b"\n", b""):
                            break
                    piece, ended = b"", True
                else:
                    piece = await reader.readexactly(size)
                    await reader.readexactly(2)
                    ended = False
        else:
            piece = await reader.read(64 * 1024)
            ended = not piece

        buffer += piece
        while True:
            at = buffer.find(b"\n")
            if at < 0:
                break
            line = bytes(buffer[:at]).strip()
            del buffer[: at + 1]
            if line:
                yield line
        if ended:
            last = bytes(buffer).strip()
            if last:
                yield last
            return


async def _read_chunked(reader: asyncio.StreamReader) -> bytes:
    """Read a chunked body.

    Not expected from this server — hyper answers a whole buffer with a content
    length — but a proxy in between may re-encode, and a client that assumed
    otherwise would fail in a way nobody could diagnose from the traceback.
    """
    body = bytearray()
    while True:
        header = (await reader.readline()).strip()
        size = int(header.split(b";", 1)[0] or b"0", 16)
        if size == 0:
            while True:
                trailer = await reader.readline()
                if trailer in (b"\r\n", b"\n", b""):
                    break
            return bytes(body)
        body += await reader.readexactly(size)
        if len(body) > MAX_BODY:
            raise TransportError("the API answered with more than this client's limit")
        await reader.readexactly(2)


async def _shut(writer: asyncio.StreamWriter) -> None:
    try:
        writer.close()
        await writer.wait_closed()
    except (OSError, RuntimeError):
        # Already gone, or the loop is closing. Neither is worth reporting
        # from a close path.
        pass


__all__ = ["AsyncClient", "AsyncFunctionHandle", "connect"]
