# SPDX-License-Identifier: Apache-2.0
defmodule Zygo.ClientTest do
  # What the Elixir client promises, checked against a stand-in API.
  #
  # These need no Linux, no kernel and no sandbox: what is under test is the
  # client — the transport, the error mapping, the connection pool. The same
  # cases as the Python client's suite, one for one where the idea carries.
  use ExUnit.Case, async: true

  alias Zygo.{Error, FakeApi, Result}

  @ok_result %{
    "result" => %{"size" => [80, 60]},
    "stdout" => "resized\n",
    "stderr" => "",
    "metrics" => %{"wall_ms" => 1.7, "cpu_ms" => 1.2, "peak_rss_kb" => 2048}
  }

  @busy {429, %{"error" => "busy", "in_flight" => 2, "queued" => 0, "limit" => 2}}

  defp fake(opts \\ []) do
    api = FakeApi.start(opts)
    on_exit(fn -> if Process.alive?(api), do: FakeApi.stop(api) end)
    api
  end

  defp connect(api, opts \\ []) do
    client = Zygo.connect(FakeApi.url(api), Keyword.put_new(opts, :token, nil))
    on_exit(fn -> Zygo.close(client) end)
    client
  end

  defp first(api), do: api |> FakeApi.requests() |> hd()

  describe "calling" do
    test "a call returns the handler's own value" do
      api = fake()
      FakeApi.answer(api, "POST", "/fn/resize", 200, @ok_result)

      {:ok, out} = Zygo.call(connect(api), "resize", %{"url" => "http://example.com/a.png"})

      assert out.result == %{"size" => [80, 60]}
      assert out.stdout == "resized\n"
      assert out.metrics.wall_ms == 1.7
      assert first(api).body == %{"url" => "http://example.com/a.png"}
    end

    test "a call works over a unix socket" do
      # The transport the local case actually uses. A client that only ever
      # ran over TCP would fail here and nowhere else.
      api = fake(unix: true)
      FakeApi.answer(api, "POST", "/fn/resize", 200, @ok_result)
      assert %Result{result: %{"size" => [80, 60]}} = Zygo.call!(connect(api), "resize", %{})
    end

    test "the token is sent, and the timeout header with it" do
      api = fake()
      FakeApi.answer(api, "POST", "/fn/f", 200, @ok_result)
      Zygo.call!(connect(api, token: "s3cret"), "f", %{}, timeout: 2500)

      headers = first(api).headers
      assert headers["authorization"] == "Bearer s3cret"
      assert headers["x-zygo-timeout-ms"] == "2500"
    end

    test "the token comes from ZYGO_API_TOKEN when none is passed" do
      api = fake()
      FakeApi.answer(api, "GET", "/fn", 200, %{"functions" => []})
      System.put_env("ZYGO_API_TOKEN", "from-env")

      try do
        client = Zygo.connect(FakeApi.url(api))
        on_exit(fn -> Zygo.close(client) end)
        Zygo.functions!(client)
      after
        System.delete_env("ZYGO_API_TOKEN")
      end

      assert first(api).headers["authorization"] == "Bearer from-env"
    end

    test "health is asked without the token" do
      api = fake()
      FakeApi.answer(api, "GET", "/healthz", 200, %{"ok" => true, "status" => "ok"})
      assert Zygo.health!(connect(api, token: "s3cret"))["status"] == "ok"
      refute Map.has_key?(first(api).headers, "authorization")
    end

    test "a name that needs escaping reaches the right route" do
      api = fake()
      FakeApi.answer(api, "POST", "/fn/a%2Fb", 200, @ok_result)
      Zygo.call!(connect(api), "a/b", %{})
      # Unescaped this would be `POST /fn/a/b`: a different route.
      assert first(api).path == "/fn/a%2Fb"
    end

    test "a timeout or key the server would refuse is never sent" do
      api = fake()
      client = connect(api)

      for opts <- [[timeout: 0], [timeout: 25 * 60 * 60 * 1000], [key: ""], [key: "a b"]] do
        assert {:error, %Error{kind: :spec}} = Zygo.call(client, "f", %{}, opts)
      end

      assert FakeApi.requests(api) == []
    end

    test "the bang form returns the value or raises the error" do
      api = fake()
      FakeApi.answer(api, "POST", "/fn/f", 404, %{"error" => "no function f"})

      error = assert_raise Error, fn -> Zygo.call!(connect(api), "f") end
      assert error.kind == :not_found
      assert Exception.message(error) =~ "no function f"
    end
  end

  describe "failures" do
    test "a handler that raised carries its output" do
      api = fake()

      FakeApi.answer(api, "POST", "/fn/f", 500, %{
        "error" => "ZeroDivisionError: division by zero",
        "stdout" => "before\n",
        "stderr" => "Traceback...\n",
        "exit_code" => 1,
        "metrics" => %{"wall_ms" => 2.0}
      })

      {:error, error} = Zygo.call(connect(api), "f", %{})
      assert error.kind == :handler
      assert error.message =~ "ZeroDivisionError"
      assert error.stderr == "Traceback...\n"
      assert error.exit_code == 1
      assert error.metrics.wall_ms == 2.0
    end

    test "backpressure is not a failure of the call" do
      # A 429 means the request never ran: the answer is to retry, where the
      # answer to a handler that failed is to fix the code.
      api = fake()

      FakeApi.answer(api, "POST", "/fn/f", 429, %{
        "error" => "`f` is at its concurrency limit",
        "in_flight" => 4,
        "queued" => 16,
        "limit" => 4
      })

      {:error, error} = Zygo.call(connect(api), "f", %{})
      assert error.kind == :busy
      assert error.body["limit"] == 4
      assert error.retry_after == 3000
      assert Error.retryable?(error)
    end

    test "a deadline kill is its own kind" do
      api = fake()

      FakeApi.answer(api, "POST", "/fn/f", 408, %{
        "error" => "the request exceeded the function's timeout",
        "stderr" => "killed\n"
      })

      assert {:error, %Error{kind: :timeout, stderr: "killed\n"}} =
               Zygo.call(connect(api), "f", %{})
    end

    test "a refused deploy says which flag turns it on" do
      api = fake()

      FakeApi.answer(api, "POST", "/run", 403, %{
        "error" =>
          "this API may only call functions that are already served\n" <>
            "  -> start it with `zygo api --allow-deploy`"
      })

      {:error, error} = Zygo.run(connect(api), "alpine:3", ["echo", "hi"])
      assert error.kind == :auth
      assert error.message =~ "--allow-deploy"
    end

    test "an unreachable API is a transport error, not a sandbox one" do
      # Nothing ran, so nothing about the sandbox can be concluded from it.
      client = Zygo.connect("http://127.0.0.1:1", timeout: 2000)
      on_exit(fn -> Zygo.close(client) end)
      assert {:error, %Error{kind: :transport} = error} = Zygo.functions(client)
      assert error.message =~ "zygo api"
    end

    test "a missing unix socket says how to start one" do
      client = Zygo.connect("unix:///nonexistent/zygo/api.sock")
      on_exit(fn -> Zygo.close(client) end)
      {:error, error} = Zygo.functions(client)
      assert error.kind == :transport
      assert error.message =~ "zygo api --listen unix:///nonexistent/zygo/api.sock"
    end

    test "a stuck request is not a timeout" do
      api = fake()

      FakeApi.answer(api, "POST", "/fn/render", 504, %{
        "error" => "the sandbox stopped reporting this request",
        "stuck" => true,
        "request_id" => "00000009",
        "metrics" => %{"wall_ms" => 61000.0}
      })

      # "Too slow, raise the limit" is the wrong advice for a request that
      # still had budget when the sandbox stopped answering.
      assert {:error, %Error{kind: :stuck, request_id: "00000009"}} =
               Zygo.call(connect(api), "render", %{})
    end

    test "a status nobody mapped keeps its number" do
      api = fake()
      FakeApi.answer(api, "PATCH", "/tenants/acme/limits", 422, %{"error" => "mem above ceiling"})
      {:error, error} = Zygo.set_limits(connect(api), "acme", mem: "64G")
      assert {error.kind, error.status} == {:other, 422}
      assert error.message =~ "HTTP 422"
    end
  end

  describe "batch" do
    test "one refused event does not hide the others" do
      api = fake()

      FakeApi.answer(api, "POST", "/fn/f/batch", 200, [
        Map.put(@ok_result, "status", 200),
        %{"status" => 429, "error" => "at its limit", "limit" => 4},
        Map.put(@ok_result, "status", 200)
      ])

      {:ok, answers} = Zygo.batch(connect(api), "f", [%{}, %{}, %{}])

      assert [{:ok, %Result{}}, {:error, %Error{kind: :busy}}, {:ok, %Result{}}] = answers
    end
  end

  describe "the connection pool" do
    test "connections are reused between calls" do
      # Keep-alive keeps the client's overhead off the warm path.
      api = fake()
      FakeApi.answer(api, "POST", "/fn/f", 200, @ok_result)
      client = connect(api)
      for _ <- 1..5, do: Zygo.call!(client, "f", %{})
      assert FakeApi.connections(api) == 1
    end

    for unix <- [true, false] do
      test "a connection the server closed is replaced, not reported (unix: #{unix})" do
        # The server closes each kept-alive connection after answering. On a
        # unix socket the next send may fail; over TCP the send goes through
        # and the read meets end-of-file where the status line should be.
        api = fake(unix: unquote(unix))
        FakeApi.answer(api, "POST", "/fn/f", 200, @ok_result)
        FakeApi.set(api, :hang_up, true)
        client = connect(api)
        for _ <- 1..3, do: Zygo.call!(client, "f", %{})
        assert length(FakeApi.requests(api)) == 3
      end
    end

    test "a reused connection the server drops is sent again once" do
      # Every call after the first meets a dropped connection, goes once more
      # on a fresh one, and is answered there.
      api = fake()
      FakeApi.answer(api, "POST", "/fn/f", 200, @ok_result)
      FakeApi.set(api, :drop_reused, true)
      client = connect(api)
      for _ <- 1..3, do: Zygo.call!(client, "f", %{})
      assert length(FakeApi.requests(api)) == 3
      assert FakeApi.connections(api) == 3
    end

    test "a fresh connection that fails is reported, not retried" do
      api = fake()
      FakeApi.answer(api, "POST", "/fn/f", 200, @ok_result)
      FakeApi.set(api, :drop, true)
      assert {:error, %Error{kind: :transport}} = Zygo.call(connect(api), "f", %{})
      assert FakeApi.connections(api) == 1
    end

    test "a connection idle past the limit is not reused" do
      # The server hangs up on a connection idle for 30 s; the pool drops one
      # before that. The limit is shortened here instead of waiting.
      api = fake()
      FakeApi.answer(api, "POST", "/fn/f", 200, @ok_result)
      client = connect(api, idle_limit: 50)

      Zygo.call!(client, "f", %{})
      Process.sleep(100)
      Zygo.call!(client, "f", %{})
      assert FakeApi.connections(api) == 2
      Zygo.call!(client, "f", %{})
      assert FakeApi.connections(api) == 2, "the fresh connection was pooled"
    end

    test "the idle limit is under the server's" do
      # A margin, not equality: the client's clock starts when the answer is
      # read, the server's when it finishes writing it.
      listen = Path.expand("../../../crates/zygo-cli/src/cmd/api/listen.rs", __DIR__)

      if File.exists?(listen) do
        [_, secs] = Regex.run(~r/HEADER_READ_TIMEOUT[^;]*from_secs\((\d+)\)/, File.read!(listen))
        assert Zygo.Pool.idle_limit_ms() <= (String.to_integer(secs) - 5) * 1000
      end
    end

    test "concurrent callers are concurrent at the socket" do
      # One connection would serialise these. Eight calls against a server
      # that holds each for 200 ms take 1.6 s in a queue, ~200 ms in parallel.
      api = fake()
      FakeApi.answer(api, "POST", "/fn/f", 200, @ok_result)
      FakeApi.set(api, :delay, 200)
      client = connect(api)

      {elapsed, _} =
        :timer.tc(fn ->
          1..8
          |> Task.async_stream(fn _ -> Zygo.call!(client, "f", %{}) end, max_concurrency: 8)
          |> Enum.to_list()
        end)

      assert elapsed < 1_000_000, "the calls were serialised behind one connection"
      assert FakeApi.connections(api) >= 2
    end

    test "a caller that crashes mid-request does not poison the pool" do
      api = fake()
      FakeApi.answer(api, "POST", "/fn/f", 200, @ok_result)
      FakeApi.set(api, :delay, 300)
      client = connect(api, pool_size: 1)

      {pid, ref} = spawn_monitor(fn -> Zygo.call(client, "f", %{}) end)
      Process.sleep(100)
      Process.exit(pid, :kill)
      assert_receive {:DOWN, ^ref, _, _, :killed}

      FakeApi.set(api, :delay, 0)
      assert {:ok, %Result{}} = Zygo.call(client, "f", %{})
    end

    test "closing the client leaves no socket open" do
      # What `-W error::ResourceWarning` checks for the Python client.
      api = fake()
      FakeApi.answer(api, "POST", "/fn/f", 200, @ok_result)
      client = connect(api)
      Zygo.call!(client, "f", %{})
      pool = GenServer.whereis(client.pool)
      assert owned_sockets([pool, self()]) != []

      :ok = Zygo.close(client)
      assert Zygo.close(client) == :ok, "closing twice is harmless"
      assert owned_sockets([pool, self()]) == []
      assert {:error, %Error{message: "this client has been closed"}} = Zygo.call(client, "f")
    end

    test "a pool under a supervisor is reached by name" do
      api = fake()
      FakeApi.answer(api, "GET", "/fn", 200, %{"functions" => [%{"name" => "f"}]})
      name = :"zygo_#{System.unique_integer([:positive])}"
      start_supervised!({Zygo, name: name, url: FakeApi.url(api), token: "op"})

      assert [%Zygo.Function{name: "f"}] = Zygo.functions!(Zygo.client(name))
      assert first(api).headers["authorization"] == "Bearer op"
    end
  end

  defp owned_sockets(pids) do
    for port <- Port.list(),
        info = Port.info(port),
        info != nil,
        info[:name] in [~c"tcp_inet", ~c"unix"] or to_string(info[:name]) =~ "tcp",
        info[:connected] in pids,
        do: port
  end

  describe "deploying" do
    test "serving sends an absolute base directory" do
      api = fake()

      FakeApi.answer(api, "PUT", "/fn/resize", 200, %{
        "name" => "resize",
        "change" => "started",
        "warm_ms" => 40.0
      })

      served =
        Zygo.serve!(connect(api), "resize", %{"entry" => "./resize.py"}, base_dir: "/srv/app")

      assert served.change == "started"
      assert first(api).body["base_dir"] == "/srv/app"
    end

    test "a one-shot run reports a non-zero exit without an error" do
      # The sandbox ran; this is what it said.
      api = fake()

      FakeApi.answer(api, "POST", "/run", 200, %{
        "exit_code" => 2,
        "stdout" => "",
        "stderr" => "boom\n",
        "timed_out" => false,
        "wall_ms" => 21.0
      })

      {:ok, run} = Zygo.run(connect(api), "alpine:3", ["false"], mem: "64M", network: "none")
      refute run.ok
      assert run.exit_code == 2
      assert run.stderr == "boom\n"

      sent = first(api).body
      assert sent["layer"]["image"] == "alpine:3"
      assert sent["layer"]["mem"] == "64M"
      assert sent["layer"]["network"] == "none"
      assert sent["stdin"] == ""
    end
  end

  describe "dependency sets" do
    @deps_id "deps_" <> String.duplicate("a", 32)

    test "the files go up as base64 and the answer is an id" do
      api = fake()

      FakeApi.answer(api, "POST", "/deps", 202, %{
        "id" => @deps_id,
        "state" => "building",
        "kind" => "python",
        "image" => "python:3.12-slim",
        "files" => %{"requirements.txt" => 15}
      })

      deps =
        Zygo.put_deps!(connect(api), "python:3.12-slim", %{
          "requirements.txt" => "requests==2.32\n"
        })

      assert deps.id == @deps_id
      assert Zygo.Deps.building?(deps)
      refute Zygo.Deps.ready?(deps)

      sent = first(api).body
      assert sent["image"] == "python:3.12-slim"
      assert Base.decode64!(sent["files"]["requirements.txt"]) == "requests==2.32\n"
    end

    test "a failed build carries its log" do
      api = fake()

      FakeApi.answer(api, "GET", "/deps/#{@deps_id}", 200, %{
        "id" => @deps_id,
        "state" => "failed",
        "error" => "pip exited 1",
        "log" => "ERROR: No matching distribution found for nosuchpkg"
      })

      deps = Zygo.deps!(connect(api), @deps_id)
      assert deps.state == "failed"
      assert deps.log =~ "nosuchpkg"
      refute Zygo.Deps.ready?(deps)
    end

    test "a pool on a dependency set that is still building is told to retry" do
      api = fake()

      FakeApi.answer(api, "POST", "/runtimes", 503, %{
        "error" => "deps_xyz is still building",
        "code" => "deps_building"
      })

      {:error, error} =
        Zygo.serve_runtime(connect(api), "pool", %{"image" => "x"}, deps: @deps_id)

      assert error.kind == :unavailable
      assert error.message =~ "still building"
      # The five seconds are the host's, from `Retry-After`.
      assert {error.code, error.retry_after} == {"deps_building", 5000}
      assert first(api).body["deps"] == @deps_id
    end

    test "a pool whose warm-up failed is unavailable too" do
      api = fake()

      FakeApi.answer(api, "POST", "/runtimes", 503, %{
        "error" => "zygote died",
        "code" => "warm_failed"
      })

      {:error, error} = Zygo.serve_runtime(connect(api), "pool", %{"image" => "x"})
      # No header this time, so the default a busy refusal gets.
      assert {error.kind, error.code, error.retry_after} == {:unavailable, "warm_failed", 1000}
    end

    test "a stopping API is unavailable from health" do
      api = fake()

      FakeApi.answer(api, "GET", "/healthz", 503, %{
        "ok" => false,
        "status" => "stopping",
        "uptime_s" => 9
      })

      {:error, error} = Zygo.health(connect(api))
      assert error.kind == :unavailable
      assert error.message =~ "stopping"
    end
  end

  describe "the script store" do
    @digest "sha256:" <> String.duplicate("0", 64)

    test "a script is sent as itself and its digest comes back" do
      source = "def handler(event):\n    return event\n"
      api = fake()

      FakeApi.answer(api, "PUT", "/scripts", 201, %{
        "sha256" => @digest,
        "size" => 37,
        "existed" => false
      })

      FakeApi.answer(api, "GET", "/scripts/#{@digest}", 200, %{"sha256" => @digest, "size" => 37})
      FakeApi.answer(api, "DELETE", "/scripts/#{@digest}", 200, %{"deleted" => true})
      client = connect(api)

      script = Zygo.put_script!(client, source)
      assert script.sha256 == @digest
      refute script.existed
      assert Zygo.script!(client, @digest).size == 37
      assert Zygo.delete_script!(client, @digest)

      assert first(api).raw == source, "the body is the script itself"
      assert first(api).headers["content-type"] =~ "text/plain"
    end

    test "a script the host does not have is not_found" do
      api = fake()

      FakeApi.answer(api, "GET", "/scripts/#{@digest}", 404, %{
        "error" => "no script",
        "code" => "not_found"
      })

      assert {:error, %Error{kind: :not_found}} = Zygo.script(connect(api), @digest)
    end

    test "something that is not a digest never reaches the wire" do
      api = fake()
      client = connect(api)

      for bad <- [
            "../../etc/passwd",
            "sha256:nope",
            "",
            "SHA256:" <> String.duplicate("A", 64),
            nil
          ] do
        assert {:error, %Error{kind: :spec}} = Zygo.script(client, bad)
      end

      assert FakeApi.requests(api) == []
    end
  end

  describe "tokens and tenants" do
    test "a secret arrives once and the listing never carries one" do
      api = fake()

      FakeApi.answer(api, "POST", "/tenants/acme/tokens", 201, %{
        "token" => %{"id" => "tok_1a2b3c4d5e6f", "tenant" => "acme", "created_ms" => 1},
        "secret" => "zygo_deadbeef"
      })

      FakeApi.answer(api, "GET", "/tokens", 200, %{
        "tokens" => [%{"id" => "tok_1a2b3c4d5e6f", "tenant" => "acme", "created_ms" => 1}]
      })

      FakeApi.answer(api, "DELETE", "/tokens/tok_1a2b3c4d5e6f", 200, %{"revoked" => true})
      client = connect(api)

      minted = Zygo.mint_token!(client, "acme")
      assert minted.secret == "zygo_deadbeef"
      assert minted.token.tenant == "acme"
      refute Zygo.Token.revoked?(minted.token)
      refute inspect(minted) =~ "deadbeef", "inspect printed the secret"

      assert [%Zygo.Token{id: "tok_1a2b3c4d5e6f"}] = Zygo.tokens!(client)
      assert Zygo.revoke_token!(client, "tok_1a2b3c4d5e6f")["revoked"]
      assert first(api).path == "/tenants/acme/tokens"
    end

    test "an operator token is minted on its own route" do
      api = fake()

      FakeApi.answer(api, "POST", "/tokens", 201, %{
        "token" => %{"id" => "tok_000000000000", "created_ms" => 1},
        "secret" => "zygo_x"
      })

      assert Zygo.mint_token!(connect(api)).token.tenant == nil, "an operator token names nobody"
      assert first(api).path == "/tokens"
    end

    test "acting for a tenant is a header on the same connection" do
      api = fake()
      FakeApi.answer(api, "GET", "/fn", 200, %{"functions" => []})
      client = connect(api)
      Zygo.functions!(client)
      Zygo.functions!(Zygo.for_tenant(client, "acme"))

      [plain, acting] = FakeApi.requests(api)
      refute Map.has_key?(plain.headers, "x-zygo-tenant")
      assert acting.headers["x-zygo-tenant"] == "acme"
      assert FakeApi.connections(api) == 1
    end

    test "inspecting a client never prints its token" do
      api = fake()
      client = connect(api, token: "zygo_verysecret")
      refute inspect(client) =~ "verysecret"
      assert inspect(client) =~ "redacted"
    end

    test "the admin routes go where they should, with the right bodies" do
      api = fake()

      FakeApi.answer(api, "POST", "/tenants", 200, %{
        "tenant" => %{"id" => "acme", "created_ms" => 1}
      })

      FakeApi.answer(api, "GET", "/tenants", 200, %{"tenants" => [%{"id" => "acme"}]})
      FakeApi.answer(api, "GET", "/tenants/acme", 200, %{"tenant" => %{"id" => "acme"}})

      FakeApi.answer(api, "PATCH", "/tenants/acme/limits", 200, %{
        "tenant" => %{"id" => "acme", "limits" => %{"mem" => "1G"}}
      })

      FakeApi.answer(api, "PUT", "/tenants/acme/secrets/KEY", 200, %{"secrets" => ["KEY"]})
      FakeApi.answer(api, "GET", "/tenants/acme/secrets", 200, %{"secrets" => ["KEY"]})
      FakeApi.answer(api, "DELETE", "/tenants/acme/secrets/KEY", 200, %{"secrets" => []})

      FakeApi.answer(api, "PUT", "/blobs", 200, %{
        "sha256" => "sha256:" <> String.duplicate("b", 64),
        "size" => 4
      })

      FakeApi.answer(api, "POST", "/drain", 200, %{"drained" => true, "in_flight" => 0})
      FakeApi.answer(api, "DELETE", "/tenants/acme", 200, %{"deleted" => true})
      client = connect(api)

      assert Zygo.create_tenant!(client, "acme").id == "acme"
      assert [%Zygo.Tenant{id: "acme"}] = Zygo.tenants!(client)
      assert Zygo.tenant!(client, "acme").id == "acme"
      assert Zygo.set_limits!(client, "acme", mem: "1G").limits == %{"mem" => "1G"}
      assert Zygo.put_secret!(client, "acme", "KEY", "v") == ["KEY"]
      assert Zygo.secrets!(client, "acme") == ["KEY"]
      assert Zygo.delete_secret!(client, "acme", "KEY") == []
      assert Zygo.put_blob!(client, <<0, "tar">>).size == 4
      assert Zygo.drain!(client, grace: 1500)["in_flight"] == 0
      assert Zygo.delete_tenant!(client, "acme")["deleted"]

      by_route = Map.new(FakeApi.requests(api), &{{&1.method, &1.path}, &1})
      secret = by_route[{"PUT", "/tenants/acme/secrets/KEY"}]
      assert secret.raw == "v"
      assert secret.headers["content-type"] == "text/plain; charset=utf-8"
      assert by_route[{"PUT", "/blobs"}].headers["content-type"] == "application/octet-stream"
      assert by_route[{"PATCH", "/tenants/acme/limits"}].body == %{"mem" => "1G"}
      assert by_route[{"POST", "/tenants"}].body == %{"id" => "acme"}
      assert Map.has_key?(by_route, {"POST", "/drain?grace_ms=1500"})
    end
  end

  describe "streaming" do
    @lines [
      %{"stream" => "stdout", "data" => "page 1\n"},
      %{"stream" => "progress", "data" => "halfway"},
      %{"stream" => "stderr", "data" => "a warning\n"},
      %{
        "status" => 200,
        "result" => %{"pages" => 2},
        "stdout" => "page 1\n",
        "stderr" => "a warning\n"
      }
    ]

    test "a stream yields its lines, then exactly one result" do
      api = fake()
      FakeApi.stream(api, "POST", "/fn/render", @lines)
      events = connect(api) |> Zygo.stream("render", %{"pages" => 2}) |> Enum.to_list()

      assert [
               {:stdout, "page 1\n"},
               {:progress, "halfway"},
               {:stderr, "a warning\n"},
               {:result, %Result{result: %{"pages" => 2}}}
             ] = events

      assert first(api).path == "/fn/render?stream=1"
      assert first(api).headers["accept"] == "application/x-ndjson"
    end

    test "lines arrive as they are sent, not at the end" do
      # What a stream promises is *when*. The server pauses between lines,
      # and the first has to be in hand before the last has been sent.
      api = fake()
      FakeApi.stream(api, "POST", "/fn/render", @lines, 300)
      began = System.monotonic_time(:millisecond)

      times =
        connect(api)
        |> Zygo.stream("render", %{})
        |> Enum.map(fn _ -> System.monotonic_time(:millisecond) - began end)

      assert hd(times) < List.last(times) / 2,
             "the first line took #{hd(times)} of #{List.last(times)} ms"
    end

    test "a failed request ends in its error, after its output" do
      api = fake()

      FakeApi.stream(api, "POST", "/fn/render", [
        %{"stream" => "stdout", "data" => "starting\n"},
        %{"status" => 500, "error" => "boom", "exit_code" => 1, "stdout" => "starting\n"}
      ])

      client = connect(api)

      assert [{:stdout, "starting\n"}, {:error, %Error{kind: :handler}}] =
               client |> Zygo.stream("render", %{}) |> Enum.to_list()

      seen = :ets.new(:seen, [:bag, :public])

      assert_raise Error, fn ->
        client |> Zygo.stream!("render", %{}) |> Enum.each(&:ets.insert(seen, {elem(&1, 0)}))
      end

      assert :ets.lookup(seen, :stdout) != [], "the output was not delivered first"
    end

    test "a refusal is the stream's only element" do
      api = fake()
      FakeApi.answer(api, "POST", "/fn/render", 404, %{"error" => "no function"})

      assert [{:error, %Error{kind: :not_found}}] =
               connect(api) |> Zygo.stream("render", %{}) |> Enum.to_list()
    end

    test "halting early closes the connection" do
      api = fake()
      FakeApi.stream(api, "POST", "/fn/render", @lines, 200)
      assert [{:stdout, _}] = connect(api) |> Zygo.stream("render", %{}) |> Enum.take(1)
      assert owned_sockets([self()]) == []
    end

    test "a pool's script streams the same way" do
      api = fake(unix: true)
      FakeApi.stream(api, "POST", "/runtimes/py/call", @lines)
      events = connect(api) |> Zygo.stream_script("py", "x = 1") |> Enum.to_list()
      assert {:result, _} = List.last(events)
      assert first(api).body["script"] == %{"source" => "x = 1"}
    end
  end

  describe "cancelling" do
    test "a cancelled request is its own kind" do
      api = fake()

      FakeApi.answer(api, "POST", "/fn/slow", 499, %{
        "error" => "the request was cancelled",
        "cancelled" => true,
        "request_id" => "00000007",
        "metrics" => %{"wall_ms" => 1200.0}
      })

      assert {:error, %Error{kind: :cancelled, request_id: "00000007"}} =
               Zygo.call(connect(api), "slow", %{})
    end

    test "a call can be named so that it can be stopped" do
      api = fake()
      FakeApi.answer(api, "POST", "/fn/slow", 200, %{"result" => nil, "request_id" => "00000008"})

      FakeApi.answer(api, "DELETE", "/requests/job-4711", 200, %{
        "cancelled" => true,
        "request_id" => "00000008",
        "started" => true
      })

      client = connect(api)
      assert Zygo.call!(client, "slow", %{}, key: "job-4711").request_id == "00000008"
      assert Zygo.cancel!(client, "job-4711")["cancelled"]
      assert first(api).headers["x-zygo-request-key"] == "job-4711"
    end

    test "another process can stop a call by its key while it runs" do
      api = fake()
      FakeApi.set(api, :delay, 300)
      FakeApi.answer(api, "POST", "/fn/slow", 200, %{"result" => nil})
      client = connect(api)
      key = Zygo.request_key()
      assert key =~ ~r/\Ak-[0-9a-f]{32}\z/

      task = Task.async(fn -> Zygo.call(client, "slow", %{}, key: key) end)
      Process.sleep(50)
      Zygo.cancel(client, key)
      Task.await(task)

      paths = Enum.map(FakeApi.requests(api), & &1.path)
      assert "/requests/#{key}" in paths
    end
  end

  describe "one-shot outcomes" do
    test "out of memory and out of time are different answers" do
      api = fake()

      FakeApi.answer(api, "POST", "/run", 200, %{
        "exit_code" => 137,
        "timed_out" => false,
        "oom_killed" => true,
        "peak_rss_kb" => 65536,
        "wall_ms" => 412.7
      })

      starved =
        Zygo.run!(connect(api), "python:3.12-slim", ["python3", "-c", "b=bytearray(1<<30)"])

      assert {starved.exit_code, starved.oom_killed, starved.timed_out} == {137, true, false}
      assert starved.peak_rss_kb == 65536
      refute starved.ok

      api = fake()
      FakeApi.answer(api, "POST", "/run", 408, %{"error" => "the sandbox exceeded its timeout"})

      assert {:error, %Error{kind: :timeout}} =
               Zygo.run(connect(api), "alpine:3", ["sleep", "60"])
    end

    test "a sandbox that never started is the host's problem, not the program's" do
      api = fake()

      FakeApi.answer(api, "POST", "/run", 200, %{
        "exit_code" => 125,
        "stderr" => "error: this host cannot run sandboxes\n",
        "started" => false,
        "phase" => "start"
      })

      never = Zygo.run!(connect(api), "alpine:3", ["true"])
      assert {never.started, never.phase, never.ok} == {false, "start", false}
    end

    test "an older API that does not report a reason still parses" do
      api = fake()
      FakeApi.answer(api, "POST", "/run", 200, %{"exit_code" => 0, "stdout" => "hi\n"})
      run = Zygo.run!(connect(api), "alpine:3", ["echo", "hi"])
      assert run.ok
      refute run.oom_killed
      assert run.peak_rss_kb == 0
      assert run.started, "an older API only answered once the program ran"
      assert run.phase == "run"
    end

    test "stdin is sent beside the layer, not in it" do
      api = fake()
      FakeApi.answer(api, "POST", "/run", 200, %{"exit_code" => 0})
      Zygo.run!(connect(api), "alpine:3", ["cat"], stdin: "hello")
      assert first(api).body["stdin"] == "hello"
      refute Map.has_key?(first(api).body["layer"], "stdin")
    end
  end

  describe "retries" do
    # Off by default. When on, only the two errors that mean "the request
    # never ran" qualify, and the wait honours what the server asked for.

    test "a refused call is sent again and the answer is the last one" do
      api = fake()
      FakeApi.answer_then(api, "POST", "/fn/f", [@busy, @busy, {200, @ok_result}], 0)
      out = Zygo.call!(connect(api, retries: 3, backoff: 10), "f", %{"n" => 1})
      assert out.result == %{"size" => [80, 60]}
      requests = FakeApi.requests(api)
      assert length(requests) == 3, "two refusals, then the answer"
      assert Enum.uniq(Enum.map(requests, & &1.raw)) == [~s({"n":1})]
    end

    test "the wait is at least what the server asked for" do
      api = fake()
      FakeApi.answer_then(api, "POST", "/fn/f", [@busy, {200, @ok_result}], 0.3)
      client = connect(api, retries: 1, backoff: 0)
      {elapsed, _} = :timer.tc(fn -> Zygo.call!(client, "f", %{}) end)
      assert elapsed >= 300_000
      assert elapsed < 1_500_000
    end

    test "the wait grows when the host keeps refusing" do
      api = fake()
      FakeApi.answer_then(api, "POST", "/fn/f", [@busy, @busy, @busy, {200, @ok_result}], 0)
      client = connect(api, retries: 3, backoff: 100)
      {elapsed, _} = :timer.tc(fn -> Zygo.call!(client, "f", %{}) end)
      # 100, then 200, then 400: doubled each time, from the base.
      assert elapsed >= 700_000
      assert elapsed < 2_000_000
    end

    test "retries are off unless asked for" do
      api = fake()
      FakeApi.answer_then(api, "POST", "/fn/f", [@busy, {200, @ok_result}])
      assert {:error, %Error{kind: :busy}} = Zygo.call(connect(api), "f", %{})
      assert length(FakeApi.requests(api)) == 1
    end

    test "the last refusal is the one returned" do
      api = fake()
      {status, body} = @busy
      FakeApi.answer(api, "POST", "/fn/f", status, body, 0)
      assert {:error, %Error{kind: :busy}} = Zygo.call(connect(api, retries: 2, backoff: 0), "f")
      assert length(FakeApi.requests(api)) == 3, "the first try and two retries"
    end

    test "a pool waiting on a build is retried" do
      api = fake()

      FakeApi.answer_then(
        api,
        "POST",
        "/runtimes",
        [
          {503, %{"error" => "still building", "code" => "deps_building"}},
          {200, %{"name" => "pool", "warm" => 1, "change" => "started"}}
        ],
        0
      )

      served =
        Zygo.serve_runtime!(connect(api, retries: 1, backoff: 10), "pool", %{"image" => "x"},
          deps: "deps_1"
        )

      assert served["warm"] == 1
      assert length(FakeApi.requests(api)) == 2
    end

    test "a handler that raised is never sent again" do
      # A handler that raised will raise again, and a request the deadline
      # killed *ran*; sending either twice is how a side effect happens twice.
      failed = %{"error" => "boom", "exit_code" => 1, "stdout" => "", "stderr" => "Traceback"}
      api = fake()
      FakeApi.answer_then(api, "POST", "/fn/f", [{500, failed}, {200, @ok_result}])

      FakeApi.answer_then(api, "POST", "/fn/slow", [
        {408, %{"error" => "timed out"}},
        {200, @ok_result}
      ])

      FakeApi.answer_then(api, "POST", "/fn/gone", [
        {404, %{"error" => "no such"}},
        {200, @ok_result}
      ])

      client = connect(api, retries: 5, backoff: 0)

      assert {:error, %Error{kind: :handler}} = Zygo.call(client, "f", %{})
      assert {:error, %Error{kind: :timeout}} = Zygo.call(client, "slow", %{})
      assert {:error, %Error{kind: :not_found}} = Zygo.call(client, "gone", %{})
      assert length(FakeApi.requests(api)) == 3, "each was sent exactly once"
    end

    test "a stream refused before its first line is retried" do
      api = fake()
      {status, body} = @busy
      FakeApi.answer(api, "POST", "/fn/f", status, body, 0)

      assert [{:error, %Error{kind: :busy}}] =
               connect(api, retries: 1, backoff: 10) |> Zygo.stream("f", %{}) |> Enum.to_list()

      requests = FakeApi.requests(api)
      assert length(requests) == 2
      assert List.last(requests).headers["accept"] == "application/x-ndjson"
    end
  end

  describe "runtime pools" do
    @pool_digest "sha256:" <> String.duplicate("1", 64)

    test "a pool is registered, listed, called and stopped" do
      api = fake()

      FakeApi.answer(api, "POST", "/runtimes", 200, %{
        "name" => "py312",
        "runtime" => "python/3.12.4",
        "warm" => 2,
        "change" => "started"
      })

      FakeApi.answer(api, "GET", "/runtimes", 200, %{
        "runtimes" => [
          %{"name" => "py312", "warm" => 2, "cold" => 2, "max_warm" => 4, "new" => 1}
        ]
      })

      FakeApi.answer(api, "POST", "/runtimes/py312/call", 200, %{
        "result" => %{"ok" => true},
        "metrics" => %{"wall_ms" => 2.1}
      })

      FakeApi.answer(api, "DELETE", "/runtimes/py312", 200, %{"stopped" => ["py312"]})
      client = connect(api)

      served =
        Zygo.serve_runtime!(
          client,
          "py312",
          %{"image" => "python:3.12-slim", "agent" => "python", "min_warm" => 2},
          secrets: ["STRIPE_KEY"]
        )

      assert served["warm"] == 2

      [pool] = Zygo.runtimes!(client)
      assert {pool.name, pool.max_warm} == {"py312", 4}
      # A field this package does not know is kept, not dropped.
      assert pool.extra == %{"new" => 1}

      assert Zygo.run_script!(client, "py312", @pool_digest, %{"n" => 1}).result == %{
               "ok" => true
             }

      # And a one-off, where there is nothing registered to name.
      Zygo.run_script!(client, "py312", "def handler(e):\n    return e\n")
      assert Zygo.stop_runtime!(client, "py312") == ["py312"]

      [serve, _, called, one_off, _] = FakeApi.requests(api)
      assert serve.body["layer"]["agent"] == "python"
      # Names in the layer; the values are the calling tenant's.
      assert serve.body["layer"]["secrets"] == ["STRIPE_KEY"]
      refute Map.has_key?(serve.body, "secrets")
      assert called.body["script"] == @pool_digest, "a digest goes as a string"
      assert called.body["event"] == %{"n" => 1}
      assert one_off.body["script"]["source"] =~ "def handler"
    end

    test "a workspace and out go in the query for a function, in the body for a pool" do
      digest = "sha256:" <> String.duplicate("c", 64)
      packed = Base.encode64("tar bytes")
      api = fake()
      FakeApi.answer(api, "POST", "/fn/f", 200, Map.put(@ok_result, "workspace", packed))
      FakeApi.answer(api, "POST", "/runtimes/py/call", 200, @ok_result)
      client = connect(api)

      out = Zygo.call!(client, "f", %{}, workspace: digest, out: true)
      assert out.workspace == "tar bytes", "the tar comes back decoded"
      Zygo.run_script!(client, "py", "x=1", nil, workspace: %{"files" => %{}}, out: true)

      assert {:error, %Error{kind: :spec}} =
               Zygo.call(client, "f", %{}, workspace: "not-a-digest")

      [fun, pool] = FakeApi.requests(api)
      assert fun.path == "/fn/f?workspace=#{digest}&out=1"
      assert pool.path == "/runtimes/py/call?out=1"
      assert pool.body["workspace"] == %{"files" => %{}}
    end
  end
end
