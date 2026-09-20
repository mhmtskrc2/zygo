# A webhook handler

One warm function, reachable over HTTP, with a secret it never keeps.

```bash
export WEBHOOK_SECRET=change-me            # the shell running `up` supplies it
zygo up                                    # warm; ~300 ms once

export ZYGO_API_TOKEN=$(openssl rand -hex 16)
zygo api &                                 # 127.0.0.1:7700, bearer auth

curl -s -H "Authorization: Bearer $ZYGO_API_TOKEN" \
     -d '{"payload": {"kind": "order", "items": [1, 2, 3]}}' \
     http://127.0.0.1:7700/fn/webhook
# {"status":200,"received":"order","items":3}
```

What the spec says, and why:

* **`secrets = ["WEBHOOK_SECRET"]`** — the value is read from the environment
  of the shell that runs `zygo up`, written by the supervisor to
  `/run/secrets/WEBHOOK_SECRET` inside the sandbox for the duration of each
  request, and removed afterwards. The zygote never sees it, it is not in the
  agent's memory, and it is not in any message on the control socket.
* **No `network`** — a webhook receives. The handler cannot open a connection
  to anything, so a bug in it cannot become an exfiltration.
* **`timeout = "5s"`** — the supervisor kills the request's whole process tree
  at the deadline; the API answers `408`.
* **`concurrency = 8`** — nine simultaneous requests queue, and past the queue
  the API answers `429` with `Retry-After`, which is what a caller can act on.

`zygo logs webhook -f` shows every request with its stdout, stderr and exit
code; `zygo logs webhook --failed` shows the ones to look at.
