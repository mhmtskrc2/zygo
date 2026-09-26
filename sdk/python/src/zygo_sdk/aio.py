# SPDX-License-Identifier: Apache-2.0
"""The asynchronous client: ``from zygo_sdk.aio import connect``.

Agent frameworks are asynchronous, and a tool that blocks the event loop for
the length of a sandbox request is not usable inside one. This is the same API
as :mod:`zygo_sdk`, with every call a coroutine: the same method names, the
same arguments, the same exceptions. A test in the suite holds the two to that.

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
import time
from typing import Any, Dict, Iterable, List, Mapping, Optional, Sequence, Tuple, Union

from ._endpoint import Endpoint, resolve
from ._errors import Busy, TransportError, Unavailable, ZygoError, from_response
from ._models import (
    Deps,
    Event,
    Function,
    LogPage,
    Minted,
    Result,
    Run,
    Runtime,
    Script,
    Served,
    Tenant,
    Token,
)
from ._sync import (
    MAX_BODY,
    POOL_IDLE_LIMIT,
    _ConnectionLost,
    _batch_element,
    _body,
    _decode,
    _escape,
    _escape_digest,
    _request_key,
    _retry_after,
    _retry_wait,
    _timeout_header,
    _workspace_query,
)

_Connection = Tuple[asyncio.StreamReader, asyncio.StreamWriter]


class AsyncClient:
    """A connection to a Zygo API, for an event loop.

    Safe to share between tasks: connections are pooled and one is held for the
    duration of each request, so concurrent callers become concurrent sandbox
    requests rather than a queue behind one socket.

    ``timeout``, ``retries`` and ``backoff`` mean what they mean on
    :class:`~zygo_sdk.Client`. A retry waits with ``asyncio.sleep``, so the
    loop keeps running while it does.
    """

    def __init__(
        self,
        url: Optional[str] = None,
        *,
        token: Optional[str] = None,
        timeout: float = 300.0,
        retries: int = 0,
        backoff: float = 1.0,
    ) -> None:
        self.endpoint: Endpoint = resolve(url)
        self.token = token if token is not None else os.environ.get("ZYGO_API_TOKEN")
        self.timeout = timeout
        self.retries = max(0, int(retries))
        self.backoff = max(0.0, float(backoff))
        self._idle: List[Tuple[_Connection, float]] = []
        self._lock = asyncio.Lock()
        self._closed = False
        #: Set by :meth:`for_tenant`; sent as ``X-Zygo-Tenant`` on every call.
        self._tenant: Optional[str] = None

    # ---- lifecycle ----------------------------------------------------

    async def close(self) -> None:
        async with self._lock:
            self._closed = True
            idle, self._idle = self._idle, []
        for (_, writer), _ in idle:
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

    async def drain(self, grace: float = 30.0) -> Dict[str, Any]:
        """Stop admitting, let what is running finish, then exit. See
        :meth:`zygo_sdk.Client.drain`."""
        return await self._request("POST", f"/drain?grace_ms={int(grace * 1000)}")

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
        workspace: Optional[str] = None,
        out: bool = False,
    ) -> Result:
        """Call a warm function.

        **Cancelling the task cancels the request.** ``asyncio.CancelledError`` —
        from ``task.cancel()``, from a ``timeout`` block, from the loop
        shutting down — sends ``DELETE /requests/<key>`` before it propagates,
        so the sandbox stops rather than running on unread. That is the whole
        reason the request carries a key: the server's own id only arrives
        *with the answer*, which is too late to stop the call it belongs to.

        Pass ``key`` to choose the name yourself; otherwise one is generated.
        ``workspace`` names a blob from :meth:`put_blob` to unpack into the
        sandbox, and ``out=True`` asks for the sandbox's ``/out`` back, as a
        tar on the result; see :meth:`zygo_sdk.Client.call`.
        """
        key = key or _request_key()
        headers = _timeout_header(timeout)
        headers["x-zygo-request-key"] = key
        try:
            body = await self._request(
                "POST",
                f"/fn/{_escape(name)}{_workspace_query(workspace, out)}",
                body=event,
                headers=headers,
            )
        except asyncio.CancelledError:
            await self._cancel_quietly(key)
            raise
        return Result.parse(body)

    async def cancel(self, request_id: str) -> Dict[str, Any]:
        """Stop a request that is running. See :meth:`zygo_sdk.Client.cancel`."""
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

        An async iterator of :class:`~zygo_sdk._models.Event`; see
        :meth:`zygo_sdk.Client.stream` for what the items are.

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
        if self._tenant:
            sent["x-zygo-tenant"] = self._tenant
        sent.update(headers)
        head = f"POST {path} HTTP/1.1\r\n"
        head += "".join(f"{k}: {v}\r\n" for k, v in sent.items())
        head += "\r\n"

        attempt = 0
        reader, writer = await self._open()
        try:
            while True:
                writer.write(head.encode("latin-1") + payload)
                await writer.drain()
                status, response_headers = await _read_head(reader)
                if status < 400:
                    break
                error = from_response(
                    status,
                    _body(await _read_body(reader, response_headers)),
                    _retry_after(response_headers.get("retry-after")),
                )
                wait = _retry_wait(self.retries, self.backoff, error, attempt)
                if wait is None:
                    raise error
                # Nothing has been yielded yet: the same request again, on a
                # fresh connection, after the wait the refusal asked for.
                await _shut(writer)
                await asyncio.sleep(wait)
                attempt += 1
                reader, writer = await self._open()
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

    async def create_tenant(self, id: str) -> Tenant:
        """Register a customer, or find the one already registered. See
        :meth:`zygo_sdk.Client.create_tenant`."""
        body = await self._request("POST", "/tenants", body={"id": id})
        return Tenant.parse(body.get("tenant") or {})

    async def tenants(self) -> List[Tenant]:
        """Every tenant this host holds. Operator-only."""
        body = await self._request("GET", "/tenants")
        return [Tenant.parse(t) for t in body.get("tenants", [])]

    async def tenant(self, id: str) -> Tenant:
        """One tenant. Raises :class:`~zygo_sdk.NotFound` if there is no such id."""
        body = await self._request("GET", f"/tenants/{_escape(id)}")
        return Tenant.parse(body.get("tenant") or {})

    async def delete_tenant(self, id: str) -> Dict[str, Any]:
        """Forget a tenant: stop its work, then remove the scripts only it
        had. See :meth:`zygo_sdk.Client.delete_tenant`."""
        return await self._request("DELETE", f"/tenants/{_escape(id)}")

    async def put_blob(self, tar: bytes) -> Script:
        """Store a tar this host will hold under its digest. See
        :meth:`zygo_sdk.Client.put_blob`."""
        body = await self._request("PUT", "/blobs", raw_body=tar, binary=True)
        return Script.parse(body)

    async def blob(self, digest: str) -> Script:
        """Whether this host holds a blob, and how big it is."""
        return Script.parse(await self._request("GET", f"/blobs/{_escape_digest(digest)}"))

    async def delete_blob(self, digest: str) -> bool:
        """Forget a blob. Operator-only: the store is shared by digest."""
        body = await self._request("DELETE", f"/blobs/{_escape_digest(digest)}")
        return bool(body.get("deleted", False))

    async def set_limits(self, tenant: str, **limits: Any) -> Tenant:
        """What a tenant may not exceed. See :meth:`zygo_sdk.Client.set_limits`."""
        body = await self._request(
            "PATCH", f"/tenants/{_escape(tenant)}/limits", body=dict(limits)
        )
        return Tenant.parse(body.get("tenant") or {})

    async def put_secret(self, tenant: str, name: str, value: str) -> List[str]:
        """Store one of a tenant's secrets, and get back their names. See
        :meth:`zygo_sdk.Client.put_secret`."""
        body = await self._request(
            "PUT",
            f"/tenants/{_escape(tenant)}/secrets/{_escape(name)}",
            raw_body=value.encode(),
        )
        return list(body.get("secrets", []))

    async def secrets(self, tenant: str) -> List[str]:
        """The **names** a tenant has. There is no way to read a value back."""
        body = await self._request("GET", f"/tenants/{_escape(tenant)}/secrets")
        return list(body.get("secrets", []))

    async def delete_secret(self, tenant: str, name: str) -> List[str]:
        """Forget one. Operator-only, and needs deploy rights."""
        body = await self._request(
            "DELETE", f"/tenants/{_escape(tenant)}/secrets/{_escape(name)}"
        )
        return list(body.get("secrets", []))

    async def mint_token(self, tenant: Optional[str] = None) -> Minted:
        """Mint an API token, and get its secret — once. See
        :meth:`zygo_sdk.Client.mint_token`."""
        path = "/tokens" if tenant is None else f"/tenants/{_escape(tenant)}/tokens"
        return Minted.parse(await self._request("POST", path))

    async def tokens(self) -> List[Token]:
        """Every token this host holds, revoked ones included. Never a secret."""
        body = await self._request("GET", "/tokens")
        return [Token.parse(t) for t in body.get("tokens", [])]

    async def revoke_token(self, id: str) -> Dict[str, Any]:
        """Revoke one, from the next request onwards."""
        return await self._request("DELETE", f"/tokens/{_escape(id)}")

    def for_tenant(self, id: str) -> "AsyncClient":
        """A view of this client that acts for one tenant.

        A header on the same connection pool, not a second client; see
        :meth:`zygo_sdk.Client.for_tenant`. Closing either closes both.
        """
        view = AsyncClient.__new__(AsyncClient)
        view.__dict__.update(self.__dict__)
        view._tenant = id
        return view

    async def serve_runtime(
        self,
        name: str,
        layer: Mapping[str, Any],
        *,
        base_dir: Optional[str] = None,
        deps: Optional[str] = None,
        secrets: Optional[Sequence[str]] = None,
    ) -> Dict[str, Any]:
        payload: Dict[str, Any] = {"name": name, "layer": dict(layer)}
        if secrets is not None:
            payload["layer"]["secrets"] = list(secrets)
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
        workspace: Optional[Dict[str, Any]] = None,
        out: bool = False,
    ) -> Result:
        """Run one script in a pool. Cancelling the task cancels the request;
        see :meth:`call`. ``workspace`` and ``out`` are as on
        :meth:`zygo_sdk.Client.run_script`."""
        payload: Dict[str, Any] = {
            "script": script if script.startswith("sha256:") else {"source": script},
            "event": event,
        }
        if entry_point is not None:
            payload["entry_point"] = entry_point
        if workspace is not None:
            payload["workspace"] = dict(workspace)
        key = key or _request_key()
        headers = _timeout_header(timeout)
        headers["x-zygo-request-key"] = key
        try:
            body = await self._request(
                "POST",
                f"/runtimes/{_escape(runtime)}/call{'?out=1' if out else ''}",
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
        binary: bool = False,
    ) -> Any:
        if self._closed:
            raise ZygoError("this client has been closed")

        # `raw_body` is for the routes whose body is not JSON: see the
        # synchronous client, which says why a script travels as itself and
        # why a tar says it is binary.
        payload = raw_body if raw_body is not None else (
            b"" if body is None else json.dumps(body).encode()
        )
        sent = {
            "host": "localhost" if self.endpoint.is_unix else f"{self.endpoint.host}:{self.endpoint.port}",
            "accept": "application/json",
            "content-length": str(len(payload)),
        }
        if raw_body is not None:
            sent["content-type"] = (
                "application/octet-stream" if binary else "text/plain; charset=utf-8"
            )
        elif body is not None:
            sent["content-type"] = "application/json"
        if authenticated and self.token:
            sent["authorization"] = f"Bearer {self.token}"
        if self._tenant:
            sent["x-zygo-tenant"] = self._tenant
        sent.update(headers or {})

        head = f"{method} {path} HTTP/1.1\r\n"
        head += "".join(f"{k}: {v}\r\n" for k, v in sent.items())
        head += "\r\n"

        attempt = 0
        while True:
            try:
                return await self._exchange(method, path, head.encode("latin-1") + payload)
            except (Busy, Unavailable) as error:
                wait = _retry_wait(self.retries, self.backoff, error, attempt)
                if wait is None:
                    raise
                await asyncio.sleep(wait)
                attempt += 1

    async def _exchange(self, method: str, path: str, message: bytes) -> Any:
        """One request and its answer, on a pooled connection."""
        connection, reused = await self._take()
        try:
            return await self._exchange_on(connection, method, path, message)
        except _ConnectionLost as lost:
            # A pooled connection the server had closed while it waited.
            # Nothing of an answer arrived, so the request did not run, and
            # sending it again on a fresh connection cannot run it twice.
            # Once, and only for a reused connection: a new one that fails is
            # a real failure, reported as such.
            if not reused:
                raise _transport_error(method, path, self.endpoint, lost.cause) from lost.cause
        try:
            return await self._exchange_on(await self._open(), method, path, message)
        except _ConnectionLost as lost:
            raise _transport_error(method, path, self.endpoint, lost.cause) from lost.cause

    async def _exchange_on(
        self, connection: _Connection, method: str, path: str, message: bytes
    ) -> Any:
        """The exchange itself, on this connection.

        Raises :class:`_ConnectionLost` if the connection failed before a
        byte of the answer arrived — the write, or end-of-file or a reset
        where the status line should be — and :class:`TransportError` for
        anything after that.
        """
        reader, writer = connection
        try:
            try:
                writer.write(message)
                await writer.drain()
            except OSError as e:
                raise _ConnectionLost(e) from e
            try:
                status, response_headers, raw = await asyncio.wait_for(
                    _read_response(reader), timeout=self.timeout
                )
            except _NoAnswer as e:
                raise _ConnectionLost(e) from e
        except asyncio.CancelledError:
            # The exchange is half-finished: the request went out and the
            # answer is still coming. The connection can neither be reused —
            # the next request would read this one's response — nor left, which
            # is what used to happen: a cancelled task leaked its socket, and
            # the warning only appeared once cancelling became an ordinary
            # thing to do. Close it and let the cancel propagate.
            await _shut(writer)
            raise
        except _ConnectionLost:
            await _shut(writer)
            raise
        except (OSError, asyncio.IncompleteReadError, asyncio.TimeoutError, ValueError) as e:
            await _shut(writer)
            raise _transport_error(method, path, self.endpoint, e) from e

        # Only a connection that completed an exchange cleanly goes back: one
        # that raised may be mid-message, and the next request must not inherit
        # that.
        if response_headers.get("connection", "").lower() == "close":
            await _shut(writer)
        else:
            await self._give_back((reader, writer))

        return _decode(status, raw, _retry_after(response_headers.get("retry-after")))

    async def _take(self) -> Tuple[_Connection, bool]:
        """A connection, and whether it has been used before.

        The most recently returned connection first, so the pool's youngest
        connections stay in use and its oldest go stale and are dropped. One
        that has waited longer than :data:`~zygo_sdk._sync.POOL_IDLE_LIMIT`
        is closed here rather than handed out: the server has hung up on it,
        or is about to.
        """
        stale: List[_Connection] = []
        taken: Optional[_Connection] = None
        async with self._lock:
            while self._idle:
                connection, since = self._idle.pop()
                if time.monotonic() - since <= POOL_IDLE_LIMIT:
                    taken = connection
                    break
                stale.append(connection)
        for _, writer in stale:
            await _shut(writer)
        if taken is not None:
            return taken, True
        return await self._open(), False

    async def _give_back(self, connection: _Connection) -> None:
        async with self._lock:
            if self._closed:
                await _shut(connection[1])
                return
            self._idle.append((connection, time.monotonic()))

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


def connect(
    url: Optional[str] = None,
    *,
    token: Optional[str] = None,
    timeout: float = 300.0,
    retries: int = 0,
    backoff: float = 1.0,
) -> AsyncClient:
    """Open an asynchronous client.

    Not a coroutine: nothing is connected until the first call, so there is
    nothing to await here and ``async with zygo_sdk.aio.connect() as c`` works.
    """
    return AsyncClient(url, token=token, timeout=timeout, retries=retries, backoff=backoff)


# ---- a small HTTP/1.1 reader -----------------------------------------


async def _read_head(reader: asyncio.StreamReader) -> Tuple[int, Dict[str, str]]:
    """The status line and headers, stopping before the body.

    Split out from reading the whole answer because a stream's body has no end
    to wait for: the point is to look at the status, decide, and then read the
    body a piece at a time.
    """
    # End-of-file here, or a reset, is the whole answer failing to arrive.
    # The server closes an idle connection cleanly, so end-of-file is the
    # usual shape; a reset is what the kernel answers a write on a socket the
    # peer has closed with, and nothing has been read by then either.
    try:
        status_line = await reader.readline()
    except (ConnectionResetError, BrokenPipeError) as e:
        raise _NoAnswer("the API closed the connection without answering") from e
    if not status_line:
        raise _NoAnswer("the API closed the connection without answering")
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


class _NoAnswer(TransportError):
    """The connection ended before the status line.

    A :class:`TransportError` like any other to a caller — a stream, which
    holds its own connection, reports it as one — but distinguishable by the
    pooled exchange, which may send the request once more.
    """


def _transport_error(method: str, path: str, endpoint: Endpoint, cause: BaseException) -> TransportError:
    return TransportError(f"{method} {path} failed against {endpoint}: {cause}")


async def _shut(writer: asyncio.StreamWriter) -> None:
    try:
        writer.close()
        await writer.wait_closed()
    except (OSError, RuntimeError):
        # Already gone, or the loop is closing. Neither is worth reporting
        # from a close path.
        pass


__all__ = ["AsyncClient", "AsyncFunctionHandle", "connect"]
