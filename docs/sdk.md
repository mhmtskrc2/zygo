# The SDKs

Two clients, Python and Node, over the HTTP API. Both have **no dependencies**
— the standard library has an HTTP client in each language, and a unix socket
is a few lines on top of it. An SDK for a runtime whose argument is a small,
auditable boundary should not arrive with a dependency tree of its own.

Neither is a second implementation of Zygo. Every boundary a sandbox has is
built by the `zygo` binary and enforced by the kernel. Nothing in either client
can widen one.

```
your process ──HTTP──► zygo api ──control socket──► supervisor ──► warm sandboxes
```

On one machine the HTTP hop is a unix socket at `0600`, so there is no port and
no token. Across machines it is TCP with a bearer token.

## Python

```bash
pip install zygo            # or: pip install -e sdk/python
```

```python
import zygo

client = zygo.connect()                       # ZYGO_API_URL, or 127.0.0.1:7700
resize = client.fn("resize")                  # a function from sandbox.toml

out = resize({"url": "https://example.com/a.png"})
print(out.result, out.metrics.wall_ms)
```

Asynchronous, which is what an agent framework needs:

```python
import asyncio, zygo.aio

async def main():
    async with zygo.aio.connect() as client:
        results = await asyncio.gather(*(client.call("resize", e) for e in events))

asyncio.run(main())
```

## Node

```bash
npm install zygo
```

```js
import { connect } from 'zygo';

const client = connect();
const resize = client.fn('resize');

const out = await resize({ url: 'https://example.com/a.png' });
console.log(out.result, out.metrics.wallMs);
```

TypeScript types ship beside the JavaScript, hand-written rather than compiled,
so the package has no build step and what you read in the repository is what
executes.

## What the clients can do

| | Python | Node | Needs `--allow-deploy` |
|---|---|---|---|
| Call a warm function | `client.call(name, event)` | `client.call(name, event)` | no |
| Several events at once | `client.batch(name, events)` | `client.batch(name, events)` | no |
| List functions | `client.functions()` | `client.functions()` | no |
| Counters | `client.stats(name)` | `client.stats(name)` | no |
| Warm one now | `client.warm(name)` | `client.warm(name)` | no |
| Recent log | `client.logs(name)` | `client.logs(name)` | no |
| Version | `client.version()` | `client.version()` | no |
| Serve a function | `client.serve(name, layer)` | `client.serve(name, layer)` | **yes** |
| Stop one | `client.stop(name)` | `client.stop(name)` | **yes** |
| One-shot sandbox | `client.run(image, cmd)` | `client.run(image, cmd)` | **yes** |

A one-shot run answers with more than an exit code, because the exit code
cannot carry what a caller needs:

```json
{"exit_code": 137, "timed_out": false, "oom_killed": true,
 "peak_rss_kb": 65536, "wall_ms": 412.7, "stdout": "", "stderr": ""}
```

A deadline kill and an out-of-memory kill are both `SIGKILL`, so both are 137.
`timed_out` comes from the launcher, which enforced the deadline; `oom_killed`
comes from the kernel's own counter in the sandbox's cgroup. Neither is a
guess, and a caller deciding between "too slow" and "too much memory" — an
online judge, a CI step — has nothing else to go on.

## The deploy gate

`zygo api` starts **call-only**. A token then reaches the functions somebody
declared in a spec file and nothing else, which is the shape most deployments
want: the boundary lives in a file that was reviewed.

`--allow-deploy` adds `PUT /fn/<name>`, `DELETE /fn/<name>` and `POST /run`.
Those let a caller name any image, any mount and any command, which is running
arbitrary code as the user the API runs as — a shell, not an API. Turn it on
for a local SDK or an embedder you control, and think twice anywhere else.

Without it, those three calls raise `AuthError` and the message names the flag.

Two things stay off even then, because a request body must not be able to
remove a guarantee: host networking and private-range egress are refused over
HTTP whatever the body says. Set them where the sandbox is declared.

## Errors

Each kind of failure is its own type, because each one implies something
different about what to do next.

| Type | Means | What to do |
|---|---|---|
| `Busy` | the pool is full; **the request never ran** | retry after `retry_after` |
| `Timeout` | the deadline killed the request | the work is too slow, or the limit is too tight |
| `HandlerError` | the handler raised; carries both streams | fix the function |
| `NotFound` | no function under that name | `serve` it |
| `AuthError` | wrong token, or a deploy call on a call-only API | check the token or the flag |
| `SpecError` | the sandbox as described cannot be resolved | fix the request |
| `TransportError` | the API could not be reached | nothing ran; nothing about the sandbox follows |

The distinction `Busy` versus `HandlerError` is the one worth caring about.
Backpressure means the request was refused before anything happened, so
retrying is correct; a handler that raised will raise again.

`Timeout` is not a guess. The supervisor records that *it* killed the request,
because a deadline kill and an out-of-memory kill both arrive as exit 137, and
only the side that enforced the deadline can tell them apart.

A `batch` is the exception to all of this: each element is a result *or* an
error, returned rather than raised, because one refused event must not hide the
answers to the others.

## Connecting

Both clients resolve the address in the same order: the argument, then
`ZYGO_API_URL`, then `http://127.0.0.1:7700`. Accepted forms:

```
unix:///run/user/1000/zygo/api.sock     a local API; no token needed
http://127.0.0.1:7700                   the default
https://zygo.internal:8443              across a network
box:9000                                bare host and port
```

The token comes from `ZYGO_API_TOKEN` unless one is passed, which is the same
variable the server reads — a shell that can start the API can talk to it.

Both clients pool connections and are safe to share between threads or tasks.
That is not an optimisation detail: one connection would serialise concurrent
callers behind a socket, and the warm path is measured in milliseconds.

## Versioning

`GET /version` reports three numbers:

- `version` — the Zygo release.
- `api` — the HTTP surface. Bumped only on an incompatible change to a route,
  so it stays put across releases that change what happens behind them. This is
  what a client checks.
- `control` — the CLI-to-supervisor protocol, which no SDK speaks. Reported
  because a mismatch there explains an API that is up and answering errors.

Both packages are `0.1.0` and follow the repository's `0.x` policy: the shape
may change with a release note, and after `1.0` it will not without a major
version.

## Running one against a real Zygo

```bash
export ZYGO_API_TOKEN=$(head -c 32 /dev/urandom | base64)
zygo up                            # warm what sandbox.toml declares
zygo api --allow-deploy &          # 127.0.0.1:7700

python -c "import zygo; print(zygo.connect().functions())"
```

The test suites do **not** need any of that. Both run against a stand-in API
and check the client: the transport, the error mapping, the connection pool.
A test that needs a real sandbox belongs in the Rust suites, against a real
kernel.

```bash
make test-sdk
```
