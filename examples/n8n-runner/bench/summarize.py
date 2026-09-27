#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""work/results.jsonl and work/cold.jsonl -> the comparison's tables, as
markdown on stdout.

Every figure is the mean of the rounds run; the last table says how far
apart the rounds were, which is the first thing to look at before believing
any of the others.
"""
import json
import os
import statistics
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
WORK = os.path.join(HERE, "work")
NAMES = {"stock": "stock n8n", "zygo": "Zygo runner", "box": "stock runner in Zygo"}
WORKLOADS = ["trivial", "cpu", "items", "deps"]


def load(name):
    path = os.path.join(WORK, name)
    if not os.path.exists(path):
        return []
    return [json.loads(line) for line in open(path) if line.strip()]


rows = load("results.jsonl")
stacks = [s for s in NAMES if any(r["stack"].split("#")[0] == s for r in rows)]


def of(stack, wf, mode):
    return [r for r in rows if r["stack"].split("#")[0] == stack and r["workflow"] == wf and r["mode"] == mode]


def mean(xs):
    return sum(xs) / len(xs) if xs else float("nan")


def fmt(x, nd=0):
    return "–" if x != x else f"{x:.{nd}f}"


def runner_cpu(r):
    return sum(v for k, v in r["cpu_ms_per_exec"].items() if k != "n8n")


def total_cpu(r):
    return sum(r["cpu_ms_per_exec"].values())


def table(title, header, lines):
    out.append(f"\n### {title}\n")
    out.append("| " + " | ".join(header) + " |")
    out.append("|" + "---|" * len(header))
    out.extend("| " + " | ".join(line) + " |" for line in lines)


out = []
if not rows:
    sys.exit("no results in work/results.jsonl: run `sh run.sh` first")
cols = [NAMES[s] for s in stacks]

for lang, name in (("js", "JavaScript"), ("py", "Python")):
    table(f"{name}, one request at a time: end-to-end ms, median / 1 in 100", ["work"] + cols, [
        [w] + [f"{fmt(mean([r['p50'] for r in of(s, f'{w}-{lang}', 'seq')]))} / "
               f"{fmt(mean([r['p99'] for r in of(s, f'{w}-{lang}', 'seq')]))}" for s in stacks]
        for w in WORKLOADS])

for lang, name in (("js", "JavaScript"), ("py", "Python")):
    table(f"{name}, 200 requests at once from 32 connections: finished per second", ["work"] + cols, [
        [w] + [fmt(mean([r["per_s"] for r in of(s, f"{w}-{lang}", "burst")]), 1) for s in stacks]
        for w in WORKLOADS])

for lang, name in (("js", "JavaScript"), ("py", "Python")):
    table(f"{name}, CPU ms per task in the bursts: the runner side / with n8n itself", ["work"] + cols, [
        [w] + [f"{fmt(mean([runner_cpu(r) for r in of(s, f'{w}-{lang}', 'burst')]), 1)} / "
               f"{fmt(mean([total_cpu(r) for r in of(s, f'{w}-{lang}', 'burst')]), 1)}" for s in stacks]
        for w in WORKLOADS])

idle = {s: [r for r in rows if r["stack"].split("#")[0] == s and r["mode"] == "idle"] for s in stacks}
mem_lines = [
    ["idle, the runner side"] + [fmt(mean([sum(v for k, v in r["anon_mb"].items() if k != "n8n") for r in idle[s]])) for s in stacks],
    ["idle, n8n itself"] + [fmt(mean([r["anon_mb"].get("n8n", 0) for r in idle[s]])) for s in stacks],
]
for w in ("trivial-js", "trivial-py", "items-js", "items-py", "cpu-py"):
    mem_lines.append([f"peak in the {w} burst, the runner side"] + [
        fmt(mean([sum(v for k, v in r["peak_anon_mb"].items() if k != "n8n") for r in of(s, w, "burst")])) for s in stacks])
table("Memory, MB of anonymous memory (page cache excluded)", [""] + cols, mem_lines)

table("A steady 5 requests a second for 30 s: ms, median / 1 in 100", ["work"] + cols, [
    [w] + [f"{fmt(mean([r['p50'] for r in of(s, w, 'rate')]))} / {fmt(mean([r['p99'] for r in of(s, w, 'rate')]))}" for s in stacks]
    for w in ("trivial-js", "trivial-py")])

table("n8n's own share: a workflow with no Code node, ms, median / 1 in 100", cols, [
    [f"{fmt(mean([r['p50'] for r in of(s, 'noop', 'seq')]))} / {fmt(mean([r['p99'] for r in of(s, 'noop', 'seq')]))}" for s in stacks]])

cold = load("cold.jsonl")
if cold:
    lines = []
    for kind, label in (("restart", "after the runner side restarts"), ("idle30", "after 30 s idle")):
        line = [label]
        for s in stacks:
            rs = [c for c in cold if c["stack"] == s and c["cold"] == kind]
            ok = all(c["js"].endswith(" 200") and c["py"].endswith(" 200") for c in rs)
            js = statistics.median([float(c["js"].split()[0]) * 1000 for c in rs]) if rs else float("nan")
            py = statistics.median([float(c["py"].split()[0]) * 1000 for c in rs]) if rs else float("nan")
            line.append(f"{fmt(js)} / {fmt(py)}" + ("" if ok else " (failures)"))
        lines.append(line)
    table("First run, trivial JS / Python: ms, median", [""] + cols, lines)

spread = []
for s in stacks:
    for w in WORKLOADS:
        for lang in ("js", "py"):
            rs = of(s, f"{w}-{lang}", "seq")
            if len(rs) > 1:
                spread.append((max(r["p50"] for r in rs) - min(r["p50"] for r in rs), s, f"{w}-{lang}", [r["p50"] for r in rs]))
out.append("\n### How far apart the rounds were (median ms, widest five)\n")
out.extend(f"- {NAMES[s]} {w}: {xs}" for _, s, w, xs in sorted(spread, reverse=True)[:5])

other = [r["other_ms_per_exec"] for r in rows if "other_ms_per_exec" in r]
if other:
    out.append(f"\nCPU the machine spent on anything else, per task: {min(other):.1f}–{max(other):.1f} ms "
               "(docker-proxy and the kernel; a large figure means the run was disturbed).")

failed = [(r["stack"], r["workflow"], r["mode"], r["n"] - r["ok"], r["codes"]) for r in rows if r.get("n") and r["ok"] != r["n"]]
out.append("\n### Failed requests\n")
out.extend(f"- {f}" for f in failed) if failed else out.append("- none")
print("\n".join(out))
