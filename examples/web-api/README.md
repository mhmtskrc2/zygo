# A tiny web API, three ways to run code

One small Python web server with three endpoints. Each endpoint runs code in
Zygo in a different way, so you can see the three side by side:

| Endpoint | How it runs the code | What is ready before the request |
|---|---|---|
| `POST /lower` | a **warm function** | the sandbox, Python, and our handler |
| `POST /script` | a **runtime pool** | the sandbox and Python — the code comes with the request |
| `POST /cold` | a **fresh sandbox** | nothing — everything is built for this request |

```text
  curl ──▶ app.py (port 8000) ──▶ zygo api (port 7700) ──▶ supervisor
                                                             │
                 /lower  ─── fork the warm `lower` zygote ◀──┤  ready: sandbox + Python + lower.py
                 /script ─── fork the `py312` pool, load  ◀──┤  ready: sandbox + Python
                             the caller's code in the copy   │
                 /cold   ─── build a new sandbox, run,    ◀──┘  ready: nothing
                             remove it
```

The web server uses only Python's standard library, plus Zygo's own Python
client from [`sdk/python`](../../sdk/python), which has no dependencies
either. No Flask, no `pip install`.

## The files

```text
  web-api/
  ├── sandbox.toml   declares the warm function `lower` and the pool `py312`
  ├── lower.py       the handler behind /lower: two lines
  └── app.py         the web server: three endpoints, about 70 lines
```

## Run it

You need a host where `zygo doctor` passes (on a Mac, Zygo runs everything
in its Linux VM for you). Run each step from this folder.

```bash
cd examples/web-api

# 1. Warm the two things that stay warm.
zygo up                              # the function `lower`
zygo serve --runtime py312           # the pool `py312`

# 2. Start Zygo's HTTP API, with a token the app will also use.
export ZYGO_API_TOKEN=$(head -c16 /dev/urandom | od -An -tx1 | tr -d ' \n')
zygo api --allow-deploy &            # why --allow-deploy: see below

# 3. Start the web app.
PYTHONPATH=../../sdk/python/src python3 app.py &
```

## Call it

```bash
curl -X POST localhost:8000/lower  -d '{"text": "Hello WORLD"}'
# {"text": "hello world"}

curl -X POST localhost:8000/script \
     -d '{"code": "def handler(e):\n    return {\"words\": len(e[\"text\"].split())}",
          "event": {"text": "one two three"}}'
# {"result": {"words": 3}, "stdout": ""}

curl -X POST localhost:8000/cold   -d '{"code": "print(6 * 7)"}'
# {"exit_code": 0, "stdout": "42\n", "stderr": ""}

curl -X POST localhost:8000/script -d '{"code": "def handler(e):\n    1/0"}'
# 422, with the traceback: the caller's code raised, and only its copy died
```

These are the real answers from a test run on an Ubuntu 24.04 VM. On a Mac,
the `curl` calls work from macOS itself too: Lima passes the VM's ports
through.

## What each endpoint teaches

**`/lower` — a warm function.** `zygo up` loaded `lower.py` once. Each request
is a copy (a `fork`) of that loaded process, so it starts in about 2 ms and
starts clean. Use this when the same code runs again and again.

**`/script` — a runtime pool.** The pool keeps Python warm but holds none of
your code. Each request carries its own script; the copy loads it, runs its
`handler`, and is thrown away. Use this when every caller brings different
code — one pool serves thousands of scripts.

**`/cold` — a fresh sandbox.** Nothing is warm. Zygo builds a sandbox, starts
Python, runs the code and removes everything. It is slower — the sandbox and
Python's start come first — but needs no setup. Use this for code that runs
once.

## Why `--allow-deploy`?

`/lower` and `/script` only *call* things that are already warm, and any
token may do that. `/cold` asks the API to *create* a new sandbox
(`client.run(...)`), and that needs **deploy rights**: whoever holds a token
with them can run any image as your user. That is fine here, on
`127.0.0.1`, with a token only you know. Do not start an API like this on an
address other machines can reach.

## How fast?

In the test above, each call took about 13 ms for `/lower` and `/script` and
about 25 ms for `/cold`, measured with `curl`. Most of the 13 ms is `curl`
itself and the little web server; Zygo's own share of a warm call is about
2 ms. [What Zygo costs](../../docs/book/25-performance.md) has the measured
numbers for each path.

## Stop it

```bash
kill %1 %2        # the web app and the API (or close their terminals)
zygo down         # the function `lower`
zygo stop --all   # everything else, including the pool, and the supervisor
```

`zygo stop py312` does not work: `zygo stop NAME` knows functions, not pools.
`zygo stop --all` does end the pool — it may still print "nothing to stop",
because it counts only functions.

## Where to read more

- Warm functions and pools: [chapter 13](../../docs/book/13-warm-functions.md)
- One-shot sandboxes: [chapter 12](../../docs/book/12-one-shot-sandboxes.md)
- The API and the Python client: [chapter 17](../../docs/book/17-api-sdk-mcp.md)
