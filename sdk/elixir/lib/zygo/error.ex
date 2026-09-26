# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Error do
  @moduledoc """
  What can go wrong, as one exception with a `kind` a caller can branch on.

  The kinds are the distinctions that change what a caller should do next,
  and no others. A handler that raised is not the same as a sandbox that ran
  out of time, which is not the same as a pool that is full — the first is a
  bug in the function, the second is a limit doing its job, and the third is
  worth retrying in a moment. They are the same kinds the Python and Node
  clients raise as separate classes.

  | `kind`         | HTTP                  | Means                                                  | What to do |
  |----------------|-----------------------|--------------------------------------------------------|------------|
  | `:busy`        | 429                   | the pool is full; **the request never ran**            | retry after `retry_after` |
  | `:timeout`     | 408                   | the deadline killed the request                        | the work is too slow, or the limit too tight |
  | `:cancelled`   | 499                   | somebody stopped the request                           | nothing: this is what was asked for |
  | `:stuck`       | 504                   | the sandbox went quiet with budget left                | look at the function, not its `timeout` |
  | `:handler`     | 500 from a handler    | the handler raised; `stdout`, `stderr`, `exit_code`    | fix the function |
  | `:not_found`   | 404                   | no function (or script, or request) by that name       | serve it, or check the name |
  | `:auth`        | 401, 403              | wrong token, or a deploy call without deploy rights    | check the token or the flag |
  | `:spec`        | 400                   | the sandbox as described cannot be resolved            | fix the request |
  | `:transport`   | no connection         | the API could not be reached                           | nothing ran |
  | `:unavailable` | 503                   | still building, failed to warm, or stopping; never ran | retry after `retry_after` |
  | `:other`       | anything else         | e.g. 413, 422; the message has the status              | read the message |

  `retry_after` is in **milliseconds**, like every duration in this package.
  `body` is the whole JSON answer, so a field added by a newer Zygo is
  reachable without waiting for this package to catch up.
  """

  @kinds [
    :transport,
    :auth,
    :not_found,
    :busy,
    :unavailable,
    :timeout,
    :stuck,
    :cancelled,
    :handler,
    :spec,
    :other
  ]

  defexception kind: :other,
               message: "",
               status: nil,
               code: nil,
               retry_after: nil,
               request_id: nil,
               stdout: "",
               stderr: "",
               exit_code: nil,
               metrics: nil,
               body: %{}

  @type kind ::
          :transport
          | :auth
          | :not_found
          | :busy
          | :unavailable
          | :timeout
          | :stuck
          | :cancelled
          | :handler
          | :spec
          | :other

  @type t :: %__MODULE__{
          kind: kind(),
          message: String.t(),
          status: non_neg_integer() | nil,
          code: String.t() | nil,
          retry_after: non_neg_integer() | nil,
          request_id: String.t() | nil,
          stdout: String.t(),
          stderr: String.t(),
          exit_code: integer() | nil,
          metrics: Zygo.Metrics.t() | nil,
          body: map()
        }

  @doc "Every `kind` this module can produce."
  @spec kinds() :: [kind()]
  def kinds, do: @kinds

  @doc """
  Whether sending the same request again is safe: `:busy` and `:unavailable`,
  the two refusals that mean the request never ran.
  """
  @spec retryable?(t()) :: boolean()
  def retryable?(%__MODULE__{kind: kind}), do: kind in [:busy, :unavailable]

  @doc false
  def new(kind, message, fields \\ []) when kind in @kinds do
    struct!(__MODULE__, [kind: kind, message: message] ++ fields)
  end

  @doc """
  Map one HTTP answer onto the error a caller should see.

  Status first, then the body where the status is ambiguous, in the same
  order as the Python client's `from_response`. The fallback carries the
  status, because an unmapped code is a version skew worth reporting rather
  than swallowing. `retry_after` is the `Retry-After` header, in ms.
  """
  @spec from_response(non_neg_integer(), map(), non_neg_integer()) :: t()
  def from_response(status, body, retry_after \\ 1000) when is_map(body) do
    message = to_string(body["error"] || body["message"] || "HTTP #{status}")
    base = [status: status, body: body]

    output = [
      request_id: string(body["request_id"]),
      stdout: string(body["stdout"]) || "",
      stderr: string(body["stderr"]) || "",
      metrics: metrics(body["metrics"])
    ]

    cond do
      status in [401, 403] ->
        new(:auth, message, base)

      status == 404 ->
        new(:not_found, message, base ++ [code: string(body["code"])])

      status == 408 ->
        new(:timeout, message, base ++ output)

      # 499 is nginx's for a client that went away, and the nearest thing to
      # a registered code for a request the caller stopped.
      status == 504 or body["stuck"] == true ->
        new(:stuck, message, base ++ output)

      status == 499 or body["cancelled"] == true ->
        new(:cancelled, message, base ++ output)

      status == 429 ->
        new(:busy, message, base ++ [retry_after: retry_after])

      status == 400 ->
        new(:spec, message, base ++ [code: string(body["code"])])

      status == 503 ->
        # `/healthz` says `status: stopping` and carries no `error`.
        message =
          if is_nil(body["error"]) and is_nil(body["message"]) and body["status"],
            do: "the API is #{body["status"]}",
            else: message

        new(:unavailable, message, base ++ [code: string(body["code"]), retry_after: retry_after])

      status == 500 and is_map_key(body, "exit_code") ->
        exit_code = if is_integer(body["exit_code"]), do: body["exit_code"], else: 1
        new(:handler, message, base ++ output ++ [exit_code: exit_code])

      true ->
        new(:other, "#{message} (HTTP #{status})", base ++ [code: string(body["code"])])
    end
  end

  defp string(nil), do: nil
  defp string(value) when is_binary(value), do: value
  defp string(value), do: to_string(value)

  defp metrics(raw) when is_map(raw), do: Zygo.Metrics.parse(raw)
  defp metrics(_), do: nil
end
