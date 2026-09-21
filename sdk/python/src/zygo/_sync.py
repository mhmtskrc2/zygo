"""The synchronous client.

Built on :mod:`http.client` from the standard library, so this package has no
dependencies at all. That is worth a little code: an SDK for a sandbox runtime
whose selling point is a small, auditable boundary should not arrive with a
dependency tree of its own.
"""

from __future__ import annotations

import http.client
import json
import os
import socket
import threading
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence

from ._endpoint import Endpoint, resolve
from ._errors import NotFound, TransportError, ZygoError, from_response
from ._models import Function, LogPage, Result, Run, Served

#: Largest answer read into memory. The API's own request limit is the same
#: order, and an answer past it is a bug rather than a large result.
MAX_BODY = 64 * 1024 * 1024


class _UnixConnection(http.client.HTTPConnection):
    """``HTTPConnection`` over a unix socket.

    The only thing that differs from a TCP connection is how it is opened, so
    the rest of :mod:`http.client` — keep-alive, chunked bodies, header
    parsing — works unchanged.
    """

    def __init__(self, path: str, timeout: float) -> None:
        super().__init__("localhost", timeout=timeout)
        self._path = path

    def connect(self) -> None:  # noqa: D102 - documented on the base class
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(self.timeout)
        try:
            sock.connect(self._path)
        except OSError as e:
            sock.close()
            raise TransportError(
                f"no Zygo API at {self._path}: {e}\n"
                "  -> start one with `zygo api --listen unix://" + self._path + "`"
            ) from e
        self.sock = sock


class Client:
    """A connection to a Zygo API.

    Safe to share between threads: connections are pooled, and one is held for
    the duration of each request. That is what turns concurrent callers into
    concurrent sandbox requests rather than a queue behind one socket.

    ``timeout`` bounds a single HTTP exchange. It is deliberately generous by
    default, because the request's real limit is the function's own ``timeout``
    and the supervisor enforces it — a client that gives up first only loses
    the answer, it does not stop the work.
    """

    def __init__(
        self,
        url: Optional[str] = None,
        *,
        token: Optional[str] = None,
        timeout: float = 300.0,
    ) -> None:
        self.endpoint: Endpoint = resolve(url)
        # The environment by default, the same variable the server reads, so a
        # shell that can start the API can also talk to it.
        self.token = token if token is not None else os.environ.get("ZYGO_API_TOKEN")
        self.timeout = timeout
        self._idle: List[http.client.HTTPConnection] = []
        self._lock = threading.Lock()
        self._closed = False

    # ---- lifecycle ----------------------------------------------------

    def close(self) -> None:
        """Close every pooled connection. Calling it twice is harmless."""
        with self._lock:
            self._closed = True
            idle, self._idle = self._idle, []
        for connection in idle:
            try:
                connection.close()
            except Exception:  # noqa: BLE001 - closing must not raise
                pass

    def __enter__(self) -> "Client":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()

    # ---- the API ------------------------------------------------------

    def version(self) -> Dict[str, Any]:
        """The server's version, and the version of the HTTP surface itself.

        ``api`` is what to check against: it is bumped only when a route
        changes incompatibly, so it stays put across Zygo releases that change
        what happens behind them.
        """
        return self._request("GET", "/version")

    def health(self) -> Dict[str, Any]:
        """``GET /healthz``, which needs no token."""
        return self._request("GET", "/healthz", authenticated=False)

    def functions(self) -> List[Function]:
        """Every warm function the supervisor holds."""
        body = self._request("GET", "/fn")
        return [Function.parse(f) for f in body.get("functions", [])]

    def stats(self, name: str) -> Function:
        """One function's counters."""
        return Function.parse(self._request("GET", f"/fn/{_escape(name)}/stats"))

    def warm(self, name: str) -> Dict[str, Any]:
        """Bring a registered function up now, without calling it.

        For the moment after a deploy: a function that went cold, or one that
        is paused, is made ready so the first real request does not pay for it.
        """
        return self._request("POST", f"/fn/{_escape(name)}/warm")

    def call(self, name: str, event: Any = None, *, timeout: Optional[float] = None) -> Result:
        """Call a warm function and return what its handler returned.

        Raises :class:`~zygo.HandlerError` when the handler raised,
        :class:`~zygo.Timeout` when the deadline killed the request, and
        :class:`~zygo.Busy` when the function is at its concurrency limit —
        the last of which means the request never ran and is worth retrying.
        """
        headers = _timeout_header(timeout)
        body = self._request("POST", f"/fn/{_escape(name)}", body=event, headers=headers)
        return Result.parse(body)

    def batch(
        self,
        name: str,
        events: Sequence[Any],
        *,
        timeout: Optional[float] = None,
    ) -> List[Any]:
        """Call a function with several events at once, answers in order.

        Each element is a :class:`~zygo._models.Result` or the exception that
        element would have raised — returned rather than raised, because one
        event being refused must not hide the answers to the others.
        """
        headers = _timeout_header(timeout)
        answers = self._request(
            "POST", f"/fn/{_escape(name)}/batch", body=list(events), headers=headers
        )
        return [_batch_element(a) for a in answers]

    def logs(
        self,
        name: str,
        *,
        after: int = 0,
        limit: int = 50,
        failed: bool = False,
    ) -> LogPage:
        """A function's recent log.

        ``after`` is a sequence number: zero for the last ``limit`` entries,
        and the previous page's ``next`` for everything since.
        """
        query = f"?after={int(after)}&limit={int(limit)}&failed={'true' if failed else 'false'}"
        return LogPage.parse(self._request("GET", f"/fn/{_escape(name)}/logs{query}"))

    def serve(
        self,
        name: str,
        layer: Mapping[str, Any],
        *,
        base_dir: Optional[str] = None,
        secrets: Optional[Mapping[str, str]] = None,
        if_changed: bool = False,
    ) -> Served:
        """Warm a function, replacing whatever held the name.

        ``layer`` is a ``[fn.<name>]`` table as a dictionary. ``base_dir`` is
        what its relative paths are relative to **on the host the API runs
        on**; it defaults to this process's working directory, which is right
        only when the two are the same machine.

        Needs an API started with ``--allow-deploy``; without it this raises
        :class:`~zygo.AuthError` and says so.
        """
        payload = {
            "layer": dict(layer),
            "base_dir": os.path.abspath(base_dir or os.getcwd()),
            "secrets": dict(secrets or {}),
            "if_changed": bool(if_changed),
        }
        return Served.parse(self._request("PUT", f"/fn/{_escape(name)}", body=payload))

    def stop(self, name: str) -> List[str]:
        """Stop one function. Raises :class:`~zygo.NotFound` if it is not there."""
        body = self._request("DELETE", f"/fn/{_escape(name)}")
        return list(body.get("stopped", []))

    def run(
        self,
        image: str,
        cmd: Optional[Sequence[str]] = None,
        *,
        stdin: str = "",
        **layer: Any,
    ) -> Run:
        """Run one command in a fresh sandbox and collect its output.

        Keyword arguments are spec fields: ``mem``, ``cpu``, ``pids``,
        ``timeout``, ``network``, ``allow``, ``mounts``, ``env`` and the rest,
        written exactly as they would be in ``sandbox.toml``. Mount sources
        must be absolute, because a relative path in a request body has no
        directory to be relative to.

        Needs an API started with ``--allow-deploy``.
        """
        described: Dict[str, Any] = dict(layer)
        described["image"] = image
        if cmd is not None:
            described["cmd"] = list(cmd)
        return Run.parse(self._request("POST", "/run", body={"layer": described, "stdin": stdin}))

    def fn(self, name: str) -> "FunctionHandle":
        """A callable bound to one function.

        ``client.fn("resize")(event)`` reads better than repeating the name at
        every call site, and it is the shape an embedder wraps as a tool.
        """
        return FunctionHandle(self, name)

    # ---- transport ----------------------------------------------------

    def _request(
        self,
        method: str,
        path: str,
        *,
        body: Any = None,
        headers: Optional[Dict[str, str]] = None,
        authenticated: bool = True,
    ) -> Any:
        if self._closed:
            raise ZygoError("this client has been closed")

        payload = None if body is None else json.dumps(body).encode()
        sent = {"accept": "application/json"}
        if payload is not None:
            sent["content-type"] = "application/json"
        if authenticated and self.token:
            sent["authorization"] = f"Bearer {self.token}"
        sent.update(headers or {})

        connection = self._take()
        try:
            connection.request(method, path, body=payload, headers=sent)
            response = connection.getresponse()
            raw = response.read(MAX_BODY)
            status = response.status
            retry_after = _retry_after(response.getheader("retry-after"))
            # Only a connection that completed an exchange cleanly goes back:
            # one that raised may be mid-message, and the next request must not
            # inherit that.
            reusable = response.getheader("connection", "").lower() != "close"
        except (http.client.HTTPException, OSError) as e:
            _discard(connection)
            hint = ""
            if isinstance(e, ConnectionRefusedError):
                hint = "\n  -> nothing is listening there; start one with `zygo api`"
            raise TransportError(f"{method} {path} failed against {self.endpoint}: {e}{hint}") from e

        if reusable:
            self._give_back(connection)
        else:
            _discard(connection)

        return _decode(status, raw, retry_after)

    def _take(self) -> http.client.HTTPConnection:
        with self._lock:
            if self._idle:
                return self._idle.pop()
        return self._open()

    def _give_back(self, connection: http.client.HTTPConnection) -> None:
        with self._lock:
            if self._closed:
                _discard(connection)
                return
            self._idle.append(connection)

    def _open(self) -> http.client.HTTPConnection:
        endpoint = self.endpoint
        if endpoint.is_unix:
            assert endpoint.socket_path is not None
            return _UnixConnection(endpoint.socket_path, self.timeout)
        if endpoint.tls:
            return http.client.HTTPSConnection(endpoint.host, endpoint.port, timeout=self.timeout)
        return http.client.HTTPConnection(endpoint.host, endpoint.port, timeout=self.timeout)


class FunctionHandle:
    """One function, bound to a client. Call it like a function."""

    __slots__ = ("_client", "name")

    def __init__(self, client: Client, name: str) -> None:
        self._client = client
        self.name = name

    def __call__(self, event: Any = None, *, timeout: Optional[float] = None) -> Result:
        return self._client.call(self.name, event, timeout=timeout)

    def batch(self, events: Iterable[Any], *, timeout: Optional[float] = None) -> List[Any]:
        return self._client.batch(self.name, list(events), timeout=timeout)

    def stats(self) -> Function:
        return self._client.stats(self.name)

    def logs(self, *, after: int = 0, limit: int = 50, failed: bool = False) -> LogPage:
        return self._client.logs(self.name, after=after, limit=limit, failed=failed)

    def warm(self) -> Dict[str, Any]:
        return self._client.warm(self.name)

    def stop(self) -> List[str]:
        return self._client.stop(self.name)

    def __repr__(self) -> str:
        return f"<zygo function {self.name!r}>"


def connect(url: Optional[str] = None, *, token: Optional[str] = None, timeout: float = 300.0) -> Client:
    """Open a client. See :class:`Client` for what the arguments mean."""
    return Client(url, token=token, timeout=timeout)


# ---- shared helpers ---------------------------------------------------
#
# Used by the asynchronous client too, so the two cannot disagree about what a
# status code means or how a batch element is read.


def _discard(connection: http.client.HTTPConnection) -> None:
    """Close a connection that must not be reused.

    Every error path goes through here. A connection that raised mid-exchange
    may have half a response still in its buffer, and handing it to the next
    request is how one failure becomes a stream of unrelated ones.
    """
    try:
        connection.close()
    except Exception:  # noqa: BLE001 - closing must not raise
        pass


def _escape(name: str) -> str:
    from urllib.parse import quote

    return quote(name, safe="")


def _timeout_header(timeout: Optional[float]) -> Dict[str, str]:
    if timeout is None:
        return {}
    ms = max(1, int(timeout * 1000))
    return {"x-zygo-timeout-ms": str(ms)}


def _retry_after(header: Optional[str]) -> float:
    try:
        return float(header) if header else 1.0
    except ValueError:
        return 1.0


def _decode(status: int, raw: bytes, retry_after: float) -> Any:
    try:
        body = json.loads(raw) if raw else {}
    except ValueError as e:
        raise TransportError(f"the API answered HTTP {status} with something that is not JSON") from e

    if 200 <= status < 300:
        return body
    if not isinstance(body, dict):
        body = {"error": str(body)}
    raise from_response(status, body, retry_after)


def _batch_element(answer: Any) -> Any:
    """One element of a batch: a result, or the error it would have raised.

    Returned rather than raised. A batch exists so that one refused event does
    not hide the answers to the others, and raising on the first bad element
    would undo exactly that.
    """
    if not isinstance(answer, dict):
        return ZygoError(f"unreadable batch element: {answer!r}")
    status = int(answer.get("status", 200))
    if 200 <= status < 300:
        return Result.parse(answer)
    return from_response(status, answer)


__all__ = ["Client", "FunctionHandle", "connect", "NotFound"]
