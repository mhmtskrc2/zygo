# Examples

Each directory is a complete, runnable thing with a README that says what it
does and why; all but `plugin-host/`, `plugin-host-node/` and `kubernetes/` carry a `sandbox.toml`. `make examples-go-linux` builds
the Go one and runs the specs for real against a kernel — including the
workflow-engine worker end to end, in two languages — and the rest are
validated with `zygo spec validate` in the same run.

| | What it shows |
|---|---|
| [`web-api/`](web-api) | A tiny web API with three endpoints — a warm function, a runtime pool and a fresh sandbox — side by side. Standard library only; tested end to end. |
| [`webhook/`](webhook) | One warm Python function behind `zygo api`, with a secret delivered as a per-request file and no network at all. |
| [`agent-tool/`](agent-tool) | A tool an LLM can call: untrusted input, `seccomp = "strict"`, four pids, a two-second deadline. |
| [`ci-job/`](ci-job) | `zygo run` as a test runner: the repository mounted read-only, a sealed sandbox, the program's own exit code. |
| [`warm-exec/go/`](warm-exec/go) | A compiled program as a warm function. No agent, no protocol — a `cmd` is the whole integration. |
| [`plugin-host/`](plugin-host) | A plugin host built on the HTTP API alone: the embedder API's exit criterion, written as a program rather than a checklist. |
| [`plugin-host-node/`](plugin-host-node) | The same host from the Node SDK, as an HTTP server: `node:http` in front, one operator client and `forTenant` behind, Zygo's errors mapped to status codes. Tested against a fake Zygo with `node --test`, and end to end. |
| [`kubernetes/`](kubernetes) | `zygo api` as a Deployment: two replicas, an `emptyDir` image store, a readiness probe, a `preStop` drain and a rolling update that drops no request. |
| [`workflow-engine/`](workflow-engine) | The embedder Zygo is built for: a worker draining a queue of other people's scripts, warm per script version and one fork per run. Maps onto Windmill and n8n. |
| [`agents/`](agents) | Writing an agent for a runtime that is expensive to start: the guide and a complete agent in POSIX sh. The Python and Node agents Zygo ships are in [`agents/`](../agents). |
