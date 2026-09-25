# zygo — Python client

Warm, isolated sandboxes for function-shaped code. A warm function costs about
a millisecond and gets a clean process per request.

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

Asynchronous, which is what an agent framework needs:

```python
import asyncio, zygo.aio

async def main():
    async with zygo.aio.connect() as client:
        return await asyncio.gather(*(client.call("resize", e) for e in events))

asyncio.run(main())
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
