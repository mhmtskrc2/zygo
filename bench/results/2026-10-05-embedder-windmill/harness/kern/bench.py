#!/usr/bin/env python3
"""Zygo against kern under concurrent load, running what the embedder runs.

Run inside a delegated scope, the way a service runs:
    systemd-run --user --scope -p Delegate=yes -q -- python3 bench.py RUNTIME WORKLOAD N CONC...

RUNTIME   zygo | zygo-nobc | kern | kern-pyc
WORKLOAD  true      /bin/true in python:3.12-slim — the runtime alone
          harness   the embedder's harness with an event, a rw scratch mount, its limits
Reports, per concurrency: wall, runs/s, latency p50/p95/p99, CPU-seconds per run
(from /proc/stat, the whole VM), peak memory above idle, failures.
"""
import os
import shutil
import subprocess
import sys
import threading
import time
from concurrent.futures import ThreadPoolExecutor

Z = "/opt/zygo-bench/zygo"
K = "/opt/zygo-bench/kern"
W = os.path.expanduser("~/kbench/work")
HARNESS = open(f"{W}/harness.py").read()
CG = "/sys/fs/cgroup"


def own_cgroup():
    return CG + open("/proc/self/cgroup").read().strip().split("::", 1)[1]


def prepare_cgroups():
    """Leave the scope's root empty so Zygo can build under it: this process moves to
    a leaf, and every zygo starts inside zygo.slice/system, which Zygo recognises and
    reuses. kern needs none of this — it places boxes in user@UID.service/kern.slice."""
    scope = own_cgroup()
    os.makedirs(f"{scope}/bench", exist_ok=True)
    open(f"{scope}/bench/cgroup.procs", "w").write(str(os.getpid()))
    open(f"{scope}/cgroup.subtree_control", "w").write("+cpu +memory +pids")
    os.makedirs(f"{scope}/zygo.slice/system", exist_ok=True)
    return f"{scope}/zygo.slice/system"


def cpu_busy():
    f = open("/proc/stat").readline().split()[1:]
    v = list(map(int, f))
    idle = v[3] + v[4]
    return (sum(v) - idle) / os.sysconf("SC_CLK_TCK")


def mem_used():
    m = {}
    for line in open("/proc/meminfo"):
        k, v = line.split(":")
        m[k] = int(v.split()[0])
    return (m["MemTotal"] - m["MemAvailable"]) / 1024


def command(rt, workload, d, name):
    if workload == "true":
        prog = ["/bin/true"]
    else:
        prog = ["python", "-c", HARNESS, "/embedder/event.json", "/embedder/result.json", "/embedder"]
    if rt.startswith("zygo"):
        return [Z, "run", "--mem", "256M", "--cpu", "1", "--pids", "128", "--scratch", "64M",
                "--timeout", "30s", "--workdir", "/embedder", "--mount", f"{d}:/embedder:rw",
                "--outcome", f"{d}/outcome.json", "--env", "HOME=/tmp",
                "--env", "PYTHONDONTWRITEBYTECODE=1", "--net", "none", "python:3.12-slim", *prog]
    image = "python-pyc:3.12" if rt == "kern-pyc" else "python:3.12-slim"
    return [K, "box", name, "--image", image, "--pull", "never", "--rm", "-q",
            "--security-profile", "untrusted", "-m", "256m", "--cpus", "1", "--pids-limit", "128",
            "--tmpfs", "/tmp:64m", "--timeout", "30", "-w", "/embedder", "-v", f"{d}:/embedder",
            "-e", "HOME=/tmp", "-e", "PYTHONDONTWRITEBYTECODE=1", "--net", "none", "--", *prog]


def pct(xs, p):
    s = sorted(xs)
    return s[min(len(s) - 1, round(p / 100 * (len(s) - 1)))]


def main():
    rt, workload, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
    concs = [int(c) for c in sys.argv[4:]] or [1, 4, 8, 16, 32]
    zygo_cg = None  # both start from the scope that holds this process
    env = dict(os.environ)
    if rt == "zygo-nobc":
        env["ZYGO_BYTECODE"] = "0"

    def enter_zygo_slice():
        if rt.startswith("zygo") and zygo_cg:
            open(f"{zygo_cg}/cgroup.procs", "w").write(str(os.getpid()))

    def one(i, run_dir):
        d = f"{run_dir}/{i}"
        os.makedirs(d)
        shutil.copy(f"{W}/event.json", d)
        os.chmod(d, 0o777)
        t = time.monotonic()
        p = subprocess.run(command(rt, workload, d, f"b{os.getpid()}x{i}"), env=env,
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                           preexec_fn=enter_zygo_slice)
        ms = (time.monotonic() - t) * 1000
        ok = p.returncode == 0 and (workload == "true" or os.path.exists(f"{d}/result.json"))
        return ms, ok, p.stderr.decode()[-300:]

    # A few runs first, so nothing below pays for a cold page cache or a first layer.
    warm = f"{W}/runs/warm-{rt}"
    shutil.rmtree(warm, ignore_errors=True)
    for i in range(3):
        one(i, warm)

    print(f"{rt} {workload}, {n} runs per level")
    print(f"{'conc':>5} {'wall s':>7} {'runs/s':>7} {'p50':>7} {'p95':>7} {'p99':>7} "
          f"{'cpu ms/run':>10} {'peak MB':>8} {'failed':>6}")
    for conc in concs:
        run_dir = f"{W}/runs/{rt}-{workload}-{conc}"
        shutil.rmtree(run_dir, ignore_errors=True)
        base_mem, peak = mem_used(), [0.0]
        stop = threading.Event()

        def sample():
            while not stop.is_set():
                peak[0] = max(peak[0], mem_used())
                time.sleep(0.01)

        sampler = threading.Thread(target=sample)
        sampler.start()
        c0, t0 = cpu_busy(), time.monotonic()
        with ThreadPoolExecutor(conc) as pool:
            results = list(pool.map(lambda i: one(i, run_dir), range(n)))
        wall, cpu = time.monotonic() - t0, cpu_busy() - c0
        stop.set()
        sampler.join()
        lat = [r[0] for r in results]
        failed = [r for r in results if not r[1]]
        print(f"{conc:>5} {wall:>7.2f} {n / wall:>7.1f} {pct(lat, 50):>7.1f} {pct(lat, 95):>7.1f} "
              f"{pct(lat, 99):>7.1f} {cpu * 1000 / n:>10.1f} {peak[0] - base_mem:>8.0f} "
              f"{len(failed):>6}")
        if failed:
            print("   first failure:", failed[0][2].strip().splitlines()[-1:] or "(no stderr)")
        shutil.rmtree(run_dir, ignore_errors=True)


if __name__ == "__main__":
    main()
