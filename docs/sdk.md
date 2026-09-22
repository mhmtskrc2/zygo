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

**Who** is the token the call is made with: a *tenant* token is one customer's,
an *operator* token is the host's. **Deploy** marks the calls that need an API
started with `--allow-deploy`, or an operator token minted deliberately — they
name images, mounts and commands, which is running code as the user Zygo runs
as.

| | Python | Node | Who | Deploy |
|---|---|---|---|---|
| Call a warm function | `client.call(name, event)` | `client.call(name, event)` | either | no |
| A callable for one function | `client.fn(name)` | `client.fn(name)` | either | no |
| Several events at once | `client.batch(name, events)` | `client.batch(name, events)` | either | no |
| List functions | `client.functions()` | `client.functions()` | either | no |
| Counters | `client.stats(name)` | `client.stats(name)` | either | no |
| Warm one now | `client.warm(name)` | `client.warm(name)` | either | no |
| Recent log | `client.logs(name)` | `client.logs(name)` | either | no |
| Version | `client.version()` | `client.version()` | either | no |
| Register a script | `client.put_script(source)` | `client.putScript(source)` | either | no |
| Stop a running request | `client.cancel(id)` | `client.cancel(id)` | own, or operator | no |
| Watch a call's output | `client.stream(name, event)` | `client.stream(name, event)` | either | no |
| The same, for a pool | `client.stream_script(rt, script)` | `client.streamScript(rt, script)` | either | no |
| Look a script up | `client.script(digest)` | `client.script(digest)` | either | no |
| Run a script in a pool | `client.run_script(runtime, script)` | `client.runScript(runtime, script)` | either | no |
| List runtime pools | `client.runtimes()` | `client.runtimes()` | either | no |
| Read a tenant | `client.tenant(id)` | `client.tenant(id)` | own, or operator | no |
| Act for a tenant | `client.for_tenant(id)` | `client.forTenant(id)` | operator | no |
| List tenants | `client.tenants()` | `client.tenants()` | operator | no |
| Create a tenant | `client.create_tenant(id)` | `client.createTenant(id)` | operator | no |
| Serve a function | `client.serve(name, layer)` | `client.serve(name, layer)` | operator | **yes** |
| Stop one | `client.stop(name)` | `client.stop(name)` | operator | **yes** |
| One-shot sandbox | `client.run(image, cmd)` | `client.run(image, cmd)` | operator | **yes** |
| Forget a script | `client.delete_script(digest)` | `client.deleteScript(digest)` | operator | **yes** |
| Delete a tenant | `client.delete_tenant(id)` | `client.deleteTenant(id)` | operator | **yes** |
| Serve a runtime pool | `client.serve_runtime(name, layer)` | `client.serveRuntime(name, layer)` | operator | **yes** |
| Stop one | `client.stop_runtime(name)` | `client.stopRuntime(name)` | operator | **yes** |
| Mint a token | `client.mint_token(tenant)` | `client.mintToken(tenant)` | operator | **yes** |
| List tokens | `client.tokens()` | `client.tokens()` | operator | **yes** |
| Revoke one | `client.revoke_token(id)` | `client.revokeToken(id)` | operator | **yes** |

A listing is scoped to the caller: `functions()` and `runtimes()` through a
tenant token show that tenant's, and nobody else's names.

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

## Tenants

An embedder has customers. A **tenant** is one of them, and it is what makes
"whose script is this?" a question the API can answer:

```python
client.create_tenant("acme")                    # POST /tenants, idempotent
acme = client.for_tenant("acme")                # a view; the same connection

script = acme.put_script(source)                # registered against acme
acme.run_script("py312", script.sha256, event)  # and only acme may run it
```

```js
await client.createTenant('acme');
const acme = client.forTenant('acme');
```

What a tenant gets:

* **Its own scripts.** A digest is not a capability — anyone holding the bytes
  can compute one — so a tenant naming a digest it did not register is told
  the script does not exist. The same answer an unregistered digest gets, on
  purpose: "it exists but is not yours" is a fact about another customer.
* **Its own cgroup.** `tenants/<tenant>/<function>/…`, so everything one
  customer runs is in one place, killable in one write, and countable in one
  read. Per-tenant *limits* on that cgroup are the next piece of work.
* **Deletion that means it.** `client.delete_tenant(id)` stops their functions
  and pools and removes the scripts nothing else refers to — and answers with
  both lists, because neither can be reconstructed afterwards.

Listing or creating tenants is the **operator's**: a customer that could
enumerate the other customers is a leak whatever the limits say. A tenant may
read its own record, which is how a client discovers what it registered.

## Tokens

A tenant is only worth having if the server can tell whose request this is
without being told. That is what a token is: the one part of a request the
caller cannot choose.

```python
operator = zygo.connect(url, token=os.environ["ZYGO_API_TOKEN"])
minted = operator.mint_token("acme")        # POST /tenants/acme/tokens
print(minted.secret)                        # the only time this exists

acme = zygo.connect(url, token=minted.secret)
acme.put_script(source)                     # registered against acme, no header
```

```js
const minted = await operator.mintToken('acme');
const acme = connect(url, { token: minted.secret });
```

Two kinds, deliberately not a scope lattice:

* **Operator** (`mint_token()`, no tenant). Whoever runs this Zygo: tenants,
  functions, pools, and more tokens.
* **Tenant** (`mint_token(id)`). One customer: registers scripts for itself,
  calls the pools and functions the operator declared, reads its own record —
  and cannot see that any other tenant exists.

What follows from that:

* **The secret exists once.** The server keeps a SHA-256, so `client.tokens()`
  can list every token on the host without being a way to steal one, and
  nothing can print a secret again. Lose one and you revoke it and mint
  another.
* **`X-Zygo-Tenant` is the operator's.** `for_tenant(id)` says which of *your*
  customers you are acting for. A tenant token already names its tenant, and a
  header that disagrees with it is **refused**, not ignored — a client that
  thinks it is acting for somebody else should be told it is not.
* **Revocation is immediate.** The next request with a revoked token is a
  401; the record stays, marked, so an id in a log line still resolves to
  something.
* **Deleting a tenant takes their keys** along with the scripts only they
  referred to.

`ZYGO_API_TOKEN` is the **bootstrap operator token**: the same variable an
existing deployment already sets, with the same rights it already had, which
is what keeps one working across this change. On the host, `zygo token mint`,
`zygo token ls` and `zygo token revoke <id>` do the same three things without
an HTTP round trip.

## Watching a request

A call that takes a minute has something to say before it finishes:

```python
for event in client.stream("render", {"pages": 400}):
    if event.is_result:
        print(event.result.result)
    else:
        print(event.kind, event.data, end="")      # stdout, stderr, progress
```

```js
for await (const event of client.stream('render', { pages: 400 })) {
  if (event.kind === 'result') console.log(event.result.result);
  else process.stdout.write(event.data);
}
```

Each item is a piece of the request's output, and the last one is the result —
exactly what the non-streaming call would have returned, or what it would have
raised. A handler that printed and then failed produced both, so the output is
delivered *first* and the exception comes when you iterate past the result.

Three kinds arrive. `stdout` and `stderr` are what the request's process wrote,
kept apart as everywhere else. **`progress` is its own kind**, not a line of
stdout: a long request has two things to say — what it printed, and how far it
has got — and a caller that had to parse the first to find the second would be
parsing a handler's log messages. The handler reports it by calling
`event.progress(...)`, which is there whether or not anybody is listening, so a
handler does not break depending on who called it:

```python
def handler(event):
    for n, page in enumerate(event["pages"]):
        event.progress(f"{n} of {len(event['pages'])}")
    return {"done": True}
```

The result still carries the whole of `stdout` and `stderr`, bounded as always.
So a caller that streamed and one that did not see the same text; what
streaming changes is when.

**It is per request, not per function.** A `CHUNK` per `print()` is a syscall
per `print()` on a path measured in milliseconds, so a caller that wants to
watch pays for it and everybody else keeps the shape that was measured — the
request does not even carry the flag. Over HTTP it is `?stream=1`, and the
answer is newline-delimited JSON rather than server-sent events: every client
in every language can read a line and parse JSON, and SSE's framing buys
nothing here.

The streaming connection is held for the whole request and is not pooled.
Abandoning the iterator closes it, which does **not** cancel the request —
pass a `key` and use `cancel` for that.

## Cancelling a request

A request that is running can be stopped:

```python
out = client.call("render", event, key="job-4711")   # name it on the way in
...
client.cancel("job-4711")                            # from anywhere
```

```js
const controller = new AbortController();
const call = client.call('render', event, { signal: controller.signal });
controller.abort();                                  // cancels it on the server too
```

The caller of the cancelled request gets `Cancelled`, which is deliberately
**not** `Timeout`: a timeout says the work is too slow or the limit is too
tight, and this says the answer stopped being wanted. Both arrive as exit 137
from the kernel, and only the side that sent the signal can tell them apart —
so the supervisor records which it was rather than guessing.

Asynchronous Python needs no key at all:

```python
task = asyncio.ensure_future(client.call("render", event))
task.cancel()          # sends the cancel before CancelledError propagates
```

Why a **key** and not the request id: the id is assigned by the host and
arrives *with the answer*, which is too late to stop the call it belongs to.
`X-Zygo-Request-Id` comes back on every response and in the body as
`request_id`, and it is what joins a log line to the request it describes; the
key is what a caller uses to name a request it is still waiting for. Reusing a
key is allowed and means one cancel stops every call under it.

What actually stops the work is the supervisor writing `cgroup.kill` on the
request's own cgroup, from outside the sandbox. That reaches everything the
handler spawned and does not depend on tenant code being in a state where a
signal helps. The agent is *told*, so it can mark the answer — but an agent
that ignores the message changes nothing about whether the request stops.

A cancel that arrives before the request has been let go is the best case: the
process exists and has run nothing, so it is stopped without a line of the
handler having run. `started: false` in the answer says that is what happened.

Cancelling something that has already finished, or that belongs to another
tenant, is the same `NotFound`. Request ids are a counter rather than a secret,
so ownership is what keeps a cancel honest — the same argument a script digest
gets.

## Runtime pools

A warm function is one script in one zygote. A **runtime pool** is the other
shape: an image, a dependency set and an agent, with no code in it, and the
script arrives with the call.

```python
client.serve_runtime("py312", {                 # POST /runtimes
    "image": "python:3.12-slim",
    "agent": "python",
    "min_warm": 2, "max_warm": 8,
    "mem": "512M", "timeout": "60s",
})

script = client.put_script(source)              # once, PUT /scripts
out = client.run_script("py312", script.sha256, {"month": "2026-09"})
print(out.result, out.metrics.wall_ms)
```

```js
await client.serveRuntime('py312', { image: 'python:3.12-slim', agent: 'python' });
const { sha256 } = await client.putScript(source);
const out = await client.runScript('py312', sha256, { month: '2026-09' });
```

`client.runtimes()` lists the pools with their zygote counts, and
`client.stop_runtime(name)` drops them. `run_script` also takes the source
directly, for a one-off that is not worth registering.

The request never reaches the zygote: the supervisor writes the script into
the sandbox and the forked child loads it, after the child's seccomp filter is
installed. That is what makes it safe for two tenants to share a pool, and it
is checked rather than asserted — `poc/verify_api.sh` asks a script where it
was loaded from and whether it can list what else is in flight.

## The script store

An embedder's scripts live in the embedder's database, not on the Zygo host.
`PUT /scripts` is how one gets to a sandbox without a file on the host or a
line in `sandbox.toml`:

```python
script = client.put_script(source)      # PUT /scripts, body is the script
script.sha256                           # 'sha256:71e2b5…' — its name from now on
script.existed                          # the store already had exactly these bytes
```

The body is the script itself, not JSON around it: a script is a file, and
wrapping its bytes to unwrap them again has no reader. The name is the SHA-256
of those bytes, which makes the call idempotent in the strongest sense — the
same script from two tenants is one file on disk, and neither of them can put
different bytes under a digest the other is running.

`client.script(digest)` says whether the host holds it and how big it is, and
never returns the bytes: a digest is not a capability, so a store that answered
with the script would make every tenant's code readable by anyone who could
guess what it was. `client.delete_script(digest)` forgets one.

Registering a script does not make it runnable on its own — a request still has
to name something to run it *in*, which is a runtime pool the operator
declared. That is why `put_script` is not behind the deploy gate: a tenant
registering its own code runs nothing by doing so, and the digest is theirs
from then on.

## The deploy gate

`zygo api` starts **call-only**. A token then reaches the functions somebody
declared in a spec file and nothing else, which is the shape most deployments
want: the boundary lives in a file that was reviewed.

`--allow-deploy` adds `PUT /fn/<name>`, `DELETE /fn/<name>`, `POST /run`,
`DELETE /scripts/<hash>`, `POST /runtimes`, `DELETE /runtimes/<name>`,
`DELETE /tenants/<id>` and the token routes. The first three let a caller name
any image, any mount and any command, which is running arbitrary code as the
user the API runs as — a shell, not an API. Turn it on for a local SDK or an
embedder you control, and think twice anywhere else.

Creating a pool is a deploy in its own right — it names an image and mounts.
Deleting a script is gated because the store is shared by digest: forgetting
one script forgets it for every tenant that registered the same bytes.
`PUT /scripts`, `GET /scripts/<hash>`, `GET /runtimes` and
`POST /runtimes/<name>/call` are not gated — registering code that needs a pool
to run, and calling a pool somebody else declared, is exactly what a call-only
token is for.

A **tenant token never deploys**, whatever the flag says: the flag decides what
the operator may do, and one customer does not get to name an image because
another is trusted. An **operator token** minted with `zygo token mint` does,
unconditionally — minting it already required deploy rights, so the decision
was made when it was created.

Without those rights, the calls raise `AuthError`, and the message names the
flag for an operator and says what a tenant token is for instead.

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
| `Cancelled` | somebody stopped the request | nothing: this is what was asked for |
| `Stuck` | the sandbox went quiet with budget left | look at the function, not at its `timeout` |
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
only the side that enforced the deadline can tell them apart. The same argument
gives exit 137 four readings in all — a deadline, an out-of-memory kill, a
cancel, and a sandbox that went quiet — and the supervisor is the only side
that knows which, so it says.

## Long requests

A function's `timeout` may be hours, and a caller may wait up to a day
(`X-Zygo-Timeout-Ms`). Two things make that safe rather than a way to hold a
slot for ever:

* **A heartbeat.** The agent says every second or two that a request is still
  alive. A request the supervisor has heard nothing about for a minute is
  killed and raises `Stuck` — whatever its budget said. So a wedged request
  costs a minute rather than its whole timeout, and "your code is slow" stays
  distinct from "the sandbox stopped answering".
* **The idle policy leaves working zygotes alone.** A zygote nobody has called
  for `idle_timeout` is frozen; one with a request in flight is not, whatever
  the clock says, because freezing it would stop the request it is serving.
  A request that runs for an hour under a two-second `idle_timeout` still
  finishes.

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

For one customer rather than the whole host:

```bash
zygo token mint --tenant acme      # prints the secret, once
```

The test suites do **not** need any of that. Both run against a stand-in API
and check the client: the transport, the error mapping, the connection pool.
A test that needs a real sandbox belongs in the Rust suites, against a real
kernel.

```bash
make test-sdk
```
