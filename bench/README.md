# Benchmark records

The raw output behind the numbers in
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

What is **not** here: the load generator and results behind chapter 10's
Windmill and kern comparisons, which were not kept. Those tables are a
summary of runs that cannot be repeated from this repository, and the chapter
says so. `make bench-embed ARGS="--json out.json"` repeats the embedder's
benchmark in chapter 25.

## The records

| Folder | Host | Notes |
|---|---|---|
| [2026-09-25-6.8.0-aarch64](results/2026-09-25-6.8.0-aarch64) | Lima VM on an M1 Max, 2 vCPU, 4 GB, Ubuntu 24.04 | in a privileged container; cgroup2 without `favordynmods`, so the 99th percentiles miss their budget, as chapter 25 explains |
| [2026-09-25-5.10.104-aarch64](results/2026-09-25-5.10.104-aarch64) | Docker Desktop's VM on the same Mac | a 5.10 kernel: no slow tail, but a slower cold start; within budget |
| [2026-09-26-6.17.0-x86_64](results/2026-09-26-6.17.0-x86_64) | GitHub `ubuntu-24.04` runner: 4 vCPU of an AMD EPYC 9V45, 16 GB, Azure, Linux 6.17 | **the first x86_64 record.** A shared runner, so the verdict is "not a verdict: the host was throttled or busy"; the medians are close to the Lima VM's and the 99th percentiles far under it |
| [2026-09-26-6.17.0-aarch64](results/2026-09-26-6.17.0-aarch64) | GitHub `ubuntu-24.04-arm` runner: 4 vCPU, 16 GB, Azure, Linux 6.17 | the same run on the arm runner, for the pair |
