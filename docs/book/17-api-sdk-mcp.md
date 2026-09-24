# 17. The HTTP API, the SDKs and MCP

The CLI is one way in. Programs use the HTTP API — directly, or through the
Python and Node clients — and agent hosts use the MCP server. All of them end
at the same supervisor, the long-lived Zygo process that owns the warm
sandboxes.

```text
  your program ──▶ Python / Node SDK ──┐
  curl ────────────────────────────────┤  HTTP  ┌──────────┐ unix socket ┌────────────┐
                                       └───────▶│ zygo api │────────────▶│            │
                                                └──────────┘             │            │
  CLI (zygo exec, zygo up, …) ──────────────────────────────────────────▶│ supervisor │
                                                                         │            │
  agent host ──▶ zygo mcp (stdin/stdout) ───────────────────────────────▶│            │
                                                                         └────────────┘
```

This chapter has three parts. First the HTTP API itself: how to start it,
who may call what, the routes and the answers. Then the two SDKs, and the
features they reach: tenants, secrets, files, streaming, cancelling, runtime
pools and dependency sets. Last, the MCP server for agent hosts.

## Part one: the HTTP API

`zygo api` is a small web server. It turns HTTP requests into messages for the
supervisor, and turns the supervisor's answers back into HTTP. It does not run
sandboxes itself. Every boundary a sandbox has is built by the `zygo` binary
and enforced by the kernel, so nothing a caller sends over HTTP can do more
than the rules below allow.

## Starting the API

`zygo api` runs the API in the foreground; run it under systemd or in a
container for production. It listens on `127.0.0.1:7700`, or where `[api]
listen` or `--listen` says — an `IP:PORT` or `unix:///path` (created with mode
0600, so only your user can open it). By default every caller needs a bearer
token — a secret string sent in the `Authorization` header — on every kind of
listener, unix sockets included. `--no-auth` (or `[api] auth = "none"`) turns
that off, and is refused anywhere but a unix socket or a loopback address. So
a unix socket or loopback only *permits* turning auth off; it never skips it by
itself. `zygo api --openapi` prints the OpenAPI 3.1 document for this build;
the same file is committed as [`spec/openapi.json`](../../spec/openapi.json).

## Who is calling: tokens and tenants

```text
  ZYGO_API_TOKEN (bootstrap) ───────┐
                                    ├──▶ OPERATOR: the host's owner. May act for
  operator token (zygo token mint) ─┘    any tenant with the X-Zygo-Tenant header.

  tenant token (zygo token mint --tenant acme) ──▶ TENANT acme: sees and calls
                                                   only its own things.
```

A **tenant** is one customer of whoever embeds Zygo. A **token** proves who
is calling, and the caller cannot choose it. The bootstrap token comes from
`ZYGO_API_TOKEN`; more are minted with `zygo token mint` (secrets look like
`zygo_` plus 64 hex characters). Only a hash is stored, so a lost token
cannot be shown again — revoke it and mint another. A revoke works from the
next request. An operator acting for a tenant sends `X-Zygo-Tenant: acme`; a
tenant token that sends a different tenant gets 403. The sections
[Tenants](#tenants) and [Tokens](#tokens) below show both from the SDKs.

## The deploy gate

Calling a function that exists is one thing; creating sandboxes is another,
because whoever can create one can run any image with any mount as your user.
That is a shell, not an API. So `zygo api` starts **call-only**: a token
reaches the functions somebody declared in a spec file that was reviewed, and
nothing else. The routes that create, change or destroy things need
**deploy rights**. An operator token minted with `zygo token mint` always has
them, because minting it already needed them. The bootstrap token has them
only when `zygo api` was started with `--allow-deploy`. A tenant token never
does, whatever the flag says: one customer does not get to name an image
because another is trusted.

```text
  request with a valid token
        │
        ▼
  route needs deploy rights? ──no──▶ allowed (a tenant sees only its own things)
        │ yes
        ▼
  tenant token? ──yes──▶ 403
        │ no
        ▼
  operator token minted with `zygo token mint` ──▶ allowed
  bootstrap ZYGO_API_TOKEN ──▶ allowed only if `zygo api --allow-deploy`, else 403
```

### Which routes are gated

These need deploy rights: `PUT` and `DELETE /fn/{name}`, `POST /run`,
`POST` and `DELETE /runtimes`, `PATCH /tenants/{id}/limits`, `PUT` and
`DELETE /tenants/{id}/secrets/{name}`, `DELETE /tenants/{id}`,
`DELETE /scripts/{digest}`, `DELETE /blobs/{digest}`, `DELETE /deps/{id}`,
`POST /drain`, and every token route. `POST /tenants` and `GET /tenants` are
for the operator only, but do not need deploy rights.

These are **not** gated, and any valid token may use them: `PUT /scripts`,
`GET /scripts/{digest}`, `PUT /blobs`, `POST /deps`, `GET /runtimes`,
`POST /runtimes/{name}/call` and `DELETE /requests/{id}`. Registering code
that still needs a pool to run, and calling a pool somebody else declared, is
exactly what a call-only token is for.

### Why each one is gated

Serving a function, a one-shot run and creating a pool all name an image and
mounts, which is running code as the user Zygo runs as. Deleting a script, a
blob or a dependency set is gated because each store is shared by digest or
id: forgetting one forgets it for every tenant that sent the same bytes.
Limits, secrets and tokens are the operator's relationship with a customer,
and draining stops the host serving anybody. Without the rights, the SDK
raises `AuthError`; the message names the flag for an operator, and says what
a tenant token is for instead.

### What stays off for everybody

Over HTTP, host networking, private-range egress and unlimited limits are
always refused, whatever the caller and whatever the body says. A request
body must not be able to remove a guarantee. Set those where the sandbox is
declared, in `sandbox.toml` (see [chapter 20](20-sandbox-toml.md)).

## Request and response headers

| Header | Direction | Meaning |
|---|---|---|
| `Authorization: Bearer …` | in | The token. |
| `X-Zygo-Tenant` | in | Which tenant an operator is acting for. |
| `X-Zygo-Timeout-Ms` | in | How long the caller will wait; up to 24 hours. The default wait is 60 s. |
| `X-Zygo-Request-Key` | in | The caller's own name for a request (1–128 printable characters), to cancel it by. |
| `X-Zygo-Request-Id` | out | Zygo's id for the request. |
| `Retry-After` | out | On `429` (1 s), and on `503` while dependencies build (5 s). |

## The routes

"any" means any valid token; "op" means operator; "deploy" means deploy
rights. Bodies are JSON unless marked raw; the limit is 16 MiB.

| Route | Who | What it does |
|---|---|---|
| `GET /healthz` | nobody needs a token | `ok`, `degraded` (a pool below `min_warm`) or `stopping` (503). |
| `GET /version` | any | Zygo version, API version, and whether you have deploy rights. |
| `GET /metrics` | any | Prometheus text (below). |
| `POST /drain?grace_ms=` | deploy | Stop taking requests, finish the running ones, exit. |
| `GET /fn` | any | Functions you can see: state, image, memory, request counts. |
| `PUT /fn/{name}` | deploy | Serve or replace a function. Body: `{layer, base_dir, secrets?, if_changed?}`. |
| `DELETE /fn/{name}` | deploy | Stop it. |
| `POST /fn/{name}` | any | **Call it.** Body: the event. `?stream=1` for live output; `?out=1` to get `/work` back as a tar; `?workspace=sha256:…` to send one in. |
| `POST /fn/{name}/batch` | any | Call it with an array of events (up to 1024); an array of answers comes back. |
| `GET /fn/{name}/logs` | any | Recent log entries: `?after=`, `?limit=`, `?failed=`. |
| `GET /fn/{name}/stats` | any | One function's status. |
| `POST /fn/{name}/warm` | any | Wake a paused or cold function now. |
| `GET /runtimes` | any | Pools and their load. |
| `POST /runtimes` | deploy | Start a pool: `{name, layer, base_dir?, deps?}`. |
| `DELETE /runtimes/{name}` | deploy | Stop a pool. |
| `POST /runtimes/{name}/call` | any | **Run a script in a pool**: `{script, event?, entry_point?, workspace?}`. |
| `PUT /scripts` | any | Store a script (raw body); returns its `sha256`. |
| `GET` / `DELETE /scripts/{digest}` | any / deploy | Check or remove one. |
| `PUT /blobs` | any | Store a tar (raw body), for workspaces. |
| `GET` / `DELETE /blobs/{digest}` | any / deploy | Check or remove one. |
| `POST /deps` | any | Build a dependency set from lock files: `{image, files}`. 202 while building. |
| `GET /deps`, `GET /deps/{id}` | any | Their state, and the build log. |
| `DELETE /deps/{id}` | deploy | Remove one no pool uses. |
| `POST /tenants` · `GET /tenants` | op | Create or list tenants. |
| `GET /tenants/{id}` | that tenant, or op | One tenant: scripts, limits. |
| `DELETE /tenants/{id}` | deploy | Remove it, its scripts and its functions. |
| `PATCH /tenants/{id}/limits` | deploy | Narrow its limits: `mem`, `cpu`, `pids`, `timeout`, `scratch`, `network`, `allow`. |
| `GET /tenants/{id}/secrets` | that tenant, or op | Secret names — never values. |
| `PUT` / `DELETE /tenants/{id}/secrets/{name}` | deploy | Set (raw body) or remove one. |
| `POST /tokens` · `POST /tenants/{id}/tokens` | deploy | Mint an operator or tenant token. |
| `GET /tokens` · `DELETE /tokens/{id}` | deploy | List or revoke. |
| `DELETE /requests/{id}` | any | Cancel your own request, by id or by key. |
| `POST /run` | deploy | A one-shot sandbox: `{layer, stdin?}`; returns exit code, output, and why it ended. |

A batch runs at most 16 of its events at the same time. `?workspace=` on a
function call takes only a blob digest, never an inline tar (see
[Files in and out](#files-in-and-out)).

## Status codes

| Code | Meaning |
|---|---|
| 200 / 201 / 202 | Done / created / still building. |
| 400 | A bad spec, header or body. |
| 401 | No token, or a wrong or revoked one. |
| 403 | Your token may not do this: not the operator, no deploy rights, or the wrong tenant. |
| 404 · 405 | No such thing · wrong method. |
| 408 | The function's own timeout killed the request. On `POST /run`, only when the API's own outer deadline fired (see below). |
| 413 | Body over 16 MiB, or a batch over 1024. |
| 422 | A tenant limit above what could ever apply. |
| 429 | Busy: every slot and the queue are full. Retry after the header. |
| 499 | The request was cancelled. |
| 500 | The handler raised: the body has `error`, `stdout`, `stderr`, `exit_code`. |
| 503 | Warming failed, dependencies still building, or the API is stopping. |
| 504 | The request stopped answering heartbeats: stuck. |

Errors always have the shape `{"error": "…", "code": "…"}`.

## A successful call

```json
{
  "result":     {"count": 3},
  "request_id": "req-01f3…",
  "stdout":     "",
  "stderr":     "",
  "metrics":    {"wall_ms": 4.1, "cpu_ms": 2.3, "peak_rss_kb": 18420}
}
```

A streamed call (`?stream=1`) answers `application/x-ndjson`: one line per
event, `{"stream": "stdout" | "stderr" | "progress", "data": …}`, then a last
line with the body above and a `status`. [Watching a request](#watching-a-request)
explains it.

## The answer of a one-shot run

`POST /run` answers with more than an exit code, because the exit code alone
cannot carry what a caller needs:

```json
{"exit_code": 137, "timed_out": false, "oom_killed": true,
 "peak_rss_kb": 65536, "wall_ms": 412.7, "stdout": "", "stderr": "",
 "started": true, "phase": "run"}
```

A deadline kill and an out-of-memory kill are both `SIGKILL`, so both show
exit code 137. `timed_out` comes from the launcher, which enforced the
deadline; `oom_killed` comes from the kernel's own counter in the sandbox's
cgroup (the kernel group that holds its limits). Neither is a guess. A caller
deciding between "too slow" and "too much memory" — an online judge, a CI
step — has nothing else to go on.

### `started`, and when `POST /run` says 408

`started` is whether the program ran at all. `false`, with `phase` naming
what failed (`plan` or `start`), is Zygo failing to build the sandbox. A
caller reports that as *unavailable*, not as the code's failure, and `ok` is
false for it. An API one release behind does not send the field, and only
answered once the program had run, so the SDKs default it to `true`.

The answer is a `200` whenever the sandbox *ran*, whatever ended it: a
non-zero exit, its own timeout, an out-of-memory kill. The body says which. It
is a `408` only when the API's own outer deadline fired, the child was killed,
and what it would have said is unknown — Zygo failing to finish, not a sandbox
doing its job.

## Metrics, OTLP and usage events

`/metrics` gives Prometheus series: `zygo_api_requests_total`,
`zygo_api_errors_total`, and per function `zygo_function_requests_total`,
`zygo_function_failures_total`, `zygo_function_rss_bytes` and
`zygo_function_state`. `--otlp-endpoint URL` pushes the same numbers, plus
per-tenant requests, outcomes, CPU and wall time, to an OpenTelemetry
collector. `--usage-webhook URL` POSTs batches of usage events for billing —
`{tenant, function, script, request_id, wall_ms, cpu_ms, peak_rss_kb, outcome,
finished_ms}` — **at least once**, so key on `request_id`. Both count only
requests that went through this API process.

**A caution about `/metrics`.** It has no per-tenant series. It needs a token,
but any valid token can read it, tenant tokens included. And it lists every
function name on the host, so a tenant token can see the names of other
tenants' functions there. Put `/metrics` behind your own proxy, or do not give
customers direct access to the API, if those names matter.

[Usage, for billing](#usage-for-billing) has the detail on the usage events.

## Part two: the SDKs

There are two clients, Python and Node, over the HTTP API. Both have **no
dependencies**: the standard library has an HTTP client in each language, and
a unix socket is a few lines on top of it. An SDK for a runtime whose point is
a small, auditable boundary should not arrive with a dependency tree of its
own. Neither is a second implementation of Zygo, and nothing in either client
can widen a sandbox.

```text
  your process ──HTTP──▶ zygo api ──control socket──▶ supervisor ──▶ warm sandboxes
```

On one machine the HTTP hop can be a unix socket at mode `0600`, so there is
no port. Across machines it is TCP. Either way a bearer token is needed,
unless the API was started with auth turned off on a unix socket or loopback
address.

## The Python client

```bash
pip install zygo            # or: pip install -e sdk/python
```

No dependencies. `zygo.connect()` finds the API from its `url` argument, then
`ZYGO_API_URL`, then `http://127.0.0.1:7700`, and its token from `token=` or
`ZYGO_API_TOKEN`.

```python
import zygo
client = zygo.connect()
out = client.fn("resize")({"url": "…"})               # a call; ~2 ms of overhead
res = client.call("resize", {"url": "…"}, timeout=5)   # the full Result
print(res.result, res.metrics.wall_ms)
for ev in client.stream("resize", {"url": "…"}):       # live output
    print(ev.kind, ev.data)
```

The client covers every route: `call`, `batch`, `stream`, `cancel`, `logs`,
`stats`, `warm`, `serve`, `stop`, `run`, `functions`; pools with
`serve_runtime`, `run_script`, `stream_script`, `put_script`; `put_deps`,
`put_blob`; tenants, secrets, limits and tokens; `for_tenant(id)`; `health`,
`version`, `drain`. [What the clients can do](#what-the-clients-can-do) lists
every method.

### The async Python client

`zygo.aio` is an asynchronous client, which is what an agent framework
needs:

```python
import asyncio, zygo.aio

async def main():
    async with zygo.aio.connect() as client:
        results = await asyncio.gather(*(client.call("resize", e) for e in events))

asyncio.run(main())
```

It covers calls, streaming, batches, logs, cancelling, functions, one-shot
runs, pools, scripts and dependency sets. It leaves some things out **on
purpose**: there is no `workspace` or `out` on `call` and `run_script`, and no
tenant, secret, token, limit, blob, drain or `for_tenant` methods. Admin work
is rare and fits the plain client; the async one is for the hot path.

## The Node client

```bash
npm install zygo
```

No dependencies, Node 18 or newer, ES modules. The same methods in
camelCase — `runScript`, `serveRuntime`, `putSecret` — with timeouts in
seconds and an `AbortSignal` that cancels the request for you.

```js
import { connect } from 'zygo';
const client = connect();                         // ZYGO_API_URL, ZYGO_API_TOKEN
const resize = client.fn('resize');
const out = await resize({ url: 'https://example.com/a.png' });
console.log(out.result, out.metrics.wallMs);
```

### The TypeScript types lag behind

TypeScript types ship beside the JavaScript in `index.d.ts`. They are written
by hand rather than compiled, so the package has no build step and what you
read in the repository is what runs. The cost is that they are behind the
code today. `drain`, `cancel`, `stream`, `streamScript`, `putBlob`, `blob`,
`deleteBlob`, `putDeps`, `deps` and `deleteDeps` have no types, and neither
do some options: `key`, `signal`, `workspace` and `out` on calls, `deps` on
`serveRuntime`, and `tenant` in the client options. The methods and options
all exist at run time; only the declarations are missing.

## What the clients can do

**Who** is the token the call is made with: a *tenant* token is one
customer's, an *operator* token is the host's. **Deploy** marks the calls that
need deploy rights (see [The deploy gate](#the-deploy-gate)).

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
| Health | `client.health()` | `client.health()` | anyone | no |
| Drain the host | `client.drain(grace)` | `client.drain(grace)` | operator | **yes** |
| Register a script | `client.put_script(source)` | `client.putScript(source)` | either | no |
| Build a dependency set | `client.put_deps(image, files)` | `client.putDeps(image, files)` | either | no |
| How a build went | `client.deps(id)` | `client.deps(id)` | own, or operator | no |
| Stop a running request | `client.cancel(id)` | `client.cancel(id)` | own, or operator | no |
| Limit a tenant | `client.set_limits(id, **keys)` | `client.setLimits(id, keys)` | operator | **yes** |
| A tenant's secret names | `client.secrets(id)` | `client.secrets(id)` | own, or operator | no |
| Set one | `client.put_secret(id, name, v)` | `client.putSecret(id, name, v)` | operator | **yes** |
| Forget one | `client.delete_secret(id, name)` | `client.deleteSecret(id, name)` | operator | **yes** |
| Store a blob | `client.put_blob(tar)` | `client.putBlob(tar)` | either | no |
| Look one up | `client.blob(digest)` | `client.blob(digest)` | either | no |
| Forget one | `client.delete_blob(digest)` | `client.deleteBlob(digest)` | operator | **yes** |
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
| Forget a dependency set | `client.delete_deps(id)` | `client.deleteDeps(id)` | operator | **yes** |
| Delete a tenant | `client.delete_tenant(id)` | `client.deleteTenant(id)` | operator | **yes** |
| Serve a runtime pool | `client.serve_runtime(name, layer)` | `client.serveRuntime(name, layer)` | operator | **yes** |
| Stop one | `client.stop_runtime(name)` | `client.stopRuntime(name)` | operator | **yes** |
| Mint a token | `client.mint_token(tenant)` | `client.mintToken(tenant)` | operator | **yes** |
| List tokens | `client.tokens()` | `client.tokens()` | operator | **yes** |
| Revoke one | `client.revoke_token(id)` | `client.revokeToken(id)` | operator | **yes** |

A listing is scoped to the caller: `functions()` and `runtimes()` through a
tenant token show that tenant's names, and nobody else's. (`/metrics` is the
exception; see the caution in [Metrics, OTLP and usage
events](#metrics-otlp-and-usage-events).)

## Connecting

Both clients find the address in the same order: the argument, then
`ZYGO_API_URL`, then `http://127.0.0.1:7700`. These forms are accepted:

```text
unix:///run/user/1000/zygo/api.sock     a local API over a unix socket
http://127.0.0.1:7700                   the default
https://zygo.internal:8443              across a network
box:9000                                bare host and port
```

The token comes from `ZYGO_API_TOKEN` unless one is passed. That is the same
variable the server reads, so a shell that can start the API can talk to it.
A unix socket still needs the token, unless the API was started with
`--no-auth`. Both clients pool connections and are safe to share between
threads or tasks. That matters: one connection would line concurrent callers
up behind a single socket, and the warm path is measured in milliseconds.

## Tenants

An embedder — a product that runs its customers' code on Zygo — has
customers. A **tenant** is one of them, and it is what lets the API answer
"whose script is this?":

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

Listing or creating tenants is the **operator's**: a customer that could list
the other customers is a leak, whatever the limits say. A tenant may read its
own record, which is how a client finds out what it registered.

### What a tenant gets

* **Its own scripts.** A digest (the SHA-256 hash that names a script) is not
  a key to it — anyone holding the bytes can compute one. So a tenant naming a
  digest it did not register is told the script does not exist. That is the
  same answer an unregistered digest gets, on purpose: "it exists but is not
  yours" is a fact about another customer.
* **Its own cgroup.** `tenants/<tenant>/<function>/…`, so everything one
  customer runs is in one place, can be killed in one write, and counted in
  one read. [Limiting a tenant](#limiting-a-tenant) narrows it.
* **Deletion that means it.** `client.delete_tenant(id)` stops their functions
  and pools and removes the scripts nothing else refers to. It answers with
  both lists, because neither can be rebuilt afterwards. It also takes the
  tenant's tokens.

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

There are two kinds, on purpose, and no finer ladder of scopes. An
**operator** token (`mint_token()`, no tenant) belongs to whoever runs this
Zygo: tenants, functions, pools, and more tokens. A **tenant** token
(`mint_token(id)`) belongs to one customer. It registers scripts for itself,
calls the pools and functions the operator declared, reads its own record —
and cannot see that any other tenant exists.

### What follows from that

* **The secret exists once.** The server keeps only a SHA-256 of it, so
  `client.tokens()` can list every token on the host without being a way to
  steal one. Nothing can print a secret again: lose one, revoke it, mint
  another.
* **`X-Zygo-Tenant` is the operator's.** `for_tenant(id)` says which of *your*
  customers you are acting for. A tenant token already names its tenant, and a
  header that disagrees with it is **refused**, not ignored.
* **Revoking is immediate.** The next request with a revoked token is a 401.
  The record stays, marked, so an id in a log line still points at something.
* **Deleting a tenant takes their tokens** along with the scripts only they
  referred to.

`ZYGO_API_TOKEN` is the **bootstrap operator token**: the same variable an
existing deployment already sets, with the rights it already had. On the host,
`zygo token mint`, `zygo token ls` and `zygo token revoke <id>` do the same
three things without an HTTP round trip.

## Health and draining

`GET /healthz` needs no token, so a load balancer can probe it. It answers one
of three things:

| Status | Code | Means |
|---|---|---|
| `ok` | 200 | every pool is at its floor |
| `degraded` | 200 | a pool is below `min_warm`; requests work, the first pay a cold start |
| `stopping` | **503** | the supervisor is draining |

`degraded` is a `200` on purpose. A host that can serve should be served to.
A probe that took hosts out of rotation for being slow would take every host
out at once after a restart. `stopping` is the one answer that is not a 200,
because a balancer that keeps sending to a draining host is the reason
draining fails.

### Draining

```python
client.drain(grace=30)    # {"drained": true, "in_flight": 0}
```

Draining stops taking new requests, lets the running ones finish, answers,
and *then* exits. The default grace is 30 seconds. `in_flight: 0` is a clean
drain; anything else means the grace ran out. A deploy script needs to know
which of those happened. **`SIGTERM` does the same**, so a container stop or a
`systemctl restart` needs no call at all. The grace there is 25 seconds,
chosen against the 30 that systemd and Docker give a process before
`SIGKILL`: a drain that outlived its own kill would never finish.
[Chapter 16](16-production.md) covers running Zygo in production.

## Usage, for billing

Every finished request produces one usage event, from the **supervisor** —
whether it came over HTTP, from `zygo exec` at a terminal, or from an MCP
tool:

```json
{"tenant": "acme", "function": "py312", "script": "sha256:…",
 "request_id": "00000042", "wall_ms": 812.4, "cpu_ms": 740.1,
 "peak_rss_kb": 48200, "outcome": "ok", "finished_ms": 1790000000000}
```

`outcome` is one word — `ok`, `error`, `timeout`, `cancelled`, `stuck` — so a
dashboard groups by it instead of working it out again from four true/false
fields. A caller who cancelled their own request reads `cancelled`, even if
the deadline happened to pass while the kill landed.

### Three ways to collect it

| Where | How | Which requests |
|---|---|---|
| The supervisor's log, target `zygo::usage` | nothing to set up | **all** of them: HTTP, `zygo exec`, MCP |
| OTLP | `zygo api --otlp-endpoint URL`: `zygo.tenant.requests`, `.outcomes`, `.cpu`, `.wall`, one series per tenant | only those that went through this API process |
| A webhook | `zygo api --usage-webhook URL`: batches of up to 256, posted as `{"events": [...]}` | only those that went through this API process |

The OTLP series and the webhook are counted in the memory of the `zygo api`
process. Requests from `zygo exec` or from MCP never pass through it, so they
appear **only** in the supervisor's log. If you bill from the webhook, make
sure every billable request goes through the API.

### The webhook is at least once

A batch that fails goes back on the front of the queue, in order, and is
retried. So a receiver may see an event twice and should key on `request_id`.
The queue holds at most 10 000 events. When a webhook has been down long
enough to fill it, the oldest events are dropped and the count is logged. The
alternative is the API process growing until it takes the *serving* path down
to protect the billing path, which is the wrong way round. The events are in
the supervisor's log either way.

## Limiting a tenant

A pool is declared once by the operator and called by every customer. One
customer should not be able to take the whole of it:

```python
client.set_limits("acme", mem="256M", cpu=0.5, pids=64, timeout="30s")
```

The keys are `mem`, `cpu`, `pids`, `timeout`, `scratch`, `network` and
`allow`. It is a `PATCH`, so the keys you pass are set and the rest are left
alone. `timeout` lands on the supervisor's deadline rather than the cgroup,
because a cgroup cannot enforce a wall clock. The rest are cgroup settings.

### Limits only narrow

A tenant's limits are applied as the minimum of themselves and whatever the
function or pool was declared with. They are written on the request's own
cgroup before the handler is let go. So the worst a wrong value can do is give
a customer less than they were promised, never more. No value and no key can
widen anything, which is what makes the route safe to expose.

A value above **every** ceiling the tenant can reach — their own functions,
and every pool on the host — is refused with `422` naming the key. It could
never take effect, and storing it would let you believe you had tightened
something you had not. Above *one* ceiling and below another is fine: it
narrows the larger and does nothing to the smaller. [Chapter
14](14-limits-network-secrets.md) explains the limits themselves.

## Secrets

A function's `secrets` are delivered as files at `/run/secrets/<name>`,
written from outside the sandbox and removed when the last request in flight
finishes. An operator at a terminal supplies them from their own environment.
An **embedder's customers** cannot: they have their own keys and nobody to
restart a supervisor. So Zygo can store secrets per tenant.

```bash
export ZYGO_SECRETS_KEY=$(zygo secrets keygen)   # before the supervisor starts
zygo secrets set acme STRIPE_KEY                 # reads it with echo off
```

```python
client.put_secret("acme", "STRIPE_KEY", value)   # PUT, needs deploy rights
client.secrets("acme")                           # ['STRIPE_KEY'] — names only
```

A stored secret fills in what the shell did not supply, per tenant, when a
function is served. Where both have a value, the shell wins: `zygo serve` at a
terminal is somebody saying what they want *now*. A tenant cannot set its own
secrets; the operator holds that relationship.

### No way to read a value back

This is not a missing route. There is no answer shape that could carry a
value, because a store that answered with values would make every route that
reaches it a way to read every customer's keys. Values are sealed with
ChaCha20-Poly1305 (an authenticated cipher) under a key Zygo never stores.
Each is bound to its own `tenant/name`, so it cannot be moved to another by
anything that can only rename files. A passphrase is refused rather than
stretched: turning one into a key needs a password key function, and a store
that accepted `hunter2` and stretched it badly would be worse than one that
said no.

### What that protects, and what not

It protects the bytes **at rest**: a backup, a stray `tar`, anything that can
read one user's files. It does not protect them from a process that can read
the supervisor's memory, and it is not a hardware root of trust. An operator
who needs those has a KMS (a key management service).

### Not built: secrets for a runtime pool

A pool's sandbox is shared by several tenants, and `/run/secrets` is one
directory in it. Delivering two tenants' secrets there would put each in reach
of the other, so a pool holds no secrets today. Per-request delivery needs the
same per-request directory that workspaces use, and a second place for
handlers to look is a contract change that deserves its own decision.
Functions have one tenant and are not affected.

## Files in and out

A handler that converts a document needs the document, and the caller needs
what comes back. Neither belongs in a JSON event. So a request can carry a
**workspace**: a tar archive (one file that packs many files) that Zygo
unpacks into the request's own directory. With `out`, the directory comes
back as a tar with the answer.

```text
  caller                                  one request's sandbox
  ──────                                  ─────────────────────
  tar of files ──inline or blob──▶ unpacked into a fresh directory under /work
                                   the handler starts in it: reads in.pdf,
                                   writes out.png
  answer + tar ◀──── out=1 ──────  the directory, packed on the way out
                                   then removed, whatever happened
```

```python
tar = make_tar({"in.pdf": pdf_bytes})

out = client.run_script("convert", script, {"to": "png"},
                        workspace={"inline": base64.b64encode(tar).decode()},
                        out=True)

open("result.tar", "wb").write(out.workspace)   # already decoded
```

```js
const out = await client.runScript('convert', script, { to: 'png' }, {
  workspace: { inline: tar.toString('base64') },
  out: true,
});
```

### Inside the handler

The handler is **started in** its own directory and told where it is:

```python
def handler(event):
    with open("in.pdf", "rb") as f:        # the caller's files are just here
        ...
    open("out.png", "wb").write(rendered)  # and this comes back with `out=1`
    return {"pages": 3}
```

### Send a fixture once: blobs

An embedder often sends the same fixture across a thousand calls. Store it
once as a **blob** and name it by digest:

```python
blob = client.put_blob(tar)                       # PUT /blobs, idempotent
client.run_script("convert", script, event, workspace={"blob": blob.sha256})
```

A warm function's body is the event itself, with nowhere to put a workspace.
So there it goes in the query string: `client.call(name, event,
workspace=blob.sha256, out=True)`, which is `?workspace=sha256:…&out=1`. It
takes a *blob* only, because an inline tar in a URL would be a megabyte of
base64 in a request line. The async Python client has no `workspace` or `out`.

### What keeps one request's files from another's

Not a mount namespace, and the reason was measured rather than assumed. A
forked child runs at an unprivileged uid with no `CAP_SYS_ADMIN` in the
sandbox's user namespace, so `unshare(CLONE_NEWNS)` fails with `EPERM`. That
holds even with the most permissive seccomp profile, so it is the namespace
and not a filter. One path cannot mean a different directory to each request.
What is there instead is listed plainly, because the first two are weaker than
a namespace would be:

* **`/work` cannot be listed** (mode `0311`), so a request cannot see its
  neighbours' names. The test suite checks this by trying.
* **The directory's name is 128 random bits**, not the request id, which is a
  counter.
* **It is removed when the request ends**, whatever the request did, so the
  window is one request long. This is also checked.

### Archives are unpacked, not trusted

Every archive is unpacked by Zygo under strict rules. Only files and
directories are allowed: **no symlinks, no hard links**, because that is how
an archive writes outside the directory it was unpacked into. No `..`, no
absolute paths. File modes are Zygo's, not the archive's. Entries and bytes
are capped, and counted as they are written, not read from a header an
archive is free to lie in.

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

Each item is a piece of the request's output, and the last one is the result:
exactly what the non-streaming call would have returned, or raised. A handler
that printed and then failed produced both. So the output is delivered
*first*, and the exception comes when you iterate past the result.

### Three kinds of item

`stdout` and `stderr` are what the request's process wrote, kept apart as
everywhere else. **`progress` is its own kind**, not a line of stdout. A long
request has two things to say — what it printed, and how far it has got — and
a caller should not have to parse a handler's log messages to find the second.
The handler calls `event.progress(...)`. It exists whether or not anybody is
listening, so a handler does not break depending on who called it:

```python
def handler(event):
    for n, page in enumerate(event["pages"]):
        event.progress(f"{n} of {len(event['pages'])}")
    return {"done": True}
```

The result still carries the whole of `stdout` and `stderr`, bounded as
always. A caller that streamed and one that did not see the same text;
streaming only changes *when*.

### On the wire

```text
  POST /fn/render?stream=1          Content-Type: application/x-ndjson
  ──────────────────────────────────────────────────────────────────────────
  {"stream": "stdout",   "data": "loading\n"}
  {"stream": "progress", "data": "1 of 400"}
  {"stream": "stderr",   "data": "warning: …\n"}
  …
  {"status": 200, "result": {…}, "request_id": "…", "stdout": "…", …}   ◀── last line
```

Streaming is **per request, not per function**. Sending each `print()` as it
happens costs a system call per `print()` on a path measured in milliseconds.
So a caller that wants to watch pays for it, and everybody else keeps the fast
shape. The answer is newline-delimited JSON (one JSON object per line) rather
than server-sent events: every language can read a line and parse JSON. The
streaming connection is held for the whole request and is not pooled.
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

Asynchronous Python needs no key at all:

```python
task = asyncio.ensure_future(client.call("render", event))
task.cancel()          # sends the cancel before CancelledError propagates
```

The caller of the cancelled request gets `Cancelled` (HTTP 499), which is on
purpose **not** `Timeout`. A timeout says the work is too slow or the limit
too tight; a cancel says the answer stopped being wanted. Both arrive as exit
137 from the kernel, and only the side that sent the signal can tell them
apart, so the supervisor records which it was.

```text
  caller A ── POST /fn/render  (X-Zygo-Request-Key: job-4711) ──▶ running …
  caller B ── DELETE /requests/job-4711 ──▶ supervisor
                                              └──▶ writes cgroup.kill on that
                                                   request's own cgroup
  caller A ◀── 499 Cancelled
```

### A key, not the request id

The id is assigned by the host and arrives *with the answer*, which is too
late to stop the call it belongs to. `X-Zygo-Request-Id` comes back on every
response and in the body as `request_id`; it joins a log line to its request.
The key is what a caller uses to name a request it is still waiting for.
Reusing a key is allowed, and one cancel then stops every call under it.

### What actually stops the work

The supervisor writes `cgroup.kill` on the request's own cgroup, from outside
the sandbox. That reaches everything the handler started, and does not depend
on the tenant's code being in a state where a signal helps. The agent inside
is *told*, so it can mark the answer, but an agent that ignores the message
changes nothing. A cancel that arrives before the request was let go is the
best case: nothing of the handler has run, and `started: false` in the answer
says so. Cancelling something already finished, or another tenant's request,
is the same `NotFound`. Request ids are a counter, not a secret, so ownership
is what keeps a cancel honest.

## Runtime pools

A warm function is one script in one zygote (the parked, ready process that
Zygo forks per request; see [chapter 13](13-warm-functions.md)). A **runtime
pool** is the other shape: an image, a dependency set and an agent, with no
code in it. The script arrives with the call.

```text
  WARM FUNCTION                          RUNTIME POOL
  ┌───────────────────────────┐          ┌───────────────────────────┐
  │ image + deps + agent      │          │ image + deps + agent      │
  │ + one handler, imported   │          │ no code                   │
  └───────────────────────────┘          └───────────────────────────┘
  event ──▶ handler(event)               script digest + event ──▶ the forked
                                         child loads that script, runs it
```

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

### How a script gets in

`client.runtimes()` lists the pools with their zygote counts, and
`client.stop_runtime(name)` stops one. `run_script` also takes the source
directly, for a one-off not worth registering. The request never reaches the
zygote: the supervisor writes the script into the sandbox, and the forked
child loads it after the child's seccomp filter (its list of allowed system
calls) is installed. That is what makes it safe for two tenants to share a
pool. It is checked, not just claimed: `poc/verify_api.sh` asks a script where
it was loaded from and whether it can list what else is in flight.

### A pool with no agent

A pool usually holds an agent — an interpreter, warm, forking per request. It
does not have to. Give it a `cmd` and no `agent` and you get the
**warm-exec** shape. Zygo holds the sandbox, writes each request's script into
it, and runs `cmd` with that path as its last argument and the event on
stdin.

```python
client.serve_runtime("sh", {"image": "alpine:3", "cmd": ["/bin/sh"]})
script = client.put_script(open("wordcount.sh").read())
out = client.run_script("sh", script.sha256, {"text": "warm exec in sh"})
```

That runs `sh /run/script/<digest>` inside the sandbox. It is the right shape
for `bash`, for a static binary that takes a script as an argument, and for
any language that starts in under a millisecond. There is nothing for an
agent to save there, and the warm protocol would only be one more moving
part.

### What warm-exec does not have

There is no protocol to carry them, so warm-exec has no streaming, no
`progress()`, no workspaces, and no per-tenant limits narrowed per request.
The sandbox itself is the same: same namespaces, same seccomp profile, same
cgroup per request, same deadline.
[`examples/warm-exec/`](../../examples/warm-exec/) has both shapes side by
side.

## Dependency sets

A pool's `requirements` names a file **on the Zygo host**, which is the one
thing an embedder does not have. `POST /deps` is the other half: send the lock
file itself, and get back an id a pool can be built on.

```python
deps = client.put_deps("python:3.12-slim", {        # POST /deps
    "requirements.txt": open("requirements.txt").read(),
})
deps.id        # 'deps_3f1c…' — its name from now on
deps.state     # 'building'

while client.deps(deps.id).building:                # GET /deps/<id>
    time.sleep(2)

client.serve_runtime(
    "py312",
    {"image": "python:3.12-slim", "agent": "python"},
    deps=deps.id,
)
```

```js
const deps = await client.putDeps('node:22-slim', {
  'package.json': manifest,
  'package-lock.json': lockfile,      // `npm ci` needs it, so this does too
});
await client.serveRuntime('node22', { image: 'node:22-slim', agent: 'node' },
                          { deps: deps.id });
```

Python takes a `requirements.txt` and gets a venv (a private folder of
installed packages). Node takes a `package.json` **and its lock file** and
gets `npm ci`. Either way the result is mounted read-only at `/venv` and the
environment points at it — `PATH` for Python, `NODE_PATH` for Node — so a
script just imports what the lock file named.
[Chapter 15](15-images-and-dependencies.md) covers dependencies in general.

```text
  POST /deps ──▶ 202 building ──┬──▶ ready ──▶ serve_runtime(…, deps=id)
                                │              └─▶ mounted read-only at /venv
                                └──▶ failed ──▶ deps(id).log has the reason
```

### Five things worth knowing

* **It answers before the build finishes.** A `pip install` takes minutes,
  and an HTTP request that waited would time out in every proxy on the way.
  Poll `deps(id)`, or send the `serve_runtime` and retry on the 503, which
  carries a `Retry-After`.
* **A pool named against a build still running is refused, not queued.**
  Nothing is started. A zygote warmed without the dependencies it was promised
  would serve requests that fail at `import`.
* **The id is a hash of the files and the image.** The same lock file on the
  same image is one build however many customers send it, and all of them see
  it in `deps()`. A different image is a different id, because a wheel built
  for one interpreter fails in another.
* **The build reaches the package registries and nothing else.** Installing a
  package runs its code — a `setup.py`, an npm lifecycle script — and the lock
  file came from whoever holds a token. The build sandbox has
  `network = "egress"` with only the registries allowed. A host that cannot
  enforce that (no `passt`, no `nftables`) **refuses the build** rather than
  falling back to host networking.
* **A failed build keeps its log**, on the same object as the state:
  `client.deps(id).log` is the resolver's own words.

`client.delete_deps(id)` forgets one, and is refused while a pool is built on
it. The pool holds a read-only mount of that directory, and removing it under
a warm zygote would make its imports fail one at a time.

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
wrapping its bytes only to unwrap them again helps nobody. The name is the
SHA-256 of those bytes. That makes the call idempotent (safe to repeat) in the
strongest sense: the same script from two tenants is one file on disk, and
neither can put different bytes under a digest the other is running.

### Looking up and forgetting

`client.script(digest)` says whether the host holds it and how big it is. It
never returns the bytes: a digest is not a key, so a store that answered with
the script would make every tenant's code readable by anyone who could guess
it. `client.delete_script(digest)` forgets one, and needs deploy rights,
because forgetting it forgets it for every tenant that sent the same bytes.

### Why `PUT /scripts` is not gated

Registering a script does not make it runnable on its own. A request still has
to name something to run it *in*, which is a runtime pool the operator
declared. So any authenticated caller may register a script, tenant tokens
included. A tenant registering its own code runs nothing by doing so, and the
digest is theirs from then on.

## Errors in the clients

Each kind of failure is its own type, because each one means something
different about what to do next.

| HTTP | Python and Node error | Means | What to do |
|---|---|---|---|
| 429 | `Busy`, with `retry_after` | the pool is full; **the request never ran** | retry after `retry_after` |
| 408 | `Timeout` | the deadline killed the request | the work is too slow, or the limit too tight |
| 499 | `Cancelled` | somebody stopped the request | nothing: this is what was asked for |
| 504 | `Stuck` | the sandbox went quiet with budget left | look at the function, not its `timeout` |
| 500 from a handler | `HandlerError`, with `stdout`, `stderr`, `exit_code` | the handler raised | fix the function |
| 404 | `NotFound` | no function (or script, or request) by that name | `serve` it, or check the name |
| 401, 403 | `AuthError` | wrong token, or a deploy call without deploy rights | check the token or the flag |
| 400 | `SpecError` | the sandbox as described cannot be resolved | fix the request |
| no connection | `TransportError` | the API could not be reached | nothing ran |
| anything else | `ZygoError` | e.g. 413, 422, 503; the message has the status | read the message |

There is no separate "unavailable" error: a `503` (warming failed,
dependencies building, or the API stopping) and a `422` both arrive as a plain
`ZygoError` in both SDKs. Every other error type is a subclass of `ZygoError`,
so catching it catches everything.

### Busy or HandlerError: the one that matters

`Busy` means the request was refused before anything happened, so retrying is
correct. A handler that raised will raise again. `Timeout` is not a guess
either. The supervisor records that *it* killed the request, because a
deadline kill and an out-of-memory kill both arrive as exit 137. In all, exit
137 has four readings — a deadline, an out-of-memory kill, a cancel, and a
sandbox that went quiet — and the supervisor is the only side that knows
which, so it says.

## Long requests

A function's `timeout` may be hours, and a caller may wait up to a day
(`X-Zygo-Timeout-Ms`, at most 24 hours; the default wait is 60 s). Two things
make that safe rather than a way to hold a slot for ever:

* **A heartbeat.** The agent says every second or two that a request is still
  alive. A request the supervisor has heard nothing about for a minute is
  killed and raises `Stuck`, whatever its budget said. So a wedged request
  costs a minute, not its whole timeout, and "your code is slow" stays
  different from "the sandbox stopped answering".
* **The idle policy leaves working zygotes alone.** A zygote nobody has called
  for `idle_timeout` is frozen. One with a request in flight is not, whatever
  the clock says, because freezing it would stop the request. A request that
  runs for an hour under a two-second `idle_timeout` still finishes.

A `batch` is the exception to raising: each element is a result *or* an
error, returned rather than raised, because one refused event must not hide
the answers to the others.

## A worked example

[`examples/plugin-host/`](../../examples/plugin-host/) is a plugin host in
about a hundred lines, built on this API alone: no `sandbox.toml`, no file on
the Zygo machine, no shelling out to `zygo`. It onboards customers and gives
each their own token, limits and secrets. It declares **one** runtime they all
share, installs their code by digest, runs it with files in and out, streams
the long ones, stops them, and offboards. `make verify-plugin-host` runs it
against a real kernel. That includes the two checks this whole layer exists
for: one customer cannot run another's plugin by naming its digest, and a
customer is held to their own memory limit rather than the runtime's.

## The OpenAPI document

```bash
zygo api --openapi > openapi.json
```

This prints OpenAPI 3.1 for the build that printed it;
[`spec/openapi.json`](../../spec/openapi.json) in the repository is the
committed copy. `info.version` is Zygo's release. `x-zygo-api` is the surface
version — the one a client checks. It moves only when an operation is removed
or renamed. Adding a route does not move it, because an older client does not
call a route it has never heard of.

The document is hand-written, and two tests read the router's own source to
keep it true: one fails when a route is missing from the document, the other
when the document names a route that is gone. Both SDKs have a third test,
over every operation, so a route added without a client method is a test
failure rather than something an embedder finds later.

## Versioning

`GET /version` reports:

| Field | Meaning |
|---|---|
| `version` | The Zygo release. |
| `api` | The HTTP surface. Bumped only on an incompatible change to a route, so it stays put across releases that change what happens behind them. This is what a client checks. |
| `control` | The CLI-to-supervisor protocol, which no SDK speaks. Reported because a mismatch there explains an API that is up but answering errors. |
| `deploy` | Whether *this caller* has deploy rights — the truth about your own token, not just the flag. |

Both packages are `0.1.0` and follow the repository's `0.x` policy: the shape
may change with a release note. After `1.0` it will not change without a
major version.

## Running against a real Zygo

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

The SDK test suites do **not** need any of that. Both run against a stand-in
API and check the client: the transport, the error mapping, the connection
pool. A test that needs a real sandbox belongs in the Rust suites, against a
real kernel.

```bash
make test-sdk
```

## Part three: the MCP server

`zygo mcp` gives an agent host a code interpreter whose boundary is a file
somebody reviewed. It speaks the Model Context Protocol (MCP), the standard
way an AI agent host talks to tool servers, over standard input and output.
That is what Claude Code, Claude Desktop, Cursor and the rest start as a child
process. There is no port, no token and no network: the transport is a pipe
between two processes running as the same user.

```json
{ "mcpServers": { "zygo": { "command": "zygo", "args": ["mcp"] } } }
```

That is the whole installation. On macOS it is forwarded into the Linux VM
like every other sandbox command, and the VM hop is paid once, when the host
starts the server, rather than once per tool call.

```text
  ┌────────────┐  stdin/stdout, JSON-RPC  ┌──────────┐      ┌────────────────────────────┐
  │ agent host │◀────────────────────────▶│ zygo mcp │─────▶│ run_code: a fresh one-shot │
  └────────────┘                          └────┬─────┘      │ sandbox for every call     │
                                               │            └────────────────────────────┘
                                               │ list_functions, call_function,
                                               │ function_logs
                                               ▼
                                     an existing supervisor (warm functions)
```

## The rule that shapes it

A model reads untrusted text — a web page, a file, an error message — and
that text can ask it for things. So the tools expose a **program and nothing
else**: no image, no mounts, no network mode, no limits. Those are set once,
on the command line, by the person who installed the server.

```bash
zygo mcp --mem 512M --timeout 60s --workspace ./agent-scratch
zygo mcp --net egress --allow api.github.com:443
zygo mcp -f ./sandbox.toml              # [defaults] becomes the ceiling
```

A model that needs more than this does not get to ask for it. Somebody
declares a function in `sandbox.toml` — with its dependencies, its egress
allow list and its secrets — and the model calls that by name. The boundary is
then in a file that was reviewed, which is where it belongs. A test enforces
this: `no_tool_can_widen_the_sandbox` fails if any tool schema ever grows an
`image`, `mount`, `network`, `mem` or similar field. Adding one looks harmless
on its own, which is exactly why it is checked.

### The flags

| Flag | Default | What it sets |
|---|---|---|
| `--workspace DIR` | a scratch folder, removed on exit | the host folder mounted at `/work` |
| `--python-image` | `python:3.12-slim` | the image for `language: python` |
| `--node-image` | `node:22-slim` | the image for `language: node` |
| `--sh-image` | `alpine:3` | the image for `language: sh` |
| `--mem`, `--cpu`, `--pids`, `--timeout`, … | the resolver's defaults | the limits, as for `zygo run` |
| `--net`, `--allow`, and the other sandbox flags | no network | the sandbox, as for `zygo run` |
| `-f FILE` | — | a spec file; its `[defaults]` becomes the ceiling |

## The tools

| Tool | Parameters | What it does |
|---|---|---|
| `run_code` | `language` (`python`, `node`, `sh`), `code`, `stdin?` | Runs the code in a fresh sandbox, with `/work` as its folder; returns output and, in words, why it failed. |
| `list_functions` | — | The warm functions and their state. |
| `call_function` | `name`, `event?` | Calls one by name with a JSON event (fixed 60 s limit). |
| `function_logs` | `name`, `limit?` (1–200), `failed?` | Its recent log, or only the failures. |

There are no tools for pools, scripts or tenants. `call_function` runs with no
tenant, as the host's own call. A `limit` outside 1–200 is clamped into that
range rather than refused. The server's `instructions` tell the model to
prefer a warm function over `run_code` where one exists: a millisecond against
tens of them, with dependencies already imported.

### How `run_code` runs a program

The code is written to a file, `/zygo/main.py`, `/zygo/main.js` or
`/zygo/main.sh`, in a folder mounted **read-only**. It is then run with
`python3`, `node` or `/bin/sh`. So the program can be of any length,
tracebacks name a real file, and the code cannot rewrite itself mid-run.
`stdin`, if given, is fed to the program and then closed. The outer deadline
is the sandbox's `timeout` (30 s by default) plus 300 s, which leaves room for
an image pull on the first run; the sandbox's own timeout is still enforced on
the whole process tree.

### `/work` persists between calls

`/work` is a writable directory and the program's working directory. It
persists between calls, so a model can write a file in one call and read it in
the next. Everything else written is thrown away when the call ends. Without
`--workspace` it is a scratch directory removed when the server exits; naming
one makes it real, and is how an agent is given a project to work on.

### Failures in words

When a sandbox is killed, the tool result says *why* in words: "ran out of
memory" and "exceeded its time limit" are different sentences. Both are exit
137, and a model told only "exit 137" cannot know which of its two problems to
fix.

## What a sandbox gets

Whatever `zygo run` gives. On the `ns` backend that is: no capabilities, a
read-only root with `pivot_root` and a masked `/proc`, a seccomp allow list of
about 190 system calls, Landlock where the kernel has it, required memory, CPU
and process limits, and **no network at all** unless the command line said
otherwise. [Chapter 23](23-security.md) says where that boundary is weaker
than it looks. `run_code` is not sandboxed *from the model* — running the
model's code is the point. It is sandboxed from the machine.

## Concurrency and failures

The server speaks JSON-RPC 2.0, a simple format where each request is a JSON
object with a method name and an id. It knows four methods: `initialize`,
`ping`, `tools/list` and `tools/call`. Notifications (messages with no id) are
ignored. Each request is handled on a thread of its own, so a `run_code` that
takes thirty seconds does not block a `list_functions` behind it. Answers are
written one line at a time, so two cannot mix.

| Problem | Answer |
|---|---|
| A line that is not JSON | JSON-RPC error `-32700` (parse error) |
| An unknown method | `-32601` |
| Bad parameters for a method | `-32602` |
| A tool that ran and failed | a normal result marked `isError` |

A tool that fails answers with a result marked `isError`, not a JSON-RPC
error. The difference matters: an error at the protocol layer is handled by
the host and never reaches the model, and the model is the one that could fix
a traceback. The three tools that read warm functions connect to an existing
supervisor and do not start one. There is nothing to read or call unless
somebody has already served something.

## Protocol revisions

The server speaks `2025-06-18` (its preferred one), `2025-03-26` and
`2024-11-05`. It answers `initialize` with the revision the client asked for
when it is one of those, and with its preferred one otherwise. So the client
decides whether it can live with the answer, rather than the server agreeing
to a dialect it does not know.

## Trying it without a host

The transport is a pipe, so a shell is enough:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | zygo mcp
```

Two lines of JSON come back, one per request. Standard output belongs to the
protocol, so anything a human should read — including the workspace path —
goes to standard error, which is where a host shows a server's log.
[Chapter 18](18-writing-an-agent.md) goes further with agents.

## What is not built

To say it plainly in one place:

* **Secrets for runtime pools.** Pools hold no secrets; only functions get
  `/run/secrets`.
* **Streaming, progress, workspaces and per-request tenant limits in
  warm-exec pools** (a pool with `cmd` and no agent).
* **Admin methods, workspaces and `out` in the async Python client**, on
  purpose.
* **Complete TypeScript types** for the Node client; several methods and
  options exist at run time without declarations.
* **Per-tenant series in `/metrics`**, and billing counts for `zygo exec` and
  MCP requests outside the supervisor log.
* **Pool, script and tenant tools in MCP.**

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [16. Deploying and running in production](16-production.md) · [Contents](README.md) · **Next: [18. Writing an agent](18-writing-an-agent.md) →**
