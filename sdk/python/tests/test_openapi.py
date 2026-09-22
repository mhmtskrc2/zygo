"""Every operation in the OpenAPI document has a client method.

The conformance the roadmap asks for instead of generating the SDKs from the
document. Generating them would produce a client nobody wants to read — the
value of these SDKs is the prose about *why* `?out=1` exists and why `Busy` is
worth retrying, which no generator writes.

What generation would have given for free is the guarantee that nothing is
missing. This is that guarantee, written down: a route added to the API and
documented, with no client method, fails here.

The document comes from the binary. Without one — a checkout that has not been
built — the test skips rather than passes: a check that quietly does nothing is
worse than one that says it did nothing.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))

import zygo  # noqa: E402
import zygo.aio  # noqa: E402

#: Operation to the client method that calls it.
#:
#: Written out rather than derived from a naming rule, because the mapping is
#: not mechanical and should not pretend to be: `POST /fn/{name}` is `call`,
#: not `postFnName`, and that is the whole reason these clients are worth
#: hand-writing.
METHODS = {
    "GET /healthz": "health",
    "GET /version": "version",
    "GET /metrics": None,  # Prometheus text; for a scraper, not a client.
    "GET /fn": "functions",
    "POST /fn/{name}": "call",
    "PUT /fn/{name}": "serve",
    "DELETE /fn/{name}": "stop",
    "POST /fn/{name}/batch": "batch",
    "GET /fn/{name}/stats": "stats",
    "GET /fn/{name}/logs": "logs",
    "POST /fn/{name}/warm": "warm",
    "GET /runtimes": "runtimes",
    "POST /runtimes": "serve_runtime",
    "DELETE /runtimes/{name}": "stop_runtime",
    "POST /runtimes/{name}/call": "run_script",
    "POST /deps": "put_deps",
    "GET /deps": "deps",
    "GET /deps/{id}": "deps",
    "DELETE /deps/{id}": "delete_deps",
    "PUT /scripts": "put_script",
    "GET /scripts/{digest}": "script",
    "DELETE /scripts/{digest}": "delete_script",
    "PUT /blobs": "put_blob",
    "GET /blobs/{digest}": "blob",
    "DELETE /blobs/{digest}": "delete_blob",
    "POST /run": "run",
    "DELETE /requests/{id}": "cancel",
    "POST /drain": "drain",
    "GET /tenants": "tenants",
    "POST /tenants": "create_tenant",
    "GET /tenants/{id}": "tenant",
    "DELETE /tenants/{id}": "delete_tenant",
    "PATCH /tenants/{id}/limits": "set_limits",
    "GET /tenants/{id}/secrets": "secrets",
    "PUT /tenants/{id}/secrets/{name}": "put_secret",
    "DELETE /tenants/{id}/secrets/{name}": "delete_secret",
    "POST /tenants/{id}/tokens": "mint_token",
    "GET /tokens": "tokens",
    "POST /tokens": "mint_token",
    "DELETE /tokens/{id}": "revoke_token",
}


def document():
    """The document this build prints, or `None` if there is no build."""
    override = os.environ.get("ZYGO_OPENAPI")
    if override and Path(override).is_file():
        return json.loads(Path(override).read_text())

    root = Path(__file__).resolve().parents[3]
    candidates = [
        root / "target" / "release" / "zygo",
        root / "target" / "debug" / "zygo",
        Path(shutil.which("zygo") or "/nonexistent"),
    ]
    for binary in candidates:
        if not binary.is_file():
            continue
        try:
            out = subprocess.run(
                [str(binary), "api", "--openapi"],
                capture_output=True,
                text=True,
                timeout=30,
            )
        except (OSError, subprocess.SubprocessError):
            continue
        if out.returncode == 0:
            try:
                return json.loads(out.stdout)
            except ValueError:
                continue
    return None


class OpenApiTests(unittest.TestCase):
    def setUp(self) -> None:
        self.doc = document()
        if self.doc is None:
            self.skipTest(
                "no zygo binary printed a document; `cargo build` first, or set "
                "ZYGO_OPENAPI to one"
            )

    def test_every_operation_has_a_client_method(self) -> None:
        missing = []
        for path, item in self.doc["paths"].items():
            for method in item:
                key = f"{method.upper()} {path}"
                name = METHODS.get(key, "")
                if name is None:
                    continue  # deliberately not a client method
                if not name:
                    missing.append(f"{key} is not in this test's METHODS table")
                elif not hasattr(zygo.Client, name):
                    missing.append(f"{key} maps to `{name}`, which the client lacks")
        self.assertEqual(missing, [], "\n".join(missing))

    def test_the_table_does_not_name_a_route_that_is_gone(self) -> None:
        real = {
            f"{method.upper()} {path}"
            for path, item in self.doc["paths"].items()
            for method in item
        }
        stale = sorted(set(METHODS) - real)
        self.assertEqual(stale, [], "these are in the table and not in the API")

    def test_the_async_client_covers_the_calling_surface(self) -> None:
        """The async client is the *calling* path, not the admin one.

        Deliberately a smaller surface: an embedder calls functions from an
        event loop and administers from a script. This pins which half, so
        that "the async client is missing `create_tenant`" is a decision on
        the record rather than an oversight somebody reports as a bug.
        """
        for name in ["call", "batch", "run_script", "stream", "cancel", "logs"]:
            self.assertTrue(
                hasattr(zygo.aio.AsyncClient, name),
                f"the async client should have `{name}`",
            )
        for name in ["create_tenant", "mint_token", "put_secret", "set_limits"]:
            self.assertFalse(
                hasattr(zygo.aio.AsyncClient, name),
                f"`{name}` appeared on the async client; if that is wanted, "
                "say so here rather than letting the surfaces drift",
            )


if __name__ == "__main__":
    unittest.main()
