# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Script do
  @moduledoc """
  A script — or a blob — the host holds, named by the SHA-256 of its bytes.

  `existed` is true when the store already had exactly these bytes, which is
  what deduplication looks like from outside: two tenants registering the
  same script get one file, and the second is told so.
  """
  import Zygo.Parse

  @known ~w(sha256 size existed)

  defstruct sha256: "", size: 0, existed: false, extra: %{}

  @type t :: %__MODULE__{
          sha256: String.t(),
          size: non_neg_integer(),
          existed: boolean(),
          extra: map()
        }

  @doc false
  def parse(raw) when is_map(raw) do
    %__MODULE__{
      sha256: str(raw, "sha256"),
      size: int(raw, "size"),
      existed: bool(raw, "existed"),
      extra: extra(raw, @known)
    }
  end
end
