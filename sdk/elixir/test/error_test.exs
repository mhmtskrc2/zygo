# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.ErrorTest do
  # The error table in chapter 17 and `Zygo.Error.from_response/3`, row by
  # row. The book is the source of truth: a row that says 429 is `:busy`
  # has to be `:busy` here, or one of the two is a bug.
  use ExUnit.Case, async: true
  alias Zygo.Error

  @book Path.expand("../../../docs/book/17-api-sdk-mcp.md", __DIR__)

  defp rows do
    @book
    |> File.read!()
    |> String.split("## Errors in the clients", parts: 2)
    |> List.last()
    |> String.split("\n")
    |> Enum.drop_while(&(not String.starts_with?(&1, "| HTTP")))
    |> Enum.drop(2)
    |> Enum.take_while(&String.starts_with?(&1, "|"))
    |> Enum.map(fn row ->
      [status, _python, kind | _] =
        row |> String.split("|", trim: true) |> Enum.map(&String.trim/1)

      [_, kind] = Regex.run(~r/`:(\w+)`/, kind)
      {status, String.to_atom(kind)}
    end)
  end

  test "every row of the book's table maps to the kind it names" do
    rows = rows()
    assert length(rows) >= 10, "the table in chapter 17 moved"

    for {status, kind} <- rows, code <- Regex.scan(~r/\d{3}/, status) |> List.flatten() do
      code = String.to_integer(code)
      body = if code == 500, do: %{"error" => "boom", "exit_code" => 1}, else: %{"error" => "x"}
      assert Error.from_response(code, body).kind == kind, "HTTP #{code} in the book is #{kind}"
    end

    assert {"no connection", :transport} in rows
    assert {"anything else", :other} in rows
  end

  test "every kind in the book is one this module produces, and the other way round" do
    assert rows() |> Enum.map(&elem(&1, 1)) |> Enum.sort() |> Enum.uniq() ==
             Enum.sort(Error.kinds())
  end

  test "a body flag wins over an ambiguous status" do
    assert Error.from_response(500, %{"stuck" => true}).kind == :stuck
    assert Error.from_response(500, %{"cancelled" => true}).kind == :cancelled
    # A 500 with no exit code is not a handler's failure.
    assert Error.from_response(500, %{"error" => "x"}).kind == :other
  end

  test "an error message is the server's own words" do
    error = Error.from_response(404, %{"error" => "no function `f`"})
    assert Exception.message(error) == "no function `f`"
    assert Exception.message(Error.from_response(418, %{})) == "HTTP 418 (HTTP 418)"
  end
end
