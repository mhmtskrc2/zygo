#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Load generator and meter for n8n, run on the machine n8n runs on.

    python3 bench.py STACK WORKFLOW seq N          one at a time, random 0-0.5 s pause before each
    python3 bench.py STACK WORKFLOW burst N C      N requests at once from C connections
    python3 bench.py STACK WORKFLOW rate R SECS    an open-loop stream of R requests a second
    python3 bench.py STACK - idle                  memory of every part, now

Latency is measured here: request sent -> response read. CPU is read from each
part's cgroup before and after; memory is the peak of each part's anonymous
memory (memory.stat `anon`), sampled every 20 ms. One JSON line per run is
appended to work/results.jsonl.

Each part is found by what it is, not where it is expected to be: a container
by its init's cgroup, a user unit by the cgroup systemd reports for it. So the
numbers are the same whether docker uses the systemd or the cgroupfs driver.
The load generator's own CPU, and everything else the machine did, are
reported beside them (`loadgen_ms_per_exec`, `other_ms_per_exec`), so a run
disturbed by something else says so.
"""
import http.client
import json
import os
import random
import statistics
import subprocess
import sys
import threading
import time

CG = "/sys/fs/cgroup"
HERE = os.path.dirname(os.path.abspath(__file__))
WORK = os.path.join(HERE, "work")
DOCKER = os.environ.get("DOCKER", "docker").split()
ROWS = json.dumps({"rows": [{"id": i, "name": "item-%d" % i, "qty": i % 7 + 1,
                             "price": round(1.5 + i % 13, 2)} for i in range(1000)]}).encode()


def cgroup_of_pid(pid):
    try:
        rel = open(f"/proc/{pid}/cgroup").read().strip().split("::", 1)[1]
    except (OSError, IndexError):
        return None
    return CG + rel


def container(name):
    try:
        pid = subprocess.run(DOCKER + ["inspect", "-f", "{{.State.Pid}}", name],
                             capture_output=True, text=True, check=True).stdout.strip()
    except subprocess.CalledProcessError:
        return None
    return cgroup_of_pid(pid) if pid not in ("", "0") else None


def unit(name):
    out = subprocess.run(["systemctl", "--user", "show", "-p", "ControlGroup", "--value", name],
                         capture_output=True, text=True).stdout.strip()
    return CG + out if out else None


def groups():
    g = {"n8n": container("n8n"), "stock runner": container("n8n-runners"),
         "zygo": unit("n8n-zygo-sup.service"),
         "zygo api": unit("n8n-zygo-api.service"),
         "runner (zygo_runner.py)": unit("n8n-zygo-runner.service"),
         "runners in one sandbox": unit("n8n-runner-box.service")}
    return {k: v for k, v in g.items() if v and os.path.isdir(v) and has_procs(v)}


def nested(gs):
    """For each part, the other parts inside it: their numbers are subtracted
    so that nothing is counted twice (Zygo's slice sits inside whichever unit
    first started the supervisor)."""
    return {k: [j for j, q in gs.items() if j != k and q.startswith(p + "/")] for k, p in gs.items()}


def own(values, inner):
    return {k: v - sum(values[j] for j in inner[k]) for k, v in values.items()}


def has_procs(path):
    for root, _dirs, files in os.walk(path):
        if "cgroup.procs" in files:
            try:
                if open(os.path.join(root, "cgroup.procs")).read().strip():
                    return True
            except OSError:
                pass
    return False


def host_busy_usec():
    f = [int(x) for x in open("/proc/stat").readline().split()[1:]]
    idle = f[3] + f[4]
    return (sum(f[:8]) - idle) * 1_000_000 // os.sysconf("SC_CLK_TCK")


def self_usec():
    t = os.times()
    return int((t.user + t.system) * 1_000_000)


def cpu_usec(path):
    for line in open(f"{path}/cpu.stat"):
        k, v = line.split()
        if k == "usage_usec":
            return int(v)
    return 0


def anon(path):
    for line in open(f"{path}/memory.stat"):
        k, v = line.split()
        if k == "anon":
            return int(v)
    return 0


class Sampler(threading.Thread):
    def __init__(self, gs):
        super().__init__(daemon=True)
        self.gs, self.stop = gs, False
        self.peak = own({k: anon(p) for k, p in gs.items()}, nested(gs))

    def run(self):
        inner = nested(self.gs)
        while not self.stop:
            try:
                now = own({k: anon(p) for k, p in self.gs.items()}, inner)
            except OSError:
                time.sleep(0.02)
                continue
            for k, a in now.items():
                if a > self.peak[k]:
                    self.peak[k] = a
            time.sleep(0.02)


def one(conn_box, wf):
    body = ROWS if wf.startswith("items") else b'{"a":1}'
    t = time.perf_counter()
    for attempt in (0, 1):
        try:
            c = conn_box[0]
            c.request("POST", f"/webhook/{wf}", body=body, headers={"content-type": "application/json"})
            r = c.getresponse()
            data = r.read()
            ok = r.status == 200 and b"Error in workflow" not in data
            return (time.perf_counter() - t) * 1000, ok, r.status
        except (http.client.HTTPException, OSError):
            conn_box[0] = http.client.HTTPConnection("127.0.0.1", 5678, timeout=120)
            if attempt:
                return (time.perf_counter() - t) * 1000, False, 0


def conn():
    return [http.client.HTTPConnection("127.0.0.1", 5678, timeout=120)]


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def main():
    os.makedirs(WORK, exist_ok=True)
    stack, wf, mode = sys.argv[1], sys.argv[2], sys.argv[3]
    args = [float(a) for a in sys.argv[4:]]
    gs = groups()
    rec = {"stack": stack, "workflow": wf, "mode": mode, "args": args, "at": time.strftime("%H:%M:%S")}
    if mode == "idle":
        inner = nested(gs)
        rec["anon_mb"] = {k: round(v / 2**20, 1) for k, v in own({k: anon(p) for k, p in gs.items()}, inner).items()}
        rec["current_mb"] = {k: round(v / 2**20, 1) for k, v in own({k: int(open(f"{p}/memory.current").read()) for k, p in gs.items()}, inner).items()}
        print(json.dumps(rec))
        open(os.path.join(WORK, "results.jsonl"), "a").write(json.dumps(rec) + "\n")
        return
    lat, oks, codes = [], 0, {}
    lock = threading.Lock()
    inner = nested(gs)
    base_anon = own({k: anon(p) for k, p in gs.items()}, inner)
    cpu0 = own({k: cpu_usec(p) for k, p in gs.items()}, inner)
    s = Sampler(gs)
    s.start()
    host0, me0 = host_busy_usec(), self_usec()
    t0 = time.perf_counter()
    if mode == "seq":
        box = conn()
        for _ in range(int(args[0])):
            time.sleep(random.uniform(0, 0.5))
            ms, ok, code = one(box, wf)
            lat.append(ms); oks += ok; codes[code] = codes.get(code, 0) + 1
        n = int(args[0])
    elif mode == "burst":
        n, c = int(args[0]), int(args[1])
        todo = list(range(n))
        go = threading.Event()

        def worker():
            nonlocal oks
            box = conn()
            go.wait()
            while True:
                with lock:
                    if not todo:
                        return
                    todo.pop()
                ms, ok, code = one(box, wf)
                with lock:
                    lat.append(ms); oks += ok; codes[code] = codes.get(code, 0) + 1
        ts = [threading.Thread(target=worker) for _ in range(c)]
        for t in ts:
            t.start()
        t0 = time.perf_counter()
        go.set()
        for t in ts:
            t.join()
    elif mode == "rate":
        r, secs = args
        n = int(r * secs)
        threads = []

        def fire():
            nonlocal oks
            ms, ok, code = one(conn(), wf)
            with lock:
                lat.append(ms); oks += ok; codes[code] = codes.get(code, 0) + 1
        for i in range(n):
            target = t0 + i / r
            d = target - time.perf_counter()
            if d > 0:
                time.sleep(d)
            th = threading.Thread(target=fire)
            th.start()
            threads.append(th)
        for th in threads:
            th.join()
    wall = time.perf_counter() - t0
    s.stop = True
    s.join()
    cpu1 = own({k: cpu_usec(p) for k, p in gs.items()}, inner)
    host1, me1 = host_busy_usec(), self_usec()
    rec.update(
        n=n, ok=oks, codes=codes, wall_s=round(wall, 2),
        per_s=round(n / wall, 1),
        p50=round(statistics.median(lat), 1), p95=round(pct(lat, 95), 1),
        p99=round(pct(lat, 99), 1), max=round(max(lat), 1),
        cpu_ms_per_exec={k: round((cpu1[k] - cpu0[k]) / 1000 / n, 2) for k in gs},
        peak_anon_mb={k: round(s.peak[k] / 2**20, 1) for k in gs},
        base_anon_mb={k: round(base_anon[k] / 2**20, 1) for k in gs},
    )
    rec["cpu_ms_total"] = round(sum(rec["cpu_ms_per_exec"].values()), 2)
    rec["loadgen_ms_per_exec"] = round((me1 - me0) / 1000 / n, 2)
    # everything else the VM did meanwhile: a check that nothing foreign ran
    rec["other_ms_per_exec"] = round((host1 - host0) / 1000 / n - rec["cpu_ms_total"] - rec["loadgen_ms_per_exec"], 2)
    rec["vm_busy_pct"] = round((host1 - host0) / 1e6 / wall / os.cpu_count() * 100, 1)
    print(json.dumps(rec))
    open(os.path.join(WORK, "results.jsonl"), "a").write(json.dumps(rec) + "\n")


if __name__ == "__main__":
    main()
