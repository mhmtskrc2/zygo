# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.FakeApi do
  @moduledoc false
  # A stand-in for `zygo api`, so the client can be tested without a kernel.
  #
  # It answers the routes a test tells it to, records what it received, and
  # can be told to misbehave the ways a real server does. What it is *not* is
  # a model of Zygo: nothing here runs a sandbox.
  #
  # Both transports, because they are the ones the client opens differently —
  # a unix socket and a TCP port — and a bug in either is invisible from the
  # other. Plain `:gen_tcp` with the VM's own HTTP packet parser, so the suite
  # adds no dependency.

  use GenServer

  defstruct listen: nil,
            url: nil,
            dir: nil,
            answers: %{},
            requests: [],
            connections: 0,
            delay: 0,
            # Close each connection after answering, without saying so — what
            # a server that drops idle keep-alive connections looks like.
            hang_up: false,
            # Close a connection without answering when it is asked a
            # *second* request: the server hung up on a pooled connection at
            # the moment the client reused it.
            drop_reused: false,
            # Close every connection without answering: a broken server.
            drop: false

  # ---- the test's side ---------------------------------------------------

  def start(opts \\ []) do
    {:ok, pid} = GenServer.start(__MODULE__, opts)
    pid
  end

  def stop(api), do: GenServer.stop(api)

  def url(api), do: GenServer.call(api, :url)

  @doc "Answer this route the same way every time."
  def answer(api, method, path, status, body, retry_after \\ nil),
    do: GenServer.call(api, {:answer, {method, path}, {status, body, retry_after}})

  @doc """
  Answer with each of `answers` in turn, then keep giving the last one — a
  host that refuses twice and then accepts.
  """
  def answer_then(api, method, path, answers, retry_after \\ nil) do
    planned = for {status, body} <- answers, do: {status, body, retry_after}
    GenServer.call(api, {:answer, {method, path}, planned})
  end

  @doc """
  Answer with NDJSON, one object per line, `gap` ms apart: with a gap, a
  test can tell a client that yields as lines arrive from one that waits.
  """
  def stream(api, method, path, lines, gap \\ 0),
    do: GenServer.call(api, {:answer, {method, path}, {200, {:ndjson, lines, gap}, nil}})

  def set(api, key, value), do: GenServer.call(api, {:set, key, value})

  def requests(api), do: GenServer.call(api, :requests)

  def connections(api), do: GenServer.call(api, :connections)

  # ---- the server --------------------------------------------------------

  @impl true
  def init(opts) do
    common = [:binary, active: false, reuseaddr: true, backlog: 128]

    state =
      if opts[:unix] do
        dir = Path.join(System.tmp_dir!(), "zygo-fake-#{System.unique_integer([:positive])}")
        File.mkdir_p!(dir)
        path = Path.join(dir, "api.sock")
        {:ok, listen} = :gen_tcp.listen(0, [{:ifaddr, {:local, path}} | common])
        %__MODULE__{listen: listen, url: "unix://" <> path, dir: dir}
      else
        {:ok, listen} = :gen_tcp.listen(0, [{:ip, {127, 0, 0, 1}} | common])
        {:ok, port} = :inet.port(listen)
        %__MODULE__{listen: listen, url: "http://127.0.0.1:#{port}"}
      end

    me = self()
    spawn_link(fn -> accept(state.listen, me) end)
    {:ok, state}
  end

  @impl true
  def terminate(_reason, state) do
    :gen_tcp.close(state.listen)
    if state.dir, do: File.rm_rf!(state.dir)
  end

  @impl true
  def handle_call(:url, _from, state), do: {:reply, state.url, state}
  def handle_call(:requests, _from, state), do: {:reply, Enum.reverse(state.requests), state}
  def handle_call(:connections, _from, state), do: {:reply, state.connections, state}

  def handle_call(:accepted, _from, state),
    do: {:reply, :ok, %{state | connections: state.connections + 1}}

  def handle_call({:set, key, value}, _from, state), do: {:reply, :ok, Map.put(state, key, value)}

  def handle_call({:answer, key, planned}, _from, state),
    do: {:reply, :ok, %{state | answers: Map.put(state.answers, key, planned)}}

  def handle_call({:behaviour, served}, _from, state) do
    drop? = state.drop or (state.drop_reused and served > 0)
    {:reply, {drop?, state.delay, state.hang_up}, state}
  end

  def handle_call({:record, request}, _from, state) do
    key = {request.method, request.path |> String.split("?") |> hd()}

    {answer, answers} =
      case Map.get(state.answers, key) do
        nil -> {{404, %{"error" => "no route #{elem(key, 1)}"}, nil}, state.answers}
        [only] -> {only, state.answers}
        [next | rest] -> {next, Map.put(state.answers, key, rest)}
        planned -> {planned, state.answers}
      end

    {:reply, answer, %{state | requests: [request | state.requests], answers: answers}}
  end

  defp accept(listen, api) do
    case :gen_tcp.accept(listen) do
      {:ok, socket} ->
        GenServer.call(api, :accepted)
        pid = spawn(fn -> receive(do: (:go -> serve(socket, api, 0))) end)
        :ok = :gen_tcp.controlling_process(socket, pid)
        send(pid, :go)
        accept(listen, api)

      {:error, _} ->
        :ok
    end
  end

  defp serve(socket, api, served) do
    with {:ok, method, path, headers} <- read_head(socket) do
      {drop?, delay, hang_up?} = GenServer.call(api, {:behaviour, served})

      if drop? do
        # Not recorded: the request was never answered, and a test counts
        # the ones that were.
        :gen_tcp.close(socket)
      else
        raw = read_body(socket, headers)
        json? = String.starts_with?(headers["content-type"] || "", "application/json")

        request = %{
          method: method,
          path: path,
          headers: headers,
          raw: raw,
          body: if(json? and raw != "", do: JSON.decode!(raw))
        }

        {status, body, retry_after} = GenServer.call(api, {:record, request})
        if delay > 0, do: Process.sleep(delay)
        reply(socket, status, body, retry_after)

        if hang_up? do
          :gen_tcp.close(socket)
        else
          serve(socket, api, served + 1)
        end
      end
    else
      _ -> :gen_tcp.close(socket)
    end
  end

  defp read_head(socket) do
    :ok = :inet.setopts(socket, packet: :http_bin)

    with {:ok, {:http_request, method, {:abs_path, path}, _}} <- :gen_tcp.recv(socket, 0),
         {:ok, headers} <- read_headers(socket, %{}) do
      :ok = :inet.setopts(socket, packet: :raw)
      {:ok, to_string(method), path, headers}
    end
  end

  defp read_headers(socket, acc) do
    case :gen_tcp.recv(socket, 0) do
      {:ok, {:http_header, _, name, _, value}} ->
        read_headers(socket, Map.put(acc, name |> to_string() |> String.downcase(), value))

      {:ok, :http_eoh} ->
        {:ok, acc}

      other ->
        other
    end
  end

  defp read_body(socket, headers) do
    case Integer.parse(headers["content-length"] || "0") do
      {n, _} when n > 0 ->
        {:ok, raw} = :gen_tcp.recv(socket, n)
        raw

      _ ->
        ""
    end
  end

  # A stream is chunked and written a line at a time, like the real API's.
  defp reply(socket, status, {:ndjson, lines, gap}, _retry_after) do
    :gen_tcp.send(
      socket,
      "HTTP/1.1 #{status} OK\r\ncontent-type: application/x-ndjson\r\n" <>
        "transfer-encoding: chunked\r\n\r\n"
    )

    lines
    |> Enum.with_index()
    |> Enum.each(fn {line, i} ->
      if i > 0 and gap > 0, do: Process.sleep(gap)
      piece = JSON.encode!(line) <> "\n"
      :gen_tcp.send(socket, Integer.to_string(byte_size(piece), 16) <> "\r\n" <> piece <> "\r\n")
    end)

    :gen_tcp.send(socket, "0\r\n\r\n")
  end

  defp reply(socket, status, body, retry_after) do
    payload = JSON.encode!(body)

    # What the real API sends: a second on backpressure — three here, so a
    # test can tell the header from the client's default — five while a
    # dependency set builds, and nothing on any other refusal.
    retry_after =
      cond do
        retry_after != nil -> retry_after
        status == 429 -> 3
        is_map(body) and body["code"] == "deps_building" -> 5
        true -> nil
      end

    headers =
      "content-type: application/json\r\ncontent-length: #{byte_size(payload)}\r\n" <>
        if(retry_after != nil, do: "retry-after: #{retry_after}\r\n", else: "")

    :gen_tcp.send(socket, "HTTP/1.1 #{status} Answer\r\n#{headers}\r\n#{payload}")
  end
end
