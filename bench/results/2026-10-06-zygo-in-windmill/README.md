# Zygo in Windmill's workers, in nsjail's place — 6 October 2026

The measurement behind [chapter 10](../../../docs/book/10-similar-projects.md#inside-windmill-in-place-of-nsjail)'s
"Inside Windmill, in place of nsjail": first made on 24 September 2026,
repeated here with Zygo 0.1.6 as released and extended with the two things
that round did not measure: a sandbox **with a network**, and what a job can
actually see and reach under each. The chapter's tables are this folder's.

Windmill CE v1.817.0 (image `8c3894aec879`, the same as on 24 September), three
general workers and one native worker, on the Lima VM: Ubuntu 24.04, Linux 6.8,
aarch64, 2 vCPU, 4 GB. Zygo 0.1.6 is the release tarball, checked against the
release's `SHA256SUMS`. One stack at a time; the load generator ran outside the
VM.

## What was run

| Mode | How |
|---|---|
| **nsjail** | Windmill's own: `DISABLE_NSJAIL=false`, its embedded config |
| **Zygo, no network** | `nsjail` on the workers' `PATH` is the shell stand-in in `harness/zygo-worker/nsjail`, which reads Windmill's nsjail config and calls `zygo run`; `ZYGO_NET=none`; the 24 September round ran this way too |
| **Zygo, with a network** | the same, with `ZYGO_NET=full`, on a worker image that adds `pasta` and `nftables` (`harness/windmill-image/Dockerfile`) — the mode in which a job reaches the internet, as an nsjail job does |
| **zygo run, no script** | in the per-job measurements only: `zygo run` with the arguments the stand-in would have built, to separate Zygo's cost from the script's |

The `nsjail` that ran Zygo natively on 24 September — the `zygo` binary reading
nsjail's command line itself — was built for that benchmark and never
committed, so it could not be repeated, and its results are not kept. *zygo
run, no script* is what such a translation would cost.

## Per job, one worker, nothing else running

The same Windmill job (a Python handler returning a field of its argument)
from a kept job directory, 50 runs per mode, modes interleaved over three
rounds. CPU is the worker container's whole cgroup — everything the run cost,
children or not — divided by runs. `harness/zygo-worker/percost2.sh`.

| | wall per run | CPU per run |
|---|---|---|
| nsjail | 21.0–23.5 ms | 20.4–22.6 ms |
| zygo run, no network | 22.4–23.4 ms | **20.6–21.4 ms** |
| zygo run, with a network | 31.0–31.9 ms | **27.7–27.9 ms** |
| the stand-in, no network | 25.6–30.3 ms | 24.3–28.4 ms |
| the stand-in, with a network | 34.7–35.7 ms | 31.5–32.3 ms |

Three loops at once in one worker, 30 runs each, two rounds
(`harness/zygo-worker/parcost.sh`):

| | runs/s | CPU per run |
|---|---|---|
| nsjail | 80.7–85.8 | 20.7–21.2 ms |
| zygo run, no network | 78.5–78.8 | 20.8 ms |
| zygo run, with a network | 51.8–52.1 | 28.1–28.2 ms |
| the stand-in, no network | 67.7–70.1 | 24.1–24.3 ms |
| the stand-in, with a network | 48.5–51.0 | 31.2–31.4 ms |

## Under load, the whole Windmill stack

The load generator and the cgroup accounting of chapter 10. Ranges cover the
runs of each mode; nsjail was measured before and after the Zygo modes.

| | nsjail | Zygo stand-in, no network | Zygo stand-in, with a network |
|---|---|---|---|
| burst of 200, trivial: jobs/s | 45.5–49.3 | 40.1–40.5 | 30.8–31.6 |
| CPU per job, trivial burst | 33.6–36.0 ms | 39.3–40.3 ms | 47.9–49.2 ms |
| burst of 200, ~20 ms of Python: jobs/s | 32.7–37.8 | 31.4–31.9 | 24.6–24.7 |
| CPU per job, CPU-bound burst | 45.3–50.7 ms | 52.2–52.9 ms | 63.8–64.0 ms |
| 20/s steady, p50 / p99 | 58–59 / 100–106 ms | 66 / 120 ms | 96 / 714 ms |
| 40/s steady: done/s, p50 | 37.3–39.0, 86–626 ms | 39.0, 281 ms | 29.4, 3529 ms |
| 50/s steady: done/s | 43.8–45.2 | 38.4 | 27.1 |
| failed jobs | 0 | 0 | 0 |

The first Zygo set after the store was created (`raw/load-r3-zygo-none-B1.txt`)
ran at 23–32 jobs/s and is left out of the table: the same set repeated after
the workers were recreated ran at 40/s, and one-worker measurements showed no
contention. The cause of the slow first runs was not isolated.

## What a job sees and reaches

`harness/isolation_probe.py`, run as a Windmill job under each mode
(`raw/probe-*.json`), and the limits each sets, from Windmill's nsjail config
for a Python job (read from a kept job's `run.config.proto`) and from the
stand-in's arguments:

| | nsjail | Zygo, no network | Zygo, with a network |
|---|---|---|---|
| seccomp filter (`/proc/self/status`) | 0 — none | 2 — on | 2 — on |
| Landlock | no | yes | yes |
| its own root filesystem | no — binds of the worker's | yes (`debian:trixie-slim` + the runtime) | yes |
| network namespace | **shared with the worker** (`clone_newnet: false`) | its own, empty | its own, through `pasta` |
| Windmill's Postgres, `db:5432` | **open** | unreachable | blocked (private range) |
| Windmill's server, `:8000` | open | unreachable | blocked (private range) |
| the internet, `1.1.1.1:53` | open | unreachable | open |
| cloud metadata, `169.254.169.254` | a route to it | unreachable | blocked |
| memory limit | `rlimit_as` 4096 MB per process | cgroup, 2 GB per job | the same |
| process limit | none | cgroup, 1024 per job | the same |
| capabilities, `no_new_privs` | none, set | none, set | none, set |

## How to read it

- **Zygo's sandbox costs what nsjail's does, without a network.** 20.6–21.4 ms
  of CPU per job against 20.4–22.6, and 3–9% behind nsjail's throughput with
  three jobs at once — while giving every job a cgroup with memory and process
  limits, a seccomp filter, Landlock and a root of its own, none of which
  Windmill's nsjail config sets.
- **A network per job is the expensive part: about 7 ms of CPU.** Zygo gives
  each sandbox its own network namespace and a `pasta` to reach the outside;
  Windmill's nsjail shares the worker's network, which is why its jobs reach
  Windmill's own database. That is a cost, and it is also the difference in
  what the job can reach. With three jobs at once it costs about a third of the
  throughput.
- **The shell stand-in adds 3–7 ms per job** of `sh`, `awk`, `grep` and `env`. Under the full stack that is the gap between 40 and
  45–49 jobs/s. A translation in the binary would remove it.
- **A Windmill job under Zygo with a network cannot reach `windmill_server`**,
  a private address, so the `wmill` client inside a job — variables,
  resources, sub-jobs — would need that address allowed (`--allow-private-net`
  and an `allow` rule). Not tested here.

## Files

| Path | What |
|---|---|
| `harness/override-nsjail.yml`, `override-zygo-016.yml`, `override-zygo-016-net.yml` | the compose overrides, on top of Windmill's own compose file and the slice override of the 24 September record |
| `harness/windmill-image/Dockerfile` | Windmill's worker image plus `pasta` and `nftables` |
| `harness/zygo-worker/nsjail`, `zygo-nsjail*.env` | the stand-in and its two settings files |
| `harness/zygo-worker/percost2.sh`, `parcost.sh` | the per-job measurements |
| `harness/isolation_probe.py`, `wm_probe.py` | the probe and the driver that runs it as a Windmill job |
| `harness/cgmon-lima.sh` | the 20 ms memory sampler |
| `raw/` | every result line: `load-r3-*` (A1, A2 nsjail; B1, B2 no network; B3 with a network), `percost-*`, `parcost-*`, `probe-*` |

A line in `raw/load-*.txt` reads: *system script, scenario: done/planned ok
in wall s (done/s) | latency p50 p95 p99 max | CPU ms/event | peak anon +MB
(idle MB)*; CPU is Windmill's whole cgroup divided by the jobs finished. The
load generator is the one in
[`2026-10-05-embedder-windmill/harness/load.exs`](../2026-10-05-embedder-windmill/harness/load.exs),
run with `windmill` as its system; it needs Elixir, Windmill's compose file
at v1.817.0 with the overrides here on top, and the superadmin token in a
file, as does `wm_probe.py`. In the first per-job run with a network the
stand-in's lines failed every run because the harness gave it a `PATH` without
`/usr/local/bin` and `/usr/sbin`, where `pasta` and `nft` are; Windmill's own
invocation has both. Those lines were measured again with the `PATH` fixed and
the file holds the second run.
