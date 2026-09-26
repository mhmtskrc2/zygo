# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Parse do
  @moduledoc false
  # The few coercions every model needs. Lenient on purpose: a field that is
  # missing, or `null`, is the default rather than a crash, because an API one
  # release behind or ahead of this package is the ordinary case.

  def int(map, key, default \\ 0) do
    case map[key] do
      v when is_integer(v) -> v
      v when is_float(v) -> trunc(v)
      _ -> default
    end
  end

  def float(map, key, default \\ 0.0) do
    case map[key] do
      v when is_number(v) -> v / 1
      _ -> default
    end
  end

  def str(map, key, default \\ "") do
    case map[key] do
      nil -> default
      v when is_binary(v) -> v
      v -> to_string(v)
    end
  end

  def bool(map, key, default \\ false) do
    case map[key] do
      v when is_boolean(v) -> v
      _ -> default
    end
  end

  def list(map, key) do
    case map[key] do
      v when is_list(v) -> v
      _ -> []
    end
  end

  def map(map, key) do
    case map[key] do
      v when is_map(v) -> v
      _ -> %{}
    end
  end

  @doc """
  The fields this package does not know by name, kept rather than dropped: a
  field added by a newer Zygo is reachable without waiting for a release.
  """
  def extra(map, known), do: Map.drop(map, known)
end
