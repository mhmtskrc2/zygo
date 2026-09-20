# A Go program as a warm function

No agent, no protocol. Zygo holds the sandbox warm and runs the binary once per
request with the event on stdin; whatever it prints to stdout is the result.
A compiled program starts in well under a millisecond, so there is nothing to
amortise and an agent would only add a moving part.

```bash
CGO_ENABLED=0 go build -o bin/parse .     # static: the image needs nothing
zygo up                                   # the sandbox.toml beside this file
zygo exec parse '{"text": "warm exec in go", "n": 12}'
# {"words":4,"upper":"WARM EXEC IN GO","squared":144,"language":"go"}
```

The spec is the whole integration:

```toml
[fn.parse]
image  = "alpine:3"
mounts = ["./bin/parse:/app/parse:ro"]
cmd    = ["/app/parse"]
```

`make examples-go-linux` builds it in a Go container and runs exactly this
against a real kernel; measured there, ten requests through the CLI average
under 60 ms each with the client's own start-up included, and the request
itself is about 2 ms.

A non-zero exit is a failed request, and stderr comes back to the caller as
`stderr` — so `fmt.Fprintf(os.Stderr, …)` followed by `os.Exit(1)` is the
whole error-handling story.
