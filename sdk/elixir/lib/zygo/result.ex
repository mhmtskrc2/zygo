# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Result do
  @moduledoc """
  What a warm function, or a script in a pool, returned.

  `result` is the handler's own return value, decoded from JSON. `stdout` and
  `stderr` are what the request's process wrote, which is separate from the
  zygote's own output — a handler that prints is not polluting the next
  request's answer.

  `request_id` is the id `Zygo.cancel/2` names. Of no use for *this* call,
  which has finished; it is here so a log line about a slow request can be
  joined back to it.

  `workspace` is the request's workspace as a tar, when `out: true` asked for
  it — already decoded from the base64 the API sends.
  """
  import Zygo.Parse

  @known ~w(result stdout stderr metrics request_id workspace status)

  defstruct result: nil,
            stdout: "",
            stderr: "",
            metrics: %Zygo.Metrics{},
            request_id: "",
            workspace: nil,
            extra: %{}

  @type t :: %__MODULE__{
          result: term(),
          stdout: String.t(),
          stderr: String.t(),
          metrics: Zygo.Metrics.t(),
          request_id: String.t(),
          workspace: binary() | nil,
          extra: map()
        }

  @doc false
  def parse(raw) when is_map(raw) do
    workspace =
      case raw["workspace"] do
        packed when is_binary(packed) -> Base.decode64!(packed)
        _ -> nil
      end

    %__MODULE__{
      result: raw["result"],
      stdout: str(raw, "stdout"),
      stderr: str(raw, "stderr"),
      metrics: Zygo.Metrics.parse(raw["metrics"]),
      request_id: str(raw, "request_id"),
      workspace: workspace,
      extra: extra(raw, @known)
    }
  end
end
