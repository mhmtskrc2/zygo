"""Conformance tests for the reference Python agent.

The tests drive the agent from the other side of the wire — they are a
miniature supervisor — so what is verified is the *protocol*, not the internals.
The same harness is what `zygo agent test` will run against third-party agents
(todo.md, phase 2.1).

Run with:  python3 -m unittest discover agents/python
"""

from __future__ import annotations

import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

AGENT = Path(__file__).resolve().parent / "zygo_agent.py"
HEADER = struct.Struct(">I")


class Wire:
    """The supervisor's end of the connection."""

    def __init__(self, conn: socket.socket) -> None:
        self._conn = conn
        self._conn.settimeout(30)

    def send(self, message: dict) -> None:
        body = json.dumps(message).encode()
        self._conn.sendall(HEADER.pack(len(body)) + body)

    def recv(self) -> dict:
        header = self._read(HEADER.size)
        (size,) = HEADER.unpack(header)
        return json.loads(self._read(size))

    def _read(self, count: int) -> bytes:
        chunks = []
        while count:
            chunk = self._conn.recv(count)
            if not chunk:
                raise ConnectionError("agent closed the connection")
            chunks.append(chunk)
            count -= len(chunk)
        return b"".join(chunks)

    def close(self) -> None:
        self._conn.close()


class AgentHarness:
    """Start an agent against a handler and speak the protocol to it."""

    def __init__(self, handler_source: str, mode: str = "function") -> None:
        self._dir = tempfile.TemporaryDirectory()
        root = Path(self._dir.name)

        self.handler_path = root / "handler.py"
        self.handler_path.write_text(textwrap.dedent(handler_source))

        self._sock_path = root / "agent.sock"
        self._listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._listener.bind(str(self._sock_path))
        self._listener.listen(1)
        self._listener.settimeout(30)

        self.proc = subprocess.Popen(
            [sys.executable, str(AGENT), str(self._sock_path), str(self.handler_path), mode],
            stderr=subprocess.PIPE,
        )
        conn, _ = self._listener.accept()
        self.wire = Wire(conn)


    def ready(self) -> dict:
        return self.wire.recv()

    def call(self, event, request_id: str = "req-1", timeout_ms: int = 30_000) -> dict:
        """One full EXEC → FORKED → GO → DONE exchange."""
        self.wire.send(
            {
                "type": "EXEC",
                "id": request_id,
                "event": event,
                "timeout_ms": timeout_ms,
            }
        )

        forked = self.wire.recv()
        assert forked["type"] == "FORKED", forked
        assert forked["id"] == request_id, forked
        # This is where a real supervisor moves the pid into its cgroup.
        self.forked_pid = forked["pid"]

        self.wire.send({"type": "GO", "id": request_id})
        done = self.wire.recv()
        assert done["type"] == "DONE", done
        return done

    def close(self) -> str:
        try:
            self.wire.send({"type": "SHUTDOWN", "grace_ms": 0})
        except OSError:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        stderr = (self.proc.stderr.read() or b"").decode() if self.proc.stderr else ""
        if self.proc.stderr:
            self.proc.stderr.close()
        self.wire.close()
        self._listener.close()
        self._dir.cleanup()
        return stderr


class FdHarness:
    """Start an agent on an already-connected socket, the way the launcher does.

    No socket file exists: the descriptor is inherited across the fork, which is
    how the launcher avoids needing a path inside the sandbox.
    """

    def __init__(self, handler_source: str) -> None:
        self._dir = tempfile.TemporaryDirectory()
        self.handler_path = Path(self._dir.name) / "handler.py"
        self.handler_path.write_text(textwrap.dedent(handler_source))

        ours, theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        theirs.set_inheritable(True)
        self.proc = subprocess.Popen(
            [sys.executable, str(AGENT), "--fd", str(theirs.fileno()),
             str(self.handler_path)],
            pass_fds=(theirs.fileno(),),
            stderr=subprocess.PIPE,
        )
        theirs.close()
        self.wire = Wire(ours)

    def close(self) -> None:
        try:
            self.wire.send({"type": "SHUTDOWN", "grace_ms": 0})
        except OSError:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        if self.proc.stderr:
            self.proc.stderr.close()
        self.wire.close()
        self._dir.cleanup()


class ProtocolTests(unittest.TestCase):
    def harness(self, source: str, mode: str = "function") -> AgentHarness:
        h = AgentHarness(source, mode)
        self.addCleanup(h.close)
        return h

    def test_ready_announces_the_protocol_version(self):
        h = self.harness("def handler(event):\n    return {'ok': True}\n")
        ready = h.ready()

        self.assertEqual(ready["type"], "READY")
        self.assertEqual(ready["proto"], 1)
        self.assertGreater(ready["pid"], 0)
        self.assertGreaterEqual(ready["imports_ms"], 0)
        self.assertTrue(ready["runtime"].startswith("python/"))

    def test_a_request_round_trips(self):
        h = self.harness(
            """
            def handler(event):
                return {"doubled": event["n"] * 2}
            """
        )
        h.ready()

        done = h.call({"n": 21})
        self.assertEqual(done["exit_code"], 0)
        self.assertEqual(done["result"], {"doubled": 42})
        self.assertIsNone(done.get("error"))
        self.assertGreater(done["wall_ms"], 0)

    def test_the_child_waits_for_go_before_running(self):
        """The cgroup window is the whole point of the FORKED/GO handshake."""
        h = self.harness(
            """
            import os
            def handler(event):
                return {"pid": os.getpid()}
            """
        )
        h.ready()

        h.wire.send({"type": "EXEC", "id": "r", "event": {}, "timeout_ms": 5000})
        forked = h.wire.recv()
        self.assertEqual(forked["type"], "FORKED")

        # Before GO the child exists but must not have finished; if it had run
        # to completion, a DONE would already be queued behind FORKED.
        pid = forked["pid"]
        os.kill(pid, 0)  # raises if the process is gone

        h.wire.send({"type": "GO", "id": "r"})
        done = h.wire.recv()
        self.assertEqual(done["result"]["pid"], pid, "the handler ran in the forked child")

    def test_each_request_gets_a_fresh_process(self):
        h = self.harness(
            """
            import os
            def handler(event):
                return os.getpid()
            """
        )
        h.ready()

        first = h.call({}, request_id="a")["result"]
        second = h.call({}, request_id="b")["result"]
        self.assertNotEqual(first, second)

    def test_state_does_not_leak_between_requests(self):
        """The core promise: a request cannot see what the last one left."""
        h = self.harness(
            """
            SEEN = []
            def handler(event):
                SEEN.append(event["x"])
                return SEEN
            """
        )
        h.ready()

        self.assertEqual(h.call({"x": 1}, request_id="a")["result"], [1])
        self.assertEqual(
            h.call({"x": 2}, request_id="b")["result"],
            [2],
            "the second request must not see the first one's append",
        )

    def test_module_level_state_is_shared_read_only(self):
        h = self.harness(
            """
            import time
            LOADED_AT = time.time()
            def handler(event):
                return LOADED_AT
            """
        )
        h.ready()

        first = h.call({}, request_id="a")["result"]
        second = h.call({}, request_id="b")["result"]
        self.assertEqual(first, second, "imports happen once, in the zygote")

    def test_a_handler_exception_becomes_an_error_not_a_crash(self):
        h = self.harness(
            """
            def handler(event):
                raise ValueError("boom")
            """
        )
        h.ready()

        done = h.call({})
        self.assertEqual(done["exit_code"], 1)
        self.assertIn("ValueError: boom", done["error"])
        self.assertIn("Traceback", done["error"])

        # And the zygote is still healthy afterwards.
        h.wire.send({"type": "PING", "seq": 7})
        self.assertEqual(h.wire.recv(), {"type": "PONG", "seq": 7})

    def test_stdout_and_stderr_are_captured_separately(self):
        h = self.harness(
            """
            import sys
            def handler(event):
                print("to stdout")
                print("to stderr", file=sys.stderr)
                return None
            """
        )
        h.ready()

        done = h.call({})
        self.assertEqual(done["stdout"].strip(), "to stdout")
        self.assertEqual(done["stderr"].strip(), "to stderr")

    def test_large_output_is_truncated_and_marked(self):
        h = self.harness(
            """
            def handler(event):
                print("x" * (300 * 1024))
                return None
            """
        )
        h.ready()

        done = h.call({})
        self.assertIn("truncated", done["stdout"])
        self.assertLess(len(done["stdout"]), 300 * 1024)

    def test_a_non_serialisable_result_is_reported(self):
        h = self.harness(
            """
            def handler(event):
                return object()
            """
        )
        h.ready()

        done = h.call({})
        self.assertEqual(done["exit_code"], 1)
        self.assertIn("not JSON", done["error"])

    def test_bytes_results_become_base64(self):
        h = self.harness(
            """
            def handler(event):
                return {"blob": b"hello"}
            """
        )
        h.ready()

        self.assertEqual(h.call({})["result"], {"blob": "aGVsbG8="})

    def test_async_handlers_are_awaited(self):
        h = self.harness(
            """
            async def handler(event):
                return {"async": True}
            """
        )
        h.ready()

        self.assertEqual(h.call({})["result"], {"async": True})

    def test_request_environment_is_exposed_to_the_handler(self):
        h = self.harness(
            """
            import os
            def handler(event):
                return {
                    "id": os.environ.get("ZYGO_REQUEST_ID"),
                    "deadline": os.environ.get("ZYGO_DEADLINE_MS"),
                }
            """
        )
        h.ready()

        done = h.call({}, request_id="abc123", timeout_ms=5000)
        self.assertEqual(done["result"], {"id": "abc123", "deadline": "5000"})

    def test_children_do_not_share_random_state(self):
        """Forks inherit the parent's seeded RNG unless it is reseeded."""
        h = self.harness(
            """
            import random
            def handler(event):
                return random.random()
            """
        )
        h.ready()

        values = {h.call({}, request_id=str(i))["result"] for i in range(4)}
        self.assertEqual(len(values), 4, f"forks produced repeated randomness: {values}")

    def test_a_child_killed_mid_request_is_reported(self):
        """OOM kills and deadline kills both look like this to the agent."""
        h = self.harness(
            """
            import os, signal
            def handler(event):
                os.kill(os.getpid(), signal.SIGKILL)
            """
        )
        h.ready()

        done = h.call({})
        self.assertNotEqual(done["exit_code"], 0)
        self.assertIn("SIGKILL", done["error"])

    def test_metrics_are_reported(self):
        h = self.harness(
            """
            def handler(event):
                sum(range(200000))
                return None
            """
        )
        h.ready()

        done = h.call({})
        self.assertGreater(done["wall_ms"], 0)
        self.assertGreater(done["peak_rss_kb"], 0)
        self.assertGreaterEqual(done["cpu_ms"], 0)

    def test_an_unexpected_message_is_an_error_not_a_hang(self):
        h = self.harness("def handler(event):\n    return None\n")
        h.ready()

        h.wire.send({"type": "NONSENSE", "id": "x"})
        reply = h.wire.recv()
        self.assertEqual(reply["type"], "ERROR")
        self.assertEqual(reply["code"], "bad_message")

    def test_a_handler_that_cannot_be_imported_reports_before_ready(self):
        h = self.harness("this is not valid python(")
        first = h.wire.recv()
        self.assertEqual(first["type"], "ERROR")
        self.assertEqual(first["code"], "handler_load")

    def test_a_missing_handler_function_is_reported(self):
        h = self.harness("x = 1\n")
        first = h.wire.recv()
        self.assertEqual(first["type"], "ERROR")
        self.assertIn("defines no `handler`", first["message"])


class InheritedSocketTests(unittest.TestCase):
    """The launcher hands the agent a connected socket rather than a path."""

    def test_an_agent_on_an_inherited_fd_serves_requests(self):
        h = FdHarness("""
            def handler(event):
                return {"doubled": event["n"] * 2}
            """)
        self.addCleanup(h.close)

        ready = h.wire.recv()
        self.assertEqual(ready["type"], "READY")
        self.assertEqual(ready["proto"], 1)

        h.wire.send({"type": "EXEC", "id": "r", "event": {"n": 21}, "timeout_ms": 30000})
        forked = h.wire.recv()
        self.assertEqual(forked["type"], "FORKED")
        h.wire.send({"type": "GO", "id": "r"})
        done = h.wire.recv()
        self.assertEqual(done["type"], "DONE")
        self.assertEqual(done["result"], {"doubled": 42})


class ForkFallbackTests(unittest.TestCase):
    """Risk R1: a handler that starts threads at import time cannot be forked."""

    def test_threaded_handlers_fall_back_to_spawning(self):
        h = AgentHarness(
            """
            import os, threading, time

            def _spin():
                while True:
                    time.sleep(3600)

            threading.Thread(target=_spin, daemon=True).start()

            def handler(event):
                return {"pid": os.getpid(), "n": event["n"] + 1}
            """
        )
        try:
            ready = h.ready()
            self.assertEqual(ready["type"], "READY")

            done = h.call({"n": 1})
            self.assertEqual(done["exit_code"], 0, done.get("error"))
            self.assertEqual(done["result"]["n"], 2)
            self.assertNotEqual(
                done["result"]["pid"], ready["pid"], "must run in a separate process"
            )

            # Still isolated: a second request gets its own process too.
            other = h.call({"n": 5}, request_id="b")
            self.assertNotEqual(other["result"]["pid"], done["result"]["pid"])
        finally:
            stderr = h.close()
        self.assertIn("falling back to spawn", stderr)


if __name__ == "__main__":
    unittest.main()
