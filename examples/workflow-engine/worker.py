#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""A workflow engine's worker, with Zygo underneath.

This is the integration `docs/book/10-similar-projects.md` names and nothing else in the
repository showed: a worker that runs other people's scripts, one process per
run, with the runtime already warm.

The whole of it is two calls:

* **`client.serve(name, layer, if_changed=True)`** — once per *script
  version*. A zygote for that script starts, imports everything it needs, and
  waits. `if_changed=True` makes a repeat free: a script whose source has not
  moved answers `unchanged` without restarting anything, so the worker can
  call this on every job without thinking about it.
* **`client.fn(name)(args)`** — once per *run*. A `fork()` of the warmed
  process, a fresh pid in its own cgroup, and the result back. About 2 ms on
  Linux, against 300 ms to start an interpreter and far more to start a
  container.

Everything else here is the bookkeeping any worker has to do anyway: an LRU
so a worker that has seen ten thousand scripts is not holding ten thousand
zygotes, and a mapping from Zygo's failures to the engine's own job states.

Run it:

    zygo api --allow-deploy &            # the worker talks to this
    python3 worker.py --seed             # queue some work and drain it

Mapping to a real engine:

| | Windmill | n8n | here |
|---|---|---|---|
| queue | `v2_job_queue` in Postgres | BullMQ in Redis | `jobqueue.py`, SQLite |
| script identity | script hash | node + workflow version | `digest` |
| per-run isolation | nsjail, one process per job | `vm2` / task runner | a Zygo fork |
| result | `v2_job_completed` | execution data | `Queue.complete` |

In Windmill the change is in `handle_job` — where it shells out to an
interpreter under nsjail, it calls a warm function instead. In n8n it is a
task runner: the runner process holds the client and every Code node
execution becomes one `client.fn(...)` call.
"""

from __future__ import annotations

import argparse
import collections
import json
import os
import sys
import time
from typing import Dict, Optional, Tuple

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../sdk/python/src"))

import zygo  # noqa: E402

from jobqueue import Job, Queue  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))

# How many scripts this worker keeps warm at once.
#
# A zygote is a held sandbox with an interpreter in it: tens of megabytes of
# shared pages, and the tenant's own imports on top. A worker for a platform
# with ten thousand scripts cannot hold ten thousand of them, so the least
# recently used is stopped when a new one arrives. Zygo's own idle tiering
# does the same thing on a timer; this is the bound that does not depend on
# time passing.
WARM_LIMIT = 8

# The limits every script runs under, whatever it asks for. A workflow engine
# is the one deciding these, not the script author — which is the difference
# between a platform and a script runner.
LIMITS = {
    "mem": "256M",
    "cpu": 1.0,
    "pids": 64,
    "timeout": "30s",
    "seccomp": "strict",
    # No network unless the engine grants it. A script that needs egress gets
    # `network = "egress"` and an `allow` list from the engine's own policy,
    # never from the script.
    "network": "none",
}

# Which image a language runs in. The engine owns this: a script says what it
# is, not what it runs on.
IMAGES = {
    ".py": "python:3.12-slim",
    ".js": "node:22-slim",
}


class Warm:
    """The warm functions this worker is holding, most recently used last."""

    def __init__(self, client: zygo.Client, limit: int = WARM_LIMIT) -> None:
        self._client = client
        self._limit = limit
        # name -> digest of the source that is warm under it
        self._served: "collections.OrderedDict[str, str]" = collections.OrderedDict()

    def ensure(self, job: Job) -> Tuple[str, Optional[zygo.Served]]:
        """Warm `job`'s script if it is not already, and return its name.

        The digest is in the *name*, not only compared against it: two
        versions of a script are two functions, so a run that was queued
        against the old one still gets the old one. A name that is reused
        across versions would give that run whichever version happened to be
        warm.
        """
        name = f"{job.script}-{job.digest}"
        if name in self._served:
            self._served.move_to_end(name)
            return name, None

        suffix = os.path.splitext(job.path)[1]
        image = IMAGES.get(suffix)
        if image is None:
            raise ValueError(f"no image for a `{suffix}` script")

        layer = dict(LIMITS)
        layer["image"] = image
        layer["entry"] = job.path

        served = self._client.serve(name, layer, base_dir=HERE, if_changed=True)
        self._served[name] = job.digest
        self._retire()
        return name, served

    def _retire(self) -> None:
        while len(self._served) > self._limit:
            name, _ = self._served.popitem(last=False)
            try:
                self._client.stop(name)
            except zygo.NotFound:
                pass  # already gone; idle tiering may have taken it

    def stop_all(self) -> None:
        for name in list(self._served):
            try:
                self._client.stop(name)
            except zygo.NotFound:
                pass
        self._served.clear()


def run_one(client: zygo.Client, warm: Warm, queue: Queue, job: Job) -> str:
    """Run one job and record what happened. Returns a one-word outcome."""
    try:
        name, served = warm.ensure(job)
    except (zygo.SpecError, zygo.AuthError, ValueError) as e:
        queue.complete(job, error=f"the script could not be warmed: {e}")
        return "unservable"

    if served is not None and served.change != "unchanged":
        print(
            f"  warmed {name}: {served.runtime}, {served.rss_kb // 1024} MB resident "
            f"after {served.imports_ms:.0f} ms",
            file=sys.stderr,
        )

    try:
        result = client.fn(name)(job.args)
    except zygo.Timeout:
        # The supervisor killed the request's whole process tree at the
        # deadline. A retry is the engine's decision, not Zygo's.
        queue.complete(job, error=f"the run exceeded {LIMITS['timeout']}")
        return "timeout"
    except zygo.Busy as e:
        # Every fork slot for this function is taken, and the queue behind it
        # is full. The engine's own backpressure belongs here; this worker
        # puts the job back and waits as long as the API asked it to.
        queue.release(job)
        time.sleep(e.retry_after)
        return "busy"
    except zygo.HandlerError as e:
        # The script raised. Its traceback, stdout and stderr all come back,
        # which is what a user of the engine needs to see.
        queue.complete(job, error=str(e), stdout=e.stdout, stderr=e.stderr)
        return "failed"

    queue.complete(
        job,
        result=result.result,
        stdout=result.stdout,
        stderr=result.stderr,
        # The handler's own time, as the supervisor measured it — not this
        # worker's, which includes the queue and the HTTP round trip.
        wall_ms=result.metrics.wall_ms,
    )
    return "completed"


def drain(client: zygo.Client, queue: Queue) -> Dict[str, int]:
    """Run every queued job, then stop."""
    warm = Warm(client)
    tally: Dict[str, int] = collections.Counter()
    try:
        while True:
            job = queue.claim()
            if job is None:
                return dict(tally)
            tally[run_one(client, warm, queue, job)] += 1
    finally:
        # A worker that is going away stops what it warmed. A worker that
        # stays would leave them: that is the entire point of them being warm.
        warm.stop_all()


def seed(queue: Queue, runs: int) -> None:
    """Queue `runs` jobs against each example script."""
    orders = [
        {"id": n, "items": [{"sku": f"A{n}", "qty": n % 4 + 1}], "currency": "EUR"}
        for n in range(runs)
    ]
    for order in orders:
        queue.submit("normalise", os.path.join(HERE, "scripts/normalise.py"), order)
        queue.submit("summarise", os.path.join(HERE, "scripts/summarise.js"), order)


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--queue", default=os.path.join(HERE, "jobs.db"))
    parser.add_argument("--seed", type=int, nargs="?", const=25, default=0,
                        help="queue this many runs of each example script first")
    parser.add_argument("--url", default=None, help="the `zygo api` to use")
    args = parser.parse_args(argv)

    queue = Queue(args.queue)
    if args.seed:
        seed(queue, args.seed)
        print(f"queued {args.seed * 2} jobs", file=sys.stderr)

    started = time.monotonic()
    with zygo.connect(args.url) as client:
        tally = drain(client, queue)
    elapsed = time.monotonic() - started

    total = sum(tally.values())
    print(json.dumps(tally, sort_keys=True))
    if total:
        print(
            f"{total} jobs in {elapsed * 1000:.0f} ms "
            f"({elapsed * 1000 / total:.1f} ms each, warming included)",
            file=sys.stderr,
        )
    for row in queue.summary():
        print(f"  {row['script']:<12} {row['state']:<10} {row['n']:>4}  avg {row['avg_ms']} ms",
              file=sys.stderr)
    queue.close()
    return 1 if tally.get("failed") or tally.get("unservable") else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
