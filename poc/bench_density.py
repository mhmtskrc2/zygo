#!/usr/bin/env python3
"""How much a host costs per warm script — the number a SaaS asks first.

An embedder does not have one function. It has ten thousand scripts in a
database, most of them idle, a few of them hot. Today a Zygo zygote is *one
function*: `entry` imported, forked per request. So the question this answers
is not "how fast is a request" but "what does script number 501 cost me", and
the answer decides whether the current shape can serve an embedder at all.

Three numbers, measured rather than reasoned about:

  marginal RSS      what one more warm script adds, resident
  marginal PSS      the same, with shared pages divided among their sharers —
                    the honest per-script figure, because 500 zygotes of one
                    image share nearly all of the interpreter
  paused            what an idle script costs once the supervisor has tiered
                    it down

Every script here is *distinct* — a different constant in the source — so the
supervisor cannot dedupe them, and every one shares an image and a dependency
set, which is the shape an embedder has.

And then the same question of the shape Phase 1 built to answer it. With
`--pool` every script runs in **one runtime pool** instead of getting a zygote
of its own, and the two numbers become:

  slope             what one more script adds to the pool's resident memory,
                    which should be nothing at all: the zygotes hold an
                    interpreter and a dependency set, and the script arrives
                    with the request
  p50 / p99         what a call costs once every script has been run once,
                    measured over the API an embedder actually uses

The roadmap's exit criterion for Phase 1 is that pair: p99 under 5 ms with a
flat slope, at a thousand distinct scripts.

Usage:
    bench_density.py --scripts 40 [--idle-timeout 5s] [--json out.json]
    bench_density.py --pool --scripts 1000 [--json out.json]
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ZYGO = os.environ.get("ZYGO", os.path.join(HERE, "zygo-linux-musl"))

HANDLER = '''\
"""Script {n}: distinct source, shared imports."""

import json

CONSTANT = {n}


def handler(event):
    return {{"script": CONSTANT, "echo": json.dumps(event)[:32]}}
'''


def meminfo(key: str) -> int:
    """One `/proc/meminfo` field, in kilobytes."""
    with open("/proc/meminfo") as f:
        for line in f:
            if line.startswith(key):
                return int(line.split()[1])
    return 0


def zygote_pids() -> list[int]:
    """Every process the supervisor is holding as a zygote.

    Read from the cgroup tree rather than from `zygo ps`, which reports the
    resident size at warm-up and not a pid.

    The whole subtree under `tenants/`, not one level of it: a zygote lives at
    `tenants/<tenant>/<function>/<generation>/zygote`, three levels further
    down, because a cgroup that held processes could not delegate controllers
    to the per-request cgroups beneath it. Reading only the top level found
    nothing and reported 0.00 MB per script, which is the shape of an answer
    and not one.
    """
    pids = []
    for root, _, _ in os.walk("/sys/fs/cgroup"):
        if "/tenants/" not in root + "/":
            continue
        try:
            with open(os.path.join(root, "cgroup.procs")) as f:
                pids.extend(int(line) for line in f if line.strip())
        except OSError:
            continue
    return pids


def smaps_total(pids: list[int]) -> dict:
    """Rss and Pss across `pids`, in kilobytes.

    `smaps_rollup` is one read per process and already summed, which matters
    at four hundred processes. Pss is the number that answers "per script":
    a page shared by 500 zygotes counts as 1/500 of a page in each.
    """
    rss = pss = 0
    seen = 0
    for pid in pids:
        try:
            with open(f"/proc/{pid}/smaps_rollup") as f:
                text = f.read()
        except OSError:
            continue
        seen += 1
        for key, target in (("Rss:", "rss"), ("Pss:", "pss")):
            found = re.search(rf"^{key}\s+(\d+) kB", text, re.M)
            if found:
                if target == "rss":
                    rss += int(found.group(1))
                else:
                    pss += int(found.group(1))
    return {"processes": seen, "rss_kb": rss, "pss_kb": pss}


def serve(work_dir: str, n: int, idle_timeout: str) -> tuple[bool, str]:
    path = os.path.join(work_dir, f"s{n}.py")
    with open(path, "w") as f:
        f.write(HANDLER.format(n=n))
    done = subprocess.run(
        [
            ZYGO, "serve", path, "--name", f"s{n}",
            "--mem", "256M", "--idle-timeout", idle_timeout,
        ],
        capture_output=True,
    )
    return done.returncode == 0, done.stderr.decode(errors="replace")[:200]


def percentile(sorted_ms: list[float], fraction: float) -> float:
    """The nearest-rank percentile of an already-sorted list."""
    if not sorted_ms:
        return 0.0
    rank = max(1, min(len(sorted_ms), round(fraction * len(sorted_ms))))
    return sorted_ms[rank - 1]


def pool_density(options, client) -> int:
    """Phase 1's exit criterion: N scripts, one pool, latency and slope.

    Every script is distinct, so the store cannot dedupe them and the pool
    cannot cache one and serve it as another. Each is registered once — the
    embedder's shape, `PUT /scripts` followed by ten thousand calls naming the
    digest — and then called twice: once to pay whatever a first call costs,
    and once for the measurement.
    """
    print("warm-script density: one runtime pool, many scripts")
    print(
        f"  host      {os.uname().release} {os.uname().machine}, "
        f"{meminfo('MemTotal') / 1024 / 1024:.1f} GiB"
    )
    print(f"  scripts   {options.scripts} distinct sources, one pool")
    print()

    served = client.serve_runtime(
        "bench",
        {
            "image": "python:3.12-slim",
            "agent": "python",
            "min_warm": options.min_warm,
            "max_warm": options.max_warm,
            "mem": "512M",
            "concurrency": 4,
        },
    )
    print(
        f"  the pool is warm: {served.get('warm')} zygote(s), "
        f"{served.get('warm_ms', 0):.0f} ms"
    )

    # Registered first, all of them, so the calls below measure calling
    # rather than uploading.
    digests = []
    started = time.monotonic()
    for n in range(1, options.scripts + 1):
        digests.append(client.put_script(HANDLER.format(n=n)).sha256)
    print(
        f"  {len(digests)} scripts registered in {time.monotonic() - started:.1f} s "
        f"({(time.monotonic() - started) / max(1, len(digests)) * 1000:.1f} ms each)"
    )
    print()

    # A checkpoint every eighth of the way. The slope is what matters: the
    # first call warms whatever a first call warms, and the thousandth must
    # not cost the host anything the hundredth did not.
    every = max(1, options.scripts // 8)
    checkpoints = []
    first_call_ms = []
    print("  first call to each script (the one that pays for loading it):")
    for n, digest in enumerate(digests, start=1):
        at = time.monotonic()
        client.run_script("bench", digest, {"n": n})
        first_call_ms.append((time.monotonic() - at) * 1000)
        if n % every == 0 or n == len(digests):
            totals = smaps_total(zygote_pids())
            totals["scripts"] = n
            checkpoints.append(totals)
            print(
                f"  {n:>5} scripts   {totals['processes']:>3} zygotes   "
                f"rss {totals['rss_kb'] / 1024:>7.1f} MB   "
                f"pss {totals['pss_kb'] / 1024:>7.1f} MB"
            )

    # The measured population: every script called again, after every script
    # has been called once.
    print()
    print("  second call to each, which is what the p99 is over:")
    warm_ms = []
    handler_ms = []
    for n, digest in enumerate(digests, start=1):
        at = time.monotonic()
        out = client.run_script("bench", digest, {"n": n})
        warm_ms.append((time.monotonic() - at) * 1000)
        handler_ms.append(out.metrics.wall_ms)

    # The control: the same API, the same host, the same number of calls, to
    # a *function* whose handler was imported at warm-up. Without it the
    # numbers above are a measurement of this machine rather than of the pool
    # — a Raspberry Pi and a laptop differ by more than the budget does — and
    # what Phase 1 has to answer is "what does the pool shape cost", which is
    # the difference between these two columns.
    control_ms = []
    handler_path = os.path.join(options.work_dir, "control.py")
    os.makedirs(options.work_dir, exist_ok=True)
    with open(handler_path, "w") as f:
        f.write(HANDLER.format(n=0))
    try:
        client.serve(
            "control",
            {"image": "python:3.12-slim", "entry": handler_path, "mem": "512M"},
            base_dir=options.work_dir,
        )
        for n in range(len(digests)):
            at = time.monotonic()
            client.call("control", {"n": n})
            control_ms.append((time.monotonic() - at) * 1000)
        client.stop("control")
    except Exception as e:  # noqa: BLE001 - a missing control is not a failure
        print(f"  (no control: {type(e).__name__}: {e})")

    warm_ms.sort()
    handler_ms.sort()
    first_call_ms.sort()
    control_ms.sort()
    p50, p99 = percentile(warm_ms, 0.50), percentile(warm_ms, 0.99)
    print(
        f"  p50 {p50:>6.2f} ms   p99 {p99:>6.2f} ms   max {warm_ms[-1]:>6.2f} ms"
        "   (client to client, over the API)"
    )
    print(
        f"  p50 {percentile(handler_ms, 0.50):>6.2f} ms   "
        f"p99 {percentile(handler_ms, 0.99):>6.2f} ms   "
        f"max {handler_ms[-1]:>6.2f} ms   (the handler's own clock)"
    )
    print(
        f"  first calls: p50 {percentile(first_call_ms, 0.50):.2f} ms, "
        f"p99 {percentile(first_call_ms, 0.99):.2f} ms"
    )
    control_p50 = percentile(control_ms, 0.50)
    control_p99 = percentile(control_ms, 0.99)
    if control_ms:
        print()
        print("  the control — a warm *function*, same API, same host, same count:")
        print(
            f"  p50 {control_p50:>6.2f} ms   p99 {control_p99:>6.2f} ms   "
            f"max {control_ms[-1]:>6.2f} ms"
        )
        print(
            f"  the pool costs {p50 - control_p50:+.2f} ms at p50 and "
            f"{p99 - control_p99:+.2f} ms at p99 over a warmed handler"
        )

    slope = None
    if len(checkpoints) >= 2:
        first, last = checkpoints[0], checkpoints[-1]
        span = max(1, last["scripts"] - first["scripts"])
        slope = {
            "rss_kb": (last["rss_kb"] - first["rss_kb"]) / span,
            "pss_kb": (last["pss_kb"] - first["pss_kb"]) / span,
        }
        print()
        print("what one more script costs the pool, from the slope:")
        print(f"  resident (RSS)          {slope['rss_kb']:>8.1f} kB")
        print(f"  proportional (PSS)      {slope['pss_kb']:>8.1f} kB")
        print(
            f"  {options.scripts} scripts in one pool: "
            f"{last['pss_kb'] / 1024:.1f} MB of PSS in {last['processes']} zygote(s)"
        )

    # The verdict, against the roadmap's own numbers rather than a feeling.
    print()
    flat = slope is not None and abs(slope["pss_kb"]) < 64
    fast = p99 < 5.0
    print(f"  Phase 1 exit: p99 < 5 ms  — {'PASS' if fast else 'FAIL'} ({p99:.2f} ms)")
    print(
        f"                flat slope  — {'PASS' if flat else 'FAIL'} "
        f"({slope['pss_kb'] if slope else 0:.1f} kB per script, under 64 kB is flat)"
    )
    if not fast:
        print()
        if control_ms and control_p99 >= 5.0:
            print(f"  The control missed it too ({control_p99:.2f} ms at p99), so this is")
            print("  what a request costs on this host rather than what the pool costs.")
            print("  The pool's own overhead is the difference printed above; the budget")
            print("  is a claim about a host whose warm path is inside it to begin with.")
        else:
            print("  A p99 above the budget on a nested-virtualisation host is not a")
            print("  verdict on the design: `admit` alone reaches 11-12 ms at p99 in a")
            print("  VM inside a VM and 0.8 ms on metal (docs/book/25-performance.md). Repeat")
            print("  on bare metal before reading this as a result.")

    if options.json:
        with open(options.json, "w") as f:
            json.dump(
                {
                    "mode": "pool",
                    "scripts": len(digests),
                    "checkpoints": checkpoints,
                    "slope": slope,
                    "latency_ms": {
                        "p50": p50,
                        "p99": p99,
                        "max": warm_ms[-1],
                        "handler_p50": percentile(handler_ms, 0.50),
                        "handler_p99": percentile(handler_ms, 0.99),
                        "first_call_p50": percentile(first_call_ms, 0.50),
                        "first_call_p99": percentile(first_call_ms, 0.99),
                        "control_p50": control_p50,
                        "control_p99": control_p99,
                    },
                    "exit_criterion": {"p99_under_5ms": fast, "flat_slope": flat},
                },
                f,
                indent=2,
            )
        print(f"\nwrote {options.json}")

    client.stop_runtime("bench")
    return 0


def main(args: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scripts", type=int, default=40)
    parser.add_argument("--idle-timeout", default="10s")
    parser.add_argument("--work-dir", default="/tmp/bench-density")
    parser.add_argument("--json", default=None)
    parser.add_argument(
        "--pool",
        action="store_true",
        help="run every script in one runtime pool, over the API",
    )
    parser.add_argument("--api", default=os.environ.get("ZYGO_API_URL"))
    parser.add_argument("--min-warm", type=int, default=1)
    parser.add_argument("--max-warm", type=int, default=4)
    options = parser.parse_args(args)

    if options.pool:
        sys.path.insert(0, os.path.join(HERE, "..", "sdk", "python", "src"))
        import zygo

        if not options.api:
            print("  --pool needs an API: pass --api unix:///path/to/api.sock")
            return 1
        with zygo.connect(options.api, token=None, timeout=600) as client:
            return pool_density(options, client)

    os.makedirs(options.work_dir, exist_ok=True)

    print("warm-script density: what one more script costs this host")
    print(f"  host      {os.uname().release} {os.uname().machine}, "
          f"{meminfo('MemTotal') / 1024 / 1024:.1f} GiB")
    print(f"  scripts   {options.scripts} distinct sources, one image, one runtime")
    print()

    # A checkpoint every eighth of the way, so the *slope* is measured rather
    # than the total divided by the count — the first zygote pays for the
    # interpreter's pages and the five hundredth does not.
    every = max(1, options.scripts // 8)
    checkpoints = []
    failures = []

    baseline_available = meminfo("MemAvailable")
    started = time.monotonic()

    for n in range(1, options.scripts + 1):
        ok, why = serve(options.work_dir, n, options.idle_timeout)
        if not ok:
            failures.append((n, why))
            if len(failures) >= 3:
                print(f"  three scripts in a row failed to warm; stopping at {n}")
                print(f"  last: {why}")
                break
            continue
        if n % every == 0 or n == options.scripts:
            totals = smaps_total(zygote_pids())
            if totals["processes"] == 0:
                print(
                    f"  {n} scripts are warm and not one zygote process was found "
                    f"under /sys/fs/cgroup/**/tenants/. Nothing below would be a "
                    f"measurement, so this stops here."
                )
                return 1
            totals["scripts"] = n
            totals["available_delta_kb"] = baseline_available - meminfo("MemAvailable")
            checkpoints.append(totals)
            print(
                f"  {n:>4} scripts   {totals['processes']:>4} processes   "
                f"rss {totals['rss_kb'] / 1024:>7.1f} MB   "
                f"pss {totals['pss_kb'] / 1024:>7.1f} MB"
            )

    warmed = checkpoints[-1]["scripts"] if checkpoints else 0
    elapsed = time.monotonic() - started
    print()
    if warmed:
        print(f"  {warmed} warm in {elapsed:.1f} s ({elapsed / warmed * 1000:.0f} ms each)")

    marginal = None
    if len(checkpoints) >= 2:
        first, last = checkpoints[0], checkpoints[-1]
        span = last["scripts"] - first["scripts"]
        marginal = {
            "rss_kb": (last["rss_kb"] - first["rss_kb"]) / span,
            "pss_kb": (last["pss_kb"] - first["pss_kb"]) / span,
        }
        print()
        print("what one more warm script costs, from the slope between the first")
        print("and last checkpoint rather than from the total:")
        print(f"  resident (RSS)          {marginal['rss_kb'] / 1024:>7.2f} MB")
        print(f"  proportional (PSS)      {marginal['pss_kb'] / 1024:>7.2f} MB")
        print()
        for count in (1_000, 10_000):
            print(
                f"  {count:>6} scripts would be about "
                f"{marginal['pss_kb'] * count / 1024 / 1024:>7.1f} GiB of PSS"
            )

    # --- and what an idle one costs ---------------------------------------
    print()
    print(f"waiting out the {options.idle_timeout} idle timeout…")
    time.sleep(_seconds(options.idle_timeout) + 6)
    paused = smaps_total(zygote_pids())

    # What the supervisor *says* it did, before any claim about what it cost.
    # "Idle tiering frees nothing" and "idle tiering never ran" produce the
    # same two numbers, and only one of them is a finding.
    states = {}
    listed = subprocess.run([ZYGO, "--json", "ps"], capture_output=True)
    try:
        for f in json.loads(listed.stdout.decode() or "[]"):
            states[f.get("state", "?")] = states.get(f.get("state", "?"), 0) + 1
    except (ValueError, AttributeError):
        states = {}
    print(f"  `zygo ps` says: {states or 'nothing this could parse'}")
    print(
        f"  after idle: {paused['processes']} processes, "
        f"rss {paused['rss_kb'] / 1024:.1f} MB, pss {paused['pss_kb'] / 1024:.1f} MB"
    )
    if warmed:
        print(f"  per script paused: {paused['pss_kb'] / warmed / 1024:.2f} MB PSS")
    print()
    if any(k != "warm" for k in states):
        print("A paused zygote is still a process holding its address space: idle")
        print("tiering stops it being scheduled, not being resident.")
    else:
        print("Nothing was tiered down in that window, so the figure above is the")
        print("warm cost again rather than a paused one. Raise --idle-timeout, or")
        print("read it as \"nothing had happened yet\" and not as \"pausing is free\".")
    print()
    print("Either way this is the number Phase 1 of script_runtime.md exists to")
    print("change: a zygote per *runtime* rather than per script.")

    subprocess.run([ZYGO, "stop", "--all"], capture_output=True)

    if options.json:
        with open(options.json, "w") as f:
            json.dump(
                {
                    "scripts_requested": options.scripts,
                    "scripts_warmed": warmed,
                    "checkpoints": checkpoints,
                    "marginal": marginal,
                    "paused": paused,
                    "failures": [{"n": n, "why": why} for n, why in failures],
                },
                f,
                indent=2,
            )
        print(f"\nwrote {options.json}")
    return 0 if warmed else 1


def _seconds(duration: str) -> float:
    found = re.match(r"^(\d+(?:\.\d+)?)(ms|s|m|h)?$", duration.strip())
    if not found:
        return 10.0
    value = float(found.group(1))
    return value * {"ms": 0.001, "s": 1, "m": 60, "h": 3600}[found.group(2) or "s"]


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
