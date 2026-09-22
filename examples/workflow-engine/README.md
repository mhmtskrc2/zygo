# A workflow engine's worker

Windmill, n8n, Temporal, Airflow, Prefect and the rest all have the same
problem: a queue full of other people's scripts, and one process per run.
That is the integration Zygo is built for, and this is what it looks like.

```bash
zygo api --allow-deploy &          # 127.0.0.1:7700
python3 worker.py --seed 25        # queue 50 runs and drain them
```

```
queued 50 jobs
  warmed normalise-2224de15030ea438: python/3.12.14, 16 MB resident after 5 ms
  warmed summarise-770f4aadd41f687d: node/22.23.2, 43 MB resident after 0 ms
50 jobs in 1493 ms (29.9 ms each, warming included)
  normalise    completed    25  avg 0.1 ms
  summarise    completed    25  avg 0.09 ms
{"completed": 50}
```

Two interpreters started, fifty runs, each in a process of its own that
nothing from the previous run can reach.

The two columns measure different things and both are worth reading. `avg` is
the handler's own wall clock, as the supervisor measured it inside the
request — a tenth of a millisecond, because these handlers do almost nothing.
The 29.9 ms is this worker's own view of a job: the queue, an HTTP round trip
to `zygo api`, the fork, and a share of the two ~350 ms warms amortised over
fifty runs. The fork itself is the ~2 ms in
[`docs/performance.md`](../../docs/performance.md); the numbers above were
taken inside a nested VM on a 5.10 kernel, which is the slow end of both.

## The whole integration

```python
name = f"{script}-{digest}"                           # a version is a function
client.serve(name, layer, if_changed=True)            # once per script version
result = client.fn(name)(args)                        # once per run
```

`serve` warms a zygote: the interpreter starts, the script's imports are paid,
and the process waits. `if_changed=True` makes calling it on every job free —
a script whose source has not moved answers `unchanged` and nothing restarts.
`fn(name)(args)` is a `fork()` of that warmed process: a fresh pid, its own
cgroup, its own `/tmp`, and the result back in about 2 ms.

The alternative the engines actually ship is one of:

| | What a run costs | What a run can see |
|---|---|---|
| a fresh interpreter per run | 20–300 ms | whatever the process user can |
| a container per run | 150 ms–2 s | its own namespaces, after the create |
| a shared interpreter, reset between runs | ~0 ms | everything the last run left |
| **a Zygo fork** | **~2 ms** | **a clean copy of the warmed process** |

The third row is the one to look at, because it is what a worker reaches for
when the first two are too slow. Resetting an interpreter between runs is
best-effort by construction: a monkeypatch, an `atexit` handler, a cached
connection, a module-level mutation, an `os.environ` entry. A fork is not
best-effort — request *n* runs on a copy of the memory the zygote had before
request *n-1* existed.

## What is here

| | |
|---|---|
| [`worker.py`](worker.py) | The integration: warm per script version, fork per run, an LRU over warm scripts, and Zygo's failures mapped to job states. |
| [`jobqueue.py`](jobqueue.py) | The engine's half — a SQLite job queue that has never heard of Zygo. Replace it with Postgres, BullMQ or your own. |
| [`scripts/`](scripts) | Two user scripts, one Python and one Node, written the way an engine's editor would show them. |
| [`sandbox.toml`](sandbox.toml) | The API, with no functions in it: the scripts arrive with the jobs. |

## Mapping it to a real engine

| | Windmill | n8n | here |
|---|---|---|---|
| queue | `v2_job_queue` in Postgres, `FOR UPDATE SKIP LOCKED` | BullMQ over Redis | `jobqueue.py` |
| script identity | the script hash | node type + workflow version | `digest` |
| per-run isolation | nsjail, a fresh interpreter per job | `vm2`, or a task runner process | one Zygo fork |
| where the change goes | `handle_job` in the worker | the task runner | `run_one` |
| result | `v2_job_completed` | execution data | `Queue.complete` |

In Windmill the worker already knows how to fetch a script and its arguments;
what changes is the line that shells out to an interpreter under nsjail. In
n8n the equivalent is the task runner: one long-lived process holding the
client, with every Code node execution becoming one `client.fn(...)` call.

## The parts that are policy, not plumbing

These are in `worker.py` because a platform has to decide them, and a script
author must not:

* **`LIMITS`** — memory, CPU, pids, a deadline and `seccomp = "strict"`,
  applied to every script whatever it asks for. The engine owns the limits.
* **`network = "none"`** — a script gets egress only when the engine's own
  policy grants it, through `network = "egress"` and an `allow` list of
  `host:port` entries. There is no in-between and no way for a script to widen
  it.
* **`WARM_LIMIT`** — how many scripts this worker holds warm. A zygote is a
  held sandbox with an interpreter in it; a platform with ten thousand scripts
  keeps the hot ones and lets the rest go. Zygo's idle tiering does the same
  on a timer, and `zygo ps` shows which are paused.
* **The digest in the name** — two versions of a script are two functions, so
  a run queued against the old version gets the old version. Reusing one name
  across versions gives a queued run whichever version happens to be warm.

## Secrets

A script that needs a credential does not get it in its arguments, where it
would be in the queue, in the logs and in the result row. `serve` takes a
`secrets` mapping; the supervisor writes each one to `/run/secrets/<name>`
inside the sandbox for the duration of a request and removes it afterwards.
The zygote never sees the value and it is in no message on the wire.

```python
client.serve(name, layer, secrets={"STRIPE_KEY": engine.secret_for(script)})
```

## What this example is not

It is not a Windmill fork or an n8n plugin. The queue here is ninety lines of
SQLite so that the example runs with nothing installed; the point is the three
lines in the middle, which are the same in any engine.
