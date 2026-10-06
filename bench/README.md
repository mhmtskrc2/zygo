# Benchmark records

Raw output of `zygo bench`, kept beside the numbers in
[chapter 25](../docs/book/25-performance.md), one folder per run, named
`<date>-<kernel>-<arch>`. Each file is what `zygo bench … --json`
printed, unedited: the host it ran on (kernel, cores, memory, whether it is a
VM), every percentile it measured, the budgets, the verdict, and anything
that disturbed the machine while it ran. A record whose verdict is "a budget
was missed" is kept too — that is the point of keeping them.

```bash
make bench-record     # zygo bench all, and the warm path with and without a
                      # per-request cgroup, into bench/results/<today>-…/
```

The `bench` workflow (`.github/workflows/bench.yml`) makes the same record
on GitHub's x86_64 and arm64 runners every week, as an artifact; copy one
here when it is worth keeping.

What is **not** here: the run behind chapter 25's headline table (1.44 ms
for a warm request, 10.5 ms for 1 in 100, 1,108 requests a second), made on
the Lima VM on 25 September 2026 as an ordinary user under a systemd login.
Its raw output is not in this repository. The closest record is the first row
below: the same VM, the same day, inside a privileged container, which
measured 1.21 ms, 15.4 ms and 1,176 a second. `zygo bench all` prints the
published figures beside what it measures, so any new record is held to them.

Two folders are not `zygo bench` records.
[2026-10-05-embedder-windmill](results/2026-10-05-embedder-windmill) has the
load generator and results behind chapter 25's embedder under load, beside
Windmill, and chapter 10's kern comparison; it needs the embedder's own
checkout to repeat.
[2026-10-06-zygo-in-windmill](results/2026-10-06-zygo-in-windmill) has
chapter 10's Windmill comparison — Zygo in nsjail's place inside Windmill's
workers — with the stand-in, the probe and every result line; it needs
Windmill's compose file and Elixir for the generator. Each README says what
its files are. `make bench-embed ARGS="--json out.json"` repeats the
embedder's benchmark in chapter 25.

## The records

| Folder | Host | Notes |
|---|---|---|
| [2026-09-25-6.8.0-aarch64](results/2026-09-25-6.8.0-aarch64) | Lima VM on an M1 Max, 2 vCPU, 4 GB, Ubuntu 24.04 | in a privileged container; cgroup2 without `favordynmods`, so the 99th percentiles miss their budget, as chapter 25 explains |
| [2026-09-25-5.10.104-aarch64](results/2026-09-25-5.10.104-aarch64) | Docker Desktop's VM on the same Mac | a 5.10 kernel: no slow tail, but a slower cold start; within budget |
| [2026-09-26-6.17.0-x86_64](results/2026-09-26-6.17.0-x86_64) | GitHub `ubuntu-24.04` runner: 4 vCPU of an AMD EPYC 9V45, 16 GB, Azure, Linux 6.17 | **the first x86_64 record.** A shared runner, so the verdict is "not a verdict: the host was throttled or busy"; the medians are close to the Lima VM's and the 99th percentiles far under it |
| [2026-09-26-6.17.0-aarch64](results/2026-09-26-6.17.0-aarch64) | GitHub `ubuntu-24.04-arm` runner: 4 vCPU, 16 GB, Azure, Linux 6.17 | the same run on the arm runner, for the pair |
| [2026-10-05-embedder-windmill](results/2026-10-05-embedder-windmill) | the Lima VM, 24 September and 5 October | **not a `zygo bench` record**: a real embedder under load, first on `zygo run`, then on the runtime pools, with Windmill CE + nsjail measured beside it both days; the load generator, the kern harness from chapter 10, and every raw result line |
| [2026-10-06-zygo-in-windmill](results/2026-10-06-zygo-in-windmill) | the Lima VM, 6 October | **not a `zygo bench` record**: Zygo 0.1.6 in Windmill's workers in nsjail's place, without and with a network per job, per job and under load, and what a job can reach under each; nsjail measured before and after |
