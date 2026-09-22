# Warm exec: no agent, no protocol

Zygo's warm path exists to take an interpreter's start-up off the request. For
a runtime that starts in under a millisecond there is nothing to take off, so
there is a second shape with no agent in it at all: Zygo holds the sandbox,
and each request is a fresh process with the event on stdin and JSON on
stdout.

Two ways to use it, and the difference is where the code comes from.

## One program, warmed — `[fn.<name>]`

[`go/`](go) is the whole of it: a static binary, a `cmd`, and nothing else.
The program is the operator's own, deployed once.

```toml
[fn.parse]
image  = "alpine:3"
mounts = ["./bin/parse:/app/parse:ro"]
cmd    = ["/app/parse"]
```

## Many scripts, one sandbox — `[runtime.<name>]`

[`shell/`](shell) is the other: `cmd` names a program the operator trusts, and
each **request** brings the script it runs. Zygo writes that script into the
sandbox and puts its path on the end of the command line, so
`cmd = ["/bin/sh"]` becomes `sh /run/script/<digest>`.

```toml
[runtime.sh]
image = "alpine:3"
cmd   = ["/bin/sh"]
```

```bash
zygo serve --runtime sh
zygo exec --runtime sh --script wordcount.sh '{"text": "warm exec in sh"}'
# {"words":4,"chars":15,"shell":"/bin/busybox"}
```

or over the API, which is the same thing without a shell on the Zygo host:

```python
script = client.put_script(open("wordcount.sh").read())
client.serve_runtime("sh", {"image": "alpine:3", "cmd": ["/bin/sh"]})
out = client.run_script("sh", script.sha256, {"text": "warm exec in sh"})
```

This is the shape for `bash`, for a static binary that takes a script as an
argument, and for any language whose runtime you would otherwise be warming
for no reason.

## What you give up

A warm-exec pool has no protocol between Zygo and the program, so four things
an agent pool has are simply not there:

| | agent pool | warm-exec pool |
|---|---|---|
| streaming (`?stream=1`) | chunks as they are printed | the whole output, at the end |
| `progress()` | its own kind of chunk | — |
| workspaces (`ZYGO_WORKSPACE`) | yes | — |
| per-tenant limits, narrowed per request | yes | the pool's own limits |
| cost of a request | a fork from a warm heap | an `execve` |

Everything else is identical, because everything else is the sandbox: the same
namespaces, the same seccomp profile, the same cgroup per request, the same
deadline, and the same "one process per request, and nothing of the last one
survives".

The script is a **file** here rather than bytes on the wire, because it is
named on a command line. It lands in `/run/script/<digest>`, which is a
read-only bind mount — so the file Zygo wrote is the file that runs, and a
script in the pool cannot replace another one's.
