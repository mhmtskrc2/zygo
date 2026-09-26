# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.EndpointTest do
  use ExUnit.Case, async: false
  alias Zygo.Endpoint

  test "every address form is understood" do
    unix = Endpoint.parse("unix:///run/user/1000/zygo/api.sock")
    assert unix.socket_path == "/run/user/1000/zygo/api.sock"

    tcp = Endpoint.parse("http://10.0.0.4:7700")
    assert {tcp.socket_path, tcp.host, tcp.port, tcp.tls} == {nil, "10.0.0.4", 7700, false}

    bare = Endpoint.parse("box:9000")
    assert {bare.host, bare.port} == {"box", 9000}

    assert Endpoint.parse("https://zygo.example.com:8443").tls
    assert Endpoint.parse("https://zygo.example.com").port == 443
    assert Endpoint.parse("box").port == 7700
    # A trailing path would prefix every route; dropped, not honoured.
    assert Endpoint.parse("http://box:1/api/").url == "http://box:1"
  end

  test "an address that cannot work is refused here" do
    # Reported where the mistake is, rather than as a connection failure
    # thirty seconds later against a host nobody meant.
    for bad <- ["unix://", "ftp://host:21", "host:not-a-port", "host:99999"] do
      assert_raise ArgumentError, fn -> Endpoint.parse(bad) end
    end
  end

  test "the argument, then ZYGO_API_URL, then the default" do
    previous = System.get_env("ZYGO_API_URL")

    try do
      System.delete_env("ZYGO_API_URL")
      assert Endpoint.resolve().url == "http://127.0.0.1:7700"
      System.put_env("ZYGO_API_URL", "box:9000")
      assert Endpoint.resolve().url == "http://box:9000"
      assert Endpoint.resolve("other:1").url == "http://other:1"
    after
      if previous,
        do: System.put_env("ZYGO_API_URL", previous),
        else: System.delete_env("ZYGO_API_URL")
    end
  end
end
