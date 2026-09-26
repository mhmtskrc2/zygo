# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Token do
  @moduledoc """
  One API token, as the server holds it — never the secret.

  `tenant` is `nil` for an operator token, which may create tenants and pools
  and mint more tokens; a tenant token registers scripts and calls, for its
  own tenant only. `revoked_ms` is `nil` until it is revoked.

  The secret exists once, in the answer to `Zygo.mint_token/2` (a
  `Zygo.Minted`), and is not stored anywhere: the server keeps a SHA-256 of it.
  """
  import Zygo.Parse

  @known ~w(id tenant created_ms revoked_ms)

  defstruct id: "", tenant: nil, created_ms: 0, revoked_ms: nil, extra: %{}

  @type t :: %__MODULE__{
          id: String.t(),
          tenant: String.t() | nil,
          created_ms: non_neg_integer(),
          revoked_ms: non_neg_integer() | nil,
          extra: map()
        }

  @doc "Whether the token has been revoked."
  @spec revoked?(t()) :: boolean()
  def revoked?(%__MODULE__{revoked_ms: at}), do: not is_nil(at)

  @doc false
  def parse(raw) when is_map(raw) do
    %__MODULE__{
      id: str(raw, "id"),
      tenant: raw["tenant"],
      created_ms: int(raw, "created_ms"),
      revoked_ms: if(is_integer(raw["revoked_ms"]), do: raw["revoked_ms"]),
      extra: extra(raw, @known)
    }
  end

  def parse(_), do: %__MODULE__{}
end
