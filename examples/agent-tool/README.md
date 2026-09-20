# An LLM agent tool

The model decides what to run; the model is not trusted. A tool call is
therefore a request into a sandbox with no network, a small memory limit, a
handful of pids and a two-second deadline — and it costs a `fork()`, not a
container.

```bash
zygo up
zygo exec calc '{"expression": "2 ** 10 / 4"}'
# {"result": 256.0}
zygo exec calc '{"expression": "__import__(\"os\").system(\"id\")"}'
# {"error": "ValueError: unsupported: Call(...)"}
zygo exec calc '{"expression": "9 ** 9 ** 9"}'
# exit 137: the deadline killed the whole process tree
```

From an agent loop, the call is one process spawn per tool call, or one HTTP
request against `zygo api`:

```python
import json, subprocess

def calc(expression: str) -> dict:
    out = subprocess.run(
        ["zygo", "exec", "calc", json.dumps({"expression": expression})],
        capture_output=True, text=True, timeout=10,
    )
    return json.loads(out.stdout) if out.returncode == 0 else {"error": out.stderr.strip()}
```

The spec is where the policy lives, not the handler: `seccomp = "strict"`
means a `socket()` is refused by the kernel; `pids = 4` means a fork bomb
stops at four; `timeout = "2s"` means the supervisor ends the request, and
every process it started, two seconds in. The handler parses instead of
`eval`-ing because that is good practice — but the sandbox is what makes its
mistakes survivable.

For code you did not write and cannot read, the design's answer is
`isolation = "vm"`, a hardware boundary. That backend needs a machine with KVM
and is not built yet ([todo.md](../../todo.md), 2.5); until it is, this runs
on `ns` with the tightest profile it has, and the [threat model](../../docs/threat-model.md)
says plainly what that does and does not hold against.
