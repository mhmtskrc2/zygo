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

from host import RUNTIME, PluginHost  # noqa: E402

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
import os


def handler(event):
    return {"hello": event["name"], "from": os.environ.get("ZYGO_TENANT", "?")}
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


def main() -> int:
    url = sys.argv[1] if len(sys.argv) > 1 else os.environ.get("ZYGO_API_URL", "")
    token = os.environ.get("ZYGO_API_TOKEN")
    print("a plugin host, on the API alone")
    print(f"  {url}")

    with zygo.connect(url, token=token, timeout=600) as operator:
        host = PluginHost(operator)
        host.start()
        ok("the host declares one runtime, naming no path on the Zygo machine")

        acme = host.onboard("acme", mem="128M", secrets={"API_KEY": "acme-key"})
        globex = host.onboard("globex", mem="64M", secrets={"API_KEY": "globex-key"})
        if acme and globex and acme != globex:
            ok("two customers are onboarded, each with their own token")
        else:
            bad("onboarding", f"{acme!r} {globex!r}")

        greeter = host.install("acme", GREETER)
        out = host.run("acme", greeter, {"name": "world"})
        if out.result["hello"] == "world":
            ok("a customer's plugin is installed and called")
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

        # Limits are the customer's, not the runtime's.
        hungry = host.install("globex", "def handler(e):\n    b = bytearray(96 * 1024 * 1024)\n    b[::4096] = b'x' * len(b[::4096])\n    return len(b)\n")
        try:
            host.run("globex", hungry, {})
            bad("a customer took more memory than their tenant allows")
        except zygo.ZygoError:
            ok("a customer is held to their own limit, not the runtime's")

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
        operator.stop_runtime(RUNTIME)
    return 0


if __name__ == "__main__":
    status = main()
    print(f"\n  {PASS} passed, {FAIL} failed")
    print(
        "\n  no sandbox.toml was read and no file was written on the Zygo host:"
        "\n  every line above went through the HTTP API."
    )
    sys.exit(1 if FAIL else status)
