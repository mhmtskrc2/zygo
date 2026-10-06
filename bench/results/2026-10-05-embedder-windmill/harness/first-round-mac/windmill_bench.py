#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Windmill, measured the way the embedder's own pipeline test measures it.

    python3 windmill_bench.py [hello|cpu|deps] [SEQ] [BURST]

Sequential: one job at a time through `run_wait_result`, with a random pause
before each, as real traffic arrives. Burst: BURST jobs submitted at once
through the async endpoint, then waited for. Queue wait and execution time come
from Windmill's own job records.
"""
import json
import os
import random
import statistics
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime

BASE = "http://localhost:8088/api"
WS = "bench"
TOKEN = open(os.path.expanduser("~/windmill-bench/.superadmin")).read().strip()

SCRIPTS = {
    "hello": 'def main(title: str):\n    return {"title": title}\n',
    "cpu": 'def main(title: str):\n    s = sum(i * i for i in range(300000))\n    return {"title": title + str(s % 7)}\n',
    "deps": 'import requests\n\ndef main(title: str):\n    return {"title": title, "requests": requests.__version__}\n',
}


def call(method, path, body=None, raw=False):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(BASE + path, data=data, method=method)
    req.add_header("Authorization", "Bearer " + TOKEN)
    if data is not None:
        req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=300) as r:
        text = r.read().decode()
    return text if raw else (json.loads(text) if text else None)


def ensure_script(name):
    try:
        call("POST", f"/w/{WS}/folders/create", {"name": "bench"})
    except Exception:
        pass
    path = f"f/bench/{name}_{int(time.time())}"
    call("POST", f"/w/{WS}/scripts/create", {
        "path": path, "summary": name, "description": "", "content": SCRIPTS[name],
        "language": "python3",
    }, raw=True)
    # Creating a script queues a dependency job that locks it; wait for the lock.
    for _ in range(600):
        s = call("GET", f"/w/{WS}/scripts/get/p/{path}")
        if s.get("lock") is not None or s.get("lock_error_logs"):
            break
        time.sleep(0.2)
    return path


def pct(xs, p):
    s = sorted(xs)
    return s[min(len(s) - 1, round(p / 100 * (len(s) - 1)))]


def ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()


def job(id_):
    return call("GET", f"/w/{WS}/jobs_u/get/{id_}")


def main():
    kind = sys.argv[1] if len(sys.argv) > 1 else "hello"
    seq_n = int(sys.argv[2]) if len(sys.argv) > 2 else 20
    burst_n = int(sys.argv[3]) if len(sys.argv) > 3 else 50

    t = time.time()
    path = ensure_script(kind)
    print(f"Windmill: script={kind} path={path} (created and locked in {time.time() - t:.1f} s)")

    t = time.time()
    first = call("POST", f"/w/{WS}/jobs/run_wait_result/p/{path}", {"title": "warmup"})
    print(f"first run: {(time.time() - t) * 1000:.0f} ms -> {json.dumps(first)[:120]}")

    lat = []
    for i in range(seq_n):
        time.sleep(random.random())
        t0 = time.time()
        call("POST", f"/w/{WS}/jobs/run_wait_result/p/{path}", {"title": f"seq-{i}"})
        lat.append((time.time() - t0) * 1000)
    print(f"sequential n={seq_n}: end-to-end p50={pct(lat, 50):.0f} p95={pct(lat, 95):.0f} "
          f"max={max(lat):.0f} ms")

    t0 = time.time()
    with ThreadPoolExecutor(24) as pool:
        ids = list(pool.map(lambda i: call("POST", f"/w/{WS}/jobs/run/p/{path}",
                                           {"title": f"burst-{i}"}, raw=True).strip('"'),
                            range(burst_n)))
    posted = (time.time() - t0) * 1000
    pending = set(ids)
    while pending:
        for i in list(pending):
            r = call("GET", f"/w/{WS}/jobs_u/completed/get_result_maybe/{i}")
            if r.get("completed"):
                pending.discard(i)
        time.sleep(0.01)
    total = (time.time() - t0) * 1000
    jobs = [job(i) for i in ids]
    ok = sum(1 for j in jobs if j.get("success"))
    wait = [(ts(j["started_at"]) - ts(j["created_at"])) * 1000 for j in jobs]
    dur = [j.get("duration_ms", 0) for j in jobs]
    print(f"burst n={burst_n}: all accepted in {posted:.0f} ms, all done in {total:.0f} ms "
          f"({burst_n * 1000 / total:.2f} jobs/s), ok={ok}/{burst_n}")
    print(f"  queue wait p50={pct(wait, 50):.0f} p95={pct(wait, 95):.0f} ms; "
          f"execution (duration_ms) p50={pct(dur, 50):.0f} p95={pct(dur, 95):.0f} ms")


if __name__ == "__main__":
    main()
