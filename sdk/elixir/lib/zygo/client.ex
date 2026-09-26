# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Client do
  @moduledoc """
  A connection to a Zygo API: where it is, who is calling, and the pool.

  Made by `Zygo.connect/2`, or by `Zygo.client/1` for a pool started under a
  supervisor. A plain struct, so it can be passed to any process; every
  process that holds it shares the same connections.

  All durations are milliseconds. `timeout` bounds one HTTP exchange; it is
  generous by default because the request's real limit is the function's own
  `timeout`, which the supervisor enforces — a client that gives up first
  only loses the answer, it does not stop the work.

  `inspect/1` never prints the token.
  """

  @enforce_keys [:endpoint, :pool]
  defstruct endpoint: nil,
            pool: nil,
            token: nil,
            tenant: nil,
            timeout: 300_000,
            retries: 0,
            backoff: 1_000

  @type t :: %__MODULE__{
          endpoint: Zygo.Endpoint.t(),
          pool: GenServer.server(),
          token: String.t() | nil,
          tenant: String.t() | nil,
          timeout: pos_integer(),
          retries: non_neg_integer(),
          backoff: non_neg_integer()
        }

  defimpl Inspect do
    import Inspect.Algebra

    def inspect(client, opts) do
      fields = [
        url: client.endpoint.url,
        token: if(client.token, do: :redacted, else: nil),
        tenant: client.tenant,
        retries: client.retries
      ]

      concat(["#Zygo.Client<", to_doc(fields, opts), ">"])
    end
  end
end
