#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
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

        # --- dependency sets --------------------------------------------------
        #
        # The build itself needs `passt`, `nftables` and a route to the
        # registries, which this harness deliberately does not have; the
        # working path is `make verify-deps-linux`. What is checked here is
        # everything around it, and one thing that matters more than the rest:
        # **a host that cannot restrict the build refuses it** rather than
        # falling back to host networking. The lockfile came from whoever holds
        # a token and installing a package runs that package's code.
        print("\ndependency sets")

        try:
            client.put_deps("ghcr.io/zygo/never-pulled:1", {"requirements.txt": "six\n"})
            bad("POST /deps with an image this host does not have")
        except zygo.ZygoError as e:
            if "pull it first" in str(e):
                ok("a dependency set names an image, and it has to be one this host has")
            else:
                bad("the refusal did not say what to do", e)

        try:
            client.put_deps(image, {"Gemfile": "source 'https://rubygems.org'\n"})
            bad("POST /deps with files that are neither a requirements.txt nor a package.json")
        except zygo.ZygoError as e:
            if "Gemfile" in str(e):
                ok("and files that are neither language are refused by name")
            else:
                bad("the refusal did not name the file", e)

        deps = client.put_deps(image, {"requirements.txt": "six==1.16.0\n"})
        if deps.building:
            ok(f"POST /deps answers `building` and names it ({deps.id})")
        else:
            bad("POST /deps did not answer `building`", deps.state)

        if any(d.id == deps.id for d in client.deps()):
            ok("GET /deps lists it")
        else:
            bad("GET /deps did not list the dependency set just created")

        waited = 0.0
        while client.deps(deps.id).building and waited < 60:
            time.sleep(0.5)
            waited += 0.5
        after = client.deps(deps.id)
        if after.state == "failed" and ("passt" in (after.error or "") or "nft" in (after.error or "")):
            ok("and on a host that cannot restrict the build, the build is refused, not run")
        elif after.state == "ready":
            ok("and on a host that can, it builds (this host has egress)")
        else:
            bad("the build ended somewhere else", f"{after.state}: {after.error}")

        if client.delete_deps(deps.id):
            ok("DELETE /deps/<id> forgets one nothing is built on")
        else:
            bad("DELETE /deps/<id>")

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

        # --- a warm-exec pool --------------------------------------------------
        #
        # The other pool shape (todo 3.4). No agent and no protocol: the
        # sandbox is held, the script is written into it, and each request is
        # `cmd` plus that path with the event on stdin. For a runtime that
        # starts in under a millisecond there is nothing for an agent to
        # amortise, and `sh` is the case that proves it — there is no zygote
        # to speak of.
        print("\nwarm-exec pools")

        pool = client.serve_runtime(
            "sh-pool", {"image": "alpine:3", "cmd": ["/bin/sh"], "timeout": "20s"}
        )
        if pool.get("warm", 0) >= 1:
            ok(f"a pool with a `cmd` and no agent warms ({pool.get('warm_ms', 0):.0f} ms)")
        else:
            bad("POST /runtimes with a cmd", pool)

        shell = client.put_script(
            'read -r event\n'
            'n=$(echo "$event" | tr -cd "0-9")\n'
            'printf \'{"shell":"%s","n":%s}\\n\' "$0" "${n:-0}"\n'
        )
        out = client.run_script("sh-pool", shell.sha256, {"n": 41})
        if out.result.get("n") == 41:
            ok(f"a shell script runs with the event on stdin ({out.result.get('shell')})")
        else:
            bad("POST /runtimes/sh-pool/call", f"{out.result} {out.stderr}")

        if str(out.result.get("shell", "")).startswith("/run/script/"):
            ok("and the script is a file in the sandbox, named on the command line")
        else:
            bad("the script did not arrive as a path", out.result)

        # Two requests, two processes — the same claim an agent pool makes,
        # and here it is structural: every request is an `execve`.
        first = client.run_script(
            "sh-pool", 'read -r _\nprintf \'{"pid":%s}\\n\' "$$"\n'
        ).result.get("pid")
        second = client.run_script(
            "sh-pool", 'read -r _\nprintf \'{"pid":%s}\\n\' "$$"\n'
        ).result.get("pid")
        if first and second and first != second:
            ok(f"each request is a fresh process ({first} then {second})")
        else:
            bad("two warm-exec requests shared a process", f"{first} and {second}")

        # A non-zero exit is a failed request, and stderr comes back — which
        # is the whole error-handling story for a program with no protocol.
        try:
            client.run_script(
                "sh-pool",
                'read -r _\necho "it went wrong" >&2\nexit 3\n',
            )
            bad("a script that exited non-zero was reported as a success")
        except zygo.HandlerError as e:
            if "it went wrong" in str(e) or "3" in str(e):
                ok("a non-zero exit is a failed request, carrying its stderr")
            else:
                bad("the failure said nothing useful", e)

        client.stop_runtime("sh-pool")

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

        # --- cancelling ------------------------------------------------------
        #
        # A request that has started is stopped from outside the sandbox. What
        # is checked here is the *answer*: the caller of the cancelled call
        # learns that it was cancelled rather than reading a 137 and guessing
        # between a deadline and an out-of-memory kill.
        print("\ncancel")

        import threading

        pool = client.serve_runtime(
            "slow", {"image": image, "agent": "python", "timeout": "120s"}
        )
        if pool.get("warm", 0) >= 1:
            ok("a pool for long requests is warm")
        else:
            bad("POST /runtimes", pool)

        slow = client.put_script(
            "import time\n\n\ndef handler(event):\n"
            "    time.sleep(event.get('seconds', 30))\n"
            "    return 'finished'\n"
        )

        def run_slow(key, out):
            try:
                out.append(("result", client.run_script("slow", slow.sha256, {}, key=key)))
            except BaseException as e:  # noqa: BLE001 - the test is what it raised
                out.append(("error", e))

        answer: list = []
        caller = threading.Thread(target=run_slow, args=("job-1", answer), daemon=True)
        caller.start()
        time.sleep(2.0)

        stopped = client.cancel("job-1")
        if stopped.get("cancelled") and stopped.get("started"):
            ok("a running request is cancelled by the name its caller gave it")
        else:
            bad("DELETE /requests/<key>", stopped)

        caller.join(timeout=20)
        if caller.is_alive():
            bad("the cancelled request never answered its caller")
        else:
            kind, what = answer[0]
            if kind == "error" and isinstance(what, zygo.Cancelled):
                ok("and its caller is told it was cancelled, not that it timed out")
            else:
                bad("the cancelled call answered with something else", f"{kind}: {what}")

        # A request id is a counter, so the only thing keeping a cancel honest
        # is ownership. A name nobody is running is a 404 either way.
        try:
            client.cancel("job-1")
            bad("cancelling a request that had finished succeeded")
        except zygo.NotFound:
            ok("cancelling one that has already finished is a NotFound")

        try:
            client.cancel("00000001")
            bad("an id nobody is running was accepted")
        except zygo.NotFound:
            ok("and so is an id nobody is running")

        # The id comes back with every answer, which is what joins a log line
        # to the request it describes.
        quick = client.run_script("slow", slow.sha256, {"seconds": 0})
        if quick.request_id:
            ok(f"a result carries its own request id ({quick.request_id})")
        else:
            bad("the result has no request_id", quick)

        client.delete_script(slow.sha256)
        client.stop_runtime("slow")

        # --- streaming -------------------------------------------------------
        #
        # The claim is about *when*, not what: the same text is in the result
        # either way, and an implementation that buffered every chunk and sent
        # them all at the end would pass any check that only looked at what
        # arrived. So the script prints, sleeps, and the first line has to be
        # in hand while it is still sleeping.
        print("\nstreaming")

        client.serve_runtime(
            "watch", {"image": image, "agent": "python", "timeout": "60s"}
        )
        chatty = client.put_script(
            "import sys, time\n\n\ndef handler(event):\n"
            "    print('first')\n"
            "    sys.stdout.flush()\n"
            "    event.progress('halfway')\n"
            "    time.sleep(3)\n"
            "    print('last', file=sys.stderr)\n"
            "    return {'done': True}\n"
        )

        began = time.time()
        seen = []
        first_at = None
        for ev in client.stream_script("watch", chatty.sha256, {}):
            seen.append(ev)
            if first_at is None and not ev.is_result:
                first_at = time.time() - began

        kinds = [e.kind for e in seen]
        if kinds and kinds[-1] == "result" and kinds.count("result") == 1:
            ok("a stream ends with exactly one result")
        else:
            bad("the stream did not end with one result", kinds)

        if first_at is not None and first_at < 2.0:
            ok(f"and the first line arrived after {first_at:.2f}s, while the handler slept 3s")
        else:
            bad("nothing arrived before the handler finished", first_at)

        text = "".join(e.data for e in seen if e.kind == "stdout")
        if "first" in text:
            ok("stdout is streamed as it is printed")
        else:
            bad("the handler's output is not in the stream", text)

        if any(e.kind == "progress" and "halfway" in e.data for e in seen):
            ok("and `progress` is its own kind, not a line of stdout")
        else:
            bad("no progress event", kinds)

        if any(e.kind == "stderr" and "last" in e.data for e in seen):
            ok("stderr is streamed and kept separate")
        else:
            bad("no stderr event", kinds)

        answer = seen[-1].result
        if answer.result == {"done": True} and "first" in answer.stdout:
            ok("and the result still carries the whole of what was printed")
        else:
            bad("the final result lost something", answer)

        # A caller that did not ask for a stream must see exactly what it
        # always did — the flag is per request, and this is the control.
        plain = client.run_script("watch", chatty.sha256, {})
        if plain.result == {"done": True} and "first" in plain.stdout:
            ok("a caller that did not ask for a stream is unaffected")
        else:
            bad("the non-streaming answer changed", plain)

        client.delete_script(chatty.sha256)
        client.stop_runtime("watch")

        # --- long requests ---------------------------------------------------
        #
        # The idle policy freezes a zygote nobody has called for
        # `idle_timeout`. A request that runs for longer than that must not be
        # frozen *while it is running* — the freeze would stop the request it
        # is serving, and a caller would wait out its whole deadline for an
        # answer that was never coming.
        print("\nlong requests")

        client.serve_runtime(
            "patient",
            {
                "image": image,
                "agent": "python",
                "timeout": "120s",
                # Far shorter than the request below, so the policy has every
                # chance to fire during it.
                "idle_timeout": "2s",
                "cold_after": "3s",
            },
        )
        patient = client.put_script(
            "import time\n\n\ndef handler(event):\n"
            "    time.sleep(12)\n    return 'finished'\n"
        )

        began = time.time()
        out = client.run_script("patient", patient.sha256, {}, timeout=120)
        took = time.time() - began
        if out.result == "finished" and took >= 12:
            ok(f"a request outliving `idle_timeout` still finishes ({took:.0f}s)")
        else:
            bad("a long request did not complete", f"{out.result!r} after {took:.1f}s")

        # And the heartbeat is why the ceiling could be raised at all: a
        # timeout of hours is accepted rather than refused as it used to be.
        long_call = client.run_script(
            "patient", "def handler(event):\n    return 'quick'\n", {}, timeout=6 * 3600
        )
        if long_call.result == "quick":
            ok("and a caller may wait hours, which the old one-hour ceiling refused")
        else:
            bad("a multi-hour timeout was refused", long_call)

        client.delete_script(patient.sha256)
        client.stop_runtime("patient")

        # --- files in and out ------------------------------------------------
        #
        # Two claims. A request gets the files its caller sent and can leave
        # files to be collected; and one request's directory is not another's,
        # which is the part that has to be checked rather than described.
        print("\nworkspaces")

        import base64
        import io
        import tarfile

        def make_tar(files):
            buffer = io.BytesIO()
            with tarfile.open(fileobj=buffer, mode="w") as archive:
                for name, body in files.items():
                    info = tarfile.TarInfo(name)
                    info.size = len(body)
                    archive.addfile(info, io.BytesIO(body))
            return buffer.getvalue()

        client.serve_runtime(
            "files", {"image": image, "agent": "python", "timeout": "60s"}
        )

        # A handler that reads what it was given and writes what it was asked
        # for, using the working directory the agent put it in.
        worker = client.put_script(
            "import os\n\n\ndef handler(event):\n"
            "    here = os.environ.get('ZYGO_WORKSPACE', '')\n"
            "    seen = sorted(os.listdir('.'))\n"
            "    body = open('in.txt').read() if os.path.exists('in.txt') else ''\n"
            "    with open('out.txt', 'w') as f:\n"
            "        f.write(body.upper())\n"
            "    return {'cwd': os.getcwd(), 'env': here, 'saw': seen}\n"
        )

        out = client.run_script(
            "files",
            worker.sha256,
            {},
            workspace={"inline": base64.b64encode(make_tar({"in.txt": b"hello"})).decode()},
            out=True,
        )
        if out.result["saw"] == ["in.txt"]:
            ok("a request's files are there, and only its own")
        else:
            bad("the workspace did not arrive", out.result)

        if out.result["cwd"] == out.result["env"] and out.result["env"].startswith("/work/"):
            ok(f"the handler starts in it, and ZYGO_WORKSPACE says where ({out.result['env']})")
        else:
            bad("the working directory is not the workspace", out.result)

        if out.workspace:
            back = tarfile.open(fileobj=io.BytesIO(out.workspace))
            names = sorted(m.name for m in back.getmembers())
            body = back.extractfile("out.txt").read()
            if names == ["in.txt", "out.txt"] and body == b"HELLO":
                ok("and what it left comes back as a tar")
            else:
                bad("the collected workspace is wrong", f"{names} {body!r}")
        else:
            bad("nothing came back for ?out=1")

        # Sent once, named many times: the point of a blob.
        blob = client.put_blob(make_tar({"in.txt": b"from a blob"}))
        again = client.put_blob(make_tar({"in.txt": b"from a blob"}))
        if blob.sha256 == again.sha256 and again.existed:
            ok("the same tar is one blob, stored once")
        else:
            bad("PUT /blobs is not idempotent", f"{blob} {again}")

        out = client.run_script("files", worker.sha256, {}, workspace={"blob": blob.sha256}, out=True)
        if out.result["saw"] == ["in.txt"]:
            ok("and a call can name it instead of sending it again")
        else:
            bad("a blob workspace did not arrive", out.result)

        # The isolation claim. Two requests in the same pool: neither may see
        # the other's directory, and `/work` itself cannot be listed.
        snooper = client.put_script(
            "import os\n\n\ndef handler(event):\n"
            "    try:\n"
            "        return {'listed': sorted(os.listdir('/work'))}\n"
            "    except OSError as e:\n"
            "        return {'refused': e.strerror}\n"
        )
        # `out=True` alone: a request with no files in and a directory of its
        # own anyway, which is what a handler that only produces something
        # needs.
        snoop = client.run_script("files", snooper.sha256, {}, out=True)
        if snoop.result.get("refused"):
            ok(f"/work cannot be listed from inside ({snoop.result['refused']})")
        else:
            bad("a request listed every workspace in its sandbox", snoop.result)

        # And it is gone afterwards: the same path, asked for a second time.
        vanished = client.run_script(
            "files",
            "import os\n\n\ndef handler(event):\n"
            "    return {'exists': os.path.exists(event['path'])}\n",
            {"path": out.result["env"]},
        )
        if vanished.result == {"exists": False}:
            ok("and a finished request's workspace is gone")
        else:
            bad("a workspace outlived its request", vanished.result)

        client.delete_blob(blob.sha256)
        client.delete_script(worker.sha256)
        client.delete_script(snooper.sha256)
        client.stop_runtime("files")

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


def call_only(socket_path: str, image: str = "python:3.12-slim") -> int:
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

        # Registering is *not* on the list, and that is the change scoped
        # tokens paid for: a script nobody can run is not a widened boundary,
        # and registering one for yourself is what a tenant token is for.
        # Running it still needs a pool an operator declared.
        try:
            client.put_script("x = 1\n")
            ok("PUT /scripts is allowed: registering a script runs nothing")
        except zygo.ZygoError as e:
            bad("PUT /scripts was refused on a call-only API", f"{type(e).__name__}: {e}")

        # Same reasoning for a lockfile: a dependency set nobody can name in a
        # pool runs nothing, and the pool is still the operator's to declare.
        try:
            client.put_deps(image, {"requirements.txt": "six==1.16.0\n"})
            ok("POST /deps is allowed: a dependency set is not a pool")
        except zygo.ZygoError as e:
            bad("POST /deps was refused on a call-only API", f"{type(e).__name__}: {e}")

        for what, call in (
            ("POST /run", lambda: client.run("alpine:3", ["true"])),
            ("PUT /fn/<name>", lambda: client.serve("x", {"image": "alpine:3", "cmd": ["true"]}, base_dir="/tmp")),
            ("DELETE /fn/<name>", lambda: client.stop("x")),
            ("DELETE /scripts/<hash>", lambda: client.delete_script("sha256:" + "c" * 64)),
            ("DELETE /deps/<id>", lambda: client.delete_deps("deps_" + "c" * 32)),
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


def drain(socket_path: str, image: str) -> int:
    """Draining, which ends the API it is run against.

    Its own phase for that reason, and the last one. What is checked is the
    promise a rolling restart rests on: a call already in flight when the
    drain starts still gets its answer, and only then does the process go.
    """
    import threading

    import zygo

    url = f"unix://{socket_path}"
    print("\ndrain")
    print(f"  {url}")

    with zygo.connect(url, token=None, timeout=600) as client:
        health = client.health()
        if health.get("status") == "ok":
            ok("a host with nothing below its floor is `ok`")
        else:
            bad("GET /healthz", health)

        client.serve_runtime(
            "slow-drain", {"image": image, "agent": "python", "timeout": "60s"}
        )
        slow = client.put_script(
            "import time\n\n\ndef handler(event):\n"
            "    time.sleep(6)\n    return 'finished'\n"
        )

        answer: list = []

        def call() -> None:
            try:
                answer.append(("result", client.run_script("slow-drain", slow.sha256, {})))
            except BaseException as e:  # noqa: BLE001
                answer.append(("error", e))

        caller = threading.Thread(target=call, daemon=True)
        caller.start()
        time.sleep(2.0)

        # The drain waits for that request. It answers before the process
        # goes, so this call returns rather than seeing a closed connection.
        began = time.time()
        drained = client.drain(grace=30)
        took = time.time() - began
        if drained.get("drained") and drained.get("in_flight") == 0:
            ok(f"a drain waits for what is running and says so ({took:.1f}s)")
        else:
            bad("POST /drain", drained)

        caller.join(timeout=20)
        if answer and answer[0][0] == "result" and answer[0][1].result == "finished":
            ok("and the call that was in flight still got its answer")
        else:
            bad("a request in flight lost its answer to the drain", answer)

        # And the API is gone a moment later.
        time.sleep(1.5)
        try:
            client.health()
            bad("the API is still serving after a drain")
        except zygo.ZygoError:
            ok("then the API exits")
    return 0


def tokens(socket_path: str, image: str) -> int:
    """Scoped tokens, against a listener that actually checks one.

    Every other phase runs `--no-auth`, where the caller is the operator
    because they reached a `0600` socket. That is the right default and it is
    also why it cannot test this: with no token there is nothing to resolve.
    So this phase gets a listener with bearer auth and a bootstrap token, and
    asks the only question tokens exist to answer — *whose request is this?*
    """
    import zygo

    url = f"unix://{socket_path}"
    bootstrap = os.environ["ZYGO_API_TOKEN"]
    print("\nscoped tokens")
    print(f"  {url}")

    with zygo.connect(url, token=bootstrap, timeout=600) as operator:
        try:
            version = operator.version()
        except zygo.ZygoError as e:
            print(f"\n  the API is not reachable, so nothing below would mean anything:\n  {e}")
            return 1
        if version.get("deploy") is True:
            ok("the bootstrap token is the operator, and it may deploy")
        else:
            bad("GET /version with the bootstrap token", json.dumps(version))

        with zygo.connect(url, token="zygo_not-a-token") as stranger:
            try:
                stranger.functions()
                bad("a token nobody minted was accepted")
            except zygo.AuthError:
                ok("a token nobody minted is refused")

        # --- minting ---------------------------------------------------------

        minted = operator.mint_token("acme")
        if minted.secret and minted.token.tenant == "acme":
            ok("a tenant token is minted, and the secret comes back with it")
        else:
            bad("POST /tenants/<id>/tokens", minted)

        if any(t.id == minted.token.id for t in operator.tokens()):
            ok("it is on the list")
        else:
            bad("GET /tokens", operator.tokens())
        if all(minted.secret not in json.dumps(t.__dict__) for t in operator.tokens()):
            ok("and the list does not carry the secret — the store has a hash")
        else:
            bad("a listing carried a secret")

        # Minting for a tenant registers it, so onboarding is one call.
        if operator.tenant("acme").id == "acme":
            ok("minting for a new tenant registered the tenant")
        else:
            bad("the tenant was not registered by the mint")

        globex = operator.mint_token("globex")

        # --- what a tenant token is --------------------------------------------

        acme = zygo.connect(url, token=minted.secret, timeout=600)
        other = zygo.connect(url, token=globex.secret, timeout=600)

        # No header anywhere below. The tenant comes from the token, which is
        # the whole point: it is the one part of a request a caller cannot
        # choose.
        theirs = acme.put_script("def handler(event):\n    return {'from': 'acme'}\n")
        if operator.tenant("acme").scripts == [theirs.sha256]:
            ok("a tenant token registers scripts against its own tenant, with no header")
        else:
            bad("PUT /scripts with a tenant token", operator.tenant("acme").scripts)

        if acme.tenant("acme").id == "acme":
            ok("a tenant may read its own record")
        else:
            bad("GET /tenants/<own id> with a tenant token")

        for what, call in (
            ("list the tenants", lambda: acme.tenants()),
            ("read another tenant", lambda: acme.tenant("globex")),
            ("create a runtime pool", lambda: acme.serve_runtime("x", {"image": image, "agent": "python"})),
            ("serve a function", lambda: acme.serve("x", {"image": image, "cmd": ["true"]}, base_dir="/tmp")),
            ("run a one-shot sandbox", lambda: acme.run("alpine:3", ["true"])),
            ("mint itself a token", lambda: acme.mint_token()),
            ("list the tokens", lambda: acme.tokens()),
            ("revoke a token", lambda: acme.revoke_token(globex.token.id)),
            ("delete a tenant", lambda: acme.delete_tenant("globex")),
        ):
            try:
                call()
                bad(f"a tenant token could {what}")
            except zygo.AuthError:
                ok(f"a tenant token cannot {what}")
            except zygo.ZygoError as e:
                bad(f"`{what}` failed with the wrong kind of error", f"{type(e).__name__}: {e}")

        # A dependency set belongs to the tenant whose token uploaded it. Not
        # a secret — the id is a hash of a lockfile — but a list that showed
        # every customer's would tell each of them what the others run.
        mine = acme.put_deps(image, {"requirements.txt": "six==1.16.0\n"})
        if [d.id for d in acme.deps()] == [mine.id]:
            ok("a dependency set is listed for the tenant that uploaded it")
        else:
            bad("GET /deps with a tenant token", [d.id for d in acme.deps()])
        # Shared by content, like a script: the same lockfile from a second
        # customer is the same id and one build, and both of them can see it —
        # recording only the first uploader would mean the second sent
        # something they cannot find afterwards.
        theirs_too = other.put_deps(image, {"requirements.txt": "six==1.16.0\n"})
        if theirs_too.id == mine.id and [d.id for d in other.deps()] == [mine.id]:
            ok("and a second tenant who uploads the same lockfile joins it")
        else:
            bad("the same lockfile built twice", f"{mine.id} then {theirs_too.id}")
        # And one nobody else uploaded stays invisible, named directly or not.
        only_acme = acme.put_deps(image, {"requirements.txt": "six==1.15.0\n"})
        if only_acme.id not in [d.id for d in other.deps()]:
            ok("a dependency set another tenant never uploaded is not in their list")
        else:
            bad("another tenant saw it", [d.id for d in other.deps()])
        try:
            other.deps(only_acme.id)
            bad("another tenant read a dependency set by naming its id")
        except zygo.NotFound:
            ok("and naming its id directly is a not-found, not a forbidden")

        # A token names its tenant; a header that disagrees is refused rather
        # than ignored, so a client that thinks it is somebody else is told.
        try:
            acme.for_tenant("globex").put_script("x = 1\n")
            bad("a tenant token acted for another tenant by sending a header")
        except zygo.AuthError:
            ok("a header that disagrees with the token is refused, not ignored")

        # --- a digest is still not a capability --------------------------------

        pool = operator.serve_runtime("shared", {"image": image, "agent": "python", "timeout": "20s"})
        if pool.get("warm", 0) >= 1:
            ok("an operator's pool is warm, and a tenant token may call it")
        else:
            bad("POST /runtimes", pool)

        out = acme.run_script("shared", theirs.sha256, {})
        if out.result.get("from") == "acme":
            ok("the tenant that registered a script can run it")
        else:
            bad("the owner's own script", out)

        try:
            other.run_script("shared", theirs.sha256, {})
            bad("a tenant token ran another tenant's script by naming its digest")
        except zygo.NotFound:
            ok("another tenant's token naming that digest is refused as `not found`")

        # --- and a request id is a counter, so ownership is the lock ------------
        #
        # Ids are `00000001`, `00000002`, … — a name, not a secret, on the
        # same argument a script digest gets. So a tenant counting up must not
        # be able to reach another tenant's work with a cancel.
        import threading

        slow = acme.put_script(
            "import time\n\n\ndef handler(event):\n"
            "    time.sleep(20)\n    return 'finished'\n"
        )

        answer: list = []

        def run_slow() -> None:
            try:
                answer.append(("result", acme.run_script("shared", slow.sha256, {}, key="theirs")))
            except BaseException as e:  # noqa: BLE001 - the test is what it raised
                answer.append(("error", e))

        caller = threading.Thread(target=run_slow, daemon=True)
        caller.start()
        time.sleep(2.0)

        try:
            other.cancel("theirs")
            bad("a tenant cancelled another tenant's request by name")
        except zygo.NotFound:
            ok("a tenant cannot cancel another tenant's request")

        # Every id the host could plausibly be running. If one of them is
        # acme's, this is the hole that ownership is there to close.
        stolen = False
        for n in range(1, 40):
            try:
                other.cancel(f"{n:08x}")
                stolen = True
                break
            except zygo.NotFound:
                pass
        if stolen:
            bad("a tenant cancelled a request by counting ids up")
        else:
            ok("and cannot find one by counting ids up either")

        if acme.cancel("theirs").get("cancelled"):
            ok("while its own caller can stop it")
        else:
            bad("the owner could not cancel their own request")
        caller.join(timeout=20)
        # The operator's, not acme's: the store is shared by digest, so
        # forgetting one script forgets it for every tenant that registered
        # the same bytes.
        operator.delete_script(slow.sha256)

        # --- usage events --------------------------------------------------------
        #
        # What an embedder bills on. The claim checked here is the one the
        # task names: a cancelled and a stuck request each produce one event
        # with the right `outcome` — which is the same field a 499 and a 504
        # map from, so it is checked through the answers.
        print()
        counted = operator.put_script(
            "import time\n\n\ndef handler(event):\n"
            "    time.sleep(event.get('seconds', 0))\n    return 'done'\n"
        )

        before = json.loads(operator._request("GET", "/metrics", headers={"accept": "text/plain"})) \
            if False else None
        _ = before

        good = operator.run_script("shared", counted.sha256, {})
        if good.result == "done":
            ok("a finished request answers, and is counted")
        else:
            bad("the control request failed", good)

        # A cancelled one. `outcome` is what the 499 maps from.
        import threading

        answer: list = []

        def slow() -> None:
            try:
                answer.append(
                    operator.run_script(
                        "shared", counted.sha256, {"seconds": 20}, key="usage-1"
                    )
                )
            except BaseException as e:  # noqa: BLE001
                answer.append(e)

        caller = threading.Thread(target=slow, daemon=True)
        caller.start()
        time.sleep(2.0)
        operator.cancel("usage-1")
        caller.join(timeout=20)
        if answer and isinstance(answer[0], zygo.Cancelled):
            ok("a cancelled request ends as `cancelled`, not `timeout`")
        else:
            bad("the cancelled request answered wrongly", answer)

        # A timed-out one, which is the third reading of 137.
        try:
            operator.run_script(
                "shared", counted.sha256, {"seconds": 30}, timeout=None
            )
            bad("a request past the pool's timeout was not killed")
        except zygo.Timeout:
            ok("and one past its deadline ends as `timeout`")
        except zygo.ZygoError as e:
            bad("the timed-out request answered wrongly", f"{type(e).__name__}: {e}")

        # And the webhook got one event per request, with those outcomes.
        # The receiver is started by `verify_api.sh` and appends one JSON line
        # per batch; delivery is on an interval, so this waits for it.
        sink = os.environ.get("ZYGO_USAGE_SINK", "")
        flat = []
        for _ in range(40):
            time.sleep(0.5)
            try:
                with open(sink) as handle:
                    batches = [json.loads(line) for line in handle if line.strip()]
            except (FileNotFoundError, ValueError):
                continue
            flat = [e for batch in batches for e in batch.get("events", [])]
            if {"cancelled", "timeout"} <= {e["outcome"] for e in flat}:
                break

        outcomes = {e["outcome"] for e in flat}
        if {"ok", "cancelled", "timeout"} <= outcomes:
            ok(f"the usage webhook received each outcome ({len(flat)} events)")
        else:
            bad("the webhook did not receive every outcome", sorted(outcomes))

        # One event per request, keyed by the id a caller cancels by.
        ids = [e["request_id"] for e in flat]
        if ids and len(ids) == len(set(ids)):
            ok("one event per request, each with its own id")
        else:
            bad("the webhook saw a request twice in one run", ids)

        if flat and all(e["tenant"] and e["function"] for e in flat):
            ok("and every event says whose it was and what it ran")
        else:
            bad("an event is missing its tenant or function", flat[:2])

        operator.delete_script(counted.sha256)

        # --- per-tenant limits ---------------------------------------------------
        #
        # A tenant's limits narrow the pool's and never widen them. What is
        # checked here is that they *bind*: a memory limit the operator's own
        # request survives kills the tenant's.
        print()
        tight = operator.set_limits("acme", mem="24M", pids=24)
        if tight.limits.get("mem") and tight.limits.get("pids") == 24:
            ok("a tenant's limits are stored")
        else:
            bad("PATCH /tenants/<id>/limits", tight.limits)

        # The pool was declared without a `mem`, so it has the default — far
        # above 24 MiB. A request that allocates 64 MiB survives for the
        # operator and is killed for acme, which is the whole claim.
        # Registered by acme, because a digest is not a capability: a script
        # the operator registered is not acme's to run.
        hungry = acme.put_script(
            "def handler(event):\n    b = bytearray(64 * 1024 * 1024)\n"
            "    b[::4096] = b'x' * len(b[::4096])\n    return len(b)\n"
        )
        # The control: the same script, the same pool, with the limit lifted.
        operator.set_limits("acme", mem=None, pids=None)
        try:
            got = acme.run_script("shared", hungry.sha256, {})
            unlimited = got.result == 64 * 1024 * 1024
        except zygo.ZygoError:
            unlimited = False
        if unlimited:
            ok("with no tenant limit the request allocates 64 MiB")
        else:
            bad("the control allocation failed, so the limit below proves nothing")

        operator.set_limits("acme", mem="24M", pids=24)
        try:
            acme.run_script("shared", hungry.sha256, {})
            bad("a tenant's memory limit did not bind")
        except zygo.ZygoError as e:
            ok(f"and the same request is killed at 24M ({type(e).__name__})")

        # A value above every ceiling the tenant has could never take effect,
        # so it is refused rather than stored silently.
        try:
            operator.set_limits("acme", mem="999G")
            bad("a limit above every ceiling was accepted")
        except zygo.ZygoError as e:
            if "above every ceiling" in str(e):
                ok("and one that could never take effect is refused, naming the key")
            else:
                bad("the refusal does not say why", e)

        operator.set_limits("acme", mem=None, pids=None)
        operator.delete_script(hungry.sha256)

        # --- per-tenant secrets --------------------------------------------------
        #
        # Encrypted at rest, never readable back, and delivered to a request as
        # a file the zygote does not have.
        print()
        secret_value = "sk_live_" + "9" * 20
        names = operator.put_secret("acme", "STRIPE_KEY", secret_value)
        if names == ["STRIPE_KEY"]:
            ok("a secret is stored for a tenant")
        else:
            bad("PUT /tenants/<id>/secrets/<name>", names)

        if operator.secrets("acme") == ["STRIPE_KEY"]:
            ok("and the API answers with names")
        else:
            bad("GET /tenants/<id>/secrets", operator.secrets("acme"))

        # The one thing the whole design rests on: nothing gives the value
        # back. There is no route that could, so this checks the store.
        data = os.environ.get("ZYGO_DATA_HOME", "")
        leaked = []
        for root, _dirs, files in os.walk(os.path.join(data, "secrets")):
            for f in files:
                with open(os.path.join(root, f), "rb") as handle:
                    if secret_value.encode() in handle.read():
                        leaked.append(os.path.join(root, f))
        if not leaked:
            ok("and the value is not on disk in the clear")
        else:
            bad("a secret is readable on disk", leaked)

        # A tenant may read its own names, and nobody else's.
        if acme.secrets("acme") == ["STRIPE_KEY"]:
            ok("a tenant can list its own secret names")
        else:
            bad("a tenant cannot read its own names")
        try:
            other.secrets("acme")
            bad("a tenant read another tenant's secret names")
        except zygo.AuthError:
            ok("and not another tenant's")
        try:
            acme.put_secret("acme", "X", "mine")
            bad("a tenant set its own secret")
        except zygo.AuthError:
            ok("nor may a tenant set one: that is the operator's")

        if operator.delete_secret("acme", "STRIPE_KEY") == []:
            ok("a secret can be forgotten")
        else:
            bad("DELETE /tenants/<id>/secrets/<name>")

        # --- revocation --------------------------------------------------------

        operator.revoke_token(minted.token.id)
        try:
            acme.functions()
            bad("a revoked token still works")
        except zygo.AuthError:
            ok("a revoked token stops working on the very next request")

        if any(t.id == minted.token.id and t.revoked for t in operator.tokens()):
            ok("and it is still listed, marked revoked, so an id in a log resolves")
        else:
            bad("a revoked token vanished from the list", operator.tokens())

        # Deleting a customer takes their keys with their code.
        operator.delete_tenant("globex")
        try:
            other.functions()
            bad("a deleted tenant's token still works")
        except zygo.AuthError:
            ok("deleting a tenant revokes its tokens")

        # --- an operator token the operator minted ------------------------------

        second = operator.mint_token()
        if second.token.tenant is None:
            ok("an operator token names nobody")
        else:
            bad("POST /tokens", second)
        with zygo.connect(url, token=second.secret, timeout=600) as deputy:
            if deputy.version().get("deploy") is True and isinstance(deputy.tenants(), list):
                ok("and it may do what the operator may: deploy, and see the tenants")
            else:
                bad("a minted operator token could not act as one")

        acme.close()
        other.close()
        operator.stop_runtime("shared")
        operator.delete_tenant("acme")
    return 0


if __name__ == "__main__":
    if sys.argv[1] == "--call-only":
        status = call_only(
            sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else "python:3.12-slim"
        )
    elif sys.argv[1] == "--tokens":
        status = tokens(sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else "python:3.12-slim")
    elif sys.argv[1] == "--drain":
        status = drain(sys.argv[2], sys.argv[3] if len(sys.argv) > 3 else "python:3.12-slim")
    else:
        status = main()
    print(f"\n  {PASS} passed, {FAIL} failed")
    sys.exit(1 if FAIL else status)
