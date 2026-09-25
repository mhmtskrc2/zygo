# SPDX-License-Identifier: Apache-2.0
"""A plugin host, built on the Zygo API alone.

The exit criterion for the embedder API (roadmap 2.12), and it is written as a
program rather than a checklist because the question it answers is not "does
each route work" — the suites answer that — but **can somebody build the thing
the API is for without reaching past it**. No `sandbox.toml`, no file on the
Zygo host, no shell on the Zygo host. If this needed one, the API was missing a
route.

What a plugin host is: software with customers who supply code. A CI service, a
notebook backend, a webhook platform, an agent framework with tools. Each
customer's code is theirs, runs under their own limits, and must not be able to
reach anybody else's.

    the host (this file)                       zygo api
    ─────────────────────                      ────────
    onboard a customer      ──────────────►    POST /tenants, POST .../tokens
    set what they may use   ──────────────►    PATCH /tenants/<id>/limits
    give them a key         ──────────────►    PUT  /tenants/<id>/secrets/<n>
    declare one runtime     ──────────────►    POST /runtimes
    install a plugin        ──────────────►    PUT  /scripts        (as them)
    run one                 ──────────────►    POST /runtimes/<r>/call
    watch a long one        ──────────────►    ?stream=1
    stop one                ──────────────►    DELETE /requests/<key>
    bill for it             ◄──────────────    --usage-webhook

Run it against a Zygo whose API is up:

    ZYGO_API_URL=unix:///run/zygo/api.sock ZYGO_API_TOKEN=... python3 host.py
"""

from __future__ import annotations

import base64
import io
import os
import sys
import tarfile

sys.path.insert(0, os.environ.get("ZYGO_SDK", "../../sdk/python/src"))

import zygo  # noqa: E402

#: One runtime per language, and no more than that. Each holds an interpreter
#: and a dependency set and **no code at all**, which is what makes it safe for
#: several customers to share one — and why ten thousand plugins are not ten
#: thousand warm processes.
#:
#: A customer's limits are on the *customer*, not on the runtime, so a plugin
#: is held to the same memory and the same deadline whichever of these it runs
#: in. That is the property this example exists to show: a host adds a language
#: by adding a pool, and nothing about its customers changes.
RUNTIMES = {
    "python": {
        "image": os.environ.get("ZYGO_IMAGE", "python:3.12-slim"),
        "agent": "python",
    },
    "javascript": {
        "image": os.environ.get("ZYGO_NODE_IMAGE", "node:22-slim"),
        "agent": "node",
    },
}

#: What a plugin is written in, when its customer did not say. Python, because
#: that is what most plugin authors reach for.
DEFAULT_LANGUAGE = "python"


def runtime_of(language: str) -> str:
    """The pool a plugin in this language runs in."""
    if language not in RUNTIMES:
        raise ValueError(f"no runtime for {language!r}; there is {', '.join(RUNTIMES)}")
    return f"plugins-{language}"


def tar_of(files: dict[str, bytes]) -> bytes:
    """A tar in memory. A plugin's input arrives as one and leaves as one."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as archive:
        for name, body in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(body)
            archive.addfile(info, io.BytesIO(body))
    return buffer.getvalue()


class Plugin:
    """One customer's plugin: its digest, and what it is written in.

    Two fields, because Zygo only knows about the first. A digest names bytes;
    which pool those bytes can be loaded by is the host's own record, kept in
    the same row of the same table as everything else about the plugin.
    """

    __slots__ = ("digest", "language")

    def __init__(self, digest: str, language: str) -> None:
        self.digest = digest
        self.language = language

    @property
    def runtime(self) -> str:
        return runtime_of(self.language)

    def __repr__(self) -> str:
        return f"Plugin({self.digest[:19]}…, {self.language})"


class PluginHost:
    """Everything this host knows how to do, in about a hundred lines."""

    def __init__(self, operator: zygo.Client) -> None:
        self.operator = operator
        self.tokens: dict[str, str] = {}

    # ---- the operator's side -------------------------------------------

    def start(self) -> None:
        """Declare one runtime per language every plugin may be written in.

        No `base_dir`: this host has no files on the Zygo machine, and a pool
        that named one would be the first thing to break when the API moved to
        another box.

        The limits here are the *pool's* ceiling, and are deliberately
        generous: what holds a plugin down is its customer's own limits, set in
        `onboard`, which narrow these per request. Adding a language is this
        dictionary getting one more entry.
        """
        for language, runtime in RUNTIMES.items():
            self.operator.serve_runtime(
                runtime_of(language),
                {
                    **runtime,
                    "min_warm": 1,
                    "max_warm": 8,
                    "timeout": "120s",
                    "seccomp": "strict",
                },
            )

    def onboard(self, customer: str, *, mem: str, secrets: dict[str, str]) -> str:
        """A new customer: their tenant, their limits, their keys, their token.

        The token is the only thing that leaves this method. Everything the
        customer's code can reach afterwards follows from it — which is the
        property that makes a plugin host possible at all.
        """
        self.operator.create_tenant(customer)
        self.operator.set_limits(customer, mem=mem, pids=64, timeout="60s")
        for name, value in secrets.items():
            self.operator.put_secret(customer, name, value)
        minted = self.operator.mint_token(customer)
        self.tokens[customer] = minted.secret
        return minted.secret

    def offboard(self, customer: str) -> dict:
        """Everything of theirs goes: code, tokens, secrets, running work."""
        return self.operator.delete_tenant(customer)

    def stop(self) -> None:
        """Drop every pool. The reverse of `start`."""
        for language in RUNTIMES:
            self.operator.stop_runtime(runtime_of(language))

    # ---- a customer's side ---------------------------------------------

    def client(self, customer: str) -> zygo.Client:
        """A client acting as that customer, with their own token."""
        return zygo.connect(self.operator.endpoint.url, token=self.tokens[customer])

    def install(
        self, customer: str, source: str, language: str = DEFAULT_LANGUAGE
    ) -> "Plugin":
        """Register a plugin's code against its customer, by digest.

        Sent once, called any number of times. The digest is theirs: another
        customer naming it is told the script does not exist.

        The language is the host's own bookkeeping — Zygo has no opinion about
        which pool a digest belongs in, and would happily hand a Python script
        to the Node pool, where it would fail to load. A real host keeps this
        in the same row as the plugin.
        """
        with self.client(customer) as client:
            return Plugin(client.put_script(source).sha256, language)

    def run(
        self, customer: str, plugin: "Plugin", event: dict, files: dict | None = None
    ):
        """Call a plugin, optionally with files in and files out."""
        with self.client(customer) as client:
            if files is None:
                return client.run_script(plugin.runtime, plugin.digest, event)
            return client.run_script(
                plugin.runtime,
                plugin.digest,
                event,
                workspace={"inline": base64.b64encode(tar_of(files)).decode()},
                out=True,
            )

    def watch(self, customer: str, plugin: "Plugin", event: dict, on_output) -> object:
        """Call a plugin and hand its output on as it is produced."""
        with self.client(customer) as client:
            for item in client.stream_script(plugin.runtime, plugin.digest, event):
                if item.is_result:
                    return item.result
                on_output(item.kind, item.data)
        return None

    def run_cancellable(self, customer: str, plugin: "Plugin", event: dict, key: str):
        """Call a plugin under a name this host can stop it by."""
        with self.client(customer) as client:
            return client.run_script(plugin.runtime, plugin.digest, event, key=key)

    def cancel(self, customer: str, key: str) -> dict:
        with self.client(customer) as client:
            return client.cancel(key)
