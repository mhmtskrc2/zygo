# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Deps do
  @moduledoc """
  A dependency set built from a lockfile you uploaded.

  `state` is `"building"`, `"ready"` or `"failed"`. A build is minutes, so
  `Zygo.put_deps/3` answers as soon as the files are on disk and the work
  happens on the host — poll `Zygo.deps/2`, or send the `Zygo.serve_runtime/4`
  and let `retries:` wait out the 503.

  `log` is the build's own output. It is on this struct rather than at a
  second URL because the caller looking at `"failed"` is the caller who needs
  it. `tenants` are the tenants that uploaded these files; empty is the
  operator's own.
  """
  import Zygo.Parse

  @known ~w(id state kind image error log files tenants)

  defstruct id: "",
            state: "building",
            kind: "",
            image: "",
            error: nil,
            log: "",
            files: %{},
            tenants: [],
            extra: %{}

  @type t :: %__MODULE__{
          id: String.t(),
          state: String.t(),
          kind: String.t(),
          image: String.t(),
          error: String.t() | nil,
          log: String.t(),
          files: %{String.t() => non_neg_integer()},
          tenants: [String.t()],
          extra: map()
        }

  @doc "Whether the set finished building and can be named by a pool."
  @spec ready?(t()) :: boolean()
  def ready?(%__MODULE__{state: state}), do: state == "ready"

  @doc "Whether the build is still running."
  @spec building?(t()) :: boolean()
  def building?(%__MODULE__{state: state}), do: state == "building"

  @doc false
  def parse(raw) when is_map(raw) do
    %__MODULE__{
      id: str(raw, "id"),
      state: str(raw, "state", "building"),
      kind: str(raw, "kind"),
      image: str(raw, "image"),
      error: raw["error"],
      log: str(raw, "log"),
      files: map(raw, "files"),
      tenants: list(raw, "tenants"),
      extra: extra(raw, @known)
    }
  end
end
