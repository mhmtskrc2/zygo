# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Runtime do
  @moduledoc """
  One runtime pool: several anonymous zygotes any script can run in.

  `warm` and `paused` are zygotes that exist; `cold` is the room left between
  them and `max_warm`. A pool has no single state — four zygotes of which two
  are frozen is working and idle at once — so the counts are what is
  reported rather than a word.
  """
  import Zygo.Parse

  @ints ~w(warm paused cold min_warm max_warm in_flight queued requests failures rss_kb uptime_s)a
  @known ["name", "image", "runtime" | Enum.map(@ints, &Atom.to_string/1)]

  defstruct [name: "", image: "", runtime: "", extra: %{}] ++ Enum.map(@ints, &{&1, 0})

  @type t :: %__MODULE__{
          name: String.t(),
          image: String.t(),
          runtime: String.t(),
          warm: non_neg_integer(),
          paused: non_neg_integer(),
          cold: non_neg_integer(),
          min_warm: non_neg_integer(),
          max_warm: non_neg_integer(),
          in_flight: non_neg_integer(),
          queued: non_neg_integer(),
          requests: non_neg_integer(),
          failures: non_neg_integer(),
          rss_kb: non_neg_integer(),
          uptime_s: non_neg_integer(),
          extra: map()
        }

  @doc false
  def parse(raw) when is_map(raw) do
    counts = for key <- @ints, do: {key, int(raw, Atom.to_string(key))}

    struct!(
      __MODULE__,
      [
        name: str(raw, "name"),
        image: str(raw, "image"),
        runtime: str(raw, "runtime"),
        extra: extra(raw, @known)
      ] ++ counts
    )
  end
end
