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

#: One runtime for every customer's plugins. The operator declares it; it holds
#: an interpreter and a dependency set and **no code at all**, which is what
#: makes it safe for several customers to share one.
RUNTIME = "plugins"

IMAGE = os.environ.get("ZYGO_IMAGE", "python:3.12-slim")


def tar_of(files: dict[str, bytes]) -> bytes:
    """A tar in memory. A plugin's input arrives as one and leaves as one."""
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as archive:
        for name, body in files.items():
            info = tarfile.TarInfo(name)
            info.size = len(body)
            archive.addfile(info, io.BytesIO(body))
    return buffer.getvalue()


class PluginHost:
    """Everything this host knows how to do, in about a hundred lines."""

    def __init__(self, operator: zygo.Client) -> None:
        self.operator = operator
        self.tokens: dict[str, str] = {}

    # ---- the operator's side -------------------------------------------

    def start(self) -> None:
        """Declare the one runtime every plugin runs in.

        No `base_dir`: this host has no files on the Zygo machine, and a pool
        that named one would be the first thing to break when the API moved to
        another box.
        """
        self.operator.serve_runtime(
            RUNTIME,
            {
                "image": IMAGE,
                "agent": "python",
                "min_warm": 2,
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

    # ---- a customer's side ---------------------------------------------

    def client(self, customer: str) -> zygo.Client:
        """A client acting as that customer, with their own token."""
        return zygo.connect(self.operator.endpoint.url, token=self.tokens[customer])

    def install(self, customer: str, source: str) -> str:
        """Register a plugin's code against its customer, by digest.

        Sent once, called any number of times. The digest is theirs: another
        customer naming it is told the script does not exist.
        """
        with self.client(customer) as client:
            return client.put_script(source).sha256

    def run(self, customer: str, plugin: str, event: dict, files: dict | None = None):
        """Call a plugin, optionally with files in and files out."""
        with self.client(customer) as client:
            if files is None:
                return client.run_script(RUNTIME, plugin, event)
            return client.run_script(
                RUNTIME,
                plugin,
                event,
                workspace={"inline": base64.b64encode(tar_of(files)).decode()},
                out=True,
            )

    def watch(self, customer: str, plugin: str, event: dict, on_output) -> object:
        """Call a plugin and hand its output on as it is produced."""
        with self.client(customer) as client:
            for item in client.stream_script(RUNTIME, plugin, event):
                if item.is_result:
                    return item.result
                on_output(item.kind, item.data)
        return None

    def run_cancellable(self, customer: str, plugin: str, event: dict, key: str):
        """Call a plugin under a name this host can stop it by."""
        with self.client(customer) as client:
            return client.run_script(RUNTIME, plugin, event, key=key)

    def cancel(self, customer: str, key: str) -> dict:
        with self.client(customer) as client:
            return client.cancel(key)
