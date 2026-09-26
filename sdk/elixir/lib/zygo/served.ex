# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Served do
  @moduledoc """
  What serving a function did to the name it was served under.

  `change` is `"started"`, `"replaced"` or `"unchanged"`. A deploy tool reads
  it to say "3 replaced, 7 unchanged" rather than printing ten ticks that
  hide which functions actually restarted.
  """
  import Zygo.Parse

  @known ~w(name change runtime rss_kb imports_ms warm_ms warnings)

  defstruct name: "",
            change: "started",
            runtime: "",
            rss_kb: 0,
            imports_ms: 0.0,
            warm_ms: 0.0,
            warnings: [],
            extra: %{}

  @type t :: %__MODULE__{
          name: String.t(),
          change: String.t(),
          runtime: String.t(),
          rss_kb: non_neg_integer(),
          imports_ms: float(),
          warm_ms: float(),
          warnings: [String.t()],
          extra: map()
        }

  @doc false
  def parse(raw) when is_map(raw) do
    %__MODULE__{
      name: str(raw, "name"),
      change: str(raw, "change", "started"),
      runtime: str(raw, "runtime"),
      rss_kb: int(raw, "rss_kb"),
      imports_ms: float(raw, "imports_ms"),
      warm_ms: float(raw, "warm_ms"),
      warnings: list(raw, "warnings"),
      extra: extra(raw, @known)
    }
  end
end
