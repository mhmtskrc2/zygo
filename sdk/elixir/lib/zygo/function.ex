# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Function do
  @moduledoc "One warm function, as `zygo ps` shows it."
  import Zygo.Parse

  @known ~w(name state image runtime rss_kb imports_ms requests failures)

  defstruct name: "",
            state: "",
            image: "",
            runtime: "",
            rss_kb: 0,
            imports_ms: 0.0,
            requests: 0,
            failures: 0,
            extra: %{}

  @type t :: %__MODULE__{
          name: String.t(),
          state: String.t(),
          image: String.t(),
          runtime: String.t(),
          rss_kb: non_neg_integer(),
          imports_ms: float(),
          requests: non_neg_integer(),
          failures: non_neg_integer(),
          extra: map()
        }

  @doc false
  def parse(raw) when is_map(raw) do
    %__MODULE__{
      name: str(raw, "name"),
      state: str(raw, "state"),
      image: str(raw, "image"),
      runtime: str(raw, "runtime"),
      rss_kb: int(raw, "rss_kb"),
      imports_ms: float(raw, "imports_ms"),
      requests: int(raw, "requests"),
      failures: int(raw, "failures"),
      extra: extra(raw, @known)
    }
  end
end
