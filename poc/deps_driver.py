#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""A dependency set, built for real, and then used by a pool (todo 3.3).

Driven by the client that ships with Zygo, against a real API on a unix
socket. What it checks, in order:

1. `POST /deps` answers **before** the build finishes. A `pip install` is
   minutes; an API that waited would time out in every proxy between here and
   the caller.
2. A pool named against a dependency set that is still building is refused
   with `503` and a `Retry-After`, and **starts nothing** — a zygote warmed
   without the dependencies it was promised serves requests that fail at
   import, which is worse than not serving them.
3. The build finishes, and a script in that pool can import what was in the
   lockfile — which is the only check that proves the whole path rather than
   the bookkeeping around it.
4. A lockfile naming a package that does not exist fails with the resolver's
   own words in the log, rather than silently producing an empty set.
5. `DELETE /deps/<id>` is refused while a pool is built on it, and works once
   the pool is stopped.

Needs `passt` and `nftables` on the host and a route to PyPI: the build
sandbox has `network = "egress"` with the registries allowed and nothing else.
"""

from __future__ import annotations

import os
import sys
import time

sys.path.insert(0, "/src/sdk/python/src")

import zygo_sdk as zygo  # noqa: E402

SOCK = os.environ.get("SOCK", "/tmp/api.sock")
IMAGE = os.environ.get("IMAGE", "python:3.12-slim")

PASSED = 0
FAILED = 0

#: A real manifest and a real lockfile, for a package with no dependencies of
#: its own and nothing to compile: what is under test is the path, not npm.
NODE_MANIFEST = """{
  "dependencies": {
    "is-number": "^7.0.0"
  }
}
"""

NODE_LOCKFILE = """{
  "name": "zygo-deps-probe",
  "lockfileVersion": 3,
  "requires": true,
  "packages": {
    "": {
      "dependencies": {
        "is-number": "^7.0.0"
      }
    },
    "node_modules/is-number": {
      "version": "7.0.0",
      "resolved": "https://registry.npmjs.org/is-number/-/is-number-7.0.0.tgz",
      "integrity": "sha512-41Cifkg6e8TylSpdtTpeLVMqvSBEVzTttHvERD741+pnZ8ANv0004MRL43QKPDlK9cGvNp6NZWZUBlbGXYxxng==",
      "license": "MIT",
      "engines": {
        "node": ">=0.12.0"
      }
    }
  }
}
"""


def ok(what: str) -> None:
    global PASSED
    PASSED += 1
    print(f"  PASS  {what}")


def bad(what: str, detail: object = "") -> None:
    global FAILED
    FAILED += 1
    print(f"  FAIL  {what}" + (f" — {detail}" if detail else ""))


def wait_for(client: "zygo.Client", deps_id: str, seconds: int = 420) -> "zygo.Deps":
    """Poll until the build is over, printing nothing but the wait."""
    started = time.monotonic()
    while time.monotonic() - started < seconds:
        deps = client.deps(deps_id)
        if not deps.building:
            return deps
        time.sleep(2)
    return client.deps(deps_id)


def main() -> int:
    with zygo.connect(f"unix://{SOCK}") as client:
        # A package with no compiled parts and no dependencies of its own, so
        # the build is a download rather than a toolchain hunt — what is under
        # test is the path, not pip.
        lockfile = "six==1.16.0\n"
        started = time.monotonic()
        deps = client.put_deps(IMAGE, {"requirements.txt": lockfile})
        answered_ms = (time.monotonic() - started) * 1000

        if deps.building and answered_ms < 2000:
            ok(f"POST /deps answers `building` in {answered_ms:.0f} ms, not when it is built")
        else:
            bad("POST /deps did not answer immediately", f"{deps.state} after {answered_ms:.0f} ms")

        if deps.id.startswith("deps_"):
            ok(f"and names it: {deps.id}")
        else:
            bad("no id came back", deps)

        again = client.put_deps(IMAGE, {"requirements.txt": lockfile})
        if again.id == deps.id:
            ok("the same lockfile against the same image is the same id")
        else:
            bad("identical input built twice", f"{deps.id} then {again.id}")

        # 2. A pool that names it now.
        try:
            client.serve_runtime(
                "early", {"image": IMAGE, "agent": "python", "min_warm": 1}, deps=deps.id
            )
            bad("a pool was started against a dependency set that was still building")
        except zygo.ZygoError as e:
            if "building" in str(e):
                ok("a pool on a dependency set that is still building is refused, not queued")
            else:
                bad("the refusal was not about the build", e)
        if not any(r.name == "early" for r in client.runtimes()):
            ok("and nothing was started: there is no half-warmed pool to clean up")
        else:
            bad("a pool exists after the refusal")

        # 3. The build itself.
        built = wait_for(client, deps.id)
        if built.ready:
            seconds = (time.monotonic() - started)
            ok(f"the build finished: {built.state} after {seconds:.0f}s")
        else:
            bad("the build did not succeed", f"{built.state}: {built.error}\n{built.log[-800:]}")
            print(f"\n  {PASSED} passed, {FAILED} failed")
            return 1

        client.serve_runtime(
            "py-deps", {"image": IMAGE, "agent": "python", "min_warm": 1}, deps=deps.id
        )
        ok("and a pool starts on it")

        out = client.run_script(
            "py-deps",
            "import six\n\n\ndef handler(event):\n"
            "    return {'six': six.__version__, 'path': six.__file__}\n",
            {},
        )
        if out.result.get("six") == "1.16.0":
            ok(f"a script in the pool imports what the lockfile named ({out.result['path']})")
        else:
            bad("the dependency was not importable", out.result)

        # The mount is read only: one build is shared by every sandbox that
        # names it, including other tenants'.
        out = client.run_script(
            "py-deps",
            "import os\n\n\ndef handler(event):\n"
            "    try:\n"
            "        open('/venv/zygo-probe', 'w').write('x')\n"
            "        return {'wrote': True}\n"
            "    except OSError as e:\n"
            "        return {'wrote': False, 'errno': e.errno}\n",
            {},
        )
        if out.result.get("wrote") is False:
            ok(f"and cannot write into it (errno {out.result.get('errno')})")
        else:
            bad("a tenant wrote into a shared dependency set", out.result)

        # 5. Deleting it under the pool.
        try:
            client.delete_deps(deps.id)
            bad("a dependency set was deleted out from under a running pool")
        except zygo.ZygoError as e:
            if "py-deps" in str(e):
                ok("deleting it while a pool is built on it is refused, naming the pool")
            else:
                bad("the refusal did not name the pool", e)

        client.stop_runtime("py-deps")
        if client.delete_deps(deps.id):
            ok("and works once the pool is stopped")
        try:
            client.deps(deps.id)
            bad("it is still there after being deleted")
        except zygo.NotFound:
            ok("after which the host does not have it")

        # The other language. Same mechanism, three different lines: which
        # files are accepted, what builds them, and what environment the
        # result needs — and the last of those is the one worth checking,
        # because a pool's scripts load from `/run/script/<digest>` and Node's
        # directory walk from there finds no `node_modules` anywhere.
        node_image = os.environ.get("NODE_IMAGE", "node:22-slim")
        node_deps = client.put_deps(
            node_image,
            {"package.json": NODE_MANIFEST, "package-lock.json": NODE_LOCKFILE},
        )
        if node_deps.kind == "node":
            ok("a package.json and its lockfile are a node dependency set")
        else:
            bad("the language was not inferred from the files", node_deps.kind)

        built = wait_for(client, node_deps.id)
        if built.ready:
            ok("npm ci builds it")
        else:
            bad("the node build failed", f"{built.error}\n{built.log[-800:]}")

        if built.ready:
            client.serve_runtime(
                "node-deps",
                {"image": node_image, "agent": "node", "min_warm": 1},
                deps=node_deps.id,
            )
            out = client.run_script(
                "node-deps",
                "const isNumber = require('is-number');\n"
                "module.exports = function handler(event) {\n"
                "  return { yes: isNumber(7), no: isNumber('seven'),\n"
                "           from: require.resolve('is-number') };\n"
                "};\n",
                {},
            )
            if out.result.get("yes") is True and out.result.get("no") is False:
                ok(f"and a script requires it through NODE_PATH ({out.result.get('from')})")
            else:
                bad("the dependency was not requirable", out.result)
            client.stop_runtime("node-deps")

        # A package.json with no lockfile: `npm ci` needs one, and falling back
        # to `npm install` would install whatever is newest rather than what
        # the caller tested.
        try:
            client.put_deps(node_image, {"package.json": NODE_MANIFEST})
            bad("a package.json with no lockfile was accepted")
        except zygo.ZygoError as e:
            if "lockfile" in str(e):
                ok("a package.json with no lockfile is refused, saying why")
            else:
                bad("the refusal did not mention the lockfile", e)

        # 4. A build that fails says why.
        broken = client.put_deps(
            IMAGE, {"requirements.txt": "zygo-no-such-package-9e2f1a==1.0\n"}
        )
        failed = wait_for(client, broken.id)
        if failed.state == "failed":
            ok("a lockfile that cannot resolve ends as `failed`")
        else:
            bad("a broken lockfile did not fail", failed.state)
        if "zygo-no-such-package" in failed.log or "zygo-no-such-package" in (failed.error or ""):
            ok("and the log carries the resolver's own words")
        else:
            bad("the failure said nothing useful", f"{failed.error}\n{failed.log[-800:]}")

    print(f"\n  {PASSED} passed, {FAILED} failed")
    return 1 if FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
