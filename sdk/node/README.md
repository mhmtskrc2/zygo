# zygo — Node client

Run code you did not write — a customer's script, a plugin, something an
LLM just generated — without letting it touch your host. Zygo keeps a warm
sandbox per runtime, forks a fresh process for each request inside
namespaces, cgroups and seccomp, and answers in about a millisecond.
This package is the Node side of that: register a script once, then call it
with JSON in and JSON out.

## Before you start

This package is a client. The sandboxes are run by `zygo`, one static binary
for Linux; on macOS it runs everything in a Linux VM it manages, and the API
it starts there answers on the Mac at the same address. Install it as
[the project's README](https://github.com/mhmtskrc2/zygo#install) shows, then:

```bash
zygo doctor                                    # can this host run sandboxes?
zygo pull node:22-slim                         # the API never pulls an image itself
export ZYGO_API_TOKEN=$(openssl rand -hex 16)  # `zygo api` will not start without one
zygo api --allow-deploy                        # 127.0.0.1:7700; leave it running
```

`--allow-deploy` lets a client start functions and one-shot sandboxes.
The first example calls a function named `resize` that a `sandbox.toml`
declares and `zygo up` starts; [chapter 11 of the Zygo book](https://github.com/mhmtskrc2/zygo/blob/main/docs/book/11-getting-started.md)
walks through one. `connect()` finds the API at `ZYGO_API_URL`, or
`http://127.0.0.1:7700`, and sends `ZYGO_API_TOKEN`, so a program started
from the same shell needs no settings.

## Using it

```bash
npm install zygo-sdk
```

```js
import { connect } from 'zygo-sdk';

const client = connect();               // `zygo api`, on loopback or a unix socket
const resize = client.fn('resize');     // a function declared in sandbox.toml

const out = await resize({ url: 'https://example.com/a.png' });
console.log(out.result, out.metrics.wallMs);
```

A refused request can be retried for you. `Busy` (the pool was full) and
`Unavailable` (the host is still building or warming something) both mean the
request never ran, so sending it again is safe; nothing else is retried:

```js
const client = connect(undefined, { retries: 3 });   // waits the server's Retry-After, then again
```

A one-shot sandbox, needing nothing declared in advance:

```js
const r = await client.run('node:22-slim', ['node', '-e', 'console.log(6 * 7)'], { mem: '128M' });
console.log(r.stdout);
```

**No dependencies, and no build step.** `node:http` handles both transports and
connection reuse; the TypeScript declarations are written by hand and ship
beside the JavaScript, so what you read in the repository is what executes.

Full documentation: [docs/book/17-api-sdk-mcp.md](../../docs/book/17-api-sdk-mcp.md). The project:
[zygo](../../README.md).

## Tests

```bash
npm test
```

They run against a stand-in API and need no Linux, no kernel and no sandbox:
what is under test is the client. A test that needs a real sandbox belongs in
the Rust suites, against a real kernel.
