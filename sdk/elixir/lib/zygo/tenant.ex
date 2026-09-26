# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Tenant do
  @moduledoc """
  One customer of whoever embedded Zygo.

  `scripts` are the digests registered for this tenant. The bytes are shared
  with any other tenant that registered the same script; the reference is
  not, and it is what deleting a tenant takes with it.

  `limits` is what this tenant may not exceed, empty when nothing was set.
  They only ever narrow: each is applied as the minimum of itself and what
  the function or pool was declared with.
  """
  import Zygo.Parse

  @known ~w(id created_ms scripts limits)

  defstruct id: "", created_ms: 0, scripts: [], limits: %{}, extra: %{}

  @type t :: %__MODULE__{
          id: String.t(),
          created_ms: non_neg_integer(),
          scripts: [String.t()],
          limits: map(),
          extra: map()
        }

  @doc false
  def parse(raw) when is_map(raw) do
    %__MODULE__{
      id: str(raw, "id"),
      created_ms: int(raw, "created_ms"),
      scripts: list(raw, "scripts"),
      limits: map(raw, "limits"),
      extra: extra(raw, @known)
    }
  end

  def parse(_), do: %__MODULE__{}
end
