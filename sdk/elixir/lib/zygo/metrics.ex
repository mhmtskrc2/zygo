# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Metrics do
  @moduledoc "What one request cost."
  import Zygo.Parse

  defstruct wall_ms: 0.0, cpu_ms: 0.0, peak_rss_kb: 0

  @type t :: %__MODULE__{wall_ms: float(), cpu_ms: float(), peak_rss_kb: non_neg_integer()}

  @doc false
  def parse(raw) when is_map(raw) do
    %__MODULE__{
      wall_ms: float(raw, "wall_ms"),
      cpu_ms: float(raw, "cpu_ms"),
      peak_rss_kb: int(raw, "peak_rss_kb")
    }
  end

  def parse(_), do: %__MODULE__{}
end
