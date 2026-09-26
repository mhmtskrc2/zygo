# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.Bang do
  @moduledoc false
  # Every public function in `Zygo` that returns `{:ok, value} | {:error,
  # error}` gets a `!` twin that returns the value or raises the error.
  # Generated rather than written out so the two can never drift: a function
  # added to `Zygo` has its `!` the moment it compiles.

  @not_results [
    :connect,
    :close,
    :child_spec,
    :start_link,
    :client,
    :for_tenant,
    :request_key,
    :stream,
    :stream_script
  ]

  defmacro __before_compile__(env) do
    defs = Module.definitions_in(env.module, :def)

    for {name, arity} <- defs,
        name not in @not_results,
        not String.ends_with?(Atom.to_string(name), "!") do
      bang = :"#{name}!"
      args = Macro.generate_arguments(arity, __MODULE__)

      largest =
        defs |> Enum.filter(&(elem(&1, 0) == name)) |> Enum.map(&elem(&1, 1)) |> Enum.max()

      doc =
        if arity == largest,
          do: "Like `#{name}/#{arity}`, but returns the value or raises `Zygo.Error`.",
          else: false

      quote do
        @doc unquote(doc)
        def unquote(bang)(unquote_splicing(args)) do
          case unquote(name)(unquote_splicing(args)) do
            {:ok, value} -> value
            {:error, %Zygo.Error{} = error} -> raise error
          end
        end
      end
    end
  end
end
