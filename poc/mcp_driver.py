#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Drive `zygo mcp` the way an agent host does, and check what comes back.

Every check here is an *attempt*. "The sandbox has no network" is checked by
opening a socket inside one and reading the error; "the root filesystem is
read-only" by writing to it. Reading a flag would pass on a kernel that ignores
the flag, which is the failure mode this repository's test rules exist to stop.

The transport is a pipe, so this needs no host and no port: the server is
started as a child process and spoken to over its standard input and output,
exactly as Claude Code or Cursor would.

Run:  make verify-mcp
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
import time
from typing import Any, Dict, List, Optional

PASS = 0
FAIL = 0


def ok(what: str) -> None:
    global PASS
    PASS += 1
    print(f"  PASS  {what}", flush=True)


def bad(what: str, detail: str = "") -> None:
    global FAIL
    FAIL += 1
    print(f"  FAIL  {what}", flush=True)
    if detail:
        for line in str(detail).splitlines()[:8]:
            print(f"          {line}", flush=True)


class Server:
    """One `zygo mcp` process, spoken to over its pipes."""

    def __init__(self, launcher: str, *args: str) -> None:
        self.process = subprocess.Popen(
            [launcher, *args],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
        )
        self._next_id = 0
        self._lock = threading.Lock()
        self._stderr: List[str] = []
        # Drained on a thread: a server whose standard error filled its pipe
        # would block, and the failure would look like a hung tool call.
        self._drain = threading.Thread(target=self._read_stderr, daemon=True)
        self._drain.start()

    def _read_stderr(self) -> None:
        assert self.process.stderr is not None
        for line in self.process.stderr:
            self._stderr.append(line)

    @property
    def stderr(self) -> str:
        return "".join(self._stderr)

    def send(self, method: str, params: Optional[Dict[str, Any]] = None, *, notify: bool = False) -> Optional[int]:
        message: Dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        if not notify:
            with self._lock:
                self._next_id += 1
                message["id"] = self._next_id
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()
        return message.get("id")

    def read(self, timeout: float = 600.0) -> Dict[str, Any]:
        """One answer. Raises if the server died or said nothing in time."""
        assert self.process.stdout is not None
        line = _readline(self.process.stdout, timeout)
        if not line:
            raise RuntimeError(f"the server said nothing; stderr:\n{self.stderr}")
        return json.loads(line)

    def call(self, method: str, params: Optional[Dict[str, Any]] = None, timeout: float = 600.0) -> Dict[str, Any]:
        self.send(method, params)
        return self.read(timeout)

    def tool(self, name: str, arguments: Dict[str, Any], timeout: float = 600.0) -> Dict[str, Any]:
        return self.call("tools/call", {"name": name, "arguments": arguments}, timeout)

    def handshake(self, version: str = "2025-06-18") -> Dict[str, Any]:
        answer = self.call("initialize", {"protocolVersion": version, "capabilities": {}})
        self.send("notifications/initialized", {}, notify=True)
        return answer

    def close(self) -> None:
        try:
            assert self.process.stdin is not None
            self.process.stdin.close()
            self.process.wait(timeout=30)
        except Exception:
            self.process.kill()

    def __enter__(self) -> "Server":
        return self

    def __exit__(self, *_: object) -> None:
        self.close()


def _readline(stream: Any, timeout: float) -> str:
    """Read one line, giving up after `timeout` seconds.

    `readline` on a pipe has no deadline, and a server that hangs would hang
    this suite with it — reported as a timeout by whatever runs it, with no
    indication of which check was in flight.
    """
    result: List[str] = []

    def read() -> None:
        result.append(stream.readline())

    thread = threading.Thread(target=read, daemon=True)
    thread.start()
    thread.join(timeout)
    if thread.is_alive():
        raise RuntimeError(f"no answer within {timeout:.0f}s")
    return result[0] if result else ""


def text_of(answer: Dict[str, Any]) -> str:
    return "".join(c.get("text", "") for c in answer.get("result", {}).get("content", []))


def is_error(answer: Dict[str, Any]) -> bool:
    return bool(answer.get("result", {}).get("isError"))


# ---- the checks -------------------------------------------------------


def check_handshake(launcher: str) -> None:
    print("\nhandshake")
    with Server(launcher) as server:
        answer = server.handshake("2024-11-05")
        result = answer.get("result", {})
        if result.get("protocolVersion") == "2024-11-05":
            ok("a revision the server knows is echoed back, not overridden")
        else:
            bad("the client's revision was not honoured", json.dumps(result))

        if result.get("serverInfo", {}).get("name") == "zygo":
            ok("the server identifies itself")
        else:
            bad("no serverInfo", json.dumps(result))

        if "instructions" in result and "sandbox" in result["instructions"]:
            ok("the model is told what it is running inside")
        else:
            bad("no instructions for the model", json.dumps(result)[:400])

        # The workspace banner is for a human, and standard output belongs to
        # the protocol. A server that printed it to stdout would break the
        # first JSON parse a host attempts — which is how an MCP server fails
        # in a way nobody can read.
        if "workspace" in server.stderr:
            ok("the human-readable banner went to standard error")
        else:
            bad("the banner is missing from stderr", server.stderr[:400])

        # A notification is answered with silence. Replying to one is a
        # protocol error that some hosts report as a broken server. Checked by
        # sending a notification and then a request: if the notification were
        # answered, the next read would return *its* answer rather than the
        # ping's.
        server.send("notifications/cancelled", {"requestId": 1}, notify=True)
        ping_id = server.send("ping", {})
        answer = server.read(30)
        if answer.get("id") == ping_id:
            ok("a notification is answered with silence")
        else:
            bad("something answered a notification", json.dumps(answer))


def check_tools(launcher: str) -> None:
    print("\ntools")
    with Server(launcher) as server:
        server.handshake()
        answer = server.call("tools/list")
        tools = answer.get("result", {}).get("tools", [])
        names = sorted(t["name"] for t in tools)
        expected = sorted(["run_code", "list_functions", "call_function", "function_logs"])
        if names == expected:
            ok(f"the four tools are advertised: {', '.join(names)}")
        else:
            bad("the tool list is not what it should be", str(names))

        # The module's security claim, attempted rather than read: if any tool
        # grew a parameter that moves the boundary, this is where it shows.
        widening = {"image", "mount", "mounts", "network", "net", "allow", "mem",
                    "cpu", "pids", "timeout", "isolation", "seccomp", "env", "user", "secrets"}
        offenders = [
            f"{t['name']}.{key}"
            for t in tools
            for key in (t.get("inputSchema", {}).get("properties") or {})
            if key in widening
        ]
        if not offenders:
            ok("no tool lets the model choose an image, a mount, a network or a limit")
        else:
            bad("a tool can widen the sandbox", ", ".join(offenders))

        answer = server.call("tools/call", {"name": "no_such_tool", "arguments": {}})
        if "error" in answer:
            ok("an unknown tool is a protocol error")
        else:
            bad("an unknown tool was accepted", json.dumps(answer)[:300])

        # A tool that ran and failed has to be a *result*: a JSON-RPC error is
        # handled by the host and never reaches the model that could fix it.
        answer = server.tool("run_code", {"language": "python"})
        if "error" not in answer and is_error(answer) and "code" in text_of(answer):
            ok("a tool that failed answers the model rather than the host")
        else:
            bad("a failing tool was reported at the wrong layer", json.dumps(answer)[:300])


def check_running(launcher: str, workspace: str) -> None:
    print("\nrunning code")
    with Server(launcher, "--workspace", workspace) as server:
        server.handshake()

        # Exactly "42": nothing else. Zygo's own progress lines — "pulling
        # python:3.12-slim" — share a descriptor with the sandbox's standard
        # error, and a `run_code` that passed them on would put them in front
        # of a model as something the program said. `zygo run --quiet` is what
        # separates the two, and this is the check that it is being used.
        answer = server.tool("run_code", {"language": "python", "code": "print(6 * 7)"})
        if not is_error(answer) and text_of(answer).strip() == "42":
            ok("python runs, and the answer is the program's output and nothing else")
        else:
            bad("python did not run cleanly", json.dumps(answer)[:600])

        answer = server.tool("run_code", {"language": "sh", "code": "echo $((6 * 7))"})
        if not is_error(answer) and text_of(answer).strip() == "42":
            ok("sh runs, on an image of its own")
        else:
            bad("sh did not run cleanly", json.dumps(answer)[:600])

        answer = server.tool(
            "run_code",
            {"language": "python", "code": "import sys; print(sys.stdin.read().upper())", "stdin": "hello"},
        )
        if text_of(answer).strip() == "HELLO":
            ok("standard input reaches the program")
        else:
            bad("stdin did not arrive", json.dumps(answer)[:600])

        # Two calls, one file: this is what makes a workspace worth having.
        server.tool("run_code", {"language": "python", "code": "open('/work/note.txt','w').write('kept')"})
        answer = server.tool("run_code", {"language": "python", "code": "print(open('/work/note.txt').read())"})
        if text_of(answer).strip() == "kept":
            ok("the workspace persists between calls")
        else:
            bad("the workspace did not survive", json.dumps(answer)[:600])

        # And it is a real directory on the host, which is how an agent hands
        # its work back to the person who asked for it.
        if os.path.exists(os.path.join(workspace, "note.txt")):
            ok("what the sandbox wrote is on the host, where the user can see it")
        else:
            bad("the workspace is not the directory that was named")

        # A program that fails is not a tool that failed: the tool worked, and
        # the exit code is part of what it has to say.
        answer = server.tool("run_code", {"language": "sh", "code": "echo out; echo err >&2; exit 3"})
        body = text_of(answer)
        if "out" in body and "err" in body and "Exit code 3" in body:
            ok("a non-zero exit is reported with both streams, not hidden")
        else:
            bad("a failing program was not reported usefully", json.dumps(answer)[:600])

        answer = server.tool("run_code", {"language": "python", "code": "pass"})
        if "no output" in text_of(answer):
            ok("a silent, successful run still says something")
        else:
            bad("an empty result reads to a model as a broken tool", json.dumps(answer)[:400])


def check_boundary(launcher: str) -> None:
    print("\nthe boundary the model cannot move")
    with Server(launcher) as server:
        server.handshake()

        # Each of these is the sandbox doing its job, attempted from inside.
        # The marker is printed only on success, and it is built at runtime so
        # it cannot appear in a traceback that echoes the source line — which
        # is how the first version of this check "found" a write that never
        # happened.
        answer = server.tool(
            "run_code",
            {
                "language": "python",
                "code": (
                    "marker = 'WRO' + 'TE'\n"
                    "try:\n"
                    "    open('/etc/zygo-probe', 'w').write('x')\n"
                    "    print(marker)\n"
                    "except OSError as e:\n"
                    "    print('refused:', e.strerror)\n"
                ),
            },
        )
        body = text_of(answer)
        if "WROTE" not in body and "Read-only" in body:
            ok("the root filesystem is read-only")
        elif "WROTE" in body:
            bad("a program wrote to the image's root", body[:400])
        else:
            bad("the write failed, but not because the root is read-only", body[:400])

        answer = server.tool(
            "run_code",
            {
                "language": "python",
                "code": (
                    "import socket\n"
                    "s = socket.socket(); s.settimeout(5)\n"
                    "try:\n"
                    "    s.connect(('1.1.1.1', 80)); print('REACHED')\n"
                    "except OSError as e:\n"
                    "    print('refused:', e)\n"
                ),
            },
        )
        if "REACHED" not in text_of(answer):
            ok("there is no network unless the command line said otherwise")
        else:
            bad("the sandbox reached the internet", text_of(answer)[:400])

        answer = server.tool(
            "run_code",
            {"language": "python", "code": "import os; print('caps', open('/proc/self/status').read().count('CapEff:\\t0000000000000000'))"},
        )
        if "caps 1" in text_of(answer):
            ok("the program holds no capabilities")
        else:
            bad("capabilities were not dropped", text_of(answer)[:400])

        # A parameter the schema does not declare must not become a sandbox
        # setting. A host will not send one, but a model that has read a web
        # page might, and "ignored" is the only safe answer.
        answer = server.tool(
            "run_code",
            {
                "language": "python",
                "code": "import os; print('mounted' if os.path.exists('/hostroot') else 'not mounted')",
                "mounts": ["/:/hostroot:rw"],
                "network": "full",
            },
        )
        if "not mounted" in text_of(answer):
            ok("an undeclared parameter is ignored rather than honoured")
        else:
            bad("a parameter outside the schema changed the sandbox", text_of(answer)[:400])


def check_limits(launcher: str) -> None:
    print("\nlimits set on the command line")
    with Server(launcher, "--timeout", "3s", "--mem", "64M") as server:
        server.handshake()

        started = time.monotonic()
        answer = server.tool("run_code", {"language": "sh", "code": "sleep 60; echo FINISHED"}, timeout=400)
        elapsed = time.monotonic() - started
        body = text_of(answer)
        if "FINISHED" not in body and elapsed < 300:
            ok(f"the timeout on the command line killed a long program ({elapsed:.0f}s)")
        else:
            bad("the sleep outlived its timeout", f"{elapsed:.0f}s: {body[:300]}")

        # Attempted, not read. A sandbox has no `/sys/fs/cgroup` of its own to
        # inspect, and reading a limit from outside would pass on a kernel that
        # ignores it. So: allocate past the limit and see whether the kernel
        # ends the process.
        answer = server.tool(
            "run_code",
            {
                "language": "python",
                "code": (
                    "block = bytearray()\n"
                    "for _ in range(200):\n"
                    "    block += bytearray(1024 * 1024)\n"
                    "print('ALLOCATED', len(block) // (1024 * 1024), 'MB')\n"
                ),
            },
            timeout=400,
        )
        body = text_of(answer)
        if "ALLOCATED" not in body:
            ok("the memory limit on the command line is enforced by the kernel")
        else:
            bad("200 MB was allocated under a 64 MB limit", body[:300])

        # The positive case, so the check above cannot pass because nothing
        # ran: the same program well under the limit must succeed.
        answer = server.tool(
            "run_code",
            {"language": "python", "code": "b = bytearray(8 * 1024 * 1024); print('ALLOCATED', len(b))"},
        )
        if "ALLOCATED" in text_of(answer):
            ok("a program that fits inside the limit still runs")
        else:
            bad("the limit refused a program that fits", text_of(answer)[:300])

        # The two kills are both exit 137, so the *reason* has to come from
        # somewhere else — `memory.events` for one, the launcher's own deadline
        # for the other. A caller that can only see the status cannot tell
        # "too slow" from "too much memory", which is the difference between
        # two verdicts for an online judge (UC6).
        starved = server.tool(
            "run_code",
            {
                "language": "python",
                "code": "b = bytearray()\nfor _ in range(200): b += bytearray(1024 * 1024)\n",
            },
            timeout=400,
        )
        slow = server.tool("run_code", {"language": "sh", "code": "sleep 60"}, timeout=400)
        starved_text, slow_text = text_of(starved), text_of(slow)
        if "out of memory" in starved_text and "time limit" in slow_text:
            ok("running out of memory and running out of time read differently")
        else:
            bad(
                "the two kinds of kill are indistinguishable",
                f"memory: {starved_text[:150]} || time: {slow_text[:150]}",
            )


def check_warm_functions(launcher: str) -> None:
    print("\nwarm functions")
    with Server(launcher) as server:
        server.handshake()

        # No supervisor here. The answer has to be a readable sentence telling
        # the model what to do, not a stack trace and not an empty list.
        answer = server.tool("list_functions", {})
        body = text_of(answer)
        if "sandbox.toml" in body or "No warm functions" in body:
            ok("with no supervisor, the model is told how to get one")
        else:
            bad("an unhelpful answer about warm functions", json.dumps(answer)[:400])

        answer = server.tool("call_function", {"name": "nothing", "event": {}})
        if is_error(answer):
            ok("calling a function that is not there is an error the model can read")
        else:
            bad("a missing function was not reported", json.dumps(answer)[:400])


def check_concurrency(launcher: str) -> None:
    print("\nconcurrency")
    with Server(launcher) as server:
        server.handshake()
        # Warm the image cache first, so the slow call's cost is the sleep and
        # not a pull. Without this the "fast" call can be the one that waits.
        server.tool("run_code", {"language": "sh", "code": "true"})

        slow = server.send("tools/call", {"name": "run_code", "arguments": {"language": "sh", "code": "sleep 20"}})
        time.sleep(0.5)
        fast = server.send("ping", {})
        first = server.read(60)
        if first.get("id") == fast:
            ok("a long tool call does not block the next request")
        else:
            bad("requests are answered strictly in order", json.dumps(first)[:300])
        # Drain the slow one so the server is not killed mid-sandbox.
        server.read(400)
        _ = slow


def main() -> int:
    launcher = sys.argv[1]
    workspace = sys.argv[2] if len(sys.argv) > 2 else "/tmp/zygo-mcp-workspace"
    os.makedirs(workspace, exist_ok=True)

    print("zygo mcp — driven over a pipe, as an agent host drives it")
    for check in (
        lambda: check_handshake(launcher),
        lambda: check_tools(launcher),
        lambda: check_running(launcher, workspace),
        lambda: check_boundary(launcher),
        lambda: check_limits(launcher),
        lambda: check_warm_functions(launcher),
        lambda: check_concurrency(launcher),
    ):
        try:
            check()
        except Exception as e:  # noqa: BLE001 - one group failing must not hide the rest
            bad(f"the group raised: {type(e).__name__}", str(e))

    print(f"\n  {PASS} passed, {FAIL} failed")
    return 1 if FAIL else 0


if __name__ == "__main__":
    sys.exit(main())
