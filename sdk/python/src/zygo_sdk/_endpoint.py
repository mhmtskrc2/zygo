# SPDX-License-Identifier: Apache-2.0
"""Where the API is, and how it was decided.

One function, because the rule has to be the same for the synchronous and the
asynchronous client. Two discovery orders that drift is a support question
nobody can answer from the traceback.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from typing import Optional

#: What ``zygo api`` listens on when nothing says otherwise.
DEFAULT_URL = "http://127.0.0.1:7700"


@dataclass(frozen=True)
class Endpoint:
    """A parsed address: either a unix socket path or a host and port."""

    url: str
    socket_path: Optional[str] = None
    host: str = "127.0.0.1"
    port: int = 7700
    tls: bool = False

    @property
    def is_unix(self) -> bool:
        return self.socket_path is not None

    def __str__(self) -> str:
        return self.url


def resolve(url: Optional[str] = None) -> Endpoint:
    """Work out which API to talk to.

    In order: the argument, then ``ZYGO_API_URL``, then loopback on the port
    ``zygo api`` uses by default. Accepts ``unix:///path/to.sock``,
    ``http://host:port``, ``https://host:port`` and a bare ``host:port``.
    """
    text = url or os.environ.get("ZYGO_API_URL") or DEFAULT_URL
    return parse(text)


def parse(text: str) -> Endpoint:
    text = text.strip()
    if text.startswith("unix://"):
        path = text[len("unix://") :]
        if not path:
            raise ValueError("unix:// needs a path, e.g. unix:///run/user/1000/zygo/api.sock")
        return Endpoint(url=text, socket_path=path)

    tls = False
    rest = text
    if text.startswith("https://"):
        tls, rest = True, text[len("https://") :]
    elif text.startswith("http://"):
        rest = text[len("http://") :]
    elif "://" in text:
        scheme = text.split("://", 1)[0]
        raise ValueError(f"`{scheme}://` is not an address Zygo serves; use http://, https:// or unix://")

    # A trailing path is dropped rather than honoured: every route this client
    # calls is rooted, and silently prefixing them would turn a typo in the
    # address into a 404 on every call.
    rest = rest.split("/", 1)[0]
    host, _, port_text = rest.rpartition(":")
    if not host:
        host, port_text = rest, "443" if tls else "7700"
    try:
        port = int(port_text)
    except ValueError as e:
        raise ValueError(f"`{port_text}` is not a port number, in `{text}`") from e

    scheme = "https" if tls else "http"
    return Endpoint(url=f"{scheme}://{host}:{port}", host=host, port=port, tls=tls)
