# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Transport do
  @moduledoc false
  # One HTTP/1.1 exchange over Mint, the retry of a refused request, and the
  # stream of a request whose answer arrives a line at a time. Everything the
  # public functions in `Zygo` have in common, so no two of them can disagree
  # about a header, a status or when it is safe to send something twice.

  alias Zygo.{Client, Error}

  # Largest answer read into memory. The API's own request limit is 16 MiB,
  # and an answer past this is a bug rather than a large result.
  @max_body 64 * 1024 * 1024

  # Longest a caller may ask the API to wait: `X-Zygo-Timeout-Ms` is at most
  # a day, and the server refuses more.
  @max_timeout_ms 24 * 60 * 60 * 1000

  @user_agent "zygo-sdk-elixir/#{Mix.Project.config()[:version]}"

  @type request :: %{
          method: String.t(),
          path: String.t(),
          headers: [{String.t(), String.t()}],
          body: iodata() | nil,
          recv_timeout: pos_integer()
        }

  # ---- building a request ---------------------------------------------

  @doc false
  def build(%Client{} = client, method, path, opts \\ []) do
    {body, content_type} =
      cond do
        Keyword.has_key?(opts, :raw) ->
          type =
            if opts[:binary],
              do: "application/octet-stream",
              else: "text/plain; charset=utf-8"

          {opts[:raw], type}

        Keyword.has_key?(opts, :json) ->
          {JSON.encode_to_iodata!(opts[:json]), "application/json"}

        true ->
          {nil, nil}
      end

    with {:ok, extra} <- per_request_headers(opts) do
      headers =
        [{"accept", opts[:accept] || "application/json"}, {"user-agent", @user_agent}] ++
          if(content_type, do: [{"content-type", content_type}], else: []) ++
          if(client.token && Keyword.get(opts, :authenticated, true),
            do: [{"authorization", "Bearer " <> client.token}],
            else: []
          ) ++
          if(client.tenant, do: [{"x-zygo-tenant", client.tenant}], else: []) ++
          extra

      recv_timeout =
        case opts[:timeout] do
          nil -> client.timeout
          # The API waits up to the caller's timeout for the answer, so the
          # socket has to wait at least that long, and a little more.
          ms -> max(client.timeout, ms + 10_000)
        end

      {:ok,
       %{method: method, path: path, headers: headers, body: body, recv_timeout: recv_timeout}}
    end
  end

  defp per_request_headers(opts) do
    with {:ok, timeout} <- timeout_header(opts[:timeout]),
         {:ok, key} <- key_header(opts[:key]) do
      {:ok, timeout ++ key}
    end
  end

  defp timeout_header(nil), do: {:ok, []}

  defp timeout_header(ms) when is_integer(ms) and ms > 0 and ms <= @max_timeout_ms,
    do: {:ok, [{"x-zygo-timeout-ms", Integer.to_string(ms)}]}

  defp timeout_header(ms),
    do:
      {:error,
       Error.new(:spec, "timeout: #{inspect(ms)} is not 1 to #{@max_timeout_ms} milliseconds")}

  # Checked here as the server checks it, so that a key the server would
  # refuse is named before anything is sent: a key is compared with request
  # ids and appears in log lines, so it is short printable ASCII.
  defp key_header(nil), do: {:ok, []}

  defp key_header(key) when is_binary(key) do
    if byte_size(key) in 1..128 and key |> :binary.bin_to_list() |> Enum.all?(&(&1 in 33..126)) do
      {:ok, [{"x-zygo-request-key", key}]}
    else
      key_header(:invalid)
    end
  end

  defp key_header(_),
    do: {:error, Error.new(:spec, "key: must be 1 to 128 printable ASCII characters")}

  # ---- one exchange, with retries ---------------------------------------

  @doc false
  def request(%Client{} = client, method, path, opts \\ []) do
    with {:ok, req} <- build(client, method, path, opts) do
      with_retries(client, fn -> exchange(client, req) end)
    end
  end

  @doc """
  Send a refused request again while `retries` allows.

  Only `:busy` and `:unavailable` qualify: both mean the request never ran.
  Each wait is the longer of the server's `Retry-After` and `backoff`
  doubled per attempt, and the last refusal is the one returned.
  """
  def with_retries(client, fun, attempt \\ 0) do
    case fun.() do
      {:error, error} = refused ->
        case retry_wait(client, error, attempt) do
          nil ->
            refused

          wait ->
            Process.sleep(wait)
            with_retries(client, fun, attempt + 1)
        end

      other ->
        other
    end
  end

  @doc false
  def retry_wait(%Client{retries: retries, backoff: backoff}, %Error{} = error, attempt) do
    if attempt < retries and Error.retryable?(error) do
      max(error.retry_after || 1000, backoff * Integer.pow(2, attempt))
    end
  end

  # One request and its answer, on a pooled connection.
  defp exchange(client, req) do
    pool = client.pool

    NimblePool.checkout!(
      pool,
      :checkout,
      fn _from, pooled ->
        {outcome, conn} = attempt(client, req, pooled)
        {outcome, give_back(conn, pool)}
      end,
      client.timeout
    )
  catch
    :exit, {:timeout, _} ->
      {:error,
       Error.new(
         :transport,
         "no pooled connection to #{client.endpoint} came free in #{client.timeout} ms; " <>
           "raise :pool_size, or send fewer requests at once"
       )}

    :exit, _ ->
      {:error, Error.new(:other, "this client has been closed")}
  end

  # A connection that came from the pool gets one more chance: if the
  # server had closed it while it waited, nothing of an answer arrived, so
  # the request did not run and sending it again on a fresh connection
  # cannot run it twice. A fresh connection that fails is a real failure.
  defp attempt(client, req, nil), do: fresh(client, req)

  defp attempt(client, req, conn) do
    case exchange_on(conn, req) do
      {:lost, conn, _reason} ->
        Mint.HTTP1.close(conn)
        fresh(client, req)

      other ->
        finish(client, req, other)
    end
  end

  defp fresh(client, req) do
    case open(client) do
      {:ok, conn} ->
        finish(client, req, exchange_on(conn, req))

      {:error, error} ->
        {{:error, error}, nil}
    end
  end

  defp finish(client, req, {:lost, conn, reason}),
    do: finish(client, req, {:failed, conn, reason})

  defp finish(client, req, {:failed, conn, reason}) do
    Mint.HTTP1.close(conn)
    {{:error, transport(client, req, reason)}, nil}
  end

  defp finish(_client, _req, {:ok, conn, status, headers, body}) do
    outcome = decode(status, body, retry_after(headers))
    keep = if Mint.HTTP1.open?(conn) and not closing?(headers), do: conn

    unless keep, do: Mint.HTTP1.close(conn)
    {outcome, keep}
  end

  # Only a connection that completed an exchange cleanly goes back, and the
  # socket is handed to the pool first so it outlives this process.
  defp give_back(nil, _pool), do: nil

  defp give_back(conn, pool) do
    with pid when is_pid(pid) <- GenServer.whereis(pool),
         {:ok, conn} <- Mint.HTTP1.controlling_process(conn, pid) do
      {:keep, conn}
    else
      _ ->
        Mint.HTTP1.close(conn)
        nil
    end
  end

  # ---- the wire ----------------------------------------------------------

  @doc false
  def open(%Client{endpoint: endpoint} = client) do
    opts = [mode: :passive, transport_opts: [timeout: min(client.timeout, 30_000)]]

    result =
      cond do
        endpoint.socket_path ->
          Mint.HTTP1.connect(:http, {:local, endpoint.socket_path}, 0, [
            {:hostname, "localhost"} | opts
          ])

        endpoint.tls ->
          Mint.HTTP1.connect(:https, endpoint.host, endpoint.port, opts)

        true ->
          Mint.HTTP1.connect(:http, endpoint.host, endpoint.port, opts)
      end

    case result do
      {:ok, conn} ->
        {:ok, conn}

      {:error, reason} ->
        hint =
          cond do
            endpoint.socket_path ->
              "\n  -> start one with `zygo api --listen unix://#{endpoint.socket_path}`"

            match?(%Mint.TransportError{reason: :econnrefused}, reason) ->
              "\n  -> nothing is listening there; start one with `zygo api`"

            true ->
              ""
          end

        {:error, Error.new(:transport, "no Zygo API at #{endpoint}: #{describe(reason)}#{hint}")}
    end
  end

  # Send one request and read the whole answer.
  #
  # `{:lost, conn, reason}` is the connection failing before a byte of the
  # answer arrived — the send fails, or the socket ends where the status line
  # should be. That is what a kept-alive connection the server has closed
  # looks like, and the only failure the caller may send again. A timeout is
  # not one of them: the request may be running.
  defp exchange_on(conn, req) do
    case Mint.HTTP1.request(conn, req.method, req.path, req.headers, req.body) do
      {:ok, conn, ref} ->
        deadline = System.monotonic_time(:millisecond) + req.recv_timeout
        read(conn, ref, deadline, %{status: nil, headers: [], body: [], size: 0})

      {:error, conn, reason} ->
        if gone?(reason), do: {:lost, conn, reason}, else: {:failed, conn, reason}
    end
  end

  defp read(conn, ref, deadline, acc) do
    wait = max(deadline - System.monotonic_time(:millisecond), 0)

    case Mint.HTTP1.recv(conn, 0, wait) do
      {:ok, conn, responses} ->
        case collect(responses, ref, acc) do
          {:done, acc} ->
            {:ok, conn, acc.status, acc.headers, IO.iodata_to_binary(acc.body)}

          {:more, acc} when acc.size > @max_body ->
            {:failed, conn, "an answer over #{@max_body} bytes"}

          {:more, acc} ->
            read(conn, ref, deadline, acc)
        end

      {:error, conn, reason, responses} ->
        {_, acc} = collect(responses, ref, acc)

        if is_nil(acc.status) and gone?(reason),
          do: {:lost, conn, reason},
          else: {:failed, conn, reason}
    end
  end

  defp collect(responses, ref, acc) do
    Enum.reduce(responses, {:more, acc}, fn
      {:status, ^ref, status}, {state, acc} ->
        {state, %{acc | status: status}}

      {:headers, ^ref, headers}, {state, acc} ->
        {state, %{acc | headers: acc.headers ++ headers}}

      {:data, ^ref, data}, {state, acc} ->
        {state, %{acc | body: [acc.body | data], size: acc.size + byte_size(data)}}

      {:done, ^ref}, {_, acc} ->
        {:done, acc}

      {:error, ^ref, reason}, {_, acc} ->
        {:more, Map.put(acc, :error, reason)}

      _, state ->
        state
    end)
  end

  defp gone?(%Mint.TransportError{reason: reason}), do: reason in [:closed, :econnreset, :epipe]
  defp gone?(%Mint.HTTPError{reason: :closed}), do: true
  defp gone?(_), do: false

  defp closing?(headers) do
    Enum.any?(headers, fn {name, value} ->
      String.downcase(name) == "connection" and String.downcase(value) == "close"
    end)
  end

  defp transport(client, req, reason) do
    Error.new(
      :transport,
      "#{req.method} #{req.path} failed against #{client.endpoint}: #{describe(reason)}"
    )
  end

  defp describe(%{__exception__: true} = e), do: Exception.message(e)
  defp describe(reason) when is_binary(reason), do: reason
  defp describe(reason), do: inspect(reason)

  # ---- reading an answer -------------------------------------------------

  @doc false
  def retry_after(headers) do
    Enum.find_value(headers, 1000, fn {name, value} ->
      if String.downcase(name) == "retry-after" do
        case Float.parse(String.trim(value)) do
          {seconds, ""} when seconds >= 0 -> round(seconds * 1000)
          _ -> nil
        end
      end
    end)
  end

  @doc false
  def decode(status, raw, retry_after) do
    case json(raw) do
      {:ok, body} when status in 200..299 ->
        {:ok, body}

      {:ok, body} ->
        body = if is_map(body), do: body, else: %{"error" => to_string(inspect(body))}
        {:error, Error.from_response(status, body, retry_after)}

      :error ->
        {:error,
         Error.new(:transport, "the API answered HTTP #{status} with something that is not JSON",
           status: status
         )}
    end
  end

  @doc false
  def json(""), do: {:ok, %{}}

  def json(raw) do
    case JSON.decode(raw) do
      {:ok, value} -> {:ok, value}
      {:error, _} -> :error
    end
  end

  # ---- streaming ---------------------------------------------------------

  @doc """
  A request whose answer is NDJSON, as a lazy stream of events.

  Deliberately outside the pool. A pooled connection is one that is finished
  with; this one is in use for as long as the handler runs, and returning it
  when the stream is abandoned would hand the next caller a socket with
  somebody else's request still on it. Enumerating opens the connection;
  finishing, or halting early, closes it.
  """
  def stream(%Client{} = client, method, path, opts) do
    Stream.resource(
      fn -> {:start, client, method, path, opts} end,
      &stream_next/1,
      &stream_close/1
    )
  end

  defp stream_next({:start, client, method, path, opts}) do
    case build(client, method, path, Keyword.put(opts, :accept, "application/x-ndjson")) do
      {:ok, req} ->
        stream_open(client, req, 0)

      {:error, error} ->
        {[{:error, error}], :done}
    end
  end

  defp stream_next({:reading, conn, ref, deadline, buffer}) do
    wait = max(deadline - System.monotonic_time(:millisecond), 0)

    case Mint.HTTP1.recv(conn, 0, wait) do
      {:ok, conn, responses} ->
        {data, done?} = stream_data(responses, ref)
        lines(conn, ref, deadline, buffer <> data, done?)

      {:error, conn, reason, responses} ->
        {data, _} = stream_data(responses, ref)
        Mint.HTTP1.close(conn)
        {events, _} = parse_lines(buffer <> data, false)

        error =
          Error.new(:transport, "the stream broke before its result: #{describe(reason)}")

        {events ++ [{:error, error}], :done}
    end
  end

  defp stream_next(:done), do: {:halt, :done}

  defp stream_close({:reading, conn, _, _, _}), do: Mint.HTTP1.close(conn)
  defp stream_close(_), do: :ok

  # A refusal — a bad token, no such function, a full pool — is an ordinary
  # JSON answer with a status, not a stream. It is the one element of the
  # stream, after as many retries as the client allows: nothing has been
  # yielded yet, so the same request can go again on a fresh connection.
  defp stream_open(client, req, attempt) do
    with {:ok, conn} <- open(client),
         {:ok, conn, ref} <- start_request(client, conn, req),
         deadline = System.monotonic_time(:millisecond) + req.recv_timeout,
         {:ok, conn, status, headers, buffer, done?} <- read_head(conn, ref, deadline) do
      if status in 200..299 do
        lines(conn, ref, deadline, buffer, done?)
      else
        {:ok, conn, _, _, body} = read_rest(conn, ref, deadline, buffer, done?)
        Mint.HTTP1.close(conn)
        {:error, error} = decode_refusal(status, body, retry_after(headers))

        case retry_wait(client, error, attempt) do
          nil ->
            {[{:error, error}], :done}

          wait ->
            Process.sleep(wait)
            stream_open(client, req, attempt + 1)
        end
      end
    else
      {:error, %Error{} = error} -> {[{:error, error}], :done}
    end
  end

  defp decode_refusal(status, body, retry_after) do
    case decode(status, body, retry_after) do
      {:error, _} = refused -> refused
      {:ok, _} -> {:error, Error.new(:other, "HTTP #{status}", status: status)}
    end
  end

  defp start_request(client, conn, req) do
    case Mint.HTTP1.request(conn, req.method, req.path, req.headers, req.body) do
      {:ok, conn, ref} ->
        {:ok, conn, ref}

      {:error, conn, reason} ->
        Mint.HTTP1.close(conn)
        {:error, transport(client, req, reason)}
    end
  end

  defp read_head(conn, ref, deadline, acc \\ {nil, [], "", false}) do
    wait = max(deadline - System.monotonic_time(:millisecond), 0)

    case Mint.HTTP1.recv(conn, 0, wait) do
      {:ok, conn, responses} ->
        {status, headers, data, done?} =
          Enum.reduce(responses, acc, fn
            {:status, ^ref, s}, {_, h, d, f} -> {s, h, d, f}
            {:headers, ^ref, hs}, {s, h, d, f} -> {s, h ++ hs, d, f}
            {:data, ^ref, bin}, {s, h, d, f} -> {s, h, d <> bin, f}
            {:done, ^ref}, {s, h, d, _} -> {s, h, d, true}
            _, a -> a
          end)

        if status && (headers != [] or done?) do
          {:ok, conn, status, headers, data, done?}
        else
          read_head(conn, ref, deadline, {status, headers, data, done?})
        end

      {:error, conn, reason, _} ->
        Mint.HTTP1.close(conn)
        {:error, Error.new(:transport, "no answer to a streaming call: #{describe(reason)}")}
    end
  end

  defp read_rest(conn, _ref, _deadline, buffer, true), do: {:ok, conn, nil, [], buffer}

  defp read_rest(conn, ref, deadline, buffer, false) do
    case read(conn, ref, deadline, %{status: nil, headers: [], body: [buffer], size: 0}) do
      {:ok, conn, _, _, body} -> {:ok, conn, nil, [], body}
      {_, conn, _} -> {:ok, conn, nil, [], buffer}
    end
  end

  defp stream_data(responses, ref) do
    Enum.reduce(responses, {"", false}, fn
      {:data, ^ref, bin}, {d, f} -> {d <> bin, f}
      {:done, ^ref}, {d, _} -> {d, true}
      _, a -> a
    end)
  end

  # Every complete line becomes an event as soon as it is here. The stream
  # ends with its result line: whatever comes after it is not read.
  defp lines(conn, ref, deadline, buffer, done?) do
    case parse_lines(buffer, done?) do
      {events, :result} ->
        Mint.HTTP1.close(conn)
        {events, :done}

      {events, _rest} when done? ->
        Mint.HTTP1.close(conn)
        error = Error.new(:transport, "the stream ended without a result")
        {events ++ [{:error, error}], :done}

      {events, rest} ->
        {events, {:reading, conn, ref, deadline, rest}}
    end
  end

  defp parse_lines(buffer, final?) do
    parts = String.split(buffer, "\n")

    {complete, rest} =
      if final?, do: {parts, ""}, else: {Enum.drop(parts, -1), List.last(parts)}

    Enum.reduce_while(complete, {[], rest}, fn line, {events, rest} ->
      case String.trim(line) do
        "" ->
          {:cont, {events, rest}}

        text ->
          event = event(text)

          if elem(event, 0) in [:result, :error],
            do: {:halt, {events ++ [event], :result}},
            else: {:cont, {events ++ [event], rest}}
      end
    end)
  end

  @doc false
  def event(text) do
    case json(text) do
      {:ok, %{"stream" => kind} = raw} when is_binary(kind) ->
        data = Zygo.Parse.str(raw, "data")

        case kind do
          "stdout" -> {:stdout, data}
          "stderr" -> {:stderr, data}
          "progress" -> {:progress, data}
          other -> {:output, other, data}
        end

      # The last line: what the plain call would have answered, with the
      # status it would have had.
      {:ok, %{} = raw} ->
        status = Zygo.Parse.int(raw, "status", 200)

        if status in 200..299,
          do: {:result, Zygo.Result.parse(raw)},
          else: {:error, Error.from_response(status, raw)}

      _ ->
        {:error, Error.new(:transport, "the API sent a line that is not JSON: #{text}")}
    end
  end
end
