# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.MixProject do
  use Mix.Project

  @version "0.1.3"
  @source "https://github.com/mhmtskrc2/zygo"

  def project do
    [
      app: :zygo_sdk,
      version: @version,
      elixir: "~> 1.18",
      elixirc_paths: elixirc_paths(Mix.env()),
      start_permanent: Mix.env() == :prod,
      deps: deps(),
      description:
        "Run untrusted code safely from Elixir: a client for Zygo, which gives every " <>
          "request a fresh, kernel-isolated sandbox in about a millisecond",
      package: package(),
      docs: docs(),
      name: "Zygo",
      source_url: @source <> "/tree/main/sdk/elixir"
    ]
  end

  def application do
    [extra_applications: [:logger, :ssl]]
  end

  defp elixirc_paths(:test), do: ["lib", "test/support"]
  defp elixirc_paths(_), do: ["lib"]

  # Two, both small, both Dashbit's, neither with a dependency of its own.
  # Mint is an HTTP client that is a data structure rather than a process, and
  # it opens a unix socket; NimblePool is what lends one of those data
  # structures to one caller at a time. JSON is the standard library's.
  defp deps do
    [
      {:mint, "~> 1.6"},
      {:nimble_pool, "~> 1.1"},
      {:ex_doc, "~> 0.34", only: :dev, runtime: false}
    ]
  end

  defp package do
    [
      licenses: ["Apache-2.0"],
      links: %{
        "Source" => @source <> "/tree/main/sdk/elixir",
        "Documentation" => @source <> "/blob/main/docs/book/17-api-sdk-mcp.md"
      },
      files: ~w(lib mix.exs README.md LICENSE NOTICE .formatter.exs)
    ]
  end

  defp docs do
    [main: "Zygo", extras: ["README.md"], source_ref: "v#{@version}"]
  end
end
