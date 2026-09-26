# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Pool do
  @moduledoc false
  # The connection pool. A Mint connection is a data structure, not a process,
  # so it can be lent: one caller at a time holds it, and the socket's
  # ownership moves with it so that a caller that crashes takes its socket
  # down rather than leaving one half-read in the pool.
  #
  # A worker is either empty or an idle connection and the time it went idle.
  # Connections are opened by the caller that needs one, not here: the pool
  # never blocks on the network, and a pool nobody calls holds no sockets.

  @behaviour NimblePool

  # How long a pooled connection may sit unused before it is dropped rather
  # than reused. `zygo api` closes a connection that has waited 30 s for its
  # next request (hyper's header read timeout), so one older than that is one
  # the server has already hung up on. Ten seconds under it, so a slow network
  # or a busy scheduler cannot close the gap. The same number as the Python
  # and Node clients.
  @idle_limit_ms 20_000

  def idle_limit_ms, do: @idle_limit_ms

  # Lazy, because NimblePool lends its workers in turn: with every slot
  # made up front, each request would get the next empty slot and open a
  # connection of its own. Made on demand, the slots are only ever as many
  # as the most requests that were in flight at once, and each holds a
  # connection that is reused. `worker_idle_timeout` is when the pool looks
  # for connections that sat too long and closes them, rather than waiting
  # for a caller to find out.
  def start_link(opts) do
    idle_limit = Keyword.get(opts, :idle_limit, @idle_limit_ms)

    NimblePool.start_link(
      worker: {__MODULE__, idle_limit},
      pool_size: Keyword.get(opts, :pool_size, 32),
      lazy: true,
      worker_idle_timeout: idle_limit,
      name: Keyword.get(opts, :name)
    )
  end

  @impl NimblePool
  def init_worker(pool_state), do: {:ok, nil, pool_state}

  @impl NimblePool
  def handle_checkout(:checkout, {pid, _ref}, worker, idle_limit = pool_state) do
    case worker do
      nil ->
        {:ok, nil, nil, pool_state}

      {conn, since} ->
        if System.monotonic_time(:millisecond) - since > idle_limit do
          Mint.HTTP1.close(conn)
          {:ok, nil, nil, pool_state}
        else
          case Mint.HTTP1.controlling_process(conn, pid) do
            {:ok, conn} ->
              {:ok, conn, nil, pool_state}

            {:error, _} ->
              Mint.HTTP1.close(conn)
              {:ok, nil, nil, pool_state}
          end
        end
    end
  end

  # The caller hands the socket to this process before checking it in, so
  # it outlives the caller.
  @impl NimblePool
  def handle_checkin({:keep, conn}, _from, _worker, pool_state) do
    {:ok, {conn, System.monotonic_time(:millisecond)}, pool_state}
  end

  def handle_checkin(nil, _from, _worker, pool_state), do: {:ok, nil, pool_state}

  @impl NimblePool
  def handle_ping({conn, _since}, _pool_state) do
    Mint.HTTP1.close(conn)
    {:remove, :idle}
  end

  def handle_ping(nil, _pool_state), do: {:remove, :idle}

  @impl NimblePool
  def terminate_worker(_reason, worker, pool_state) do
    with {conn, _} <- worker, do: Mint.HTTP1.close(conn)
    {:ok, pool_state}
  end
end
