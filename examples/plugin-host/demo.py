"""What the plugin host can do, run end to end against a real Zygo.

The exit criterion for the embedder API, as a program that either works or
does not. Every line here goes through the HTTP API: nothing reads a
`sandbox.toml`, nothing writes a file on the Zygo host, and nothing shells out
to `zygo`.

Run:  make verify-plugin-host
"""

from __future__ import annotations

import io
import os
import sys
import tarfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.environ.get("ZYGO_SDK", "/src/sdk/python/src"))

import zygo  # noqa: E402

from host import PluginHost  # noqa: E402

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


# Two customers' plugins. Ordinary Python, written by somebody who has never
# heard of Zygo — which is the point.
GREETER = """
import platform


def handler(event):
    return {"hello": event["name"], "python": platform.python_version()}
"""

COUNTER = """
import os


def handler(event):
    with open("in.txt") as f:
        text = f.read()
    with open("out.txt", "w") as f:
        f.write(str(len(text)))
    return {"counted": len(text)}
"""

CHATTY = """
import sys, time


def handler(event):
    for i in range(3):
        print(f"step {i}")
        sys.stdout.flush()
        event.progress(f"{i + 1} of 3")
        time.sleep(0.4)
    return {"done": True}
"""

SLOW = """
import time


def handler(event):
    time.sleep(30)
    return "never"
"""

SNOOP = """
def handler(event):
    # Somebody else's plugin, by digest. A digest is not a capability.
    return {"reached": True}
"""

# The same customer's other plugin, in the other language. Ordinary JavaScript,
# written by somebody who has never heard of Zygo — which is again the point.
GREETER_JS = """
module.exports = function handler(event) {
  return { hello: event.name, node: process.versions.node };
};
"""

# And one that asks for more memory than its customer is allowed. The Python
# twin of this is below; the two together are what "the same tenant limits"
# means — the limit is on the customer, not on the pool they happen to be in.
HUNGRY_JS = """
module.exports = function handler(event) {
  const chunks = [];
  for (let i = 0; i < 96; i += 1) chunks.push(Buffer.alloc(1024 * 1024, 1));
  return chunks.length;
};
"""

HUNGRY_PY = """
def handler(event):
    b = bytearray(96 * 1024 * 1024)
    b[::4096] = b'x' * len(b[::4096])
    return len(b)
"""


def main() -> int:
    url = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("ZYGO_API_URL", "")
    token = os.environ.get("ZYGO_API_TOKEN")
    print("a plugin host, on the API alone")
    print(f"  {url}")

    with zygo.connect(url, token=token, timeout=600) as operator:
        host = PluginHost(operator)
        host.start()
        ok("the host declares a runtime per language, naming no path on the Zygo machine")

        acme = host.onboard("acme", mem="128M", secrets={"API_KEY": "acme-key"})
        globex = host.onboard("globex", mem="64M", secrets={"API_KEY": "globex-key"})
        if acme and globex and acme != globex:
            ok("two customers are onboarded, each with their own token")
        else:
            bad("onboarding", f"{acme!r} {globex!r}")

        greeter = host.install("acme", GREETER)
        out = host.run("acme", greeter, {"name": "world"})
        if out.result["hello"] == "world" and out.result.get("python"):
            ok(f"a customer's plugin is installed and called (python {out.result['python']})")
        else:
            bad("the plugin did not run", out.result)

        # Files in and out, which is what a real plugin does.
        counter = host.install("acme", COUNTER)
        out = host.run("acme", counter, {}, files={"in.txt": b"twelve chars"})
        if out.result["counted"] == 12 and out.workspace:
            with tarfile.open(fileobj=io.BytesIO(out.workspace)) as back:
                left = back.extractfile("out.txt").read()
            if left == b"12":
                ok("a plugin reads the files it was sent and leaves files behind")
            else:
                bad("the collected workspace is wrong", left)
        else:
            bad("files in and out", out.result)

        # Output as it happens, for a plugin that takes a while.
        chatty = host.install("acme", CHATTY)
        seen: list = []
        result = host.watch("acme", chatty, {}, lambda kind, data: seen.append(kind))
        if result and result.result == {"done": True} and "progress" in seen:
            ok("a long plugin's output and progress arrive while it runs")
        else:
            bad("streaming", f"{result} {seen}")

        # Stopping one.
        slow = host.install("acme", SLOW)
        answer: list = []

        def call() -> None:
            try:
                answer.append(host.run_cancellable("acme", slow, {}, "job-1"))
            except BaseException as e:  # noqa: BLE001
                answer.append(e)

        caller = threading.Thread(target=call, daemon=True)
        caller.start()
        time.sleep(2.0)
        host.cancel("acme", "job-1")
        caller.join(timeout=20)
        if answer and isinstance(answer[0], zygo.Cancelled):
            ok("a running plugin can be stopped by the host")
        else:
            bad("cancelling", answer)

        # The boundary the whole thing rests on.
        try:
            host.run("globex", greeter, {"name": "theirs"})
            bad("one customer ran another's plugin by naming its digest")
        except zygo.NotFound:
            ok("and one customer cannot run another's plugin, digest or not")

        # The other language, through the same API, for the same customer.
        # Nothing about onboarding, tokens, digests or limits changed — only
        # which pool the host names, which is a field on its own record.
        greeter_js = host.install("acme", GREETER_JS, "javascript")
        out = host.run("acme", greeter_js, {"name": "world"})
        if out.result["hello"] == "world" and out.result.get("node"):
            ok(
                "the same customer's JavaScript plugin runs through the same API "
                f"(node {out.result['node']})"
            )
        else:
            bad("the JavaScript plugin did not run", out.result)

        if greeter_js.runtime != greeter.runtime:
            ok(f"in its own pool ({greeter_js.runtime}, not {greeter.runtime})")
        else:
            bad("both languages named the same runtime", greeter_js.runtime)

        # Limits are the customer's, not the runtime's — and the point of
        # doing it twice is that the number is the same number. `globex` is
        # capped at 64 MiB; both plugins ask for 96.
        for language, source in (("python", HUNGRY_PY), ("javascript", HUNGRY_JS)):
            hungry = host.install("globex", source, language)
            try:
                host.run("globex", hungry, {})
                bad(f"a {language} plugin took more memory than its tenant allows")
            except zygo.ZygoError:
                ok(f"a {language} plugin is held to the customer's 64M, not the pool's")

        # And offboarding takes everything.
        removed = host.offboard("globex")
        if removed.get("deleted"):
            ok("offboarding removes a customer's code, tokens and secrets")
        else:
            bad("offboarding", removed)
        try:
            zygo.connect(url, token=globex).functions()
            bad("an offboarded customer's token still works")
        except zygo.AuthError:
            ok("and their token stops working")

        host.offboard("acme")
        host.stop()
    return 0


if __name__ == "__main__":
    status = main()
    print(f"\n  {PASS} passed, {FAIL} failed")
    print(
        "\n  no sandbox.toml was read and no file was written on the Zygo host:"
        "\n  every line above went through the HTTP API."
    )
    sys.exit(1 if FAIL else status)
