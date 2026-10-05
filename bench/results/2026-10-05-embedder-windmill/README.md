# An embedder on Zygo, under load, beside Windmill

The load generator and every raw result behind two measurements of a real
embedder — a platform that runs customer scripts on Zygo — and of Windmill
CE beside it, on the same VM:

* **24 September 2026**: the embedder ran every event through `zygo run`, a
  fresh sandbox and a fresh interpreter per event. Windmill ran its jobs
  through nsjail. [Chapter 10](../../../docs/book/10-similar-projects.md#inside-windmill-in-place-of-nsjail)
  also put Zygo *inside* Windmill in nsjail's place that day; the stand-in is
  in `harness/zygo-as-nsjail/`. The kern comparison of the same chapter was
  run the same way; its harness is in `harness/kern/`.
* **5 October 2026**: the same embedder, now on Zygo 0.1.5's runtime pools
  through the HTTP API — a fork of a warm interpreter per event — and the same
  Windmill, from the same image, beside it.
  [Chapter 25](../../../docs/book/25-performance.md#the-warm-path-inside-an-embedder-under-load)
  has that day's table.

Unlike the other folders here, these are not `zygo bench` records and cannot
be repeated from this repository: the embedder's side of the harness is its
own code and is not included, so what is here is the generator that drove
both systems, Windmill's side in full, and the complete output. The tables
in the book are a summary of something that can be read.

In the copies here the embedder's working name has been replaced by
`embedder`, in file names, in the label at the start of each result line and
in the generator's comments and arguments. Nothing else in a result line was
edited.

## The host

Lima VM on a MacBook Pro M1 Max: Ubuntu 24.04, Linux 6.8.0, aarch64, 2 vCPU,
4 GB. One stack ran at a time, in a cgroup of its own (`bench.slice` for the
embedder, `windmill.slice` for Windmill). The load generator ran on the Mac,
so its CPU is nobody's. Windmill: CE v1.817.0, image `8c3894aec879`, three
general workers and one native worker, `DISABLE_NSJAIL=false`.

## What is here

| Path | What |
|---|---|
| `harness/load.exs` | the load generator, both rounds and both systems: `burst N` and `rate R SECONDS`; latency client-side to a 10 ms poll, CPU and peak memory from the stack's cgroup. `load.exs.first-round` is the 24 September copy; the only difference is where the second reads the embedder's database name |
| `harness/cgmon*.sh` | the 20 ms sampler of each slice's anonymous memory, on the Lima VM and on Docker Desktop |
| `harness/windmill/` | the compose overrides: Windmill under its slice, nsjail on, and the ones that put Zygo in nsjail's place; the superadmin secret replaced. The compose file itself is Windmill's own at v1.817.0 |
| `harness/zygo-as-nsjail/` | the `nsjail` stand-in that ran Zygo inside Windmill's workers, and the per-run cost scripts; chapter 10 says this translation is not in Zygo |
| `harness/kern/` | the kern comparison's harness (`bench.py`, `matrix.sh` and the `why*.sh` that chased its tail) — chapter 10's "embedder's harness, against kern" |
| `harness/first-round-mac/` | the first attempt on Docker Desktop's VM, before the Lima VM: `windmill_bench.py` and the probe script |
| `raw/` | every result line. First round: `load-embedder-lima-*.txt`, `load-windmill-*.txt`, the Mac files `windmill-*.txt`; Zygo inside Windmill: `load-windmill-zygo*.txt`, `load-windmill-nsjail*.txt`; second round: `load-embedder-r2-*.txt`, `load-windmill-r2*.txt`, and `load-embedder-r2-split.txt` for the CPU of each part of the embedder's stack; kern: `kern-matrix-*.txt` |

A result line reads: *system script, scenario: done/planned ok in wall s
(done/s) | latency p50 p95 p99 max | CPU ms/event | peak anon +MB (idle MB)*.
CPU is the whole stack's cgroup divided by events finished; idle is the
stack's anonymous memory before the run.

## The two rounds, in one table

| | embedder, 24 Sep (`zygo run`) | embedder, 5 Oct (runtime pools) | Windmill + nsjail, 24 Sep | Windmill + nsjail, 5 Oct |
|---|---|---|---|---|
| highest rate sustained | 58.2–58.4/s | 98–102/s (2 runner slots); 105–115/s with 4 | 48.5–49.6/s | 48–50/s |
| CPU per event, trivial script | 27–31 ms | 12–16 ms | 33–38 ms | 33–43 ms |
| of which the sandboxes | 19.7 ms | 7.9–8.6 ms | | |
| CPU per event, ~20 ms of Python | 41.8 ms | 26.0–26.6 ms | 45.4 ms | 47.2–47.7 ms |
| 20/s steady, p50 / p99 | 36 / 51 ms | 26–32 / 41–57 ms | 55–57 / 97–102 ms | 59–62 / 100–122 ms |
| burst of 200, trivial | 51.4–58.2/s | 87–94/s | 51.4/s | 47.7–50.2/s |
| idle memory of the stack | 87–118 MB | 120–151 MB | 384–634 MB | 406–602 MB |

Windmill moving by a few percent between the rounds is the VM's own noise,
and is what says the embedder's change is not noise.

## What to read with care

* Between the rounds the embedder also **stopped writing a row per event**
  (its scripts write with SQL now, and a return value is kept with the run
  rather than in a table). Its PostgreSQL share fell from 4.5 to 1.3–2.5 ms
  an event, and that part of the gain is not Zygo's. It also makes the second
  round the fairer one against Windmill, whose jobs never wrote a row.
* The embedder's steady-rate results **vary between runs** in the second
  round: eight of ten runs at 50/s had a p50 of 28–40 ms, two had 0.5 and
  2.3 s. Its event log showed the run itself steady at 16–19 ms p50 while the
  end-to-end time waited in its own queue; so this is the embedder's, not the
  sandbox's.
* The embedder's ceiling in the second round is its own runner concurrency,
  two slots on two cores, with a run of 16–19 ms each. Four slots moved it to
  105–115/s, where the VM's CPU ran out.
* **This is not Zygo against Windmill.** Two products, one of them on its
  warm path and the other on its cold one; Windmill Enterprise's dedicated
  workers, its warm equivalent, were not measured. The like-for-like
  comparison is chapter 10's, with Zygo in nsjail's place inside Windmill.
  What this folder shows is one embedder on Zygo's cold path and then on its
  warm one, on the same host.
