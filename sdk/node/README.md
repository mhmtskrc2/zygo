# zygo — Node client

Warm, isolated sandboxes for function-shaped code. A warm function costs about
a millisecond and gets a clean process per request.

```bash
npm install zygo
```

```js
import { connect } from 'zygo';

const client = connect();               // `zygo api`, on loopback or a unix socket
const resize = client.fn('resize');     // a function declared in sandbox.toml

const out = await resize({ url: 'https://example.com/a.png' });
console.log(out.result, out.metrics.wallMs);
```

A one-shot sandbox, needing nothing declared in advance:

```js
const r = await client.run('node:22-slim', ['node', '-e', 'console.log(6 * 7)'], { mem: '128M' });
console.log(r.stdout);
```

**No dependencies, and no build step.** `node:http` handles both transports and
connection reuse; the TypeScript declarations are written by hand and ship
beside the JavaScript, so what you read in the repository is what executes.

Full documentation: [docs/sdk.md](../../docs/sdk.md). The project:
[zygo](../../README.md).

## Tests

```bash
npm test
```

They run against a stand-in API and need no Linux, no kernel and no sandbox:
what is under test is the client. A test that needs a real sandbox belongs in
the Rust suites, against a real kernel.
