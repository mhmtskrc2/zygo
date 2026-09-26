# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Endpoint do
  @moduledoc """
  Where the API is, and how that was decided.

  In order: the argument, then `ZYGO_API_URL`, then loopback on the port
  `zygo api` uses by default. The same order, and the same four forms, as the
  Python and Node clients — two discovery rules that drift is a support
  question nobody can answer from the error.

      unix:///run/user/1000/zygo/api.sock     a local API over a unix socket
      http://127.0.0.1:7700                   the default
      https://zygo.internal:8443              across a network
      box:9000                                bare host and port
  """

  @default_url "http://127.0.0.1:7700"

  @enforce_keys [:url]
  defstruct [:url, socket_path: nil, host: "127.0.0.1", port: 7700, tls: false]

  @type t :: %__MODULE__{
          url: String.t(),
          socket_path: String.t() | nil,
          host: String.t(),
          port: :inet.port_number(),
          tls: boolean()
        }

  @doc "What `zygo api` listens on when nothing says otherwise."
  @spec default_url() :: String.t()
  def default_url, do: @default_url

  @doc """
  Work out which API to talk to: `url`, then `ZYGO_API_URL`, then the default.

  Raises `ArgumentError` for an address that cannot work, here rather than as
  a connection failure thirty seconds later against a host nobody meant.
  """
  @spec resolve(String.t() | nil) :: t()
  def resolve(url \\ nil) do
    text =
      Enum.find([url, System.get_env("ZYGO_API_URL")], &(is_binary(&1) and &1 != "")) ||
        @default_url

    parse(text)
  end

  @doc "Parse one address. See the module documentation for the forms."
  @spec parse(String.t()) :: t()
  def parse(text) when is_binary(text) do
    case String.trim(text) do
      "unix://" <> "" ->
        raise ArgumentError,
              "unix:// needs a path, e.g. unix:///run/user/1000/zygo/api.sock"

      "unix://" <> path = url ->
        %__MODULE__{url: url, socket_path: path}

      "https://" <> rest ->
        host_port(text, rest, true)

      "http://" <> rest ->
        host_port(text, rest, false)

      other ->
        case String.split(other, "://", parts: 2) do
          [scheme, _] ->
            raise ArgumentError,
                  "`#{scheme}://` is not an address Zygo serves; use http://, https:// or unix://"

          [_] ->
            host_port(text, other, false)
        end
    end
  end

  # A trailing path is dropped rather than honoured: every route this client
  # calls is rooted, and silently prefixing them would turn a typo in the
  # address into a 404 on every call.
  defp host_port(text, rest, tls) do
    [authority | _] = String.split(rest, "/", parts: 2)

    {host, port_text} =
      case :binary.matches(authority, ":") do
        [] ->
          {authority, if(tls, do: "443", else: "7700")}

        matches ->
          {at, _} = List.last(matches)

          {binary_part(authority, 0, at),
           binary_part(authority, at + 1, byte_size(authority) - at - 1)}
      end

    host = if host == "", do: raise_port(port_text, text), else: host

    port =
      case Integer.parse(port_text) do
        {port, ""} when port in 0..65535 -> port
        _ -> raise_port(port_text, text)
      end

    scheme = if tls, do: "https", else: "http"
    %__MODULE__{url: "#{scheme}://#{host}:#{port}", host: host, port: port, tls: tls}
  end

  defp raise_port(port_text, text) do
    raise ArgumentError, "`#{port_text}` is not a port number, in `#{text}`"
  end

  defimpl String.Chars do
    def to_string(endpoint), do: endpoint.url
  end
end
