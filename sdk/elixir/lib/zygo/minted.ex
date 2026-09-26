# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Minted do
  @moduledoc """
  A token and the one copy of its secret.

  Keep `secret`: nothing can produce it again. `inspect/1` does not print it,
  so a minted token that ends up in a log line does not take its secret along.
  """
  defstruct token: %Zygo.Token{}, secret: ""

  @type t :: %__MODULE__{token: Zygo.Token.t(), secret: String.t()}

  @doc false
  def parse(raw) when is_map(raw) do
    %__MODULE__{
      token: Zygo.Token.parse(raw["token"]),
      secret: Zygo.Parse.str(raw, "secret")
    }
  end

  defimpl Inspect do
    import Inspect.Algebra

    def inspect(minted, opts) do
      concat(["#Zygo.Minted<", to_doc(minted.token, opts), ", secret: \"…\">"])
    end
  end
end
