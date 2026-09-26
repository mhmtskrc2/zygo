# zygo — Elixir client

Warm, isolated sandboxes for function-shaped code. A warm function costs about
a millisecond and gets a clean process per request.

```elixir
# mix.exs
{:zygo_sdk, "~> 0.1"}
```

```elixir
client = Zygo.connect()                  # `zygo api`, on loopback or a unix socket

{:ok, out} = Zygo.call(client, "resize", %{"url" => "https://example.com/a.png"})
IO.inspect({out.result, out.metrics.wall_ms})
```

Every function returns `{:ok, value}` or `{:error, %Zygo.Error{}}`, and has a
`!` twin that returns the value or raises. The error has a `kind` to branch
on — `:busy` means the request never ran and is worth sending again,
`:handler` means your code raised:

```elixir
case Zygo.call(client, "resize", event) do
  {:ok, out} -> out.result
  {:error, %Zygo.Error{kind: :busy, retry_after: ms}} -> {:later, ms}
  {:error, %Zygo.Error{kind: :handler, stderr: stderr}} -> {:bug, stderr}
end
```

A refused request can be retried for you. `:busy` (the pool was full) and
`:unavailable` (the host is still building or warming something) both mean
the request never ran, so sending it again is safe; nothing else is retried:

```elixir
client = Zygo.connect(retries: 3)        # waits the server's Retry-After, then again
```

A runtime pool — one image, many scripts — with the script sent once and
named by its digest after that:

```elixir
Zygo.serve_runtime!(client, "py312", %{"image" => "python:3.12-slim", "agent" => "python"})
script = Zygo.put_script!(client, "def handler(event):\n    return event\n")
Zygo.run_script!(client, "py312", script.sha256, %{"n" => 1})
```

A one-shot sandbox, needing nothing declared in advance:

```elixir
run = Zygo.run!(client, "python:3.12-slim", ["python3", "-c", "print(6 * 7)"], mem: "128M")
IO.puts(run.stdout)
```

For a client that lives as long as your application, start its pool under
your supervisor and fetch it by name:

```elixir
children = [{Zygo, name: MyApp.Zygo, url: "unix:///run/zygo/api.sock"}]
Supervisor.start_link(children, strategy: :one_for_one)
Zygo.functions!(Zygo.client(MyApp.Zygo))
```

**Two dependencies**, both small: [Mint](https://hex.pm/packages/mint) for
HTTP/1.1 over TCP or a unix socket, and
[NimblePool](https://hex.pm/packages/nimble_pool) to lend its connections out
one caller at a time. JSON is the standard library's, so Elixir 1.18 or newer.

Full documentation:
[chapter 17 of the Zygo book](https://github.com/mhmtskrc2/zygo/blob/main/docs/book/17-api-sdk-mcp.md).
The project: [zygo](https://github.com/mhmtskrc2/zygo).

## Tests

```bash
mix test
```

They run against a stand-in API and need no Linux, no kernel and no sandbox:
what is under test is the client. A test that needs a real sandbox belongs in
the Rust suites, against a real kernel. `ZYGO_LIVE=1 mix test --only live`
runs a read-only check against the `zygo api` that `ZYGO_API_URL` names.
