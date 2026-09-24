#!/usr/bin/env python3
"""The embedder's benchmark: what one run of somebody else's script costs.

`zygo bench all` measures Zygo against Zygo's own budgets. This measures Zygo
against the alternatives an embedder is actually choosing between, on one
host, with one script, in one run — because the number that decides whether a
workflow engine can use a warm fork is not Zygo's overhead, it is the *ratio*
to what they would otherwise do.

The script is import-heavy on purpose. A handler that imports nothing makes
every runner look the same, and no real script imports nothing: sixteen
standard-library modules are a couple of hundred milliseconds of interpreter
start-up, which a per-call container pays on every single call and a warm
zygote pays once.

Runners, each measured the same way — the whole per-request command, from
process start to result:

  zygo-warm     `zygo exec` against a warm zygote
  zygo-oneshot  `zygo run`, image already in the store
  docker        `docker run --rm`
  kern          `kern run`, when ZYGO_BENCH_KERN points at a binary

A runner that is not available is reported as absent rather than skipped
quietly: a comparison with a missing column should say which column is
missing.

Usage:
    bench_embed.py --runs 100 [--json out.json]
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ZYGO = os.environ.get("ZYGO", os.path.join(HERE, "zygo-linux-musl"))
IMAGE = "python:3.12-slim"

# The script every runner runs. Its imports are the point.
#
# The standard library, not pandas — deliberately, and it cost a first
# attempt to learn why. A third-party import set means the one-shot runners
# need the packages in *their* image while the warm one gets them from a venv
# Zygo built, and then the comparison is between two different filesystems
# rather than between two ways of paying for the same work. Every runner here
# starts the same interpreter from the same image and imports the same
# modules.
#
# This is the conservative direction: these cost about 150-250 ms to import,
# where pandas and Pillow together cost more. A real script's ratio is better
# than the one below, not worse.
IMPORTS = """\
import argparse
import asyncio
import csv
import decimal
import email.parser
import hashlib
import http.client
import json
import logging.config
import pprint
import ssl
import sqlite3
import unittest
import uuid
import xml.etree.ElementTree as ET
import zipfile
"""

HANDLER = IMPORTS + '''

def work(event):
    root = ET.fromstring("<a><b>1</b><b>2</b></a>")
    digest = hashlib.sha256(json.dumps(event, sort_keys=True).encode()).hexdigest()
    return {
        "sum": sum(int(b.text) for b in root),
        "digest": digest[:16],
        "id": str(uuid.uuid5(uuid.NAMESPACE_DNS, "zygo"))[:8],
    }


def handler(event):
    """The Zygo entry point: the imports above are already done."""
    return work(event)


if __name__ == "__main__":
    # The one-shot entry point: the same work, but the imports are paid now.
    print(json.dumps(work({})))
'''


def percentile(values: list[float], p: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    at = (len(ordered) - 1) * p / 100.0
    low, high = int(at), min(int(at) + 1, len(ordered) - 1)
    return ordered[low] + (ordered[high] - ordered[low]) * (at - low)


class Runner:
    """One way to run the script once."""

    def __init__(self, name: str, argv: list[str], note: str = "") -> None:
        self.name = name
        self.argv = argv
        self.note = note

    def once(self) -> tuple[float, bool]:
        started = time.monotonic()
        done = subprocess.run(self.argv, capture_output=True)
        elapsed = (time.monotonic() - started) * 1000.0
        ok = done.returncode == 0 and b'"sum"' in done.stdout
        if not ok and elapsed > 0:
            # Kept for the report: a runner that fails every time must not
            # look like a fast one.
            self.last_error = (
                done.stderr.decode(errors="replace")[:300] or done.stdout.decode(errors="replace")[:300]
            )
        return elapsed, ok

    def measure(self, runs: int, warmup: int = 3) -> dict:
        for _ in range(warmup):
            self.once()
        samples, failures = [], 0
        for i in range(runs):
            elapsed, ok = self.once()
            if ok:
                samples.append(elapsed)
            else:
                failures += 1
            if runs >= 50 and i and i % (runs // 4) == 0:
                print(f"  {self.name}: {i * 100 // runs}%", file=sys.stderr)
        return {
            "runner": self.name,
            "runs": runs,
            "failures": failures,
            "p50_ms": percentile(samples, 50),
            "p90_ms": percentile(samples, 90),
            "p99_ms": percentile(samples, 99),
            "min_ms": min(samples) if samples else 0.0,
            "note": self.note,
            "error": getattr(self, "last_error", None) if failures else None,
        }


def run(argv: list[str], check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(argv, capture_output=True, check=check)


def main(args: list[str]) -> int:
    # This runs for minutes behind a pipe, and a benchmark with no visible
    # progress is one people kill.
    sys.stdout.reconfigure(line_buffering=True)
    sys.stderr.reconfigure(line_buffering=True)

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runs", type=int, default=100)
    parser.add_argument("--json", default=None)
    parser.add_argument("--work-dir", default="/tmp/bench-embed")
    # Where `--work-dir` is on the machine the *docker daemon* runs on. A
    # sibling container started through a mounted socket is created by that
    # daemon, so a `-v` path is resolved there and not in this container —
    # which is how the docker column first came out as 100 failures of
    # "can't find '__main__' module in '/handler.py'".
    parser.add_argument(
        "--host-work-dir", default=os.environ.get("ZYGO_BENCH_HOST_DIR") or None
    )
    options = parser.parse_args(args)

    os.makedirs(options.work_dir, exist_ok=True)
    os.chdir(options.work_dir)
    with open("handler.py", "w") as f:
        f.write(HANDLER)

    modules = len([l for l in IMPORTS.splitlines() if l.startswith("import")])
    print("the embedder's benchmark: one import-heavy script, several runners")
    print(f"  host      {os.uname().sysname} {os.uname().release} {os.uname().machine}")
    print(f"  image     {IMAGE}, the same one for every runner")
    print(f"  script    {modules} standard-library modules imported at module level")
    print(f"  runs      {options.runs} each, after 3 warm-up calls")
    print()

    # --- warm the zygote, and time that too -------------------------------
    print("warming the Zygo function (this is the cost a warm runner pays once)…")
    started = time.monotonic()
    warmed = subprocess.run(
        [ZYGO, "serve", "./handler.py", "--name", "embed", "--mem", "512M"],
        capture_output=True,
    )
    warm_ms = (time.monotonic() - started) * 1000.0
    if warmed.returncode != 0:
        print(f"  could not warm: {warmed.stderr.decode(errors='replace')[:400]}", file=sys.stderr)
        return 1
    print(f"  warm in {warm_ms:.0f} ms")
    print()

    runners = [
        Runner(
            "zygo-warm",
            [ZYGO, "exec", "embed", "{}"],
            "a fork of the warmed interpreter; the CLI's own start-up is in this number",
        ),
        Runner(
            "zygo-oneshot",
            [
                ZYGO, "run", "--mount", f"{options.work_dir}/handler.py:/handler.py:ro",
                IMAGE, "python3", "/handler.py",
            ],
            "a fresh sandbox per call, image already in the store",
        ),
    ]

    host_dir = options.host_work_dir
    if shutil.which("docker") and not host_dir:
        print(
            "  docker: on PATH, but nothing says where this directory is on the "
            "daemon's host, so a sibling container could not read the script. Set "
            "ZYGO_BENCH_HOST_DIR (make bench-embed does).",
            file=sys.stderr,
        )
    if shutil.which("docker") and host_dir:
        runners.append(
            Runner(
                "docker",
                [
                    "docker", "run", "--rm",
                    "-v", f"{host_dir}/handler.py:/handler.py:ro",
                    IMAGE, "python3", "/handler.py",
                ],
                "a container per call, image already pulled",
            )
        )
    else:
        print("  docker: not on PATH here, so that column is missing", file=sys.stderr)

    kern = os.environ.get("ZYGO_BENCH_KERN")
    if kern and os.path.isfile(kern):
        # `kern box`, not `kern run`: `run` caps a process on the host with no
        # image and no namespaces, which is a different thing entirely. `box`
        # is the sandbox, and is what `zygo run` should be compared with.
        runners.append(
            Runner(
                "kern",
                [
                    kern, "box", "bench-embed", "--image", IMAGE, "--rm",
                    "--pull", "never",
                    "-v", f"{options.work_dir}/handler.py:/handler.py:ro",
                    "--", "python3", "/handler.py",
                ],
                "a box per call, image already in kern's own store",
            )
        )
    else:
        print(
            "  kern: not measured. Set ZYGO_BENCH_KERN to a `kern` binary to add the "
            "column — this harness deliberately does not download one.",
            file=sys.stderr,
        )
    print()

    results = [r.measure(options.runs) for r in runners]

    print(f"{'runner':<14} {'p50':>9} {'p90':>9} {'p99':>9} {'min':>9}   failures")
    print(f"{'------':<14} {'---':>9} {'---':>9} {'---':>9} {'---':>9}   --------")
    for r in results:
        print(
            f"{r['runner']:<14} {r['p50_ms']:>8.1f}ms {r['p90_ms']:>8.1f}ms "
            f"{r['p99_ms']:>8.1f}ms {r['min_ms']:>8.1f}ms   {r['failures']}"
        )
        if r["error"]:
            print(f"               {r['error'][:150]}")
    print()

    warm = next((r for r in results if r["runner"] == "zygo-warm"), None)
    others = [r for r in results if r["runner"] != "zygo-warm" and r["failures"] < options.runs]
    verdict = None
    if warm and others:
        best = min(others, key=lambda r: r["p50_ms"])
        ratio = best["p50_ms"] / warm["p50_ms"] if warm["p50_ms"] else 0.0
        verdict = {"best_other": best["runner"], "ratio": ratio}
        print(
            f"the warm fork is {ratio:.1f}× faster than the best one-shot runner here "
            f"({best['runner']}, {best['p50_ms']:.1f} ms)."
        )
        print(
            f"It paid {warm_ms:.0f} ms once to get there, which {warm_ms / best['p50_ms']:.0f} "
            f"calls of {best['runner']} would have cost."
        )
    print()
    print("Every number above is a whole process: the client starting, the request, the")
    print("answer. An embedder calling `zygo api` over a unix socket does not pay the")
    print("client start-up — `zygo bench warm` measures that path and reports ~1.4 ms.")

    subprocess.run([ZYGO, "stop", "embed"], capture_output=True)

    if options.json:
        with open(options.json, "w") as f:
            json.dump(
                {
                    "host": {
                        "kernel": os.uname().release,
                        "machine": os.uname().machine,
                        "cores": os.cpu_count(),
                    },
                    "runs": options.runs,
                    "warm_ms": warm_ms,
                    "results": results,
                    "verdict": verdict,
                },
                f,
                indent=2,
            )
        print(f"\nwrote {options.json}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
