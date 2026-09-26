# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.LogEntry do
  @moduledoc "One line of a function's log: the zygote's output, or one request."
  import Zygo.Parse

  @known ~w(seq at_ms text)

  defstruct seq: 0, at_ms: 0, text: "", extra: %{}

  @type t :: %__MODULE__{
          seq: non_neg_integer(),
          at_ms: non_neg_integer(),
          text: String.t(),
          extra: map()
        }

  @doc false
  def parse(raw) when is_map(raw) do
    %__MODULE__{
      seq: int(raw, "seq"),
      at_ms: int(raw, "at_ms"),
      text: str(raw, "text"),
      extra: extra(raw, @known)
    }
  end
end

defmodule Zygo.LogPage do
  @moduledoc """
  A page of log entries, and where to continue from.

  `next` is what to pass as `after:` for the entries that arrive later, which
  is how following a log works without a stream.
  """
  import Zygo.Parse

  defstruct name: "", entries: [], next: 0

  @type t :: %__MODULE__{name: String.t(), entries: [Zygo.LogEntry.t()], next: non_neg_integer()}

  @doc false
  def parse(raw) when is_map(raw) do
    %__MODULE__{
      name: str(raw, "name"),
      entries: raw |> list("entries") |> Enum.map(&Zygo.LogEntry.parse/1),
      next: int(raw, "next")
    }
  end
end
