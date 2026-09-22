"""What can go wrong, as types a caller can branch on.

The distinctions here are the ones that change what a caller should do next,
and no others. A handler that raised is not the same as a sandbox that ran out
of time, which is not the same as a pool that is full — the first is a bug in
the function, the second is a limit doing its job, and the third is worth
retrying in a moment. Collapsing them into one exception with a message is how
a caller ends up parsing English to decide whether to retry.
"""

from __future__ import annotations

from typing import Any, Dict, Optional


class ZygoError(Exception):
    """Base class, so ``except zygo.ZygoError`` catches everything from here."""


class TransportError(ZygoError):
    """The API could not be reached, or answered with something unreadable.

    Never raised because the *sandbox* failed: that always arrives as one of
    the classes below, with the sandbox's own output attached.
    """


class AuthError(ZygoError):
    """The token was missing, wrong, or not permitted to do this.

    A 403 usually means the API was started without ``--allow-deploy`` and the
    call was one that creates or destroys a sandbox.
    """


class NotFound(ZygoError):
    """No function under that name."""


class Busy(ZygoError):
    """The function is at its concurrency limit and refused the request.

    Not a failure of the call: it is backpressure, and the request never ran.
    Retrying after ``retry_after`` seconds is the intended response.
    """

    def __init__(
        self,
        message: str,
        *,
        in_flight: int = 0,
        queued: int = 0,
        limit: int = 0,
        retry_after: float = 1.0,
    ) -> None:
        super().__init__(message)
        self.in_flight = in_flight
        self.queued = queued
        self.limit = limit
        self.retry_after = retry_after


class Timeout(ZygoError):
    """The request exceeded the function's timeout and was killed.

    Zygo's supervisor records that *it* killed the request, rather than
    inferring it: a deadline kill and an out-of-memory kill both surface as
    exit 137, and only the side that enforced the deadline can tell them apart.
    So this is never a guess.
    """

    def __init__(self, message: str, *, stderr: str = "", metrics: Optional[Dict[str, Any]] = None) -> None:
        super().__init__(message)
        self.stderr = stderr
        self.metrics = metrics or {}


class Stuck(ZygoError):
    """The sandbox stopped reporting this request, and it was killed.

    Deliberately not a :class:`Timeout`. A timeout says the work is too slow or
    the limit is too tight, and both are about numbers you chose. This says the
    sandbox went quiet with budget left — an agent that stopped scheduling, a
    child wedged where no signal it can send will reach it — so the thing to
    look at is the function, not its `timeout`.

    Only reachable for a request long enough to miss a heartbeat. A short one
    that wedges is killed by its own deadline and raises :class:`Timeout`.
    """

    def __init__(
        self,
        message: str,
        *,
        request_id: str = "",
        stdout: str = "",
        stderr: str = "",
        metrics: Optional[Dict[str, Any]] = None,
    ) -> None:
        super().__init__(message)
        self.request_id = request_id
        self.stdout = stdout
        self.stderr = stderr
        self.metrics = metrics or {}


class Cancelled(ZygoError):
    """Somebody stopped this request — usually the caller.

    The third reading of exit 137. A cancel kill, a deadline kill and an
    out-of-memory kill are one signal and three different things to tell a
    caller, and only the side that sent the signal knows which it was: the
    supervisor does, so this is never a guess either.

    Distinct from :class:`Timeout` on purpose. A timeout says the work is too
    slow or the limit is too tight; this says the answer stopped being wanted,
    which needs no action at all.
    """

    def __init__(
        self,
        message: str,
        *,
        request_id: str = "",
        stdout: str = "",
        stderr: str = "",
        metrics: Optional[Dict[str, Any]] = None,
    ) -> None:
        super().__init__(message)
        self.request_id = request_id
        self.stdout = stdout
        self.stderr = stderr
        self.metrics = metrics or {}


class HandlerError(ZygoError):
    """The handler raised. The exception text and both streams are attached."""

    def __init__(
        self,
        message: str,
        *,
        stdout: str = "",
        stderr: str = "",
        exit_code: int = 0,
        metrics: Optional[Dict[str, Any]] = None,
    ) -> None:
        super().__init__(message)
        self.stdout = stdout
        self.stderr = stderr
        self.exit_code = exit_code
        self.metrics = metrics or {}


class SpecError(ZygoError):
    """The sandbox as described could not be resolved.

    A bad image reference, a limit that contradicts another, a mount that has
    to be absolute and is not. Always a problem with the request, never with
    the host.
    """


def from_response(status: int, body: Dict[str, Any], retry_after: float = 1.0) -> ZygoError:
    """Map one HTTP answer onto the exception a caller should see.

    Status first, then the body's ``code`` where the status is ambiguous. The
    fallback carries the status, because an unmapped code is a version skew
    worth reporting rather than swallowing.
    """
    message = str(body.get("error") or body.get("message") or f"HTTP {status}")
    if status in (401, 403):
        return AuthError(message)
    if status == 404:
        return NotFound(message)
    if status == 408:
        return Timeout(message, stderr=str(body.get("stderr", "")), metrics=body.get("metrics"))
    # 499 is nginx's for a client that went away, and the nearest thing to a
    # registered code for a request the caller stopped.
    if status == 504 or body.get("stuck") is True:
        return Stuck(
            message,
            request_id=str(body.get("request_id", "")),
            stdout=str(body.get("stdout", "")),
            stderr=str(body.get("stderr", "")),
            metrics=body.get("metrics"),
        )
    if status == 499 or body.get("cancelled") is True:
        return Cancelled(
            message,
            request_id=str(body.get("request_id", "")),
            stdout=str(body.get("stdout", "")),
            stderr=str(body.get("stderr", "")),
            metrics=body.get("metrics"),
        )
    if status == 429:
        return Busy(
            message,
            in_flight=int(body.get("in_flight", 0)),
            queued=int(body.get("queued", 0)),
            limit=int(body.get("limit", 0)),
            retry_after=retry_after,
        )
    if status == 400:
        return SpecError(message)
    if status == 500 and "exit_code" in body:
        return HandlerError(
            message,
            stdout=str(body.get("stdout", "")),
            stderr=str(body.get("stderr", "")),
            exit_code=int(body.get("exit_code", 1)),
            metrics=body.get("metrics"),
        )
    return ZygoError(f"{message} (HTTP {status})")
