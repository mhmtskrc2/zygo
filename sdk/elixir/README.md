# zygo — Elixir client

Run code you did not write — a customer's script, a plugin, something an
LLM just generated — without letting it touch your host. Zygo keeps a warm
sandbox per runtime, forks a fresh process for each request inside
namespaces, cgroups and seccomp, and answers in about a millisecond. This
package is the Elixir side of that: register a script once, then call it
with JSON in and JSON out.

```elixir
# mix.exs
{:zygo_sdk, "~> 0.1"}
```

```elixir
client = Zygo.connect()                  # `zygo api`, on loopback or a unix socket

Zygo.serve_runtime!(client, "py312", %{"image" => "python:3.12-slim", "agent" => "python"})

script = Zygo.put_script!(client, """
def handler(event):
    words = event["text"].split()
    return {"words": len(words), "longest": max(words, key=len)}
""")

Zygo.run_script!(client, "py312", script.sha256, %{"text" => "the quick brown fox"}).result
#=> %{"words" => 4, "longest" => "quick"}
```

The script ran in its own process, in a sandbox that cannot see your files,
your network or the other requests. What it does wrong stays there:

```elixir
{:error, %Zygo.Error{kind: :handler, stderr: stderr}} =
  Zygo.run_script(client, "py312", "def handler(e):\n    open('/etc/shadow').read()\n")
#=> the sandbox reports a PermissionError; your host never noticed
```

Every call to the API returns `{:ok, value}` or `{:error, %Zygo.Error{}}`,
and has a `!` twin that returns the value or raises. The streams are the
exception: `stream` and `stream_script` return a lazy stream in which an error
arrives as an `{:error, %Zygo.Error{}}` element, and their `!` twins raise it
instead. The error has a `kind` to branch
on — `:busy` means the request never ran and is worth sending again,
`:handler` means the script raised, `:timeout` means it ran out of time:

```elixir
case Zygo.run_script(client, "py312", script.sha256, event) do
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
