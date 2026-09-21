# The MCP server

`zygo mcp` gives an agent host a code interpreter whose boundary is a file
somebody reviewed. It speaks the Model Context Protocol over standard input and
output, which is what Claude Code, Claude Desktop, Cursor and the rest start as
a child process. There is no port, no token and no network: the transport is a
pipe between two processes running as the same user.

```json
{ "mcpServers": { "zygo": { "command": "zygo", "args": ["mcp"] } } }
```

That is the whole installation. On macOS it is forwarded into the Linux VM like
every other sandbox command, and the VM hop is paid once when the host starts
the server rather than once per tool call.

## The rule that shapes it

A model reads untrusted text — a web page, a file, an error message — and that
text can ask it for things. So the tools expose a **program and nothing else**:
no image, no mounts, no network mode, no limits. Those are set once, on the
command line, by the person who installed the server.

```bash
zygo mcp --mem 512M --timeout 60s --workspace ./agent-scratch
zygo mcp --net egress --allow api.github.com:443
zygo mcp -f ./sandbox.toml              # [defaults] becomes the ceiling
```

A model that needs more than this gives it does not get to ask for it. It
declares a function in `sandbox.toml` — with its dependencies, its egress
allowlist and its secrets — and calls that by name. The boundary is then in a
file that was reviewed, which is where it belongs.

There is a test that enforces this: `no_tool_can_widen_the_sandbox` fails if any
tool schema ever grows an `image`, `mount`, `network`, `mem` or similar field.
Adding one looks harmless in isolation, which is exactly why it is checked.

## The tools

| Tool | What it does |
|---|---|
| `run_code` | Run a program in a fresh sandbox and return its output. |
| `list_functions` | The warm functions this project declares. |
| `call_function` | Call one by name with a JSON event. |
| `function_logs` | Recent log entries for one, including only the failures. |

`run_code` takes `language` (`python`, `node` or `sh`), `code`, and optionally
`stdin`. The program is written to a file and mounted read-only at `/zygo`, so
it can be of any length, tracebacks name a real file, and the code cannot
rewrite itself mid-run.

`/work` is a writable directory that persists between calls, so a model can
write a file in one call and read it in the next. Without `--workspace` it is a
scratch directory removed when the server exits; naming one makes it real, and
is how an agent is given a project to work on.

When a sandbox is killed, the tool result says *why* in words: "ran out of
memory" and "exceeded its time limit" are different sentences, because both are
exit 137 and a model told "exit 137" cannot know which of its two problems to
fix.

The server's `instructions` tell the model to prefer a warm function over
`run_code` where one exists — a millisecond against tens of them, with
dependencies already imported.

## What a sandbox gets

Whatever `zygo run` gives, which on the `ns` backend is: no capabilities, a
read-only root with `pivot_root` and a masked `/proc`, a seccomp allowlist of
about 190 syscalls, Landlock where the kernel has it, mandatory memory, CPU and
process limits, and **no network at all** unless the command line said
otherwise. See [the threat model](threat-model.md) for where that boundary is
weaker than it looks.

`run_code` is not sandboxed *from the model*: running the model's code is the
point. It is sandboxed from the machine.

## Concurrency and failures

Each request is handled on a thread of its own, so a `run_code` that takes
thirty seconds does not block a `list_functions` behind it. Answers are written
one line at a time, so two cannot interleave.

A tool that fails answers with a result marked `isError`, not a JSON-RPC error.
The difference matters: an error at the protocol layer is handled by the host
and never reaches the model, and the model is the one that could fix a
traceback.

The three tools that read warm functions connect to an existing supervisor and
do not start one. There is nothing to read or call unless somebody has already
served something, and a supervisor started here would answer "no functions"
from a process that did not exist a moment ago.

## Protocol revisions

The server speaks `2025-06-18`, `2025-03-26` and `2024-11-05`. It answers
`initialize` with the revision the client asked for when it is one of those,
and with its own preferred one otherwise — so the client decides whether it can
live with the answer, rather than the server agreeing to a dialect it does not
know.

## Trying it without a host

The transport is a pipe, so a shell is enough:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' \
  | zygo mcp
```

Two lines of JSON come back, one per request. Standard output belongs to the
protocol, so anything a human should read — including the workspace path — goes
to standard error, which is where a host shows a server's log.
