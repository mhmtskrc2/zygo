#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Create the isolation probe as a Windmill script (once) and run it as a job; print its answer."""
import json, os, sys, time, urllib.request
BASE = "http://localhost:8088/api"; WS = "bench"
TOKEN = open(os.path.expanduser("~/windmill-bench/.superadmin")).read().strip()
def call(method, path, body=None, raw=False):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(BASE + path, data=data, method=method)
    req.add_header("Authorization", "Bearer " + TOKEN)
    if data is not None: req.add_header("Content-Type", "application/json")
    with urllib.request.urlopen(req, timeout=300) as r: text = r.read().decode()
    return text if raw else (json.loads(text) if text else None)
path = "f/bench/isolation_probe"
try:
    call("GET", f"/w/{WS}/scripts/get/p/{path}")
except Exception:
    call("POST", f"/w/{WS}/scripts/create", {"path": path, "summary": "probe", "description": "", "content": open(os.path.expanduser("~/windmill-bench/isolation_probe.py")).read(), "language": "python3"}, raw=True)
    for _ in range(600):
        s = call("GET", f"/w/{WS}/scripts/get/p/{path}")
        if s.get("lock") is not None or s.get("lock_error_logs"): break
        time.sleep(0.2)
print(json.dumps(call("POST", f"/w/{WS}/jobs/run_wait_result/p/{path}", {}), indent=1))
