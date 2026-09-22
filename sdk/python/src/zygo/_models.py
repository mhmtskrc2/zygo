"""The shapes that come back, as plain dataclasses.

Deliberately thin. Every one of these mirrors a JSON object the API already
documents, and each keeps the raw dictionary so a field added by a newer Zygo
is reachable without waiting for this package to catch up.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional


@dataclass(frozen=True)
class Metrics:
    """What one request cost."""

    wall_ms: float = 0.0
    cpu_ms: float = 0.0
    peak_rss_kb: int = 0

    @classmethod
    def parse(cls, raw: Optional[Dict[str, Any]]) -> "Metrics":
        raw = raw or {}
        return cls(
            wall_ms=float(raw.get("wall_ms", 0.0)),
            cpu_ms=float(raw.get("cpu_ms", 0.0)),
            peak_rss_kb=int(raw.get("peak_rss_kb", 0)),
        )


@dataclass(frozen=True)
class Result:
    """What a warm function returned.

    ``result`` is the handler's own return value. ``stdout`` and ``stderr`` are
    what the request's process wrote, which is separate from the zygote's own
    output — a handler that prints is not polluting the next request's answer.
    """

    result: Any
    stdout: str = ""
    stderr: str = ""
    metrics: Metrics = field(default_factory=Metrics)
    #: The request's own id, which :meth:`~zygo.Client.cancel` names.
    #:
    #: Of no use for *this* call, which has already finished. It is here so
    #: that a log line about a slow request can be joined back to the request,
    #: and because the same id arrives in the ``X-Zygo-Request-Id`` header of a
    #: call that is still in flight.
    request_id: str = ""

    @classmethod
    def parse(cls, raw: Dict[str, Any]) -> "Result":
        return cls(
            result=raw.get("result"),
            stdout=str(raw.get("stdout", "")),
            stderr=str(raw.get("stderr", "")),
            metrics=Metrics.parse(raw.get("metrics")),
            request_id=str(raw.get("request_id", "")),
        )


@dataclass(frozen=True)
class Function:
    """One warm function, as ``zygo ps`` shows it."""

    name: str
    state: str
    image: str = ""
    runtime: str = ""
    rss_kb: int = 0
    imports_ms: float = 0.0
    requests: int = 0
    failures: int = 0
    raw: Dict[str, Any] = field(default_factory=dict)

    @classmethod
    def parse(cls, raw: Dict[str, Any]) -> "Function":
        return cls(
            name=str(raw.get("name", "")),
            state=str(raw.get("state", "")),
            image=str(raw.get("image", "")),
            runtime=str(raw.get("runtime", "")),
            rss_kb=int(raw.get("rss_kb", 0)),
            imports_ms=float(raw.get("imports_ms", 0.0)),
            requests=int(raw.get("requests", 0)),
            failures=int(raw.get("failures", 0)),
            raw=raw,
        )


@dataclass(frozen=True)
class Served:
    """What serving a function did to the name it was served under.

    ``change`` is ``started``, ``replaced`` or ``unchanged``. A deploy tool
    reads it to say "3 replaced, 7 unchanged" rather than printing ten ticks
    that hide which functions actually restarted.
    """

    name: str
    change: str
    runtime: str = ""
    rss_kb: int = 0
    imports_ms: float = 0.0
    warm_ms: float = 0.0
    warnings: List[str] = field(default_factory=list)

    @classmethod
    def parse(cls, raw: Dict[str, Any]) -> "Served":
        return cls(
            name=str(raw.get("name", "")),
            change=str(raw.get("change", "started")),
            runtime=str(raw.get("runtime", "")),
            rss_kb=int(raw.get("rss_kb", 0)),
            imports_ms=float(raw.get("imports_ms", 0.0)),
            warm_ms=float(raw.get("warm_ms", 0.0)),
            warnings=list(raw.get("warnings", [])),
        )


@dataclass(frozen=True)
class Tenant:
    """One customer of whoever embedded Zygo.

    ``scripts`` are the digests registered for this tenant. The bytes are
    shared with any other tenant that registered the same script; the
    reference is not, and it is what deleting a tenant takes with it.
    """

    id: str
    created_ms: int = 0
    scripts: List[str] = field(default_factory=list)

    @classmethod
    def parse(cls, raw: Dict[str, Any]) -> "Tenant":
        return cls(
            id=str(raw.get("id", "")),
            created_ms=int(raw.get("created_ms", 0)),
            scripts=list(raw.get("scripts", [])),
        )


@dataclass(frozen=True)
class Token:
    """One API token, as the server holds it — never the secret.

    ``tenant`` is ``None`` for an operator token, which may create tenants and
    pools and mint more tokens; a tenant token registers scripts and calls, for
    its own tenant only.

    The secret exists once, in the answer to :meth:`~zygo.Client.mint_token`,
    and is not stored anywhere: the server keeps a SHA-256 of it. A client that
    loses one revokes it and mints another.
    """

    id: str
    tenant: Optional[str] = None
    created_ms: int = 0
    revoked_ms: Optional[int] = None

    @property
    def revoked(self) -> bool:
        return self.revoked_ms is not None

    @classmethod
    def parse(cls, raw: Dict[str, Any]) -> "Token":
        revoked = raw.get("revoked_ms")
        return cls(
            id=str(raw.get("id", "")),
            tenant=raw.get("tenant"),
            created_ms=int(raw.get("created_ms", 0)),
            revoked_ms=int(revoked) if revoked is not None else None,
        )


@dataclass(frozen=True)
class Minted:
    """A token and the one copy of its secret.

    Keep ``secret``: nothing can produce it again.
    """

    token: Token
    secret: str

    @classmethod
    def parse(cls, raw: Dict[str, Any]) -> "Minted":
        return cls(
            token=Token.parse(raw.get("token") or {}),
            secret=str(raw.get("secret", "")),
        )


@dataclass(frozen=True)
class Runtime:
    """One runtime pool: several anonymous zygotes any script can run in.

    ``warm`` and ``paused`` are zygotes that exist; ``cold`` is the room left
    between them and ``max_warm``. A pool has no single state — four zygotes of
    which two are frozen is working and idle at once — so the counts are what
    is reported rather than a word.
    """

    name: str
    image: str = ""
    runtime: str = ""
    warm: int = 0
    paused: int = 0
    cold: int = 0
    min_warm: int = 0
    max_warm: int = 0
    in_flight: int = 0
    queued: int = 0
    requests: int = 0
    failures: int = 0
    rss_kb: int = 0
    uptime_s: int = 0
    raw: Dict[str, Any] = field(default_factory=dict)

    @classmethod
    def parse(cls, raw: Dict[str, Any]) -> "Runtime":
        return cls(
            name=str(raw.get("name", "")),
            image=str(raw.get("image", "")),
            runtime=str(raw.get("runtime", "")),
            warm=int(raw.get("warm", 0)),
            paused=int(raw.get("paused", 0)),
            cold=int(raw.get("cold", 0)),
            min_warm=int(raw.get("min_warm", 0)),
            max_warm=int(raw.get("max_warm", 0)),
            in_flight=int(raw.get("in_flight", 0)),
            queued=int(raw.get("queued", 0)),
            requests=int(raw.get("requests", 0)),
            failures=int(raw.get("failures", 0)),
            rss_kb=int(raw.get("rss_kb", 0)),
            uptime_s=int(raw.get("uptime_s", 0)),
            raw=raw,
        )


@dataclass(frozen=True)
class Script:
    """A script the host holds, named by the SHA-256 of its bytes.

    ``existed`` is true when the store already had exactly these bytes, which
    is what deduplication looks like from outside: two tenants registering the
    same script get one file, and the second is told so rather than left to
    assume it.
    """

    sha256: str
    size: int = 0
    existed: bool = False

    @classmethod
    def parse(cls, raw: Dict[str, Any]) -> "Script":
        return cls(
            sha256=str(raw.get("sha256", "")),
            size=int(raw.get("size", 0)),
            existed=bool(raw.get("existed", False)),
        )


@dataclass(frozen=True)
class Run:
    """What a one-shot sandbox said.

    A non-zero ``exit_code`` is not an exception: the sandbox ran, and this is
    what it reported. Only Zygo failing to run it at all raises.

    ``timed_out`` and ``oom_killed`` are why it ended, which the exit code
    cannot carry: a deadline kill and an out-of-memory kill are both
    ``SIGKILL``, so both are 137. The first comes from the launcher, which
    enforced the deadline; the second from the kernel's own counter in the
    sandbox's cgroup. Neither is a guess.
    """

    exit_code: int
    stdout: str = ""
    stderr: str = ""
    timed_out: bool = False
    oom_killed: bool = False
    peak_rss_kb: int = 0
    wall_ms: float = 0.0

    @property
    def ok(self) -> bool:
        return self.exit_code == 0 and not self.timed_out and not self.oom_killed

    @classmethod
    def parse(cls, raw: Dict[str, Any]) -> "Run":
        return cls(
            exit_code=int(raw.get("exit_code", -1)),
            stdout=str(raw.get("stdout", "")),
            stderr=str(raw.get("stderr", "")),
            timed_out=bool(raw.get("timed_out", False)),
            oom_killed=bool(raw.get("oom_killed", False)),
            peak_rss_kb=int(raw.get("peak_rss_kb", 0)),
            wall_ms=float(raw.get("wall_ms", 0.0)),
        )


@dataclass(frozen=True)
class LogEntry:
    """One line of a function's log: the zygote's output, or one request."""

    seq: int
    at_ms: int
    text: str
    raw: Dict[str, Any] = field(default_factory=dict)

    @classmethod
    def parse(cls, raw: Dict[str, Any]) -> "LogEntry":
        return cls(
            seq=int(raw.get("seq", 0)),
            at_ms=int(raw.get("at_ms", 0)),
            text=str(raw.get("text", "")),
            raw=raw,
        )


@dataclass(frozen=True)
class LogPage:
    """A page of log entries, and where to continue from.

    ``next`` is what to pass as ``after`` for the entries that arrive later,
    which is how following a log works without a stream.
    """

    name: str
    entries: List[LogEntry]
    next: int

    @classmethod
    def parse(cls, raw: Dict[str, Any]) -> "LogPage":
        return cls(
            name=str(raw.get("name", "")),
            entries=[LogEntry.parse(e) for e in raw.get("entries", [])],
            next=int(raw.get("next", 0)),
        )
