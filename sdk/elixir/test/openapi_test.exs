# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.OpenApiTest do
  # Every operation in the OpenAPI document has a client function.
  #
  # The guarantee generating the client would have given for free, written
  # down instead: a route added to the API and documented, with no function
  # here, fails this test. The same table as the Python and Node clients'.
  #
  # The document comes from the binary, or from `ZYGO_OPENAPI`. Without
  # either the test is skipped, and says so.
  use ExUnit.Case, async: true

  @methods %{
    "GET /healthz" => :health,
    "GET /version" => :version,
    # Prometheus text; for a scraper, not a client.
    "GET /metrics" => nil,
    "GET /fn" => :functions,
    "POST /fn/{name}" => :call,
    "PUT /fn/{name}" => :serve,
    "DELETE /fn/{name}" => :stop,
    "POST /fn/{name}/batch" => :batch,
    "GET /fn/{name}/stats" => :stats,
    "GET /fn/{name}/logs" => :logs,
    "POST /fn/{name}/warm" => :warm,
    "GET /runtimes" => :runtimes,
    "POST /runtimes" => :serve_runtime,
    "DELETE /runtimes/{name}" => :stop_runtime,
    "POST /runtimes/{name}/call" => :run_script,
    "POST /deps" => :put_deps,
    "GET /deps" => :deps,
    "GET /deps/{id}" => :deps,
    "DELETE /deps/{id}" => :delete_deps,
    "PUT /scripts" => :put_script,
    "GET /scripts/{digest}" => :script,
    "DELETE /scripts/{digest}" => :delete_script,
    "PUT /blobs" => :put_blob,
    "GET /blobs/{digest}" => :blob,
    "DELETE /blobs/{digest}" => :delete_blob,
    "POST /run" => :run,
    "DELETE /requests/{id}" => :cancel,
    "POST /drain" => :drain,
    "GET /tenants" => :tenants,
    "POST /tenants" => :create_tenant,
    "GET /tenants/{id}" => :tenant,
    "DELETE /tenants/{id}" => :delete_tenant,
    "PATCH /tenants/{id}/limits" => :set_limits,
    "GET /tenants/{id}/secrets" => :secrets,
    "PUT /tenants/{id}/secrets/{name}" => :put_secret,
    "DELETE /tenants/{id}/secrets/{name}" => :delete_secret,
    "POST /tenants/{id}/tokens" => :mint_token,
    "GET /tokens" => :tokens,
    "POST /tokens" => :mint_token,
    "DELETE /tokens/{id}" => :revoke_token
  }

  # The newest binary wins: a stale release build next to a fresh debug one
  # would otherwise test last week's API.
  defp document do
    with path when is_binary(path) <- System.get_env("ZYGO_OPENAPI"),
         true <- File.regular?(path) do
      JSON.decode!(File.read!(path))
    else
      _ ->
        root = Path.expand("../../..", __DIR__)

        [Path.join(root, "target/release/zygo"), Path.join(root, "target/debug/zygo")]
        |> Enum.concat(List.wrap(System.find_executable("zygo")))
        |> Enum.filter(&File.regular?/1)
        |> Enum.sort_by(&File.stat!(&1, time: :posix).mtime, :desc)
        |> Enum.find_value(fn binary ->
          case System.cmd(binary, ["api", "--openapi"], stderr_to_stdout: false) do
            {out, 0} -> with {:ok, doc} <- JSON.decode(out), do: doc, else: (_ -> nil)
            _ -> nil
          end
        end)
    end
  end

  setup_all do
    case document() do
      nil -> {:ok, skip: true}
      doc -> {:ok, doc: doc}
    end
  end

  defp operations(doc) do
    for {path, item} <- doc["paths"], {method, _} <- item, do: "#{String.upcase(method)} #{path}"
  end

  test "every operation has a client function", context do
    if context[:skip] do
      IO.puts("\n  skipped: no zygo binary printed a document; `cargo build` or set ZYGO_OPENAPI")
    else
      functions = Keyword.keys(Zygo.__info__(:functions))

      missing =
        for op <- operations(context.doc),
            name = Map.get(@methods, op, :missing),
            name != nil,
            name == :missing or name not in functions,
            do: op

      assert missing == [], "no client function for: #{Enum.join(missing, ", ")}"
    end
  end

  test "the table does not name a route that is gone", context do
    unless context[:skip] do
      stale = Map.keys(@methods) -- operations(context.doc)
      assert stale == [], "in the table and not in the API: #{Enum.join(stale, ", ")}"
    end
  end

  test "every function the table names exists, with its bang twin" do
    functions = Keyword.keys(Zygo.__info__(:functions))

    for name <- @methods |> Map.values() |> Enum.reject(&is_nil/1) do
      assert name in functions
      assert :"#{name}!" in functions
    end
  end
end
