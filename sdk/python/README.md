# zygo — Python client

Run code you did not write — a customer's script, a plugin, something an
LLM just generated — without letting it touch your host. Zygo keeps a warm
sandbox per runtime, forks a fresh process for each request inside
namespaces, cgroups and seccomp, and answers in about a millisecond.
This package is the Python side of that: register a script once, then call it
with JSON in and JSON out.

## Before you start

This package is a client. The sandboxes are run by `zygo`, one static binary
for Linux; on macOS it runs everything in a Linux VM it manages, and the API
it starts there answers on the Mac at the same address. Install it as
[the project's README](https://github.com/mhmtskrc2/zygo#install) shows, then:

```bash
zygo doctor                                    # can this host run sandboxes?
zygo pull python:3.12-slim                     # the API never pulls an image itself
export ZYGO_API_TOKEN=$(openssl rand -hex 16)  # `zygo api` will not start without one
zygo api --allow-deploy                        # 127.0.0.1:7700; leave it running
```

`--allow-deploy` lets a client start functions and one-shot sandboxes.
The first example calls a function named `resize` that a `sandbox.toml`
declares and `zygo up` starts; [chapter 11 of the Zygo book](https://github.com/mhmtskrc2/zygo/blob/main/docs/book/11-getting-started.md)
walks through one. `zygo.connect()` finds the API at `ZYGO_API_URL`, or
`http://127.0.0.1:7700`, and sends `ZYGO_API_TOKEN`, so a program started
from the same shell needs no settings.

## Using it

```bash
pip install zygo-sdk      # then `import zygo_sdk`
```

```python
import zygo_sdk as zygo

client = zygo.connect()                 # `zygo api`, on loopback or a unix socket
resize = client.fn("resize")            # a function declared in sandbox.toml

out = resize({"url": "https://example.com/a.png"})
print(out.result, out.metrics.wall_ms)
```

Asynchronous, which is what an agent framework needs — the same methods,
every one a coroutine:

```python
import asyncio
import zygo_sdk as zygo

async def main(events):
    async with zygo.aio.connect() as client:
        return await asyncio.gather(*(client.call("resize", e) for e in events))

asyncio.run(main([{"url": "https://example.com/a.png"}]))
```

A refused request can be retried for you. `Busy` (the pool was full) and
`Unavailable` (the host is still building or warming something) both mean the
request never ran, so sending it again is safe; nothing else is retried:

```python
client = zygo.connect(retries=3)        # waits the server's Retry-After, then again
```

A one-shot sandbox, needing nothing declared in advance:

```python
r = client.run("python:3.12-slim", ["python3", "-c", "print(6 * 7)"], mem="128M")
print(r.stdout)
```

**No dependencies.** The standard library has an HTTP client, and a unix socket
is thirty lines on top of it.

Full documentation: [docs/book/17-api-sdk-mcp.md](../../docs/book/17-api-sdk-mcp.md). The project:
[zygo](../../README.md).

## Tests

```bash
python -m unittest discover -s tests
```

They run against a stand-in API and need no Linux, no kernel and no sandbox:
what is under test is the client. A test that needs a real sandbox belongs in
the Rust suites, against a real kernel.
