"""What the Python client promises, checked against a stand-in API.

These do not need Linux, a kernel or a sandbox: what is under test is the
client — the transport, the error mapping, the connection pool — and putting a
real supervisor behind it would test the supervisor instead, more slowly and on
one platform.
"""

from __future__ import annotations

import asyncio
import concurrent.futures
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
sys.path.insert(0, str(Path(__file__).resolve().parent))

import zygo  # noqa: E402
import zygo.aio  # noqa: E402
from fake_api import FakeApi  # noqa: E402

OK_RESULT = {
    "result": {"size": [80, 60]},
    "stdout": "resized\n",
    "stderr": "",
    "metrics": {"wall_ms": 1.7, "cpu_ms": 1.2, "peak_rss_kb": 2048},
}


class EndpointTests(unittest.TestCase):
    def test_every_address_form_is_understood(self) -> None:
        from zygo._endpoint import parse

        unix = parse("unix:///run/user/1000/zygo/api.sock")
        self.assertTrue(unix.is_unix)
        self.assertEqual(unix.socket_path, "/run/user/1000/zygo/api.sock")

        tcp = parse("http://10.0.0.4:7700")
        self.assertFalse(tcp.is_unix)
        self.assertEqual((tcp.host, tcp.port, tcp.tls), ("10.0.0.4", 7700, False))

        bare = parse("box:9000")
        self.assertEqual((bare.host, bare.port), ("box", 9000))

        secure = parse("https://zygo.example.com:8443")
        self.assertTrue(secure.tls)

    def test_an_address_that_cannot_work_is_refused_here(self) -> None:
        from zygo._endpoint import parse

        # Reported where the mistake is, rather than as a connection failure
        # thirty seconds later against a host nobody meant.
        with self.assertRaises(ValueError):
            parse("unix://")
        with self.assertRaises(ValueError):
            parse("ftp://host:21")
        with self.assertRaises(ValueError):
            parse("host:not-a-port")


class CallTests(unittest.TestCase):
    def test_a_call_returns_the_handlers_own_value(self) -> None:
        with FakeApi() as api:
            api.answer("POST", "/fn/resize", 200, OK_RESULT)
            with zygo.connect(api.url, token=None) as client:
                out = client.fn("resize")({"url": "http://example.com/a.png"})

        self.assertEqual(out.result, {"size": [80, 60]})
        self.assertEqual(out.stdout, "resized\n")
        self.assertAlmostEqual(out.metrics.wall_ms, 1.7)
        self.assertEqual(api.requests[0]["body"], {"url": "http://example.com/a.png"})

    def test_a_call_works_over_a_unix_socket(self) -> None:
        # The transport the local case actually uses: no port, no token, and
        # permissions the operating system enforces. A client that only ever
        # ran over TCP would fail here and nowhere else.
        with FakeApi(unix=True) as api:
            api.answer("POST", "/fn/resize", 200, OK_RESULT)
            with zygo.connect(api.url) as client:
                out = client.fn("resize")({})
        self.assertEqual(out.result, {"size": [80, 60]})

    def test_the_token_is_sent_and_the_timeout_header_with_it(self) -> None:
        with FakeApi() as api:
            api.answer("POST", "/fn/f", 200, OK_RESULT)
            with zygo.connect(api.url, token="s3cret") as client:
                client.call("f", {}, timeout=2.5)

        headers = api.requests[0]["headers"]
        self.assertEqual(headers["authorization"], "Bearer s3cret")
        self.assertEqual(headers["x-zygo-timeout-ms"], "2500")

    def test_a_name_that_needs_escaping_reaches_the_right_route(self) -> None:
        with FakeApi() as api:
            api.answer("POST", "/fn/a%2Fb", 200, OK_RESULT)
            with zygo.connect(api.url) as client:
                client.call("a/b", {})
        # Without escaping this would be `POST /fn/a/b`, which is a different
        # route and would answer 404 — or, worse, some other function.
        self.assertEqual(api.requests[0]["path"], "/fn/a%2Fb")


class FailureTests(unittest.TestCase):
    """Each kind of failure has to arrive as something a caller can branch on."""

    def test_a_handler_that_raised_carries_its_output(self) -> None:
        with FakeApi() as api:
            api.answer(
                "POST",
                "/fn/f",
                500,
                {
                    "error": "ZeroDivisionError: division by zero",
                    "stdout": "before\n",
                    "stderr": "Traceback...\n",
                    "exit_code": 1,
                    "metrics": {"wall_ms": 2.0},
                },
            )
            with zygo.connect(api.url) as client:
                with self.assertRaises(zygo.HandlerError) as caught:
                    client.call("f", {})

        self.assertIn("ZeroDivisionError", str(caught.exception))
        self.assertEqual(caught.exception.stderr, "Traceback...\n")
        self.assertEqual(caught.exception.exit_code, 1)

    def test_backpressure_is_not_a_failure_of_the_call(self) -> None:
        # A 429 means the request never ran. It has to be distinguishable from
        # a handler that failed, because the answer to one is to retry and the
        # answer to the other is to fix the code.
        with FakeApi() as api:
            api.answer(
                "POST",
                "/fn/f",
                429,
                {"error": "`f` is at its concurrency limit", "in_flight": 4, "queued": 16, "limit": 4},
            )
            with zygo.connect(api.url) as client:
                with self.assertRaises(zygo.Busy) as caught:
                    client.call("f", {})

        self.assertEqual(caught.exception.limit, 4)
        self.assertEqual(caught.exception.retry_after, 3.0)
        self.assertNotIsInstance(caught.exception, zygo.HandlerError)

    def test_a_deadline_kill_is_its_own_type(self) -> None:
        with FakeApi() as api:
            api.answer(
                "POST",
                "/fn/f",
                408,
                {"error": "the request exceeded the function's timeout", "stderr": "killed\n"},
            )
            with zygo.connect(api.url) as client:
                with self.assertRaises(zygo.Timeout) as caught:
                    client.call("f", {})
        self.assertEqual(caught.exception.stderr, "killed\n")

    def test_a_refused_deploy_says_which_flag_turns_it_on(self) -> None:
        with FakeApi() as api:
            api.answer(
                "POST",
                "/run",
                403,
                {"error": "this API may only call functions that are already served\n  -> start it with `zygo api --allow-deploy`"},
            )
            with zygo.connect(api.url) as client:
                with self.assertRaises(zygo.AuthError) as caught:
                    client.run("alpine:3", ["echo", "hi"])
        self.assertIn("--allow-deploy", str(caught.exception))

    def test_an_unreachable_api_is_a_transport_error_not_a_sandbox_one(self) -> None:
        # The distinction matters: nothing ran, so nothing about the sandbox
        # can be concluded from it.
        with zygo.connect("http://127.0.0.1:1", timeout=2.0) as client:
            with self.assertRaises(zygo.TransportError):
                client.functions()


class BatchTests(unittest.TestCase):
    def test_one_refused_event_does_not_hide_the_others(self) -> None:
        # The whole reason `/batch` answers with a status per element. A client
        # that raised on the first bad one would throw away the good answers.
        with FakeApi() as api:
            api.answer(
                "POST",
                "/fn/f/batch",
                200,
                [
                    dict(OK_RESULT, status=200),
                    {"status": 429, "error": "at its limit", "limit": 4},
                    dict(OK_RESULT, status=200),
                ],
            )
            with zygo.connect(api.url) as client:
                answers = client.batch("f", [{}, {}, {}])

        self.assertEqual(len(answers), 3)
        self.assertIsInstance(answers[0], zygo.Result)
        self.assertIsInstance(answers[1], zygo.Busy)
        self.assertIsInstance(answers[2], zygo.Result)


class TransportTests(unittest.TestCase):
    def test_connections_are_reused_between_calls(self) -> None:
        # Keep-alive is what keeps the client's overhead off the warm path: a
        # fresh TCP connection per call would cost more than the request does.
        with FakeApi() as api:
            api.answer("POST", "/fn/f", 200, OK_RESULT)
            with zygo.connect(api.url) as client:
                for _ in range(5):
                    client.call("f", {})
            self.assertEqual(api.connections, 1)

    def test_concurrent_callers_are_concurrent_at_the_socket(self) -> None:
        # A single pooled connection would serialise these, and the elapsed
        # time is the only thing that can tell the difference. Eight calls
        # against a server that holds each for 200 ms take 1.6 s in a queue and
        # about 200 ms in parallel.
        with FakeApi() as api:
            api.answer("POST", "/fn/f", 200, OK_RESULT)
            api.recorder.delay = 0.2
            with zygo.connect(api.url) as client:
                import time

                started = time.monotonic()
                with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                    list(pool.map(lambda _: client.call("f", {}), range(8)))
                elapsed = time.monotonic() - started

        self.assertLess(elapsed, 1.0, "the calls were serialised behind one connection")
        self.assertGreaterEqual(api.connections, 2)


class DeployTests(unittest.TestCase):
    def test_serving_sends_an_absolute_base_directory(self) -> None:
        # The API refuses a relative one, and it is right to: a path in a
        # request body has no meaning on the host that receives it.
        with FakeApi() as api:
            api.answer("PUT", "/fn/resize", 200, {"name": "resize", "change": "started", "warm_ms": 40.0})
            with zygo.connect(api.url) as client:
                served = client.serve("resize", {"entry": "./resize.py"}, base_dir="/srv/app")

        self.assertEqual(served.change, "started")
        self.assertEqual(api.requests[0]["body"]["base_dir"], "/srv/app")

    def test_a_one_shot_run_reports_a_non_zero_exit_without_raising(self) -> None:
        # The sandbox ran; this is what it said. Raising would make it
        # impossible to read the output of a command that failed on purpose.
        with FakeApi() as api:
            api.answer(
                "POST",
                "/run",
                200,
                {"exit_code": 2, "stdout": "", "stderr": "boom\n", "timed_out": False, "wall_ms": 21.0},
            )
            with zygo.connect(api.url) as client:
                run = client.run("alpine:3", ["false"], mem="64M", network="none")

        self.assertFalse(run.ok)
        self.assertEqual(run.exit_code, 2)
        self.assertEqual(run.stderr, "boom\n")
        sent = api.requests[0]["body"]["layer"]
        self.assertEqual(sent["image"], "alpine:3")
        self.assertEqual(sent["mem"], "64M")
        self.assertEqual(sent["network"], "none")


class ScriptTests(unittest.TestCase):
    DIGEST = "sha256:" + "0" * 64

    def test_a_script_is_sent_as_itself_and_its_digest_comes_back(self) -> None:
        # Not JSON around the bytes: the API takes the script as the body, and
        # the name it answers with is the SHA-256 of exactly what was sent.
        source = "def handler(event):\n    return event\n"
        with FakeApi() as api:
            api.answer("PUT", "/scripts", 201, {"sha256": self.DIGEST, "size": 37, "existed": False})
            api.answer("GET", f"/scripts/{self.DIGEST}", 200, {"sha256": self.DIGEST, "size": 37})
            api.answer("DELETE", f"/scripts/{self.DIGEST}", 200, {"deleted": True})
            with zygo.connect(api.url) as client:
                script = client.put_script(source)
                self.assertEqual(script.sha256, self.DIGEST)
                self.assertFalse(script.existed)
                self.assertEqual(client.script(self.DIGEST).size, 37)
                self.assertTrue(client.delete_script(self.DIGEST))

        self.assertEqual(api.requests[0]["raw"], source, "the body is the script itself")
        self.assertTrue(api.requests[0]["headers"]["content-type"].startswith("text/plain"))

    def test_a_script_the_host_does_not_have_is_a_not_found(self) -> None:
        with FakeApi() as api:
            api.answer(
                "GET",
                f"/scripts/{self.DIGEST}",
                404,
                {"error": "no script", "code": "not_found"},
            )
            with zygo.connect(api.url) as client:
                with self.assertRaises(zygo.NotFound):
                    client.script(self.DIGEST)

    def test_something_that_is_not_a_digest_never_reaches_the_wire(self) -> None:
        # A digest goes into the path as it stands, so a value that is not one
        # is named here rather than becoming a request to some other route.
        with FakeApi() as api:
            with zygo.connect(api.url) as client:
                for bad in ["../../etc/passwd", "sha256:nope", "", "SHA256:" + "A" * 64]:
                    with self.assertRaises(zygo.SpecError):
                        client.script(bad)
            self.assertEqual(api.requests, [])


class OutcomeTests(unittest.TestCase):
    """Why a one-shot sandbox ended, which the exit code cannot carry.

    A deadline kill and an out-of-memory kill are both ``SIGKILL``, so both are
    137. A caller deciding between "too slow" and "too much memory" needs the
    two to be different values, not different readings of the same one.
    """

    def test_running_out_of_memory_and_out_of_time_are_different_answers(self) -> None:
        with FakeApi() as api:
            api.answer(
                "POST",
                "/run",
                200,
                {
                    "exit_code": 137,
                    "stdout": "",
                    "stderr": "",
                    "timed_out": False,
                    "oom_killed": True,
                    "peak_rss_kb": 65536,
                    "wall_ms": 412.7,
                },
            )
            with zygo.connect(api.url) as client:
                starved = client.run("python:3.12-slim", ["python3", "-c", "b=bytearray(1<<30)"])

        self.assertEqual(starved.exit_code, 137)
        self.assertTrue(starved.oom_killed)
        self.assertFalse(starved.timed_out)
        self.assertEqual(starved.peak_rss_kb, 65536)
        self.assertFalse(starved.ok)

        with FakeApi() as api:
            api.answer(
                "POST",
                "/run",
                408,
                {"error": "the sandbox exceeded its timeout"},
            )
            with zygo.connect(api.url) as client:
                with self.assertRaises(zygo.Timeout):
                    client.run("alpine:3", ["sleep", "60"])

    def test_an_older_api_that_does_not_report_a_reason_still_parses(self) -> None:
        # The fields were added after the route; a client that required them
        # would fail against a Zygo one release behind.
        with FakeApi() as api:
            api.answer("POST", "/run", 200, {"exit_code": 0, "stdout": "hi\n"})
            with zygo.connect(api.url) as client:
                run = client.run("alpine:3", ["echo", "hi"])
        self.assertTrue(run.ok)
        self.assertFalse(run.oom_killed)
        self.assertEqual(run.peak_rss_kb, 0)


class AsyncTests(unittest.TestCase):
    """The asynchronous client is a second HTTP implementation, so it gets its
    own checks rather than being assumed to match."""

    def test_it_calls_and_parses_like_the_synchronous_one(self) -> None:
        async def exercise() -> None:
            with FakeApi() as api:
                api.answer("POST", "/fn/resize", 200, OK_RESULT)
                api.answer("GET", "/fn", 200, {"functions": [{"name": "resize", "state": "ready"}]})
                async with zygo.aio.connect(api.url) as client:
                    out = await client.fn("resize")({"a": 1})
                    functions = await client.functions()

            self.assertEqual(out.result, {"size": [80, 60]})
            self.assertEqual([f.name for f in functions], ["resize"])

        asyncio.run(exercise())

    def test_it_works_over_a_unix_socket(self) -> None:
        async def exercise() -> None:
            with FakeApi(unix=True) as api:
                api.answer("POST", "/fn/f", 200, OK_RESULT)
                async with zygo.aio.connect(api.url) as client:
                    out = await client.call("f", {})
            self.assertEqual(out.stdout, "resized\n")

        asyncio.run(exercise())

    def test_it_maps_failures_to_the_same_types(self) -> None:
        async def exercise() -> None:
            with FakeApi() as api:
                api.answer("POST", "/fn/f", 429, {"error": "busy", "limit": 2})
                async with zygo.aio.connect(api.url) as client:
                    with self.assertRaises(zygo.Busy):
                        await client.call("f", {})

        asyncio.run(exercise())

    def test_concurrent_tasks_do_not_queue_behind_one_connection(self) -> None:
        async def exercise() -> None:
            with FakeApi() as api:
                api.answer("POST", "/fn/f", 200, OK_RESULT)
                api.recorder.delay = 0.2
                async with zygo.aio.connect(api.url) as client:
                    loop = asyncio.get_running_loop()
                    started = loop.time()
                    await asyncio.gather(*(client.call("f", {}) for _ in range(8)))
                    elapsed = loop.time() - started
            self.assertLess(elapsed, 1.0, "the tasks were serialised behind one connection")

        asyncio.run(exercise())


if __name__ == "__main__":
    unittest.main()


class RuntimePoolTests(unittest.TestCase):
    """The embedder's path: one pool, one registered script, many calls."""

    DIGEST = "sha256:" + "1" * 64

    def test_a_pool_is_registered_listed_called_and_stopped(self) -> None:
        with FakeApi() as api:
            api.answer(
                "POST",
                "/runtimes",
                200,
                {"name": "py312", "runtime": "python/3.12.4", "warm": 2, "change": "started"},
            )
            api.answer(
                "GET",
                "/runtimes",
                200,
                {"runtimes": [{"name": "py312", "warm": 2, "cold": 2, "max_warm": 4}]},
            )
            api.answer(
                "POST",
                "/runtimes/py312/call",
                200,
                {"result": {"ok": True}, "stdout": "", "stderr": "", "metrics": {"wall_ms": 2.1}},
            )
            api.answer("DELETE", "/runtimes/py312", 200, {"stopped": ["py312"]})

            with zygo.connect(api.url) as client:
                served = client.serve_runtime(
                    "py312", {"image": "python:3.12-slim", "agent": "python", "min_warm": 2}
                )
                self.assertEqual(served["warm"], 2)

                pools = client.runtimes()
                self.assertEqual(pools[0].max_warm, 4)
                self.assertEqual(pools[0].name, "py312")

                out = client.run_script("py312", self.DIGEST, {"n": 1})
                self.assertEqual(out.result, {"ok": True})

                # And a one-off, where there is nothing registered to name.
                client.run_script("py312", "def handler(e):\n    return e\n")

                self.assertEqual(client.stop_runtime("py312"), ["py312"])

        self.assertEqual(api.requests[0]["body"]["layer"]["agent"], "python")
        called = api.requests[2]["body"]
        self.assertEqual(called["script"], self.DIGEST, "a digest goes as a string")
        self.assertEqual(called["event"], {"n": 1})
        self.assertTrue(api.requests[3]["body"]["script"]["source"].startswith("def handler"))
