# n8n's Code node, run by Zygo

[n8n](https://n8n.io) runs a Code node through a *task runner*: a separate
process that connects to n8n's *task broker*, is offered tasks, runs the code
and sends the result back. n8n ships two runners, one for JavaScript and one
for Python. `zygo_runner.py` is a third, for both languages. It runs no user
code itself. Each task becomes one request to a Zygo runtime pool: a fork of
a warm interpreter, in its own cgroup, under seccomp and Landlock, with no
network, gone when the task ends.

```text
                  ┌────────────── n8n ───────────────┐
  webhook ───────▶│ workflow ─▶ Code node ─▶ broker  │
                  └────────────────────┬─────────────┘
                                       │ WebSocket, n8n's runner protocol
                                       ▼
                  ┌──────── zygo_runner.py ──────────┐
                  │ wraps the code, asks for items   │
                  └────────────────────┬─────────────┘
                                       │ POST /runtimes/<pool>/call
                                       ▼
                  ┌──────── zygo api + pools ────────┐
                  │ py313   python:3.13-slim         │  one fork per task:
                  │ node26  node:26-slim             │  256 MB, 1 CPU, 64 pids,
                  └──────────────────────────────────┘  no network, 60 s
```

It is an example, not a product: about 330 lines, with the parts of n8n's
Code node that a runner can serve without n8n's own internals. The list of
what it does not do is below, and each of those fails the task with a message
rather than doing something close to it.

## Run it

You need a Linux host where `zygo doctor` passes, n8n 2.x with task runners in
external mode, and Python with `websockets`. Everything below runs from this
folder.

```bash
zygo api &                                   # reads [api] from sandbox.toml
zygo serve --runtime py313
zygo serve --runtime node26
```

n8n needs to be told its runners live outside it, and share a secret with
them:

```bash
N8N_RUNNERS_MODE=external
N8N_RUNNERS_AUTH_TOKEN=<a long random string>
N8N_RUNNERS_BROKER_LISTEN_ADDRESS=127.0.0.1     # where the runner connects
```

Then the runner, with the same secret:

```bash
pip install websockets
PYTHONPATH=../../sdk/python/src \
N8N_RUNNERS_AUTH_TOKEN=<the same string> \
python3 zygo_runner.py
```

It registers for both task types, keeps five task offers open per language,
and reconnects if n8n restarts. `bench/stack.sh zygo` does all of this with a
throwaway n8n in Docker.

## What a Code node gets

| JavaScript | Python |
|---|---|
| `$input.all()`, `$input.first()`, `$input.last()`, `$input.item` | `_items`, `_item` |
| `items`, `item`, `$json` | |
| `await` anywhere in the code | any import the image has |
| Run Once for All Items, Run Once for Each Item | both modes |
| `console.log` lines passed to n8n as its runners pass them | `print` lines, the same way |

The wall is Zygo's, not a list of permitted modules. Python code may `import
os` and JavaScript may `require('fs')`; what they find is the sandbox's own
empty `/tmp` and an image read-only, no network, no environment of n8n's, and
a memory, process and time limit of their own. n8n's stock runners do it the
other way round: they block modules and builtins at the language level
(`NODE_FUNCTION_ALLOW_BUILTIN`, `N8N_RUNNERS_STDLIB_ALLOW`), and once those are
opened, the code runs with the runner's own reach.

## What it does not do

- `this.helpers.*`: HTTP requests and binary data, which n8n serves to its own
  runners over RPC.
- `$node`, `$('Other node')`, `$workflow`, `$env`, `$execution`, workflow
  static data.
- JavaScript's AI-tool mode (`runCode`) and chunked input for Run Once for
  Each Item.
- Luxon's `DateTime`, which n8n's runner provides and `node:26-slim` does not.

Each fails the task with a message naming it. Output lines are sent with the
RPC call n8n's own runners use for them; that the editor shows them was not
checked.

## Measured against n8n's own runners

`bench/` is the whole comparison: n8n 2.38.7 with its stock runner sidecar,
the same n8n with this runner, and n8n's stock runner image unchanged inside
one long-lived Zygo sandbox. The same workflows, the same load, the same
machine. What it measured, on what, and what the numbers do not say, is in
[chapter 25](../../docs/book/25-performance.md#behind-n8ns-code-node);
[chapter 10](../../docs/book/10-similar-projects.md#inside-n8n-as-its-code-node-runner)
has the short version.

```bash
cd bench
sh run.sh                              # about two hours on two cores
STACKS="stock zygo box" sh run.sh      # with the third stack
python3 summarize.py                   # the tables, as markdown
```

The harness needs Linux, Docker with compose, a systemd user session and
`zygo`. Each part it measures runs in a cgroup of its own — n8n and the stock
runner as containers, the rest as user units — and the load generator's CPU,
and whatever else the machine did, are reported beside the numbers, so a run
that was disturbed says so. `DOCKER="sudo docker"` where docker needs root;
`ZYGO_RUNTIME_DIR` and `ZYGO_DATA_HOME` to run beside another Zygo on the same
host without touching its supervisor.

| File | What it does |
|---|---|
| `bench/stack.sh` | brings one stack up behind a fresh n8n: `stock`, `zygo` or `box` |
| `bench/run.sh` | every stack twice, in opposite orders, then first-run latency and the probes |
| `bench/bench.py` | one measurement: one at a time, a burst, or a steady rate; CPU and memory per part |
| `bench/workflows.py` | the 19 workflows: four kinds of work in each language, probes, hostile code |
| `bench/coldstart.sh` | the first run after a restart and after 30 s idle |
| `bench/probes.sh` | what a Code node can reach, and what hostile code does to its neighbours |
| `bench/summarize.py` | `work/*.jsonl` into the tables |

## Running n8n's own runners inside Zygo

The third stack, `box`, needs nothing from this folder but `stack.sh`: it
starts the `n8nio/runners` image with its own entrypoint — n8n's Go launcher
and both runners — as one `zygo run` sandbox whose only network is the broker.
The launcher listens on local health-check ports, which a function's sandbox
allows ([ADR 0008](../../docs/book/adr/0008-listening-inside-a-sandbox.md)); it
also connects to them, and under `egress` a connect meets the allowlist's port
rule even on loopback, so the three ports are named in `--allow`. The broker
is reached on the Docker bridge's address, because a sandbox that dials its
host's main address reaches itself
([chapter 14](../../docs/book/14-limits-network-secrets.md#the-hosts-own-loopback)).

This gives n8n's runners a wall — a memory limit, no network but the broker —
without changing a line of them. It does not give each task its own: the
runners still run every task in one process tree, and one task's memory
overrun takes the whole box down with it.
