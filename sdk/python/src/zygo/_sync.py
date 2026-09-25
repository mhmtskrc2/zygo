"""The synchronous client.

Built on :mod:`http.client` from the standard library, so this package has no
dependencies at all. That is worth a little code: an SDK for a sandbox runtime
whose selling point is a small, auditable boundary should not arrive with a
dependency tree of its own.
"""

from __future__ import annotations

import base64
import http.client
import json
import os
import re
import socket
import threading
from typing import Any, Dict, Iterable, Iterator, List, Mapping, Optional, Sequence, Union

from ._endpoint import Endpoint, resolve
from ._errors import NotFound, SpecError, TransportError, ZygoError, from_response
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
        #: Set by :meth:`for_tenant`; sent as ``X-Zygo-Tenant`` on every call.
        self._tenant: Optional[str] = None

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
        """``GET /healthz``, which needs no token.

        ``status`` is ``ok``, ``degraded`` — a pool below its ``min_warm``, so
        requests work but the first of them pay a cold start — or ``stopping``,
        which is the only one that is not a `200`. A load balancer that kept
        sending to a draining host is the reason draining does not work.
        """
        return self._request("GET", "/healthz", authenticated=False)

    def drain(self, grace: float = 30.0) -> Dict[str, Any]:
        """Stop admitting, let what is running finish, then exit.

        Answers before the process leaves, so ``in_flight`` is the count of
        requests still going when the grace ran out — `0` is a clean drain and
        anything else is the difference between "drained" and "gave up".

        `SIGTERM` does the same, so a container stop or a `systemctl restart`
        needs no call at all. Operator-only, and needs deploy rights.
        """
        return self._request("POST", f"/drain?grace_ms={int(grace * 1000)}")

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

    def call(
        self,
        name: str,
        event: Any = None,
        *,
        timeout: Optional[float] = None,
        key: Optional[str] = None,
        workspace: Optional[str] = None,
        out: bool = False,
    ) -> Result:
        """Call a warm function and return what its handler returned.

        Raises :class:`~zygo.HandlerError` when the handler raised,
        :class:`~zygo.Timeout` when the deadline killed the request,
        :class:`~zygo.Cancelled` when somebody stopped it, and
        :class:`~zygo.Busy` when the function is at its concurrency limit —
        the last of which means the request never ran and is worth retrying.

        ``key`` is a name *you* choose for this request, so that another
        thread or process can stop it with :meth:`cancel` before it answers.
        The server's own id only arrives with the answer, which is too late to
        cancel the call it belongs to. Reusing a key is allowed and means one
        cancel stops every call under it.
        """
        headers = _timeout_header(timeout)
        if key is not None:
            headers["x-zygo-request-key"] = key
        body = self._request(
            "POST",
            f"/fn/{_escape(name)}{_workspace_query(workspace, out)}",
            body=event,
            headers=headers,
        )
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

    def create_tenant(self, id: str) -> Tenant:
        """Register a customer, or find the one already registered.

        Idempotent, so a deploy that runs twice is not an error. A tenant owns
        the scripts registered for it, and its functions and pools live in a
        cgroup of its own — which is what makes :meth:`delete_tenant` able to
        stop everything of theirs and nobody else's.

        Operator-only.
        """
        body = self._request("POST", "/tenants", body={"id": id})
        return Tenant.parse(body.get("tenant") or {})

    def tenants(self) -> List[Tenant]:
        """Every tenant this host holds. Operator-only."""
        body = self._request("GET", "/tenants")
        return [Tenant.parse(t) for t in body.get("tenants", [])]

    def tenant(self, id: str) -> Tenant:
        """One tenant. Raises :class:`~zygo.NotFound` if there is no such id."""
        body = self._request("GET", f"/tenants/{_escape(id)}")
        return Tenant.parse(body.get("tenant") or {})

    def delete_tenant(self, id: str) -> Dict[str, Any]:
        """Forget a tenant: stop its work, then remove the scripts only it had.

        Answers with what it stopped and what it removed, because both are
        facts the caller cannot reconstruct afterwards. Operator-only, and
        needs an API started with ``--allow-deploy``.
        """
        return self._request("DELETE", f"/tenants/{_escape(id)}")

    def stream(
        self,
        name: str,
        event: Any = None,
        *,
        timeout: Optional[float] = None,
        key: Optional[str] = None,
    ) -> "Iterator[Event]":
        """Call a function and yield its output as it is produced.

        Each item is an :class:`~zygo._models.Event`: ``stdout``, ``stderr`` or
        ``progress`` while the request runs, then exactly one ``result`` —
        which carries what :meth:`call` would have returned, or raises what it
        would have raised.

        ::

            for event in client.stream("render", {"pages": 400}):
                if event.is_result:
                    print(event.result.result)
                else:
                    print(event.kind, event.data, end="")

        The connection is held for the whole request and is **not** pooled:
        it is in use for as long as the handler runs. Abandoning the iterator
        closes it, which the server sees — but does not cancel the request.
        Cancelling is :meth:`cancel`, and this takes a ``key`` so you can.
        """
        headers = _timeout_header(timeout)
        if key is not None:
            headers["x-zygo-request-key"] = key
        return self._stream("POST", f"/fn/{_escape(name)}?stream=1", event, headers)

    def stream_script(
        self,
        runtime: str,
        script: str,
        event: Any = None,
        *,
        entry_point: Optional[str] = None,
        timeout: Optional[float] = None,
        key: Optional[str] = None,
    ) -> "Iterator[Event]":
        """Run a script in a pool, yielding its output. See :meth:`stream`."""
        payload: Dict[str, Any] = {
            "script": script if script.startswith("sha256:") else {"source": script},
            "event": event,
        }
        if entry_point is not None:
            payload["entry_point"] = entry_point
        headers = _timeout_header(timeout)
        if key is not None:
            headers["x-zygo-request-key"] = key
        return self._stream(
            "POST", f"/runtimes/{_escape(runtime)}/call?stream=1", payload, headers
        )

    def put_blob(self, tar: bytes) -> Script:
        """Store a tar this host will hold under its digest.

        For the case an embedder actually has: the same fixture, the same
        model weights, the same input document across a thousand calls. Sent
        once, named with ``workspace=`` on every call after — the bargain
        :meth:`put_script` makes for code.

        The body is the tar itself, not JSON around it.
        """
        body = self._request("PUT", "/blobs", raw_body=tar, binary=True)
        return Script.parse(body)

    def blob(self, digest: str) -> Script:
        """Whether this host holds a blob, and how big it is."""
        return Script.parse(self._request("GET", f"/blobs/{_escape_digest(digest)}"))

    def delete_blob(self, digest: str) -> bool:
        """Forget a blob. Operator-only: the store is shared by digest."""
        body = self._request("DELETE", f"/blobs/{_escape_digest(digest)}")
        return bool(body.get("deleted", False))

    def cancel(self, request_id: str) -> Dict[str, Any]:
        """Stop a request that is running.

        ``request_id`` comes from the ``X-Zygo-Request-Id`` header of the call
        in flight, or from a :class:`~zygo._models.Result` that has already
        come back. Answers as soon as the kill has been sent, not when the
        request has stopped — the caller waiting on that request is the one who
        gets the outcome, and they get :class:`~zygo.Cancelled`.

        ``started`` in the answer says whether the handler had begun. ``False``
        is the better outcome: the request's process existed but had not been
        let go, so no handler code ran at all.

        Raises :class:`~zygo.NotFound` when nothing is running under that id —
        which includes a request that finished a moment ago, and one belonging
        to another tenant.
        """
        return self._request("DELETE", f"/requests/{_escape(request_id)}")

    def set_limits(self, tenant: str, **limits: Any) -> Tenant:
        """What a tenant may not exceed: ``mem``, ``cpu``, ``pids``,
        ``timeout``, ``scratch``, ``network``, ``allow``.

        They only ever **narrow**. A tenant's limits are applied as the minimum
        of themselves and whatever the function or pool was declared with, so
        the worst a wrong value can do is give a customer less than they were
        promised — never more.

        A value above every ceiling the tenant currently has is refused with
        `422`, naming the key: it could not take effect, and storing it would
        leave you believing you had tightened something you had not.

        Partial: the keys you pass are set and the rest are left alone.

        Operator-only, and needs deploy rights.
        """
        body = self._request(
            "PATCH", f"/tenants/{_escape(tenant)}/limits", body=dict(limits)
        )
        return Tenant.parse(body.get("tenant") or {})

    def put_secret(self, tenant: str, name: str, value: str) -> List[str]:
        """Store one of a tenant's secrets, and get back their names.

        The body is the value itself, and the host encrypts it the moment it
        lands. Operator-only, and needs deploy rights: the operator holds the
        relationship with whoever issued the key, and a customer that could
        set one could set a value the operator's own functions then use.
        """
        body = self._request(
            "PUT",
            f"/tenants/{_escape(tenant)}/secrets/{_escape(name)}",
            raw_body=value.encode(),
        )
        return list(body.get("secrets", []))

    def secrets(self, tenant: str) -> List[str]:
        """The **names** a tenant has. There is no way to read a value back.

        A tenant may read its own; anything else is the operator's.
        """
        body = self._request("GET", f"/tenants/{_escape(tenant)}/secrets")
        return list(body.get("secrets", []))

    def delete_secret(self, tenant: str, name: str) -> List[str]:
        """Forget one. Operator-only, and needs deploy rights."""
        body = self._request(
            "DELETE", f"/tenants/{_escape(tenant)}/secrets/{_escape(name)}"
        )
        return list(body.get("secrets", []))

    def mint_token(self, tenant: Optional[str] = None) -> Minted:
        """Mint an API token, and get its secret — once.

        Without ``tenant`` this is an **operator** token: tenants, functions,
        pools, and more tokens. With one it is that tenant's, and it may
        register scripts and call, for itself only. Minting for a tenant
        registers the tenant if it is new.

        The secret is in the answer and nowhere else. The server keeps a
        SHA-256, so it cannot be fetched again; keep it or revoke it.

        Operator-only, and needs deploy rights.
        """
        path = "/tokens" if tenant is None else f"/tenants/{_escape(tenant)}/tokens"
        return Minted.parse(self._request("POST", path))

    def tokens(self) -> List[Token]:
        """Every token this host holds, revoked ones included. Never a secret."""
        body = self._request("GET", "/tokens")
        return [Token.parse(t) for t in body.get("tokens", [])]

    def revoke_token(self, id: str) -> Dict[str, Any]:
        """Revoke one, from the next request onwards.

        The record stays, marked with when it went, so an id in a log line
        still resolves to something.
        """
        return self._request("DELETE", f"/tokens/{_escape(id)}")

    def for_tenant(self, id: str) -> "Client":
        """A view of this client that acts for one tenant.

        Every call through it carries the tenant, so scripts are registered
        against them and pool calls may only name their own. The connection is
        shared — this is a header, not a second client.

        For an **operator** token: the header says which of your customers you
        are acting for. A **tenant** token already names its tenant and does
        not need this — and the server refuses a header that disagrees with
        the token rather than ignoring it, so a client that thinks it is
        acting for somebody else is told it is not.
        """
        view = Client.__new__(Client)
        view.__dict__.update(self.__dict__)
        view._tenant = id
        return view

    def serve_runtime(
        self,
        name: str,
        layer: Mapping[str, Any],
        *,
        base_dir: Optional[str] = None,
        deps: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Register a **runtime pool**: an image, a dependency set, an agent.

        A pool holds no code. Scripts arrive with each call, so one pool serves
        ten thousand of them where ten thousand functions would be ten thousand
        warm zygotes. ``layer`` is a ``[runtime.<name>]`` table as a dictionary
        — ``image``, ``agent``, ``min_warm``, ``max_warm`` and the limits.

        Needs an API started with ``--allow-deploy``.

        ``deps`` is an id from :meth:`put_deps`. A pool named against one that
        is still building raises :class:`~zygo.Unavailable` rather than
        starting: a zygote warmed without the dependencies it was promised
        serves requests that fail at ``import``. Retry — the exception carries
        the ``Retry-After`` the host suggested.
        """
        payload: Dict[str, Any] = {"name": name, "layer": dict(layer)}
        if base_dir is not None:
            payload["base_dir"] = os.path.abspath(base_dir)
        if deps is not None:
            payload["deps"] = deps
        return self._request("POST", "/runtimes", body=payload)

    def runtimes(self) -> List[Runtime]:
        """Every runtime pool this host holds."""
        body = self._request("GET", "/runtimes")
        return [Runtime.parse(r) for r in body.get("runtimes", [])]

    def stop_runtime(self, name: str) -> List[str]:
        """Stop a pool and drop its zygotes."""
        body = self._request("DELETE", f"/runtimes/{_escape(name)}")
        return list(body.get("stopped", []))

    def run_script(
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
        """Run one script in a pool.

        ``script`` is either a ``sha256:…`` digest this host holds — register
        it once with :meth:`put_script` — or the source itself. The digest is
        the shape to build on: the bytes cross the wire once rather than on
        every call, and the host can put the file in the sandbox instead of
        sending it through the zygote.

        ``key`` names the request so it can be cancelled; see :meth:`call`.

        Raises the same exceptions :meth:`call` does.
        """
        payload: Dict[str, Any] = {
            "script": script if script.startswith("sha256:") else {"source": script},
            "event": event,
        }
        if entry_point is not None:
            payload["entry_point"] = entry_point
        if workspace is not None:
            payload["workspace"] = dict(workspace)
        headers = _timeout_header(timeout)
        if key is not None:
            headers["x-zygo-request-key"] = key
        body = self._request(
            "POST",
            f"/runtimes/{_escape(runtime)}/call{'?out=1' if out else ''}",
            body=payload,
            headers=headers,
        )
        return Result.parse(body)

    def put_script(self, source: str) -> Script:
        """Register a script and get back the name the host gave it.

        The name is the SHA-256 of the bytes, so this is idempotent in the
        strongest sense: the same script registered twice — or by two tenants —
        is one file, and ``Script.existed`` says which call wrote it. Register
        once and name the digest on every call after that.

        Needs an API started with ``--allow-deploy``.
        """
        return Script.parse(self._request("PUT", "/scripts", raw_body=source.encode()))

    def put_deps(
        self,
        image: str,
        files: Mapping[str, Union[str, bytes]],
    ) -> Deps:
        """Build a dependency set from a lockfile, inside `image`.

        The API-native half of the spec's ``requirements``, which names a file
        on the Zygo host — the one thing an embedder has none of. Send the
        files themselves: ``{"requirements.txt": ...}`` for Python, or
        ``{"package.json": ..., "package-lock.json": ...}`` for Node.

        **Answers before the build finishes.** A ``pip install`` is minutes,
        so this returns as soon as the files are on disk, with
        ``state == "building"``. Idempotent by content: the same files against
        the same image are the same id, however many callers send them.

        The build runs in a sandbox that can reach the package registries and
        nothing else, because installing a package runs that package's code.
        """
        encoded = {
            name: base64.b64encode(
                content.encode() if isinstance(content, str) else content
            ).decode()
            for name, content in files.items()
        }
        return Deps.parse(
            self._request("POST", "/deps", body={"image": image, "files": encoded})
        )

    def deps(self, id: Optional[str] = None) -> Union[Deps, List[Deps]]:
        """One dependency set with its build log, or every one you can see."""
        if id is None:
            body = self._request("GET", "/deps")
            return [Deps.parse(d) for d in body.get("deps", [])]
        return Deps.parse(self._request("GET", f"/deps/{_escape(id)}"))

    def delete_deps(self, id: str) -> bool:
        """Forget a dependency set.

        Refused while a pool is built on it: the pool holds a read-only mount
        of the directory, and removing it under a warm zygote would leave the
        pool serving requests whose imports fail one at a time. Stop the pool
        first.
        """
        body = self._request("DELETE", f"/deps/{_escape(id)}")
        return bool(body.get("deleted", False))

    def script(self, digest: str) -> Script:
        """Whether this host holds a script, and how big it is.

        Never the bytes: a digest is not a capability, so a store that
        answered with the script would make every tenant's code readable by
        anyone who could guess what it was. Raises
        :class:`~zygo.NotFound` when the host does not have it.
        """
        return Script.parse(self._request("GET", f"/scripts/{_escape_digest(digest)}"))

    def delete_script(self, digest: str) -> bool:
        """Forget a script. Raises :class:`~zygo.NotFound` if it was not there."""
        body = self._request("DELETE", f"/scripts/{_escape_digest(digest)}")
        return bool(body.get("deleted", False))

    def fn(self, name: str) -> "FunctionHandle":
        """A callable bound to one function.

        ``client.fn("resize")(event)`` reads better than repeating the name at
        every call site, and it is the shape an embedder wraps as a tool.
        """
        return FunctionHandle(self, name)

    # ---- transport ----------------------------------------------------

    def _stream(
        self,
        method: str,
        path: str,
        body: Any,
        headers: Dict[str, str],
    ) -> Iterator[Event]:
        """One request whose answer arrives a line at a time.

        Deliberately outside the connection pool. A pooled connection is one
        that is finished with; this one is in use for as long as the handler
        runs, and returning it when the generator is abandoned would hand the
        next caller a socket with somebody else's request still on it.
        """
        if self._closed:
            raise ZygoError("this client has been closed")

        sent = {"accept": "application/x-ndjson", "content-type": "application/json"}
        if self.token:
            sent["authorization"] = f"Bearer {self.token}"
        if getattr(self, "_tenant", None):
            sent["x-zygo-tenant"] = self._tenant
        sent.update(headers)

        connection = self._open()
        try:
            connection.request(method, path, body=json.dumps(body).encode(), headers=sent)
            response = connection.getresponse()
            # A refusal — a bad token, no such function — is an ordinary JSON
            # answer with a status, not a stream. Raise it the usual way
            # rather than yielding a line that says the same thing.
            if response.status >= 400:
                raise from_response(
                    response.status,
                    _body(response.read(MAX_BODY)),
                    _retry_after(response.getheader("retry-after")),
                )
            for line in _stream_lines(response):
                event = Event.parse(_body(line))
                yield event
                if event.is_result:
                    # The stream ends with the result, and raising has to
                    # happen after the caller has seen it: a handler that
                    # printed and then failed produced both.
                    event.raise_for_status()
                    return
        finally:
            try:
                connection.close()
            except Exception:  # noqa: BLE001 - closing must not raise
                pass

    def _request(
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

        # `raw_body` is for the routes whose body is not JSON: a script is a
        # file and a blob is a tar, and wrapping either in a JSON string to
        # unwrap it again is a transformation with no reader. `binary` is the
        # difference between the two — a script is text, a tar is not.
        payload = raw_body if raw_body is not None else (
            None if body is None else json.dumps(body).encode()
        )
        sent = {"accept": "application/json"}
        if raw_body is not None:
            sent["content-type"] = (
                "application/octet-stream" if binary else "text/plain; charset=utf-8"
            )
        elif payload is not None:
            sent["content-type"] = "application/json"
        if authenticated and self.token:
            sent["authorization"] = f"Bearer {self.token}"
        if getattr(self, "_tenant", None):
            sent["x-zygo-tenant"] = self._tenant
        sent.update(headers or {})

        connection, reused = self._take()
        try:
            try:
                connection.request(method, path, body=payload, headers=sent)
            except (BrokenPipeError, ConnectionResetError):
                # A kept-alive connection the server had already closed. The
                # request never went out, so sending it again on a fresh
                # connection cannot run it twice. Only once, and only for a
                # reused one: a new connection that fails is a real failure.
                if not reused:
                    raise
                _discard(connection)
                connection = self._open()
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

    def _take(self) -> tuple[http.client.HTTPConnection, bool]:
        """A connection, and whether it has been used before."""
        with self._lock:
            if self._idle:
                return self._idle.pop(), True
        return self._open(), False

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


def _escape_digest(digest: str) -> str:
    """A digest, checked here so it can go into the path as it stands.

    ``quote`` would escape the colon, and the route matches on the segment it
    was given. Checking the shape instead of escaping it also means
    ``../../etc/passwd`` is a mistake this client names, rather than a request
    somebody's proxy might normalise into a different route.
    """
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest or ""):
        raise SpecError(
            f"`{digest}` is not a script digest; "
            "expected sha256: followed by 64 lowercase hex digits"
        )
    return digest


def _stream_lines(response: "http.client.HTTPResponse") -> "Iterator[bytes]":
    """Newline-delimited JSON, a line at a time, as it arrives.

    `readline` rather than iterating the response: a buffered iterator reads
    ahead, which on a stream means waiting for output that has not been
    produced yet — the one thing this exists not to do.
    """
    while True:
        line = response.readline(MAX_BODY)
        if not line:
            return
        line = line.strip()
        if line:
            yield line


def _workspace_query(blob: Optional[str], out: bool) -> str:
    """`?workspace=…&out=1`, for the route whose body is the event itself.

    A function is called with its event as the whole body, so there is nowhere
    in it to put a workspace. Only a *blob* can be named here — an inline tar
    in a URL would be a megabyte of base64 in a request line, which every
    proxy in between has an opinion about. Use a pool's body for that.
    """
    parts = []
    if blob is not None:
        if not blob.startswith("sha256:"):
            raise SpecError(f"`{blob}` is not a blob digest; store one with put_blob()")
        parts.append(f"workspace={blob}")
    if out:
        parts.append("out=1")
    return f"?{'&'.join(parts)}" if parts else ""


def _request_key() -> str:
    """A name for one request, unique enough that a cancel finds only it.

    128 bits from ``os.urandom``. Not a counter and not a UUID library call:
    the only thing this has to be is unguessable by anybody who might want to
    stop somebody else's work — and the server checks ownership as well, so
    this is the second lock rather than the only one.
    """
    return "k-" + os.urandom(16).hex()


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


def _body(raw: bytes) -> Dict[str, Any]:
    """One JSON object, or a transport error naming what came instead."""
    try:
        value = json.loads(raw) if raw else {}
    except ValueError as e:
        raise TransportError("the API sent something that is not JSON") from e
    return value if isinstance(value, dict) else {"error": str(value)}


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
