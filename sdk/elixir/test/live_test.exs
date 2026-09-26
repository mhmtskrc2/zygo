# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.LiveTest do
  # Against a real `zygo api`, when one is named: skipped otherwise, and in CI.
  #
  #   zygo api &
  #   ZYGO_LIVE=1 mix test --only live
  #
  # Read-only: nothing here serves, runs or deletes, so it is safe to point
  # at a host that is doing real work.
  use ExUnit.Case, async: true

  @moduletag :live

  test "the API answers, and speaks the surface this client was written for" do
    client = Zygo.connect()
    on_exit(fn -> Zygo.close(client) end)

    assert %{"api" => 1} = Zygo.version!(client)
    assert Zygo.health!(client)["status"] in ["ok", "degraded"]
    assert is_list(Zygo.functions!(client))
    assert is_list(Zygo.runtimes!(client))
  end
end
