# SPDX-License-Identifier: Apache-2.0
defmodule Zygo do
  @moduledoc """
  Zygo — warm sandboxes for function-shaped code, from Elixir.

      client = Zygo.connect()                          # `zygo api` on loopback
      {:ok, out} = Zygo.call(client, "resize", %{"url" => "..."})
      out.result                                       # what the handler returned

  A warm function costs about a millisecond and gets a clean process per
  request. A one-shot sandbox costs tens of milliseconds and needs nothing
  declared in advance:

      {:ok, run} = Zygo.run(client, "python:3.12-slim", ["python3", "-c", "print(6*7)"])
      run.stdout

  Every function returns `{:ok, value}` or `{:error, %Zygo.Error{}}`, and has
  a `!` twin that returns the value or raises the error. `Zygo.Error` has a
  `kind` to branch on — `:busy`, `:handler`, `:timeout` and the rest.

  A refused request — `:busy`, or `:unavailable` while the host is still
  building something — can be retried for you: `Zygo.connect(retries: 3)`
  waits the server's `Retry-After` and sends it again. Off by default.

  This package talks to `zygo api` over HTTP — on a unix socket when the API
  is on this machine. What it is *not* is a second implementation of Zygo.
  Every boundary a sandbox has is built by the Zygo binary and enforced by
  the kernel; nothing here can widen one, and an API started without
  `--allow-deploy` will not let this package create a sandbox at all.

  All durations — `timeout`, `backoff`, `grace`, `retry_after` — are
  milliseconds.
  """

  alias Zygo.{
    Client,
    Deps,
    Endpoint,
    Error,
    Function,
    LogPage,
    Minted,
    Result,
    RunResult,
    Runtime,
    Script,
    Served,
    Tenant,
    Token,
    Transport
  }

  @before_compile Zygo.Bang

  @typedoc "What every call returns."
  @type result(value) :: {:ok, value} | {:error, Error.t()}

  @typedoc "One element of a stream from `stream/4` or `stream_script/5`."
  @type event ::
          {:stdout, String.t()}
          | {:stderr, String.t()}
          | {:progress, String.t()}
          | {:output, String.t(), String.t()}
          | {:result, Result.t()}
          | {:error, Error.t()}

  # ---- lifecycle --------------------------------------------------------

  @doc """
  Open a client.

  The address is `url`, then `ZYGO_API_URL`, then `http://127.0.0.1:7700`;
  the token is `:token`, then `ZYGO_API_TOKEN` — the variable the server
  reads, so a shell that can start the API can talk to it.

  Options:

    * `:token` — the bearer token.
    * `:timeout` — how long one HTTP exchange may take, in ms. Default 300 000.
    * `:retries` — how many times a refused request (`:busy`,
      `:unavailable`) is sent again. Default 0. Nothing else is ever resent:
      a handler that raised will raise again, and a request the deadline
      killed did run.
    * `:backoff` — the base wait between retries, in ms, doubled per
      attempt; the server's `Retry-After` wins when it is longer. Default 1000.
    * `:pool_size` — most connections held at once. Default 32; a caller past
      that waits for one to come free.

  The pool is a process linked to the caller. For a client that outlives the
  process that opened it, start one under your supervisor instead — see
  `child_spec/1`.

  Raises `ArgumentError` for an address that cannot work.
  """
  @spec connect(String.t() | keyword() | nil, keyword()) :: Client.t()
  def connect(url \\ nil, opts \\ [])

  def connect(opts, []) when is_list(opts), do: connect(nil, opts)

  def connect(url, opts) do
    client = new_client(url, opts)
    {:ok, pool} = Zygo.Pool.start_link(Keyword.take(opts, [:pool_size, :idle_limit]))
    %{client | pool: pool}
  end

  defp new_client(url, opts) do
    endpoint = Endpoint.resolve(url)

    token =
      case Keyword.fetch(opts, :token) do
        {:ok, token} -> token
        :error -> System.get_env("ZYGO_API_TOKEN")
      end

    %Client{
      endpoint: endpoint,
      pool: nil,
      token: if(token in [nil, ""], do: nil, else: token),
      timeout: Keyword.get(opts, :timeout, 300_000),
      retries: max(0, Keyword.get(opts, :retries, 0)),
      backoff: max(0, Keyword.get(opts, :backoff, 1_000))
    }
  end

  @doc """
  Close every pooled connection and stop the pool. Harmless to call twice.

  A view from `for_tenant/2` shares the pool, so closing either closes both.
  """
  @spec close(Client.t()) :: :ok
  def close(%Client{pool: pool}) do
    NimblePool.stop(pool)
  catch
    :exit, _ -> :ok
  end

  @doc """
  A pool under your own supervisor, for a client that lives as long as the
  application:

      children = [{Zygo, name: MyApp.Zygo, url: "unix:///run/zygo/api.sock", retries: 3}]

  `:name` is required; every option of `connect/2` is accepted, `:url`
  included. Then `Zygo.client(MyApp.Zygo)` anywhere gives the client.
  """
  @spec child_spec(keyword()) :: Supervisor.child_spec()
  def child_spec(opts) do
    name = Keyword.fetch!(opts, :name)
    %{id: {__MODULE__, name}, start: {__MODULE__, :start_link, [opts]}, type: :worker}
  end

  @doc "Start a named pool; see `child_spec/1`."
  @spec start_link(keyword()) :: GenServer.on_start()
  def start_link(opts) do
    name = Keyword.fetch!(opts, :name)
    client = %{new_client(opts[:url], opts) | pool: name}
    :persistent_term.put({__MODULE__, name}, client)
    Zygo.Pool.start_link([name: name] ++ Keyword.take(opts, [:pool_size, :idle_limit]))
  end

  @doc "The client for a pool started with `child_spec/1` under `name`."
  @spec client(atom()) :: Client.t()
  def client(name), do: :persistent_term.get({__MODULE__, name})

  @doc """
  A view of `client` that acts for one tenant.

  Every call through it carries `X-Zygo-Tenant`, so scripts are registered
  against the tenant and pool calls may only name theirs. The connections
  are shared — this is a header, not a second client.

  For an **operator** token. A tenant token already names its tenant, and
  the server refuses a header that disagrees with it rather than ignoring it.
  """
  @spec for_tenant(Client.t(), String.t()) :: Client.t()
  def for_tenant(%Client{} = client, id) when is_binary(id), do: %{client | tenant: id}

  @doc """
  A fresh name for one request, to pass as `key:` so that another process can
  stop it with `cancel/2` before it answers.

  128 random bits: the server also checks that a cancel comes from the
  request's owner, so this is the second lock rather than the only one.
  """
  @spec request_key() :: String.t()
  def request_key, do: "k-" <> Base.encode16(:crypto.strong_rand_bytes(16), case: :lower)

  # ---- the server -------------------------------------------------------

  @doc """
  The server's version, and the version of the HTTP surface itself.

  `"api"` is what to check against: it moves only when a route changes
  incompatibly, so it stays put across Zygo releases.
  """
  @spec version(Client.t()) :: result(map())
  def version(client), do: Transport.request(client, "GET", "/version")

  @doc """
  `GET /healthz`, which needs no token.

  `"status"` is `"ok"`, or `"degraded"` — a pool below its `min_warm`, so the
  first requests pay a cold start. A stopping API answers 503, which is
  `{:error, %Zygo.Error{kind: :unavailable}}`.
  """
  @spec health(Client.t()) :: result(map())
  def health(client), do: Transport.request(client, "GET", "/healthz", authenticated: false)

  @doc """
  Stop admitting, let what is running finish, then exit.

  Answers before the process leaves: `"in_flight"` is how many requests were
  still going when `grace` (ms, default 30 000) ran out, so 0 is a clean
  drain. Operator-only, and needs deploy rights.
  """
  @spec drain(Client.t(), keyword()) :: result(map())
  def drain(client, opts \\ []) do
    grace = Keyword.get(opts, :grace, 30_000)
    Transport.request(client, "POST", "/drain?grace_ms=#{grace}")
  end

  # ---- warm functions ---------------------------------------------------

  @doc "Every warm function the supervisor holds (a tenant sees its own)."
  @spec functions(Client.t()) :: result([Function.t()])
  def functions(client) do
    with {:ok, body} <- Transport.request(client, "GET", "/fn") do
      {:ok, body |> list("functions") |> Enum.map(&Function.parse/1)}
    end
  end

  @doc "One function's counters."
  @spec stats(Client.t(), String.t()) :: result(Function.t())
  def stats(client, name) do
    with {:ok, body} <- Transport.request(client, "GET", "/fn/#{escape(name)}/stats") do
      {:ok, Function.parse(body)}
    end
  end

  @doc """
  Bring a registered function up now, without calling it — for the moment
  after a deploy, so the first real request does not pay for it.
  """
  @spec warm(Client.t(), String.t()) :: result(map())
  def warm(client, name), do: Transport.request(client, "POST", "/fn/#{escape(name)}/warm")

  @doc """
  Call a warm function and return what its handler returned.

  `event` is any term `JSON` can encode. Errors worth telling apart:
  `:handler` (the handler raised; `stdout`, `stderr` and `exit_code` are on
  the error), `:timeout` (the deadline killed it), `:cancelled` (somebody
  stopped it), and `:busy` — the function is at its limit, the request never
  ran, and a client opened with `retries:` sends it again for you.

  Options:

    * `:timeout` — how long the API waits for this request, in ms, up to a
      day. Sent as `X-Zygo-Timeout-Ms`.
    * `:key` — a name you choose for this request, so that another process
      can stop it with `cancel/2` before it answers. See `request_key/0`.
    * `:workspace` — a blob digest from `put_blob/2`, unpacked into the
      request's working directory.
    * `:out` — `true` to get the working directory back as a tar, in
      `Zygo.Result.workspace`.
  """
  @spec call(Client.t(), String.t(), term(), keyword()) :: result(Result.t())
  def call(client, name, event \\ nil, opts \\ []) do
    with {:ok, query} <- workspace_query(opts[:workspace], opts[:out]),
         {:ok, body} <-
           Transport.request(
             client,
             "POST",
             "/fn/#{escape(name)}#{query}",
             Keyword.take(opts, [:timeout, :key]) ++ [json: event]
           ) do
      {:ok, Result.parse(body)}
    end
  end

  @doc """
  Call a function with several events at once; answers in order.

  Each element is `{:ok, %Zygo.Result{}}` or `{:error, %Zygo.Error{}}` —
  returned rather than collapsed, because one event being refused must not
  hide the answers to the others. At most 1024 events.
  """
  @spec batch(Client.t(), String.t(), [term()], keyword()) ::
          result([{:ok, Result.t()} | {:error, Error.t()}])
  def batch(client, name, events, opts \\ []) when is_list(events) do
    with {:ok, answers} <-
           Transport.request(
             client,
             "POST",
             "/fn/#{escape(name)}/batch",
             Keyword.take(opts, [:timeout]) ++ [json: events]
           ) do
      if is_list(answers),
        do: {:ok, Enum.map(answers, &batch_element/1)},
        else: {:error, Error.new(:transport, "a batch answer that is not a list")}
    end
  end

  defp batch_element(%{} = answer) do
    status = Zygo.Parse.int(answer, "status", 200)

    if status in 200..299,
      do: {:ok, Result.parse(answer)},
      else: {:error, Error.from_response(status, answer)}
  end

  defp batch_element(other),
    do: {:error, Error.new(:other, "unreadable batch element: #{inspect(other)}")}

  @doc """
  Call a function and receive its output as it is produced.

  Returns a lazy `Stream`; nothing is sent until it is enumerated. Its
  elements are `{:stdout, text}`, `{:stderr, text}` and `{:progress, text}`
  while the request runs, then exactly one last element: `{:result,
  %Zygo.Result{}}`, or `{:error, %Zygo.Error{}}` for what `call/4` would have
  returned as an error. A refusal before anything ran is that one error
  alone, after any `retries:`.

      client
      |> Zygo.stream("render", %{"pages" => 400})
      |> Enum.each(fn
        {:result, out} -> IO.inspect(out.result)
        {:error, error} -> raise error
        {_kind, text} -> IO.write(text)
      end)

  The connection is held for the whole request and is not pooled; halting
  the stream early closes it. That does **not** cancel the request —
  `cancel/2` does, so this takes `key:` as `call/4` does. Enumerate it in
  one process, once.
  """
  @spec stream(Client.t(), String.t(), term(), keyword()) :: Enumerable.t(event())
  def stream(client, name, event \\ nil, opts \\ []) do
    Transport.stream(
      client,
      "POST",
      "/fn/#{escape(name)}?stream=1",
      Keyword.take(opts, [:timeout, :key]) ++ [json: event]
    )
  end

  @doc "Like `stream/4`, but the error element is raised after the output before it."
  @spec stream!(Client.t(), String.t(), term(), keyword()) :: Enumerable.t(event())
  def stream!(client, name, event \\ nil, opts \\ []),
    do: client |> stream(name, event, opts) |> raise_errors()

  @doc """
  A function's recent log.

  `after:` is a sequence number: 0 (the default) for the last `limit:`
  entries (default 50), and the previous page's `next` for everything since.
  `failed: true` keeps only the requests that failed.
  """
  @spec logs(Client.t(), String.t(), keyword()) :: result(LogPage.t())
  def logs(client, name, opts \\ []) do
    query =
      "?after=#{Keyword.get(opts, :after, 0)}&limit=#{Keyword.get(opts, :limit, 50)}" <>
        "&failed=#{Keyword.get(opts, :failed, false)}"

    with {:ok, body} <- Transport.request(client, "GET", "/fn/#{escape(name)}/logs#{query}") do
      {:ok, LogPage.parse(body)}
    end
  end

  @doc """
  Warm a function, replacing whatever held the name.

  `layer` is a `[fn.<name>]` table as a map. Options: `:base_dir` — what its
  relative paths are relative to **on the host the API runs on**, by default
  this process's working directory, which is right only when the two are
  the same machine; `:secrets`, a map of name to value; `:if_changed`.

  Needs an API started with `--allow-deploy`; without it this is `:auth`.
  """
  @spec serve(Client.t(), String.t(), map(), keyword()) :: result(Served.t())
  def serve(client, name, layer, opts \\ []) do
    payload = %{
      "layer" => Map.new(layer),
      "base_dir" => Path.expand(opts[:base_dir] || File.cwd!()),
      "secrets" => Map.new(opts[:secrets] || %{}),
      "if_changed" => Keyword.get(opts, :if_changed, false)
    }

    with {:ok, body} <- Transport.request(client, "PUT", "/fn/#{escape(name)}", json: payload) do
      {:ok, Served.parse(body)}
    end
  end

  @doc "Stop one function. `:not_found` if it is not there. Needs deploy rights."
  @spec stop(Client.t(), String.t()) :: result([String.t()])
  def stop(client, name) do
    with {:ok, body} <- Transport.request(client, "DELETE", "/fn/#{escape(name)}") do
      {:ok, list(body, "stopped")}
    end
  end

  # ---- one-shot sandboxes -----------------------------------------------

  @doc """
  Run one command in a fresh sandbox and collect its output.

  `layer` holds spec fields — `mem`, `cpu`, `pids`, `timeout`, `network`,
  `allow`, `mounts`, `env` and the rest — written exactly as in
  `sandbox.toml`, as a keyword list or a map. `:stdin` is the one key that is
  not a spec field. Mount sources must be absolute.

  A non-zero exit is `{:ok, %Zygo.RunResult{ok: false}}`: the sandbox ran.
  Needs an API started with `--allow-deploy`.
  """
  @spec run(Client.t(), String.t(), [String.t()] | nil, keyword() | map()) ::
          result(RunResult.t())
  def run(client, image, cmd \\ nil, layer \\ []) do
    layer = Map.new(layer, fn {k, v} -> {to_string(k), v} end)
    {stdin, layer} = Map.pop(layer, "stdin", "")
    layer = Map.put(layer, "image", image)
    layer = if cmd, do: Map.put(layer, "cmd", cmd), else: layer

    with {:ok, body} <-
           Transport.request(client, "POST", "/run", json: %{"layer" => layer, "stdin" => stdin}) do
      {:ok, RunResult.parse(body)}
    end
  end

  # ---- runtime pools ----------------------------------------------------

  @doc """
  Register a **runtime pool**: an image, a dependency set, an agent, no code.

  Scripts arrive with each call, so one pool serves ten thousand of them.
  `layer` is a `[runtime.<name>]` table as a map — `image`, `agent`,
  `min_warm`, `max_warm` and the limits.

  Options: `:deps`, an id from `put_deps/3`; `:base_dir`; and `:secrets`, a
  list of **names**. A pool is shared, so no value belongs to it: each call
  gets the *calling* tenant's values from `put_secret/4`.

  A pool named against a dependency set that is still building is
  `:unavailable` with `code: "deps_building"` and a `retry_after`; a client
  opened with `retries:` waits and sends it again. Needs deploy rights.
  """
  @spec serve_runtime(Client.t(), String.t(), map(), keyword()) :: result(map())
  def serve_runtime(client, name, layer, opts \\ []) do
    layer = Map.new(layer, fn {k, v} -> {to_string(k), v} end)
    layer = if opts[:secrets], do: Map.put(layer, "secrets", opts[:secrets]), else: layer

    payload =
      %{"name" => name, "layer" => layer}
      |> put_if("base_dir", opts[:base_dir] && Path.expand(opts[:base_dir]))
      |> put_if("deps", opts[:deps])

    Transport.request(client, "POST", "/runtimes", json: payload)
  end

  @doc "Every runtime pool this host holds (a tenant sees its own)."
  @spec runtimes(Client.t()) :: result([Runtime.t()])
  def runtimes(client) do
    with {:ok, body} <- Transport.request(client, "GET", "/runtimes") do
      {:ok, body |> list("runtimes") |> Enum.map(&Runtime.parse/1)}
    end
  end

  @doc "Stop a pool and drop its zygotes. Needs deploy rights."
  @spec stop_runtime(Client.t(), String.t()) :: result([String.t()])
  def stop_runtime(client, name) do
    with {:ok, body} <- Transport.request(client, "DELETE", "/runtimes/#{escape(name)}") do
      {:ok, list(body, "stopped")}
    end
  end

  @doc """
  Run one script in a pool.

  `script` is a `"sha256:…"` digest this host holds — register it once with
  `put_script/2` — or the source itself. The digest is the shape to build on:
  the bytes cross the wire once rather than on every call.

  Options: `:entry_point`, `:timeout`, `:key` (as for `call/4`),
  `:workspace` (a map, as the pool route takes it: `%{"blob" => digest}` or
  `%{"files" => …}`) and `:out`. Returns what `call/4` returns.
  """
  @spec run_script(Client.t(), String.t(), String.t(), term(), keyword()) :: result(Result.t())
  def run_script(client, runtime, script, event \\ nil, opts \\ []) do
    payload =
      script_payload(script, event, opts)
      |> put_if("workspace", opts[:workspace])

    path = "/runtimes/#{escape(runtime)}/call#{if opts[:out], do: "?out=1", else: ""}"

    with {:ok, body} <-
           Transport.request(
             client,
             "POST",
             path,
             Keyword.take(opts, [:timeout, :key]) ++ [json: payload]
           ) do
      {:ok, Result.parse(body)}
    end
  end

  @doc "Run a script in a pool, streaming its output. See `stream/4`."
  @spec stream_script(Client.t(), String.t(), String.t(), term(), keyword()) ::
          Enumerable.t(event())
  def stream_script(client, runtime, script, event \\ nil, opts \\ []) do
    Transport.stream(
      client,
      "POST",
      "/runtimes/#{escape(runtime)}/call?stream=1",
      Keyword.take(opts, [:timeout, :key]) ++ [json: script_payload(script, event, opts)]
    )
  end

  @doc "Like `stream_script/5`, but the error element is raised after the output before it."
  @spec stream_script!(Client.t(), String.t(), String.t(), term(), keyword()) ::
          Enumerable.t(event())
  def stream_script!(client, runtime, script, event \\ nil, opts \\ []),
    do: client |> stream_script(runtime, script, event, opts) |> raise_errors()

  defp script_payload(script, event, opts) do
    script = if String.starts_with?(script, "sha256:"), do: script, else: %{"source" => script}

    %{"script" => script, "event" => event}
    |> put_if("entry_point", opts[:entry_point])
  end

  # ---- the script store -------------------------------------------------

  @doc """
  Register a script and get back the name the host gave it.

  The name is the SHA-256 of the bytes, so this is idempotent in the
  strongest sense: the same script registered twice — or by two tenants — is
  one file, and `Zygo.Script.existed` says which call wrote it.
  """
  @spec put_script(Client.t(), String.t()) :: result(Script.t())
  def put_script(client, source) when is_binary(source) do
    with {:ok, body} <- Transport.request(client, "PUT", "/scripts", raw: source) do
      {:ok, Script.parse(body)}
    end
  end

  @doc """
  Whether this host holds a script, and how big it is — never the bytes.
  `:not_found` when it does not.
  """
  @spec script(Client.t(), String.t()) :: result(Script.t())
  def script(client, digest) do
    with {:ok, digest} <- check_digest(digest),
         {:ok, body} <- Transport.request(client, "GET", "/scripts/#{digest}") do
      {:ok, Script.parse(body)}
    end
  end

  @doc "Forget a script. `:not_found` if it was not there. Needs deploy rights."
  @spec delete_script(Client.t(), String.t()) :: result(boolean())
  def delete_script(client, digest) do
    with {:ok, digest} <- check_digest(digest),
         {:ok, body} <- Transport.request(client, "DELETE", "/scripts/#{digest}") do
      {:ok, body["deleted"] == true}
    end
  end

  # ---- dependency sets --------------------------------------------------

  @doc """
  Build a dependency set from a lockfile, inside `image`.

  `files` maps a file name to its contents: `%{"requirements.txt" => …}` for
  Python, `%{"package.json" => …, "package-lock.json" => …}` for Node.

  **Answers before the build finishes**, with `state: "building"`: poll
  `deps/2`, or name the id in `serve_runtime/4` and let `retries:` wait.
  Idempotent by content — the same files against the same image are the
  same id.
  """
  @spec put_deps(Client.t(), String.t(), %{String.t() => binary()}) :: result(Deps.t())
  def put_deps(client, image, files) do
    encoded = Map.new(files, fn {name, content} -> {to_string(name), Base.encode64(content)} end)

    with {:ok, body} <-
           Transport.request(client, "POST", "/deps",
             json: %{"image" => image, "files" => encoded}
           ) do
      {:ok, Deps.parse(body)}
    end
  end

  @doc "Every dependency set you can see."
  @spec deps(Client.t()) :: result([Deps.t()])
  def deps(client) do
    with {:ok, body} <- Transport.request(client, "GET", "/deps") do
      {:ok, body |> list("deps") |> Enum.map(&Deps.parse/1)}
    end
  end

  @doc "One dependency set, with its build log."
  @spec deps(Client.t(), String.t()) :: result(Deps.t())
  def deps(client, id) do
    with {:ok, body} <- Transport.request(client, "GET", "/deps/#{escape(id)}") do
      {:ok, Deps.parse(body)}
    end
  end

  @doc """
  Forget a dependency set. Refused while a pool is built on it — stop the
  pool first. Needs deploy rights.
  """
  @spec delete_deps(Client.t(), String.t()) :: result(boolean())
  def delete_deps(client, id) do
    with {:ok, body} <- Transport.request(client, "DELETE", "/deps/#{escape(id)}") do
      {:ok, body["deleted"] == true}
    end
  end

  # ---- blobs ------------------------------------------------------------

  @doc """
  Store a tar the host will hold under its digest.

  For the same fixture, model or document across a thousand calls: sent
  once, then named with `workspace:` on every call after. The body is the
  tar itself.
  """
  @spec put_blob(Client.t(), binary()) :: result(Script.t())
  def put_blob(client, tar) when is_binary(tar) do
    with {:ok, body} <- Transport.request(client, "PUT", "/blobs", raw: tar, binary: true) do
      {:ok, Script.parse(body)}
    end
  end

  @doc "Whether this host holds a blob, and how big it is."
  @spec blob(Client.t(), String.t()) :: result(Script.t())
  def blob(client, digest) do
    with {:ok, digest} <- check_digest(digest),
         {:ok, body} <- Transport.request(client, "GET", "/blobs/#{digest}") do
      {:ok, Script.parse(body)}
    end
  end

  @doc "Forget a blob. Operator-only: the store is shared by digest."
  @spec delete_blob(Client.t(), String.t()) :: result(boolean())
  def delete_blob(client, digest) do
    with {:ok, digest} <- check_digest(digest),
         {:ok, body} <- Transport.request(client, "DELETE", "/blobs/#{digest}") do
      {:ok, body["deleted"] == true}
    end
  end

  # ---- cancelling -------------------------------------------------------

  @doc """
  Stop a request that is running.

  `id` is a `key:` you gave the call, or a request id. Answers as soon as the
  kill is sent; the caller waiting on the request gets `:cancelled`.
  `"started"` in the answer says whether the handler had begun — `false` is
  the better outcome, since no handler code ran at all.

  `:not_found` when nothing is running under that id — which includes a
  request that finished a moment ago, and one belonging to another tenant.
  """
  @spec cancel(Client.t(), String.t()) :: result(map())
  def cancel(client, id), do: Transport.request(client, "DELETE", "/requests/#{escape(id)}")

  # ---- tenants ----------------------------------------------------------

  @doc "Register a customer, or find the one already registered. Operator-only."
  @spec create_tenant(Client.t(), String.t()) :: result(Tenant.t())
  def create_tenant(client, id) do
    with {:ok, body} <- Transport.request(client, "POST", "/tenants", json: %{"id" => id}) do
      {:ok, Tenant.parse(body["tenant"])}
    end
  end

  @doc "Every tenant this host holds. Operator-only."
  @spec tenants(Client.t()) :: result([Tenant.t()])
  def tenants(client) do
    with {:ok, body} <- Transport.request(client, "GET", "/tenants") do
      {:ok, body |> list("tenants") |> Enum.map(&Tenant.parse/1)}
    end
  end

  @doc "One tenant. `:not_found` if there is no such id."
  @spec tenant(Client.t(), String.t()) :: result(Tenant.t())
  def tenant(client, id) do
    with {:ok, body} <- Transport.request(client, "GET", "/tenants/#{escape(id)}") do
      {:ok, Tenant.parse(body["tenant"])}
    end
  end

  @doc """
  Forget a tenant: stop its work, then remove the scripts only it had.
  Answers with what was stopped and removed. Needs deploy rights.
  """
  @spec delete_tenant(Client.t(), String.t()) :: result(map())
  def delete_tenant(client, id),
    do: Transport.request(client, "DELETE", "/tenants/#{escape(id)}")

  @doc """
  What a tenant may not exceed: `mem`, `cpu`, `pids`, `timeout`, `scratch`,
  `network`, `allow`, as a keyword list or a map.

  They only ever **narrow** — each is applied as the minimum of itself and
  what the function or pool declared. A value above every ceiling the tenant
  has is refused (422). Partial: keys left out are left alone. Needs deploy
  rights.
  """
  @spec set_limits(Client.t(), String.t(), keyword() | map()) :: result(Tenant.t())
  def set_limits(client, tenant, limits) do
    limits = Map.new(limits, fn {k, v} -> {to_string(k), v} end)

    with {:ok, body} <-
           Transport.request(client, "PATCH", "/tenants/#{escape(tenant)}/limits", json: limits) do
      {:ok, Tenant.parse(body["tenant"])}
    end
  end

  # ---- secrets ----------------------------------------------------------

  @doc "The **names** of a tenant's secrets. There is no way to read a value back."
  @spec secrets(Client.t(), String.t()) :: result([String.t()])
  def secrets(client, tenant) do
    with {:ok, body} <- Transport.request(client, "GET", "/tenants/#{escape(tenant)}/secrets") do
      {:ok, list(body, "secrets")}
    end
  end

  @doc """
  Store one of a tenant's secrets, and get back their names. The body is the
  value itself; the host encrypts it on arrival. Needs deploy rights.
  """
  @spec put_secret(Client.t(), String.t(), String.t(), String.t()) :: result([String.t()])
  def put_secret(client, tenant, name, value) when is_binary(value) do
    path = "/tenants/#{escape(tenant)}/secrets/#{escape(name)}"

    with {:ok, body} <- Transport.request(client, "PUT", path, raw: value) do
      {:ok, list(body, "secrets")}
    end
  end

  @doc "Forget one secret. Needs deploy rights."
  @spec delete_secret(Client.t(), String.t(), String.t()) :: result([String.t()])
  def delete_secret(client, tenant, name) do
    path = "/tenants/#{escape(tenant)}/secrets/#{escape(name)}"

    with {:ok, body} <- Transport.request(client, "DELETE", path) do
      {:ok, list(body, "secrets")}
    end
  end

  # ---- tokens -----------------------------------------------------------

  @doc """
  Mint an API token, and get its secret — once.

  Without `tenant` this is an **operator** token; with one it is that
  tenant's, and minting registers the tenant if it is new. The server keeps
  only a SHA-256, so the secret in the answer is the only copy. Needs deploy
  rights.
  """
  @spec mint_token(Client.t(), String.t() | nil) :: result(Minted.t())
  def mint_token(client, tenant \\ nil) do
    path = if tenant, do: "/tenants/#{escape(tenant)}/tokens", else: "/tokens"

    with {:ok, body} <- Transport.request(client, "POST", path) do
      {:ok, Minted.parse(body)}
    end
  end

  @doc "Every token this host holds, revoked ones included. Never a secret."
  @spec tokens(Client.t()) :: result([Token.t()])
  def tokens(client) do
    with {:ok, body} <- Transport.request(client, "GET", "/tokens") do
      {:ok, body |> list("tokens") |> Enum.map(&Token.parse/1)}
    end
  end

  @doc "Revoke a token, from the next request onwards. The record stays."
  @spec revoke_token(Client.t(), String.t()) :: result(map())
  def revoke_token(client, id), do: Transport.request(client, "DELETE", "/tokens/#{escape(id)}")

  # ---- helpers ----------------------------------------------------------

  # Every name goes into a path segment escaped: `a/b` unescaped would be a
  # different route, and would answer 404 — or, worse, some other function.
  defp escape(name), do: URI.encode(to_string(name), &URI.char_unreserved?/1)

  # A digest goes into the path as it stands, so its shape is checked here:
  # `../../etc/passwd` is a mistake this client names, rather than a request
  # somebody's proxy might normalise into a different route.
  defp check_digest(digest) do
    if is_binary(digest) and Regex.match?(~r/\Asha256:[0-9a-f]{64}\z/, digest) do
      {:ok, digest}
    else
      {:error,
       Error.new(
         :spec,
         "`#{digest}` is not a digest; expected sha256: followed by 64 lowercase hex digits"
       )}
    end
  end

  # A function is called with its event as the whole body, so there is
  # nowhere in it to put a workspace: only a blob digest can be named, in
  # the query. Use a pool's body for an inline one.
  defp workspace_query(nil, out), do: {:ok, if(out, do: "?out=1", else: "")}

  defp workspace_query(blob, out) do
    with {:ok, blob} <- check_digest(blob) do
      {:ok, "?workspace=#{blob}#{if out, do: "&out=1", else: ""}"}
    end
  end

  defp raise_errors(stream) do
    Stream.map(stream, fn
      {:error, error} -> raise error
      event -> event
    end)
  end

  defp put_if(map, _key, nil), do: map
  defp put_if(map, key, value), do: Map.put(map, key, value)

  defp list(%{} = body, key), do: Zygo.Parse.list(body, key)
  defp list(_, _), do: []
end
