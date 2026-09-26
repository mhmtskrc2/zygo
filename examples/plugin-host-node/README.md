# A plugin host in Node, on the API alone

The Node twin of [`plugin-host/`](../plugin-host): the same exit criterion —
**can somebody build the thing the API is for without reaching past it** —
answered from the other SDK, and as the thing a plugin host actually is to its
customers: an HTTP server.

So: no `sandbox.toml`, no file written on the Zygo machine, no shelling out
to `zygo`, and no dependency but [`zygo-sdk`](../../sdk/node) — the host is
`node:http` and the SDK, nothing else.

```bash
make verify-plugin-host-node      # end to end, in a privileged container
node --test host.test.mjs         # the routing, against a fake Zygo; runs anywhere
```

Node 18 or newer, like the SDK; checked on 22. The import at the top of
`host.mjs` names the repository's own copy of the SDK so a checkout runs with
nothing installed; from npm it is `from 'zygo-sdk'`.

## What the host does

`host.mjs` is one class and a route table, about two hundred lines:

| A customer calls | The host does | Zygo sees |
|---|---|---|
| `PUT /plugins?language=javascript` | `forTenant(c).putScript(source)` | `PUT /scripts` |
| `POST /plugins/<digest>/run` | `forTenant(c).runScript(pool, digest, event, { key })` | `POST /runtimes/<pool>/call` |
| `POST /plugins/<digest>/stream` | `forTenant(c).streamScript(...)`, passed on as NDJSON | `...?stream=1` |
| `DELETE /runs/<key>` | `forTenant(c).cancel(key)` | `DELETE /requests/<key>` |

| The operator calls | The host does |
|---|---|
| `POST /customers` `{id, mem}` | `createTenant`, `setLimits`, `mintToken` — and hands the token back |
| `DELETE /customers/<id>` | `deleteTenant`: code, tokens, secrets and running work, gone |

Every route wants a bearer token. The operator's is the one the host itself
uses for Zygo (`ZYGO_API_TOKEN`); a customer's is the tenant token onboarding
minted, which the host recognises and acts for.

## The shape that makes it work

**One connection, a header per customer.** The host holds one operator client
and calls `forTenant(customer)` for each request. That is a view on the same
connection pool, not a second client: every call through it carries
`X-Zygo-Tenant`, so a script registered through it is that customer's and a
pool call may only name their own. A thousand customers are still one
connection.

**One runtime per language, many customers in each.** `plugins-python` and
`plugins-javascript` are declared once from one object and hold *no code*:
the script arrives with the request, by digest, and is loaded in the forked
child after its seccomp filter. Which pool a digest belongs in is the host's
own record — Zygo has no opinion about what bytes are written in.

**Zygo's errors become the host's status codes.** A customer is told what
happened to *their* plugin, never handed Zygo's answer raw:

| The SDK throws | The customer gets |
|---|---|
| `HandlerError` | `500`, with the plugin's own error text, `stderr` and exit code |
| `Timeout` | `504` |
| `Cancelled` | `499` |
| `NotFound` — theirs or not, the answer is the same | `404` |
| `Busy`, `Unavailable` — after three retries | `503`, with the server's `Retry-After` passed on |

The retries are the SDK's: `connect(url, { retries: 3 })` sends a *refused*
request again, waiting the server's `Retry-After`. Both refusals mean the
request never ran, so this is safe, and nothing else is retried. On a stream
the headers are already gone when a plugin fails, so the failure travels as
the last line with a `status` — the way Zygo's own stream does it.

**A digest is not a capability.** `verify.mjs` has one customer name
another's digest and checks the answer is `404`, the same answer an
unregistered digest gets. Zygo enforces that; the host only passes it on.

**Limits are the customer's, not the pool's.** The pools are declared with
generous limits; `onboard` narrows them per tenant, and `verify.mjs` holds a
customer capped at 64 MiB to that in both languages.

## What is not here

Files in and out (`workspace` and `out` on `runScript`), secrets and the
usage webhook are in the Python [`plugin-host/`](../plugin-host) and work the
same from Node; this example stops at the routes above to stay short. The
operator's routes are behind the operator's own token, which is enough for a
demo — a real host puts them behind its own login.
