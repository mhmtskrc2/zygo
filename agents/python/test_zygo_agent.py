"""Conformance tests for the reference Python agent.

The tests drive the agent from the other side of the wire — they are a
miniature supervisor — so what is verified is the *protocol*, not the internals.
The same harness is what `zygo agent test` will run against third-party agents.

Run with:  python3 -m unittest discover agents/python
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import socket
import struct
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path

AGENT = Path(__file__).resolve().parent / "zygo_agent.py"
HEADER = struct.Struct(">I")


def digest_of(source: str) -> str:
    """What the supervisor puts in `script.digest`, computed here rather than
    taken from the agent — the two have to agree without sharing code."""
    return "sha256:" + hashlib.sha256(source.encode()).hexdigest()


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

    def __init__(
        self,
        handler_source: str | None,
        mode: str = "function",
        env: dict | None = None,
    ) -> None:
        self._dir = tempfile.TemporaryDirectory()
        root = Path(self._dir.name)
        self.root = root

        # `None` starts a *runtime pool*: an interpreter with no tenant code in
        # it, where every request carries its own script.
        self.handler_path = None
        if handler_source is not None:
            self.handler_path = root / "handler.py"
            self.handler_path.write_text(textwrap.dedent(handler_source))

        self._sock_path = root / "agent.sock"
        self._listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._listener.bind(str(self._sock_path))
        self._listener.listen(1)
        self._listener.settimeout(30)

        argv = [sys.executable, str(AGENT), str(self._sock_path)]
        if self.handler_path is not None:
            argv += [str(self.handler_path), mode]
        self.proc = subprocess.Popen(
            argv,
            stderr=subprocess.PIPE,
            env={**os.environ, **(env or {})},
        )
        conn, _ = self._listener.accept()
        self.wire = Wire(conn)


    def ready(self) -> dict:
        return self.wire.recv()

    def call(
        self,
        event,
        request_id: str = "req-1",
        timeout_ms: int = 30_000,
        script: dict | None = None,
    ) -> dict:
        """One full EXEC → FORKED → GO → DONE exchange."""
        answer = self.call_raw(event, request_id, timeout_ms, script)
        assert answer["type"] == "DONE", answer
        return answer

    def call_raw(
        self,
        event,
        request_id: str = "req-1",
        timeout_ms: int = 30_000,
        script: dict | None = None,
    ) -> dict:
        """The same, for a request whose answer may be an `ERROR`.

        A child that refuses the request — a script whose digest does not match
        — answers `ERROR` rather than `DONE` (§2), so a caller checking that
        has to be able to see it.
        """
        exec_message = {
            "type": "EXEC",
            "id": request_id,
            "event": event,
            "timeout_ms": timeout_ms,
        }
        if script is not None:
            exec_message["script"] = script
        self.wire.send(exec_message)

        forked = self.wire.recv()
        assert forked["type"] == "FORKED", forked
        assert forked["id"] == request_id, forked
        # This is where a real supervisor moves the pid into its cgroup.
        self.forked_pid = forked["pid"]

        self.wire.send({"type": "GO", "id": request_id})
        return self.wire.recv()

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

    def test_the_traceback_starts_at_the_handlers_own_frame(self):
        """The developer's line first, not the agent's.

        A code-first runtime's whole argument is that the developer stays in
        their own code; opening every failure with a frame from `/zygo/agent.py`
        undercuts it on the one output that is read when something is wrong.
        """
        h = self.harness(
            """
            def inner():
                raise ValueError("boom")

            def handler(event):
                inner()
            """
        )
        h.ready()

        done = h.call({})
        self.assertEqual(done["exit_code"], 1)
        lines = done["error"].splitlines()
        self.assertEqual(lines[0], "Traceback (most recent call last):")
        self.assertIn("in handler", lines[1], done["error"])
        self.assertNotIn("run_request", done["error"], done["error"])
        self.assertNotIn("zygo_agent.py", done["error"], done["error"])
        # The handler's own frames are all still there, in order.
        self.assertIn("in inner", done["error"])
        self.assertIn("ValueError: boom", done["error"])

    def test_a_chained_cause_survives_the_trim(self):
        h = self.harness(
            """
            def handler(event):
                try:
                    raise KeyError("missing")
                except KeyError as exc:
                    raise RuntimeError("could not answer") from exc
            """
        )
        h.ready()

        done = h.call({})
        self.assertEqual(done["exit_code"], 1)
        self.assertIn("KeyError: 'missing'", done["error"])
        self.assertIn("direct cause", done["error"])
        self.assertIn("RuntimeError: could not answer", done["error"])
        self.assertNotIn("run_request", done["error"], done["error"])

    def test_an_error_raised_by_the_agent_itself_keeps_its_frames(self):
        """Trimming is for the handler's failures, not the harness's.

        A value that will not serialise is caught after the handler returned,
        so every frame belongs to the agent. Dropping them all would leave a
        traceback with no frames at all.
        """
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
        self.assertIn("Traceback", done["error"])

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

    def test_a_frame_that_is_not_json_is_reported_and_survived(self):
        """Found by `zygo agent test`: this used to kill the agent.

        The length prefix was honoured, so the stream is still sitting at a
        frame boundary and the agent can say so and carry on. Dying here would
        take every request in flight with it.
        """
        h = self.harness("def handler(event):\n    return event\n")
        h.ready()

        body = b"{ this is not json"
        h.wire._conn.sendall(HEADER.pack(len(body)) + body)

        reply = h.wire.recv()
        self.assertEqual(reply["type"], "ERROR")
        self.assertEqual(reply["code"], "bad_message")

        # Still serving.
        self.assertEqual(h.call({"n": 1})["result"], {"n": 1})

    def test_a_frame_that_is_json_but_not_an_object_is_also_reported(self):
        h = self.harness("def handler(event):\n    return event\n")
        h.ready()

        body = b"[1, 2, 3]"
        h.wire._conn.sendall(HEADER.pack(len(body)) + body)

        reply = h.wire.recv()
        self.assertEqual(reply["type"], "ERROR")
        self.assertEqual(reply["code"], "bad_message")
        self.assertEqual(h.call({"n": 2})["result"], {"n": 2})


class FixtureTests(unittest.TestCase):
    """The wire fixtures in spec/fixtures, shared with the Rust suite.

    Two implementations tested against each other drift together; tested
    against one file of bytes, they cannot.
    """

    FIXTURES = Path(__file__).resolve().parents[2] / "spec" / "fixtures" / "protocol-v1.json"

    def fixtures(self):
        with open(self.FIXTURES) as f:
            return json.load(f)

    def test_frames_are_the_exact_bytes_in_both_directions(self):
        from zygo_agent import Framing

        for case in self.fixtures()["frames"]:
            want = bytes.fromhex(case["hex"])
            ours, theirs = socket.socketpair()
            try:
                Framing(ours).send(case["message"])
                theirs.settimeout(5)
                got = theirs.recv(len(want) + 64)
                self.assertEqual(got, want, case["name"] + ": send")

                theirs.sendall(want)
                self.assertEqual(Framing(ours).recv(), case["message"], case["name"] + ": recv")
            finally:
                ours.close()
                theirs.close()

    def test_the_agent_answers_the_supervisor_fixtures_as_specified(self):
        """Every supervisor→agent fixture is sent to a live agent and the reply
        is what the spec says it should be."""
        h = AgentHarness("def handler(event):\n    return event\n")
        self.addCleanup(h.close)
        h.ready()
        for case in self.fixtures()["messages"]:
            if not case["direction"].startswith("supervisor"):
                continue
            message = case["message"]
            kind = message["type"]
            if kind == "SHUTDOWN":
                continue  # last, and covered by the shutdown tests
            h.wire.send(message)
            if kind == "PING":
                self.assertEqual(h.wire.recv(), {"type": "PONG", "seq": message["seq"]}, case["name"])
            elif kind == "EXEC":
                forked = h.wire.recv()
                self.assertEqual(forked["type"], "FORKED", case["name"])
                self.assertEqual(forked["id"], message["id"])
                h.wire.send({"type": "GO", "id": message["id"]})
                done = h.wire.recv()
                self.assertEqual(done["type"], "DONE", case["name"])
                self.assertEqual(done["id"], message["id"])
                if (message.get("script") or {}).get("path"):
                    # The path names a file inside a sandbox, and this harness
                    # has no sandbox. What the fixture pins is the shape on the
                    # wire; `RuntimePoolTests` covers the shape working.
                    self.assertEqual(done["exit_code"], 1, case["name"])
                    self.assertIn("cannot read", done["error"], case["name"])
                elif message.get("workspace"):
                    # Same: `/work/<random>` is a directory the supervisor
                    # makes inside a sandbox, and there is none here. The
                    # agent must say which directory it could not enter
                    # rather than running the request somewhere else —
                    # a handler that wrote its output into the last
                    # request's directory would be worse than a failure.
                    self.assertEqual(done["exit_code"], 1, case["name"])
                    self.assertIn(message["workspace"], done["error"], case["name"])
                else:
                    # Both the echo handler and the inline fixture script
                    # return the event unchanged, so one assertion covers a
                    # request with a script and one without.
                    self.assertEqual(done["result"], message["event"], case["name"])
            elif kind == "GO":
                # A GO for a request that is not in flight: nothing to do, and
                # not an error. The next PING must still be answered.
                h.wire.send({"type": "PING", "seq": 99})
                self.assertEqual(h.wire.recv()["seq"], 99, case["name"])


class ConcurrencyTests(unittest.TestCase):
    """Several requests in flight at once (design doc §3.3).

    The agent used to handle one `EXEC` to completion before reading the next,
    and that was measured to be the ceiling on throughput: with the CPU quota
    lifted, four concurrent callers got the same ~600 requests/s as one caller
    while using 1.5 of 4 cores, and one of them waited 3.7 s for its turn.
    """

    SLEEPER = """
        import time

        def handler(event):
            time.sleep(event["sleep"])
            return {"slept": event["sleep"]}
    """

    def test_a_slow_request_does_not_block_a_fast_one(self):
        # The property in one test: start a slow request, then a fast one, and
        # the fast one must answer first. On a sequential agent the second
        # `EXEC` is not even read until the first has finished.
        agent = AgentHarness(self.SLEEPER)
        self.addCleanup(agent.close)
        agent.ready()

        for request_id, seconds in (("slow", 1.0), ("fast", 0.0)):
            agent.wire.send(
                {
                    "type": "EXEC",
                    "id": request_id,
                    "event": {"sleep": seconds},
                    "timeout_ms": 30_000,
                }
            )
            forked = agent.wire.recv()
            self.assertEqual(forked["type"], "FORKED")
            self.assertEqual(forked["id"], request_id)
            agent.wire.send({"type": "GO", "id": request_id})

        first = agent.wire.recv()
        second = agent.wire.recv()
        self.assertEqual(first["type"], "DONE")
        self.assertEqual(second["type"], "DONE")
        self.assertEqual(
            first["id"],
            "fast",
            "the fast request waited for the slow one; the agent is serialising",
        )
        self.assertEqual(second["id"], "slow")

    def test_a_child_does_not_hold_another_requests_result_pipe(self):
        # The failure this guards is a deadlock, not a slowdown: a child forked
        # while another request is in flight inherits that request's pipe, so
        # its reader never reaches end of file. The first request would then
        # wait for a handler it has nothing to do with.
        #
        # Ordering the sleeps the other way round from the test above is what
        # exposes it: the *first* request finishes first, and can only do so if
        # the second child is not holding its pipe open.
        agent = AgentHarness(self.SLEEPER)
        self.addCleanup(agent.close)
        agent.ready()

        agent.wire.send(
            {"type": "EXEC", "id": "quick", "event": {"sleep": 0.0}, "timeout_ms": 30_000}
        )
        quick_forked = agent.wire.recv()
        self.assertEqual(quick_forked["id"], "quick")

        # Fork the long one *before* releasing the quick one, so it is
        # guaranteed to inherit whatever was open at that moment.
        agent.wire.send(
            {"type": "EXEC", "id": "long", "event": {"sleep": 1.5}, "timeout_ms": 30_000}
        )
        long_forked = agent.wire.recv()
        self.assertEqual(long_forked["id"], "long")

        agent.wire.send({"type": "GO", "id": "quick"})
        agent.wire.send({"type": "GO", "id": "long"})

        started = time.monotonic()
        first = agent.wire.recv()
        elapsed = time.monotonic() - started
        self.assertEqual(first["id"], "quick")
        self.assertLess(
            elapsed,
            1.0,
            "the quick request waited for the long one's child to exit, so its "
            "result pipe was being held open by a process that inherited it",
        )
        self.assertEqual(agent.wire.recv()["id"], "long")

    def test_results_are_not_mixed_up_between_requests(self):
        # Replies come back out of order, so each one has to carry enough to be
        # matched to its caller.
        agent = AgentHarness(
            """
            def handler(event):
                return {"echo": event["n"]}
            """
        )
        self.addCleanup(agent.close)
        agent.ready()

        for n in range(8):
            agent.wire.send(
                {"type": "EXEC", "id": f"r{n}", "event": {"n": n}, "timeout_ms": 30_000}
            )

        # No assumption about order: once requests overlap, the next frame may
        # be anyone's `FORKED` or anyone's `DONE`. A supervisor has to route by
        # id, and so does this.
        seen = {}
        while len(seen) < 8:
            message = agent.wire.recv()
            if message["type"] == "FORKED":
                agent.wire.send({"type": "GO", "id": message["id"]})
            elif message["type"] == "DONE":
                seen[message["id"]] = message["result"]["echo"]
            else:
                self.fail(f"unexpected {message}")
        self.assertEqual(seen, {f"r{n}": n for n in range(8)})

    def test_a_shutdown_still_answers_what_is_already_in_flight(self):
        # Otherwise a `zygo stop` during a request loses its answer.
        agent = AgentHarness(self.SLEEPER)
        self.addCleanup(agent.close)
        agent.ready()

        agent.wire.send(
            {"type": "EXEC", "id": "inflight", "event": {"sleep": 0.3}, "timeout_ms": 30_000}
        )
        self.assertEqual(agent.wire.recv()["id"], "inflight")
        agent.wire.send({"type": "GO", "id": "inflight"})
        agent.wire.send({"type": "SHUTDOWN", "grace_ms": 5_000})

        done = agent.wire.recv()
        self.assertEqual(done["type"], "DONE")
        self.assertEqual(done["id"], "inflight")


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

    def test_the_spawn_fallback_answers_everything_while_it_waits_for_go(self):
        """B-09: the fallback took the next frame and demanded it be `GO`.

        Anything else — a second `EXEC`, a `PING`, a `GO` for another request
        — killed the worker and answered nothing at all, so the supervisor
        waited out the whole deadline for a reply that was never coming.
        `zygo agent test`'s own concurrency check sends two `EXEC`s before any
        `GO`, so every handler that fell back to spawning failed conformance
        for this reason.

        Driven by hand rather than through `call`, because what is under test
        is exactly the frames `call` does not send.
        """
        h = AgentHarness(
            """
            import os, threading, time

            def _spin():
                while True:
                    time.sleep(3600)

            threading.Thread(target=_spin, daemon=True).start()

            def handler(event):
                return {"n": event["n"] + 1}
            """
        )
        try:
            self.assertEqual(h.ready()["type"], "READY")

            h.wire.send({"type": "EXEC", "id": "a", "event": {"n": 1}, "timeout_ms": 30_000})
            forked = h.wire.recv()
            self.assertEqual(forked["type"], "FORKED")

            # A second request arrives before `a` is released. It must be
            # *answered* — a supervisor that got silence would hold the slot
            # until the deadline.
            h.wire.send({"type": "EXEC", "id": "b", "event": {"n": 9}, "timeout_ms": 30_000})
            busy = h.wire.recv()
            self.assertEqual(busy["type"], "ERROR", busy)
            self.assertEqual(busy["id"], "b")
            self.assertEqual(busy["code"], "overloaded", busy)

            # And liveness still works while a request is parked.
            h.wire.send({"type": "PING", "seq": 7})
            pong = h.wire.recv()
            self.assertEqual(pong["type"], "PONG", pong)
            self.assertEqual(pong["seq"], 7)

            # A `GO` for a request nobody is running is reported, not obeyed.
            h.wire.send({"type": "GO", "id": "nonexistent"})
            confused = h.wire.recv()
            self.assertEqual(confused["type"], "ERROR", confused)
            self.assertEqual(confused["code"], "bad_message", confused)

            # The original request is still parked, and still runs.
            h.wire.send({"type": "GO", "id": "a"})
            done = h.wire.recv()
            self.assertEqual(done["type"], "DONE", done)
            self.assertEqual(done["id"], "a")
            self.assertEqual(done["exit_code"], 0, done.get("error"))
            self.assertEqual(done["result"], {"n": 2})
        finally:
            stderr = h.close()
        self.assertIn("falling back to spawn", stderr)


def _execve_denying_filter() -> bytes | None:
    """A seven-instruction seccomp program that refuses `execve` with EPERM.

    Hand-assembled here so the test depends on nothing but the kernel's ABI:
    check the architecture (kill on a mismatch), load the syscall number, deny
    `execve`, allow the rest. The numbers are per architecture; `None` on one
    this test does not know.
    """
    import platform

    numbers = {"x86_64": (0xC000003E, 59), "aarch64": (0xC00000B7, 221)}
    known = numbers.get(platform.machine())
    if known is None:
        return None
    audit_arch, execve = known
    ld_abs, jeq, ret = 0x20, 0x15, 0x06
    allow, kill, eperm = 0x7FFF0000, 0x80000000, 0x00050001
    insn = struct.Struct("HBBI")  # struct sock_filter, native order
    return b"".join(
        insn.pack(*i)
        for i in [
            (ld_abs, 0, 0, 4),  # A = arch
            (jeq, 1, 0, audit_arch),  # ours? skip the kill
            (ret, 0, 0, kill),
            (ld_abs, 0, 0, 0),  # A = syscall number
            (jeq, 0, 1, execve),  # execve? fall into the deny
            (ret, 0, 0, eperm),
            (ret, 0, 0, allow),
        ]
    )


@unittest.skipUnless(sys.platform == "linux", "seccomp is a Linux facility")
class ChildFilterTests(unittest.TestCase):
    """`ZYGO_CHILD_SECCOMP`: the supervisor's tightening of the forked child.

    The filter is installed by the agent, after `fork()` and before the
    handler, so the only honest test drives the real agent: first without the
    variable, to prove the handler *can* start a program, then with it.
    """

    SPAWNER = """
        import subprocess

        def handler(event):
            out = subprocess.run(["/bin/echo", "spawned"], capture_output=True, text=True)
            return {"spawned": out.stdout.strip()}
        """

    def setUp(self):
        self.raw = _execve_denying_filter()
        if self.raw is None:
            self.skipTest("no syscall numbers for this architecture in the test")
        import base64

        self.env = {"ZYGO_CHILD_SECCOMP": base64.b64encode(self.raw).decode()}

    def test_without_the_variable_the_handler_can_start_a_program(self):
        h = AgentHarness(self.SPAWNER)
        try:
            self.assertEqual(h.ready()["type"], "READY")
            done = h.call({})
            self.assertEqual(done["exit_code"], 0, done.get("error"))
            self.assertEqual(done["result"], {"spawned": "spawned"})
        finally:
            h.close()

    def test_with_the_variable_the_child_cannot_execve_and_the_agent_survives(self):
        h = AgentHarness(self.SPAWNER, env=self.env)
        try:
            self.assertEqual(h.ready()["type"], "READY")
            done = h.call({})
            self.assertEqual(done["exit_code"], 1, done)
            self.assertIn("PermissionError", done["error"], done["error"])
            # The filter lived and died with that child: the zygote is
            # untouched and the next request is answered the same way.
            again = h.call({}, request_id="b")
            self.assertEqual(again["exit_code"], 1, again)
            self.assertIn("PermissionError", again["error"])
        finally:
            h.close()

    def test_the_filter_does_not_stop_threads_or_ordinary_work(self):
        h = AgentHarness(
            """
            import threading

            def handler(event):
                seen = []
                t = threading.Thread(target=lambda: seen.append(1))
                t.start()
                t.join()
                with open("/dev/null") as f:
                    f.read()
                return {"threads": len(seen)}
            """,
            env=self.env,
        )
        try:
            self.assertEqual(h.ready()["type"], "READY")
            done = h.call({})
            self.assertEqual(done["exit_code"], 0, done.get("error"))
            self.assertEqual(done["result"], {"threads": 1})
        finally:
            h.close()

    def test_a_malformed_filter_is_a_start_up_failure_not_a_silent_skip(self):
        h = AgentHarness(self.SPAWNER, env={"ZYGO_CHILD_SECCOMP": "not base64!!"})
        try:
            first = h.ready()
            self.assertEqual(first["type"], "ERROR", first)
            self.assertIn("ZYGO_CHILD_SECCOMP", first["message"])
        finally:
            h.close()

    def test_the_spawn_fallback_installs_it_too(self):
        # A handler with an import-time thread cannot be forked; each request
        # is a fresh interpreter, and that interpreter is the child now.
        h = AgentHarness(
            """
            import subprocess, threading, time

            threading.Thread(target=lambda: time.sleep(3600), daemon=True).start()

            def handler(event):
                subprocess.run(["/bin/true"])
                return {"spawned": True}
            """,
            env=self.env,
        )
        try:
            self.assertEqual(h.ready()["type"], "READY")
            done = h.call({})
            self.assertEqual(done["exit_code"], 1, done)
            self.assertIn("PermissionError", done["error"], done["error"])
        finally:
            stderr = h.close()
        self.assertIn("falling back to spawn", stderr)


if __name__ == "__main__":
    unittest.main()


class RuntimePoolTests(unittest.TestCase):
    """Protocol 1.1: one zygote, many scripts.

    The shape an embedder needs. A zygote is an interpreter and its dependency
    set — no tenant code at all — and each request carries the script it wants
    run. What has to be true for that to be safe is exactly one thing, and
    every test here is a way of asking it: **the zygote never holds anything a
    request put there.**
    """

    ECHO = """
        CONSTANT = "script-one"

        def handler(event):
            return {"from": CONSTANT, "event": event}
        """

    def test_a_pool_starts_with_no_handler_and_runs_a_script_from_the_request(self):
        h = AgentHarness(None)
        try:
            ready = h.ready()
            self.assertEqual(ready["type"], "READY", ready)
            done = h.call({"n": 1}, script={"source": textwrap.dedent(self.ECHO)})
            self.assertEqual(done["exit_code"], 0, done.get("error"))
            self.assertEqual(done["result"], {"from": "script-one", "event": {"n": 1}})
        finally:
            h.close()

    def test_two_scripts_in_one_pool_cannot_see_each_other(self):
        """The property the whole design rests on.

        Script A defines a global and writes into a module it imports; script
        B looks for both. If the zygote kept anything, B finds it.
        """
        leaker = """
            import json

            LEAKED = "from-a"
            json.zygo_leak = "from-a"

            def handler(event):
                return {"wrote": True}
            """
        looker = """
            import json

            def handler(event):
                return {
                    "saw_global": "LEAKED" in globals(),
                    "saw_module_attr": hasattr(json, "zygo_leak"),
                }
            """
        h = AgentHarness(None)
        try:
            h.ready()
            first = h.call({}, request_id="a", script={"source": textwrap.dedent(leaker)})
            self.assertEqual(first["exit_code"], 0, first.get("error"))

            second = h.call({}, request_id="b", script={"source": textwrap.dedent(looker)})
            self.assertEqual(second["exit_code"], 0, second.get("error"))
            self.assertEqual(
                second["result"],
                {"saw_global": False, "saw_module_attr": False},
                "the zygote carried something from one script to the next",
            )
        finally:
            h.close()

    def test_a_script_can_arrive_as_a_path(self):
        script = self.root_script("by-path", self.ECHO.replace("script-one", "by-path"))
        h = AgentHarness(None)
        try:
            h.ready()
            done = h.call({}, script={"path": str(script)})
            self.assertEqual(done["exit_code"], 0, done.get("error"))
            self.assertEqual(done["result"]["from"], "by-path")
        finally:
            h.close()

    def test_a_script_whose_bytes_do_not_match_its_digest_is_refused(self):
        """The check that makes `path` delivery safe on a shared uid.

        A sandbox has one uid, so the child about to load `/run/script/<hash>`
        can unlink it and write its own there instead — for itself, or for
        another tenant's request in flight on the same pool zygote. The digest
        comes over the supervisor's connection, which nothing inside can reach.
        So the swap is caught, and the request is refused rather than run.
        """
        planted = self.root_script("swapped", "def handler(event):\n    return 'mine'\n")
        h = AgentHarness(None)
        try:
            h.ready()
            answer = h.call_raw(
                {},
                script={"path": str(planted), "digest": digest_of("the script asked for")},
            )
            self.assertEqual(answer["type"], "ERROR", answer)
            self.assertEqual(answer["code"], "handler_load", answer)
            self.assertEqual(answer["id"], "req-1", answer)
            self.assertIn("refusing to run it", answer["message"])

            # The pool is untouched: a refused request is one request.
            good = h.call(
                {}, request_id="after", script={"source": textwrap.dedent(self.ECHO)}
            )
            self.assertEqual(good["result"]["from"], "script-one")
        finally:
            h.close()

    def test_a_script_that_matches_its_digest_runs(self):
        """The positive path, without which the test above proves nothing."""
        source = textwrap.dedent(self.ECHO)
        h = AgentHarness(None)
        try:
            h.ready()
            done = h.call({}, script={"source": source, "digest": digest_of(source)})
            self.assertEqual(done["exit_code"], 0, done.get("error"))
            self.assertEqual(done["result"]["from"], "script-one")

            by_path = self.root_script("digested", self.ECHO)
            done = h.call(
                {},
                request_id="by-path",
                script={
                    "path": str(by_path),
                    "digest": digest_of(by_path.read_text()),
                },
            )
            self.assertEqual(done["exit_code"], 0, done.get("error"))
        finally:
            h.close()

    def test_an_inline_script_is_checked_against_its_digest_too(self):
        """One rule for both shapes, so there is one thing to get right."""
        h = AgentHarness(None)
        try:
            h.ready()
            answer = h.call_raw(
                {},
                script={
                    "source": textwrap.dedent(self.ECHO),
                    "digest": digest_of("something else entirely"),
                },
            )
            self.assertEqual(answer["type"], "ERROR", answer)
            self.assertEqual(answer["code"], "handler_load", answer)
        finally:
            h.close()

    def test_a_script_with_no_digest_is_run_as_it_always_was(self):
        """The field is optional; an `EXEC` without one still works."""
        h = AgentHarness(None)
        try:
            h.ready()
            done = h.call({}, script={"source": textwrap.dedent(self.ECHO)})
            self.assertEqual(done["exit_code"], 0, done.get("error"))
        finally:
            h.close()

    def test_a_named_entry_point_is_honoured(self):
        source = """
            def main(event):
                return {"called": "main"}
            """
        h = AgentHarness(None)
        try:
            h.ready()
            done = h.call(
                {},
                script={"source": textwrap.dedent(source), "entry_point": "main"},
            )
            self.assertEqual(done["result"], {"called": "main"})
        finally:
            h.close()

    def test_a_script_that_does_not_load_fails_its_request_and_not_the_pool(self):
        h = AgentHarness(None)
        try:
            h.ready()
            broken = h.call({}, request_id="bad", script={"source": "def handler(  \n"})
            self.assertEqual(broken["exit_code"], 1, broken)
            self.assertIn("did not load", broken["error"])

            # And the pool is untouched: the next request is served normally.
            good = h.call(
                {}, request_id="good", script={"source": textwrap.dedent(self.ECHO)}
            )
            self.assertEqual(good["exit_code"], 0, good.get("error"))
            self.assertEqual(good["result"]["from"], "script-one")
        finally:
            h.close()

    def test_a_script_with_no_entry_point_says_which_one_was_missing(self):
        h = AgentHarness(None)
        try:
            h.ready()
            done = h.call({}, script={"source": "X = 1\n"})
            self.assertEqual(done["exit_code"], 1, done)
            self.assertIn("handler", done["error"])
        finally:
            h.close()

    def test_a_pool_refuses_a_request_that_carries_no_script(self):
        h = AgentHarness(None)
        try:
            h.ready()
            done = h.call({})
            self.assertEqual(done["exit_code"], 1, done)
            self.assertIn("must carry a `script`", done["error"])
        finally:
            h.close()

    def test_a_handler_backed_agent_still_ignores_a_script_free_request(self):
        """The original shape has to keep working, unchanged."""
        h = AgentHarness(self.ECHO)
        try:
            h.ready()
            done = h.call({"n": 2})
            self.assertEqual(done["exit_code"], 0, done.get("error"))
            self.assertEqual(done["result"]["from"], "script-one")
        finally:
            h.close()

    def test_a_script_in_the_request_overrides_the_agents_own_handler(self):
        """A pool is the usual reason to send a script, but not the only one.

        An agent warmed with a handler can still be asked to run something
        else, which is what makes a "warm this, mostly" deployment possible.
        """
        h = AgentHarness(self.ECHO)
        try:
            h.ready()
            other = """
                def handler(event):
                    return {"from": "the-request"}
                """
            done = h.call({}, script={"source": textwrap.dedent(other)})
            self.assertEqual(done["result"], {"from": "the-request"})

            # And the agent's own handler is still there afterwards.
            back = h.call({}, request_id="back")
            self.assertEqual(back["result"]["from"], "script-one")
        finally:
            h.close()

    def root_script(self, name: str, source: str) -> Path:
        directory = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, directory, True)
        path = directory / f"{name}.py"
        path.write_text(textwrap.dedent(source))
        return path
