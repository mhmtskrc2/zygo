# Notes for the third post: Zygo in Windmill, in nsjail's place

Material for the post, not the post. Every number is from
`bench/results/2026-10-06-zygo-in-windmill/`; nothing here is new.

## The one-line story

Windmill runs each job through nsjail with a config that shares the worker's
network and sets no cgroup, no seccomp filter and no process limit. Zygo, put
in nsjail's place, gives every job all four — and without a network it costs
the same per job. A network per job is what costs: about 7 ms.

## Candidate titles

- "Swapping Windmill's nsjail for a sandbox with limits: what it costs per job"
- "Same cost, more walls: Zygo in place of nsjail inside Windmill"
- "Your Windmill job can reach Windmill's database. Here is what fixing that costs."

The third is the strongest hook and the most likely to be read as an attack on
Windmill; it is their default config, documented, and a self-hoster can change
it. Say so in the post.

## The three tables to use

**Per job, one worker, CPU of the whole worker cgroup** (50 runs × 3 rounds):

| | CPU per job |
|---|---|
| nsjail (Windmill's config) | 20.4–22.6 ms |
| Zygo, no network | 20.6–21.4 ms |
| Zygo, with a network | 27.7–27.9 ms |

**What a job can see and reach** (the probe, run as a Windmill job):

| | nsjail | Zygo, with a network |
|---|---|---|
| seccomp filter | none | on |
| per-job memory limit | `rlimit_as` 4 GB per process | 2 GB cgroup |
| per-job process limit | none | 1024 |
| Windmill's Postgres `db:5432` | **open** | refused |
| Windmill's server | open | refused |
| the internet | open | open |

**Under the full Windmill stack** (3 workers, 2 vCPU, burst of 200 trivial jobs):

| | jobs/s | CPU per job |
|---|---|---|
| nsjail | 45.5–49.3 | 33.6–36.0 ms |
| Zygo through a shell stand-in, no network | 40.1–40.5 | 39.3–40.3 ms |
| Zygo through a shell stand-in, with a network | 30.8–31.6 | 47.9–49.2 ms |

## What must be said in the post

- The stand-in is a shell script; it costs 3–7 ms per job on its own. The
  "same cost" claim is for `zygo run` with the same arguments, not through the
  script. A translation inside the binary does not exist in Zygo today.
- The network row is not like for like, and that is the point: nsjail's jobs
  are on the worker's network, Zygo's each get a namespace and a `pasta`. Say
  which one the reader is paying for.
- Under Zygo with a network, a job cannot reach `windmill_server` either, so
  the `wmill` client inside jobs needs the server's address allowed. Untested.
- 2 vCPU VM on a laptop. Ratios, not absolute numbers.
- The 24 September round ran Zygo without a network and did not say so;
  chapter 10 now says so and shows the 6 October round only.

## What not to claim

- "Faster than nsjail." It is level without a network and slower with one.
- "Windmill is insecure." Its default is a choice, and Enterprise has other
  options; the post is about what a per-job wall costs, measured.
- Anything about Windmill's warm workers: not measured.

## A figure worth drawing

Two bars per mode, CPU per job: nsjail 21 ms; Zygo without a network 21 ms;
Zygo with a network 28 ms. Under it, the four rows of the second table as
ticks and crosses. That is the whole post in one picture.
