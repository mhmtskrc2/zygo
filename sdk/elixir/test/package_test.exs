# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.PackageTest do
  # What the package looks like from outside: its examples, and its version.
  #
  # Every `elixir` block in the README and in chapter 17 has to parse, and
  # every `Zygo.name(...)` in it has to be a function this package exports —
  # so an example that looks right and does not run fails here rather than
  # on a reader's machine.
  use ExUnit.Case, async: true

  @readme Path.expand("../README.md", __DIR__)
  @book Path.expand("../../../docs/book/17-api-sdk-mcp.md", __DIR__)

  defp blocks(path) do
    ~r/```elixir\n(.*?)```/s
    |> Regex.scan(File.read!(path), capture: :all_but_first)
    |> List.flatten()
  end

  defp remote_calls(source) do
    {:ok, ast} = Code.string_to_quoted(source)

    {_, calls} =
      Macro.prewalk(ast, [], fn
        {{:., _, [{:__aliases__, _, [:Zygo]}, name]}, _, args} = node, acc when is_list(args) ->
          {node, [{name, length(args)} | acc]}

        node, acc ->
          {node, acc}
      end)

    calls
  end

  for {label, path} <- [readme: @readme, book: @book] do
    test "every example in the #{label} parses and names real functions" do
      blocks = blocks(unquote(path))
      assert length(blocks) >= 2, "the #{unquote(label)} lost its Elixir examples"
      exported = Zygo.__info__(:functions)

      for block <- blocks, {name, arity} <- remote_calls(block) do
        # An example piped into (`client |> Zygo.call("f")`) is one argument
        # short in the source; either arity is a real call.
        assert {name, arity} in exported or {name, arity + 1} in exported,
               "Zygo.#{name}/#{arity} in the #{unquote(label)} does not exist"
      end
    end
  end

  test "the version is the one the other SDKs carry" do
    python = Path.expand("../../python/pyproject.toml", __DIR__)
    [_, version] = Regex.run(~r/^version = "([^"]+)"/m, File.read!(python))
    assert to_string(Application.spec(:zygo_sdk, :vsn)) == version
  end
end
