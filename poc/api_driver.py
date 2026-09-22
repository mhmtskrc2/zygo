#!/usr/bin/env python3
"""The HTTP API and the Python client, against a real kernel.

The unit tests on either side of this are thorough and neither can catch what
it does. `cmd/api.rs`'s tests check the routing and the ceilings with no
supervisor behind them; `sdk/python`'s check the client against a stand-in that
runs no sandbox. What is untested until here is the whole line:

    the shipped client → HTTP over a unix socket → zygo api
        → the control socket → the supervisor → a sandbox with a kernel in it

It uses the **real** client from `sdk/python`, not a curl script, because the
client is half of what is under test — and because a scenario written in the
API a user actually holds is the one that notices when that API is awkward.

Run:  make verify-api-linux
"""

from __future__ import annotations

import json
import os
import sys
import time

PASS = 0
FAIL = 0


def ok(what: str) -> None:
    global PASS
    PASS += 1
    print(f"  PASS  {what}", flush=True)


def bad(what: str, detail: object = "") -> None:
    global FAIL
    FAIL += 1
    print(f"  FAIL  {what}", flush=True)
    if detail:
        for line in str(detail).splitlines()[:6]:
            print(f"          {line}", flush=True)


def main() -> int:
    socket_path = sys.argv[1]
    workspace = sys.argv[2]
    image = sys.argv[3] if len(sys.argv) > 3 else "python:3.12-slim"

    import zygo

    url = f"unix://{socket_path}"
    print("the HTTP API, through the client that ships with it")
    print(f"  {url}")

    with zygo.connect(url, token=None, timeout=600) as client:
        # Rule 4: prove the thing is reachable before asserting about what it
        # refuses. A suite that cannot reach the API would otherwise report
        # every refusal below as a success.
        try:
            version = client.version()
        except zygo.ZygoError as e:
            print(f"\n  the API is not reachable, so nothing below would mean anything:\n  {e}")
            return 1
        if version.get("api") and version.get("deploy") is True:
            ok(f"the API answers: zygo {version['version']}, surface {version['api']}, deploy on")
        else:
            bad("GET /version", json.dumps(version))

        health = client.health()
        ok("healthz answers without a token") if health.get("ok") else bad("healthz", health)

        # --- one-shot runs -------------------------------------------------
        print("\none-shot runs")

        run = client.run(
            image,
            ["python3", "-c", "import sys; print(sys.stdin.read().upper())"],
            stdin="hello from the model",
            mem="64M",
            timeout="10s",
        )
        if run.ok and run.stdout.strip() == "HELLO FROM THE MODEL":
            ok(f"a sandbox ran, read stdin and answered ({run.wall_ms:.0f} ms)")
        else:
            bad("the one-shot run", run)

        # Zygo's own progress output must not be in the program's stderr: the
        # child is spawned with `--quiet` for exactly this, and a pull line in
        # `stderr` is what a caller would read as the program's.
        if run.stderr == "":
            ok("Zygo's own output is not mixed into the program's")
        else:
            bad("something other than the program wrote to stderr", run.stderr[:200])

        # The two kills are both exit 137. This is the whole point of the
        # fields, so it is attempted rather than asserted about.
        # A sandbox killed by the `timeout` its caller asked for is a *result*,
        # not an exception: it ran, and this is what happened to it. Answering
        # 408 made the client raise instead — which contradicts the rule that a
        # non-zero exit is not an exception, and hid the fields that say why.
        try:
            slow = client.run(image, ["sleep", "30"], timeout="2s")
        except zygo.Timeout as e:
            bad("a sandbox that hit its own timeout was raised as an API timeout", e)
            slow = None
        if slow is not None:
            if slow.timed_out and not slow.oom_killed and slow.exit_code == 137:
                ok(f"a run over its time limit comes back with timed_out set ({slow.wall_ms:.0f} ms)")
            else:
                bad("the timeout was not reported as one", slow)

        starved = client.run(
            image,
            ["python3", "-c", "b=bytearray()\nfor _ in range(400): b += bytearray(1024*1024)"],
            mem="64M",
            timeout="60s",
        )
        if starved.oom_killed and not starved.timed_out:
            ok(f"a run over its memory limit reports oom_killed (peak {starved.peak_rss_kb} kB)")
        else:
            bad("running out of memory was not distinguished from running out of time", starved)

        # And the positive case for both, so neither above can pass by a
        # sandbox that never ran.
        quick = client.run(image, ["python3", "-c", "print(6*7)"], mem="64M", timeout="10s")
        if quick.ok and not quick.timed_out and not quick.oom_killed:
            ok("an ordinary run reports neither")
        else:
            bad("an ordinary run was reported as killed", quick)

        failing = client.run(image, ["python3", "-c", "import sys; sys.exit(3)"], timeout="10s")
        if failing.exit_code == 3 and not failing.ok:
            ok("a non-zero exit comes back as a result, not an error")
        else:
            bad("a failing program", failing)

        # A relative mount source has no directory to be relative to on the
        # host that receives it, and is refused where the mistake is.
        try:
            client.run(image, ["true"], mounts=["./here:/there:ro"])
            bad("a relative mount source was accepted")
        except zygo.SpecError as e:
            ok("a relative mount source is refused with the reason")
            if "absolute" not in str(e):
                bad("the refusal does not say why", e)

        # --- serving over the API ------------------------------------------
        print("\nserving, calling and stopping over the API")

        handler = os.path.join(workspace, "h.py")
        with open(handler, "w") as f:
            f.write(
                "import os\n\n\n"
                "def handler(event):\n"
                "    if event.get('boom'):\n"
                "        raise ValueError('as asked')\n"
                "    return {'pid': os.getpid(), 'n': event.get('n', 0) + 1}\n"
            )

        served = client.serve(
            "api-fn",
            {"image": image, "entry": "h.py", "concurrency": 2, "timeout": "20s"},
            base_dir=workspace,
        )
        if served.change == "started":
            ok(f"a function was served over the API ({served.warm_ms:.0f} ms warm)")
        else:
            bad("serve", served)

        out = client.call("api-fn", {"n": 1})
        if out.result.get("n") == 2:
            ok("and answers a call")
        else:
            bad("the call", out)

        # A fresh process per request is the product's central claim.
        first = client.call("api-fn", {}).result["pid"]
        second = client.call("api-fn", {}).result["pid"]
        if first != second:
            ok(f"each request is a fresh process ({first} then {second})")
        else:
            bad("two requests shared a process", f"{first} == {second}")

        names = [f.name for f in client.functions()]
        ok("it is listed") if "api-fn" in names else bad("GET /fn", names)

        try:
            client.call("api-fn", {"boom": True})
            bad("a raising handler was reported as success")
        except zygo.HandlerError as e:
            if "ValueError" in str(e) and "as asked" in e.stderr + str(e):
                ok("a handler that raised comes back as HandlerError with its traceback")
            else:
                ok("a handler that raised comes back as HandlerError")

        answers = client.batch("api-fn", [{"n": 1}, {"n": 2}, {"n": 3}])
        values = [a.result.get("n") for a in answers if isinstance(a, zygo.Result)]
        if values == [2, 3, 4]:
            ok("a batch answers every element, in order")
        else:
            bad("the batch", answers)

        page = client.logs("api-fn", limit=10)
        if page.entries:
            ok(f"the log carries {len(page.entries)} entries and a cursor ({page.next})")
        else:
            bad("GET /fn/<name>/logs returned nothing after several requests")

        failed_only = client.logs("api-fn", limit=10, failed=True)
        if failed_only.entries and len(failed_only.entries) < len(page.entries):
            ok("and `failed` narrows it to the requests that failed")
        else:
            bad("the failed filter", f"{len(failed_only.entries)} of {len(page.entries)}")

        stopped = client.stop("api-fn")
        ok("stopping it over the API works") if stopped == ["api-fn"] else bad("stop", stopped)

        try:
            client.stop("api-fn")
            bad("stopping a function that is gone was reported as success")
        except zygo.NotFound:
            ok("and stopping it twice is a NotFound, not a silent success")

        # --- the script store ------------------------------------------------
        print("\nthe script store")

        source = "def handler(event):\n    return {'from': 'the-store'}\n"
        registered = client.put_script(source)
        if registered.sha256.startswith("sha256:") and registered.existed is False:
            ok(f"a script registers and is named by its bytes ({registered.sha256[:20]}…)")
        else:
            bad("PUT /scripts", registered)

        # Deduplication, which is what makes a content-addressed store safe to
        # share: a second tenant registering the same bytes gets the same file,
        # not a second one — and cannot put different bytes under that name.
        again = client.put_script(source)
        if again.sha256 == registered.sha256 and again.existed is True:
            ok("the same script from a second caller is the same file, and says so")
        else:
            bad("the second registration", again)

        other = client.put_script(source.replace("the-store", "somewhere-else"))
        if other.sha256 != registered.sha256:
            ok("different bytes are a different name")
        else:
            bad("two different scripts collided", other)

        found = client.script(registered.sha256)
        if found.size == len(source):
            ok(f"and it can be looked up by name ({found.size} bytes)")
        else:
            bad("GET /scripts/<hash>", found)

        try:
            client.script("sha256:" + "b" * 64)
            bad("a digest nobody registered was found")
        except zygo.NotFound:
            ok("a digest nobody registered is a NotFound")

        if client.delete_script(registered.sha256):
            ok("a script can be forgotten")
        else:
            bad("DELETE /scripts/<hash>")
        try:
            client.script(registered.sha256)
            bad("a deleted script is still there")
        except zygo.NotFound:
            ok("and is gone afterwards")
        client.delete_script(other.sha256)

        # --- runtime pools ---------------------------------------------------
        #
        # The embedder's whole path, and the reason the rest of this exists:
        # one warm pool, scripts that live in the caller's database, and no
        # file on the Zygo host.
        print("\nruntime pools")

        pool = client.serve_runtime(
            "py-pool",
            {"image": image, "agent": "python", "min_warm": 1, "max_warm": 2, "timeout": "20s"},
        )
        if pool.get("warm", 0) >= 1:
            ok(f"a runtime pool is warm ({pool['warm']} zygote, {pool.get('warm_ms', 0):.0f} ms)")
        else:
            bad("POST /runtimes", pool)

        pools = {p.name: p for p in client.runtimes()}
        if "py-pool" in pools and pools["py-pool"].max_warm == 2:
            ok("and is listed with its floor and ceiling")
        else:
            bad("GET /runtimes", list(pools))

        registered = client.put_script(
            "import os\n\n\n"
            "def handler(event):\n"
            "    return {'pid': os.getpid(), 'n': event.get('n', 0) + 1}\n"
        )
        out = client.run_script("py-pool", registered.sha256, {"n": 1})
        if out.result.get("n") == 2:
            ok("a registered script runs in the pool, named by its digest alone")
        else:
            bad("POST /runtimes/<name>/call", out)

        # The property the whole design rests on: the zygote is anonymous, so
        # two scripts in one pool cannot see each other — and each request is
        # its own process.
        first = client.run_script("py-pool", registered.sha256, {}).result["pid"]
        second = client.run_script("py-pool", registered.sha256, {}).result["pid"]
        if first != second:
            ok(f"each script request is a fresh process ({first} then {second})")
        else:
            bad("two requests in the pool shared a process", f"{first} == {second}")

        leak = client.run_script(
            "py-pool",
            "def handler(event):\n"
            "    import sys\n"
            "    return {'mods': [m for m in sys.modules if 'zygo_request' in m]}\n",
        )
        if leak.result.get("mods") == []:
            ok("and the zygote carries nothing of the last script")
        else:
            bad("a script was left in the zygote", leak.result)

        # How the script got in, asked of the script itself. The supervisor
        # writes it into the sandbox and sends a path, so that the zygote —
        # which is shared — never holds a tenant's code; and the directory is
        # 0711, so a script cannot list what else is in flight beside it.
        delivery = client.run_script(
            "py-pool",
            "import os\n\n\n"
            "def handler(event):\n"
            "    try:\n"
            "        listed = sorted(os.listdir('/run/script'))\n"
            "    except OSError as e:\n"
            "        listed = str(e.__class__.__name__)\n"
            "    return {'file': __file__, 'listed': listed}\n",
        )
        if delivery.result.get("file", "").startswith("/run/script/"):
            ok(f"the script reached the child as a file ({delivery.result['file'][:28]}…)")
        else:
            bad("the script was sent through the zygote instead of written in", delivery.result)
        if delivery.result.get("listed") == "PermissionError":
            ok("and the directory it is in cannot be listed by the script")
        else:
            bad("/run/script is listable from inside", delivery.result.get("listed"))

        one_off = client.run_script(
            "py-pool", "def handler(event):\n    return {'from': 'a one-off'}\n"
        )
        if one_off.result.get("from") == "a one-off":
            ok("a script that was never registered runs too")
        else:
            bad("an inline script", one_off)

        named = client.run_script(
            "py-pool",
            "def main(event):\n    return {'called': 'main'}\n",
            entry_point="main",
        )
        if named.result.get("called") == "main":
            ok("and `entry_point` names the function to call")
        else:
            bad("entry_point", named)

        try:
            client.run_script("py-pool", "sha256:" + "e" * 64)
            bad("a digest nobody registered ran anyway")
        except zygo.NotFound:
            ok("a digest the host does not hold is a NotFound")

        try:
            client.run_script("nowhere", registered.sha256)
            bad("a call to a pool that does not exist succeeded")
        except zygo.NotFound:
            ok("a call to an unknown runtime is a NotFound")

        if client.stop_runtime("py-pool") == ["py-pool"]:
            ok("a pool can be stopped")
        else:
            bad("DELETE /runtimes/<name>")
        client.delete_script(registered.sha256)

        # --- tenants ---------------------------------------------------------
        #
        # The embedder's customers. What matters is that a tenant's things are
        # theirs: their scripts, their cgroup, and nobody else's reachable.
        print("\ntenants")

        acme = client.create_tenant("acme")
        other = client.create_tenant("globex")
        if acme.id == "acme" and {t.id for t in client.tenants()} >= {"acme", "globex"}:
            ok("two tenants are registered and listed")
        else:
            bad("POST /tenants", acme)

        if client.create_tenant("acme").id == "acme":
            ok("creating one twice is the same tenant, not an error")
        else:
            bad("the second create")

        try:
            client.create_tenant("../escape")
            bad("a tenant id that is a path was accepted")
        except zygo.SpecError:
            ok("an id that would escape its directory is refused")

        for_acme = client.for_tenant("acme")
        for_globex = client.for_tenant("globex")
        theirs = for_acme.put_script(
            "def handler(event):\n    return {'from': 'acme'}\n"
        )
        if client.tenant("acme").scripts == [theirs.sha256]:
            ok("a script registered for a tenant is recorded against them")
        else:
            bad("GET /tenants/<id>", client.tenant("acme"))

        pool = client.serve_runtime(
            "shared", {"image": image, "agent": "python", "timeout": "20s"}
        )
        if pool.get("warm", 0) >= 1:
            ok("an operator's pool is warm, and both tenants may call it")
        else:
            bad("POST /runtimes", pool)

        out = for_acme.run_script("shared", theirs.sha256, {})
        if out.result.get("from") == "acme":
            ok("the tenant that registered a script can run it")
        else:
            bad("the owner's own script", out)

        # The digest is not a capability: knowing one must not be enough.
        try:
            for_globex.run_script("shared", theirs.sha256, {})
            bad("a tenant ran another tenant's script by naming its digest")
        except zygo.NotFound:
            ok("another tenant naming that digest is refused as `not found`")

        # And a tenant may not look at the tenant list at all.
        try:
            for_globex.tenants()
            bad("a tenant listed the other tenants")
        except zygo.AuthError:
            ok("a tenant cannot enumerate the tenants")

        removed = client.delete_tenant("acme")
        if removed.get("removed_scripts") == [theirs.sha256]:
            ok("deleting a tenant removes the scripts only it had")
        else:
            bad("DELETE /tenants/<id>", removed)
        try:
            client.script(theirs.sha256)
            bad("the deleted tenant's script is still in the store")
        except zygo.NotFound:
            ok("and the script really is gone from the store")
        client.delete_tenant("globex")
        client.stop_runtime("shared")

        # --- the ceilings ----------------------------------------------------
        print("\nceilings")

        try:
            client.call("nothing-here", {})
            bad("a call to a function that does not exist succeeded")
        except zygo.NotFound:
            ok("a call to an unknown function is a NotFound")

        try:
            client.call("api-fn", {}, timeout=99_999)
            bad("a timeout past the ceiling was accepted")
        except zygo.SpecError as e:
            ok("a timeout past the ceiling is refused, and the message names it") if "ceiling" in str(e) else bad(
                "the refusal does not name the ceiling", e
            )
        except zygo.NotFound:
            bad("the timeout ceiling was not checked before the route")

    return 1 if FAIL else 0


def call_only(socket_path: str) -> int:
    """The same API without `--allow-deploy`: the deploy routes, all refused."""
    import zygo

    global FAIL
    print("\nwithout --allow-deploy")
    with zygo.connect(f"unix://{socket_path}", token=None, timeout=60) as client:
        version = client.version()
        if version.get("deploy") is False:
            ok("the API reports that deploy is off")
        else:
            bad("GET /version", json.dumps(version))

        # Still a working API: the refusals below mean nothing if it is simply
        # down.
        if client.health().get("ok"):
            ok("and it is otherwise working")
        else:
            bad("healthz on a call-only API")

        for what, call in (
            ("POST /run", lambda: client.run("alpine:3", ["true"])),
            ("PUT /fn/<name>", lambda: client.serve("x", {"image": "alpine:3", "cmd": ["true"]}, base_dir="/tmp")),
            ("DELETE /fn/<name>", lambda: client.stop("x")),
            ("PUT /scripts", lambda: client.put_script("x = 1\n")),
            ("DELETE /scripts/<hash>", lambda: client.delete_script("sha256:" + "c" * 64)),
            (
                "POST /runtimes",
                lambda: client.serve_runtime("x", {"image": "alpine:3", "agent": "python"}),
            ),
            ("DELETE /runtimes/<name>", lambda: client.stop_runtime("x")),
        ):
            try:
                call()
                bad(f"{what} was allowed on a call-only API")
            except zygo.AuthError as e:
                if "--allow-deploy" in str(e):
                    ok(f"{what} is refused, and the message names the flag")
                else:
                    bad(f"{what} is refused without saying how to allow it", e)
            except zygo.ZygoError as e:
                bad(f"{what} failed with the wrong kind of error", f"{type(e).__name__}: {e}")
    return 0


if __name__ == "__main__":
    if sys.argv[1] == "--call-only":
        status = call_only(sys.argv[2])
    else:
        status = main()
    print(f"\n  {PASS} passed, {FAIL} failed")
    sys.exit(1 if FAIL else status)
