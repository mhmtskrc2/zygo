# Examples

Each directory is a complete, runnable thing with a `sandbox.toml` and a
README that says what the spec does and why. `make examples-go-linux` builds
the Go one and runs the specs for real against a kernel — including the
workflow-engine worker end to end, in two languages — and the rest are
validated with `zygo spec validate` in the same run.

| | What it shows |
|---|---|
| [`webhook/`](webhook) | One warm Python function behind `zygo api`, with a secret delivered as a per-request file and no network at all. |
| [`agent-tool/`](agent-tool) | A tool an LLM can call: untrusted input, `seccomp = "strict"`, four pids, a two-second deadline. |
| [`ci-job/`](ci-job) | `zygo run` as a test runner: the repository mounted read-only, a sealed sandbox, the program's own exit code. |
| [`warm-exec/go/`](warm-exec/go) | A compiled program as a warm function. No agent, no protocol — a `cmd` is the whole integration. |
| [`workflow-engine/`](workflow-engine) | The embedder Zygo is built for: a worker draining a queue of other people's scripts, warm per script version and one fork per run. Maps onto Windmill and n8n. |
| [`agents/`](agents) | Writing an agent for a runtime that is expensive to start: the guide and a complete agent in POSIX sh. The Python and Node agents Zygo ships are in [`agents/`](../agents). |
