# SPDX-License-Identifier: Apache-2.0
"""What the Python client promises, checked against a stand-in API.

These do not need Linux, a kernel or a sandbox: what is under test is the
client — the transport, the error mapping, the connection pool — and putting a
real supervisor behind it would test the supervisor instead, more slowly and on
one platform.
"""

from __future__ import annotations

import asyncio
import base64
import concurrent.futures
import json
import sys
import time
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

    def test_a_connection_the_server_closed_is_replaced_not_reported(self) -> None:
        # The server closed the kept-alive connection between two calls. On a
        # unix socket the next send fails with EPIPE before anything went out,
        # so the call goes again on a fresh connection instead of failing.
        with FakeApi(unix=True) as api:
            api.answer("POST", "/fn/f", 200, OK_RESULT)
            api.recorder.hang_up = True
            with zygo.connect(api.url) as client:
                for _ in range(3):
                    client.call("f", {})
            self.assertEqual(len(api.requests), 3)

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


class DepsTests(unittest.TestCase):
    ID = "deps_" + "a" * 32

    def test_the_files_go_up_as_base64_and_the_answer_is_an_id(self) -> None:
        # Base64 because a lockfile is not always UTF-8, and JSON has no other
        # way to carry bytes.
        with FakeApi() as api:
            api.answer(
                "POST",
                "/deps",
                202,
                {"id": self.ID, "state": "building", "kind": "python",
                 "image": "python:3.12-slim", "files": {"requirements.txt": 15}},
            )
            with zygo.connect(api.url) as client:
                deps = client.put_deps(
                    "python:3.12-slim", {"requirements.txt": "requests==2.32\n"}
                )
                self.assertEqual(deps.id, self.ID)
                self.assertTrue(deps.building)
                self.assertFalse(deps.ready)

        sent = json.loads(api.requests[0]["raw"])
        self.assertEqual(sent["image"], "python:3.12-slim")
        self.assertEqual(
            base64.b64decode(sent["files"]["requirements.txt"]).decode(),
            "requests==2.32\n",
        )

    def test_a_failed_build_carries_its_log(self) -> None:
        # The reason is on the same object as the state: a caller looking at
        # `failed` wants it, and asking twice is how a client ends up not
        # showing it at all.
        with FakeApi() as api:
            api.answer(
                "GET",
                f"/deps/{self.ID}",
                200,
                {"id": self.ID, "state": "failed", "error": "pip exited 1",
                 "log": "ERROR: No matching distribution found for nosuchpkg"},
            )
            with zygo.connect(api.url) as client:
                deps = client.deps(self.ID)
                self.assertEqual(deps.state, "failed")
                self.assertIn("nosuchpkg", deps.log)
                self.assertFalse(deps.ready)

    def test_a_pool_on_a_dependency_set_that_is_still_building_is_told_to_retry(self) -> None:
        # Not queued and not started: a zygote warmed without the dependencies
        # it was promised serves requests that fail at import.
        with FakeApi() as api:
            api.answer(
                "POST",
                "/runtimes",
                503,
                {"error": "deps_xyz is still building", "code": "deps_building"},
            )
            with zygo.connect(api.url) as client:
                with self.assertRaises(zygo.ZygoError) as raised:
                    client.serve_runtime("pool", {"image": "x"}, deps=self.ID)
                self.assertIn("still building", str(raised.exception))

        sent = json.loads(api.requests[0]["raw"])
        self.assertEqual(sent["deps"], self.ID)


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


class TokenTests(unittest.TestCase):
    """Who a request acts for, and where the answer comes from."""

    def test_a_secret_arrives_once_and_the_listing_never_carries_one(self) -> None:
        with FakeApi() as api:
            api.answer(
                "POST",
                "/tenants/acme/tokens",
                201,
                {
                    "token": {"id": "tok_1a2b3c4d5e6f", "tenant": "acme", "created_ms": 1},
                    "secret": "zygo_deadbeef",
                },
            )
            api.answer(
                "GET",
                "/tokens",
                200,
                {"tokens": [{"id": "tok_1a2b3c4d5e6f", "tenant": "acme", "created_ms": 1}]},
            )
            api.answer("DELETE", "/tokens/tok_1a2b3c4d5e6f", 200, {"revoked": True})
            with zygo.connect(api.url) as client:
                minted = client.mint_token("acme")
                self.assertEqual(minted.secret, "zygo_deadbeef")
                self.assertEqual(minted.token.tenant, "acme")
                self.assertFalse(minted.token.revoked)

                listed = client.tokens()
                self.assertEqual([t.id for t in listed], ["tok_1a2b3c4d5e6f"])
                client.revoke_token("tok_1a2b3c4d5e6f")

        self.assertEqual(api.requests[0]["path"], "/tenants/acme/tokens")

    def test_an_operator_token_is_minted_on_its_own_route(self) -> None:
        # No tenant, so no tenant in the path: an operator token belongs to
        # the host rather than to one of its customers.
        with FakeApi() as api:
            api.answer(
                "POST",
                "/tokens",
                201,
                {"token": {"id": "tok_000000000000", "created_ms": 1}, "secret": "zygo_x"},
            )
            with zygo.connect(api.url) as client:
                minted = client.mint_token()
                self.assertIsNone(minted.token.tenant, "an operator token names nobody")
        self.assertEqual(api.requests[0]["path"], "/tokens")

    def test_acting_for_a_tenant_is_a_header_on_the_same_connection(self) -> None:
        with FakeApi() as api:
            api.answer("GET", "/fn", 200, {"functions": []})
            with zygo.connect(api.url) as client:
                client.functions()
                client.for_tenant("acme").functions()

        self.assertNotIn("x-zygo-tenant", api.requests[0]["headers"])
        self.assertEqual(api.requests[1]["headers"]["x-zygo-tenant"], "acme")


class StuckTests(unittest.TestCase):
    """A sandbox that went quiet, which is not the same as slow."""

    def test_a_stuck_request_is_not_a_timeout(self) -> None:
        with FakeApi() as api:
            api.answer(
                "POST",
                "/fn/render",
                504,
                {
                    "error": "the sandbox stopped reporting this request",
                    "stuck": True,
                    "request_id": "00000009",
                    "metrics": {"wall_ms": 61000.0},
                },
            )
            with zygo.connect(api.url) as client:
                with self.assertRaises(zygo.Stuck) as caught:
                    client.call("render", {})

        # "too slow, raise the limit" is the wrong advice for a request that
        # still had budget when the sandbox stopped answering.
        self.assertNotIsInstance(caught.exception, zygo.Timeout)
        self.assertEqual(caught.exception.request_id, "00000009")


class StreamTests(unittest.TestCase):
    """Output that arrives while the request is still running."""

    LINES = [
        {"stream": "stdout", "data": "page 1\n"},
        {"stream": "progress", "data": "halfway"},
        {"stream": "stderr", "data": "a warning\n"},
        {"status": 200, "result": {"pages": 2}, "stdout": "page 1\n", "stderr": "a warning\n"},
    ]

    def test_a_stream_yields_its_lines_then_exactly_one_result(self) -> None:
        with FakeApi() as api:
            api.stream("POST", "/fn/render", self.LINES)
            with zygo.connect(api.url) as client:
                events = list(client.stream("render", {"pages": 2}))

        self.assertEqual(
            [e.kind for e in events],
            ["stdout", "progress", "stderr", "result"],
        )
        # `progress` is its own kind, not a line of stdout: a caller that had
        # to parse the output to find it would be parsing log messages.
        self.assertEqual(events[1].data, "halfway")
        self.assertTrue(events[-1].is_result)
        self.assertEqual(events[-1].result.result, {"pages": 2})

    def test_lines_are_yielded_as_they_arrive_not_at_the_end(self) -> None:
        """The claim streaming makes, and the only one worth testing.

        The same lines arrive either way; what a stream promises is *when*. So
        the server pauses between them, and the first line has to be in hand
        before the last one has been sent.
        """
        with FakeApi() as api:
            api.stream("POST", "/fn/render", self.LINES, gap=0.3)
            with zygo.connect(api.url) as client:
                began = time.monotonic()
                first = None
                for event in client.stream("render", {}):
                    if first is None:
                        first = time.monotonic() - began
                whole = time.monotonic() - began

        self.assertIsNotNone(first)
        self.assertLess(
            first, whole / 2, f"the first line took {first:.2f}s of {whole:.2f}s"
        )

    def test_a_failed_request_raises_after_its_output_has_been_seen(self) -> None:
        # A handler that printed and then raised produced both, and a caller
        # that is streaming asked to see the first part.
        with FakeApi() as api:
            api.stream(
                "POST",
                "/fn/render",
                [
                    {"stream": "stdout", "data": "starting\n"},
                    {"status": 500, "error": "boom", "exit_code": 1, "stdout": "starting\n"},
                ],
            )
            with zygo.connect(api.url) as client:
                seen = []
                with self.assertRaises(zygo.HandlerError):
                    for event in client.stream("render", {}):
                        seen.append(event.kind)

        self.assertEqual(seen, ["stdout", "result"], "the output was not delivered first")

    def test_a_refusal_is_raised_rather_than_streamed(self) -> None:
        with FakeApi() as api:
            api.answer("POST", "/fn/render", 404, {"error": "no function"})
            with zygo.connect(api.url) as client:
                with self.assertRaises(zygo.NotFound):
                    list(client.stream("render", {}))


class CancelTests(unittest.TestCase):
    """Stopping a request, and what tells a caller that is what happened."""

    def test_a_cancelled_request_is_its_own_exception(self) -> None:
        with FakeApi() as api:
            api.answer(
                "POST",
                "/fn/slow",
                499,
                {
                    "error": "the request was cancelled",
                    "cancelled": True,
                    "request_id": "00000007",
                    "stdout": "",
                    "stderr": "",
                    "metrics": {"wall_ms": 1200.0},
                },
            )
            with zygo.connect(api.url) as client:
                with self.assertRaises(zygo.Cancelled) as caught:
                    client.call("slow", {})
        # Not a Timeout: "too slow, raise the limit" is the wrong advice for
        # a request somebody stopped on purpose.
        self.assertNotIsInstance(caught.exception, zygo.Timeout)
        self.assertEqual(caught.exception.request_id, "00000007")

    def test_a_call_can_be_named_so_that_it_can_be_stopped(self) -> None:
        with FakeApi() as api:
            api.answer("POST", "/fn/slow", 200, {"result": None, "request_id": "00000008"})
            api.answer(
                "DELETE",
                "/requests/job-4711",
                200,
                {"cancelled": True, "request_id": "00000008", "started": True},
            )
            with zygo.connect(api.url) as client:
                out = client.call("slow", {}, key="job-4711")
                self.assertEqual(out.request_id, "00000008")
                answer = client.cancel("job-4711")

        self.assertEqual(api.requests[0]["headers"]["x-zygo-request-key"], "job-4711")
        self.assertTrue(answer["cancelled"])

    def test_cancelling_the_task_cancels_the_request(self) -> None:
        """The reason the key exists.

        The server's own id arrives *with the answer*, so a caller that waits
        for it can no longer stop the call it belongs to. The async client
        names the request on the way in, and turns ``CancelledError`` into a
        ``DELETE`` for that name before it propagates.
        """
        import asyncio

        import zygo.aio

        with FakeApi() as api:
            # Long enough that the task is still waiting when it is cancelled.
            api.recorder.delay = 2.0
            api.answer("POST", "/fn/slow", 200, {"result": None})
            api.answer("DELETE", "/requests/", 200, {"cancelled": True, "started": True})

            async def main() -> None:
                async with zygo.aio.connect(api.url) as client:
                    task = asyncio.ensure_future(client.call("slow", {}))
                    await asyncio.sleep(0.2)
                    task.cancel()
                    with self.assertRaises(asyncio.CancelledError):
                        await task

            asyncio.run(main())

        sent = [r for r in api.requests if r["method"] == "DELETE"]
        self.assertTrue(sent, "the cancelled task sent no cancel")
        key = api.requests[0]["headers"]["x-zygo-request-key"]
        self.assertTrue(key.startswith("k-"), key)
        self.assertEqual(
            sent[0]["path"],
            f"/requests/{key}",
            "the cancel named a different request than the call did",
        )


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

    def test_a_sandbox_that_never_started_is_unavailable_not_the_programs_fault(self) -> None:
        # The first adoption report had to match on the error's text to tell a
        # start failure from a failing program. `started` says it outright.
        with FakeApi() as api:
            api.answer(
                "POST",
                "/run",
                200,
                {
                    "exit_code": 125,
                    "stdout": "",
                    "stderr": "error: this host cannot run sandboxes\n",
                    "timed_out": False,
                    "oom_killed": False,
                    "started": False,
                    "phase": "start",
                },
            )
            with zygo.connect(api.url) as client:
                never = client.run("alpine:3", ["true"])
        self.assertFalse(never.started)
        self.assertEqual(never.phase, "start")
        self.assertFalse(never.ok)

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
        self.assertTrue(run.started, "an older API only answered once the program ran")
        self.assertEqual(run.phase, "run")


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
