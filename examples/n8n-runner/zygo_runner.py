#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""An n8n task runner whose tasks run in Zygo runtime pools.

n8n runs a Code node through a *task runner*: a process that connects to n8n's
task broker over a WebSocket, is offered tasks, runs the code and sends the
result back. This is one. It speaks the broker protocol that
`@n8n/task-runner` and `@n8n/task-runner-python` speak, for both task types,
"javascript" and "python", and runs no user code itself: every accepted task
becomes one `POST /runtimes/<pool>/call`, a fork of a warm interpreter in its
own cgroup, under seccomp and Landlock, with no network, gone when the task
ends.

    N8N_RUNNERS_TASK_BROKER_URI   http://127.0.0.1:5679
    N8N_RUNNERS_AUTH_TOKEN        the broker's shared secret (required)
    N8N_RUNNERS_MAX_CONCURRENCY   slots per task type (default 5)
    N8N_RUNNERS_TASK_TIMEOUT      seconds (default 60)
    ZYGO_API                      http://127.0.0.1:7700
    ZYGO_API_TOKEN                if the API wants one
    ZYGO_POOL_PY / ZYGO_POOL_JS   pool names (default py313 / node26)

Not implemented, and failing the task loudly rather than approximating:
`this.helpers.*` (HTTP requests and binary data, which n8n serves over RPC),
`$node`, `$('Other node')`, `$workflow`, `$env`, `$execution`, static data,
chunked per-item input, and the AI-tool mode (`runCode`) for JavaScript.
See README.md.
"""
import asyncio
import hashlib
import json
import logging
import os
import random
import secrets
import time
import urllib.request
from urllib.parse import urlparse

from zygo_sdk import Busy, HandlerError, Timeout, ZygoError
from zygo_sdk.aio import AsyncClient

log = logging.getLogger("zygo-runner")

BROKER = os.environ.get("N8N_RUNNERS_TASK_BROKER_URI", "http://127.0.0.1:5679")
AUTH = os.environ.get("N8N_RUNNERS_AUTH_TOKEN", "")
SLOTS = int(os.environ.get("N8N_RUNNERS_MAX_CONCURRENCY", "5"))
TIMEOUT = float(os.environ.get("N8N_RUNNERS_TASK_TIMEOUT", "60"))
POOLS = {
    "python": os.environ.get("ZYGO_POOL_PY", "py313"),
    "javascript": os.environ.get("ZYGO_POOL_JS", "node26"),
}
OFFER_VALIDITY_MS = 5000
OFFER_INTERVAL = 0.25

# ---------------------------------------------------------------- scripts --
# The user's code is wrapped the way each stock runner wraps it: a function
# body with `return`, the items in scope under the same names.

PY_TEMPLATE = '''\
_items = None
_item = None
_query = None

def _user_function():
{body}

def handler(event):
    global _items, _item, _query
    if event["mode"] == "runOnceForAllItems":
        _items = event["items"]
        _query = event.get("query")
        return _user_function()
    out = []
    for i, it in enumerate(event["items"]):
        _item = it
        r = _user_function()
        if r is None:
            continue
        j = r.get("json", r) if isinstance(r, dict) else r
        o = {{"json": j, "pairedItem": {{"item": i}}}}
        if isinstance(r, dict) and "binary" in r:
            o["binary"] = r["binary"]
        out.append(o)
    return out
'''

JS_TEMPLATE = '''\
function __input(items, index) {{
  return {{
    all: () => items,
    first: () => items[0],
    last: () => items[items.length - 1],
    item: index === undefined ? undefined : items[index],
    itemMatching: (i) => items[i],
    get context() {{ return {{}}; }},
  }};
}}
async function __user($input, items, item, $json, $query, query) {{
  return await (async function () {{
{body}
  }}).call({{}});
}}
module.exports = async function handler(event) {{
  const items = event.items;
  if (event.mode === "runOnceForAllItems") {{
    const r = await __user(__input(items), items, undefined,
                           items[0] ? items[0].json : undefined, event.query, event.query);
    return r === null ? [] : r;
  }}
  const out = [];
  for (let i = 0; i < items.length; i++) {{
    const r = await __user(__input(items, i), items, items[i], items[i].json, event.query, event.query);
    if (r === null || r === undefined) continue;
    const j = (r && typeof r === "object" && "json" in r) ? r.json : r;
    const o = {{ json: j, pairedItem: {{ item: i }} }};
    if (r && r.binary) o.binary = r.binary;
    out.push(o);
  }}
  return out;
}};
'''


def build_script(task_type: str, code: str) -> str:
    body = "\n".join("    " + line for line in code.splitlines()) or "    pass"
    if task_type == "python":
        return PY_TEMPLATE.format(body=body)
    return JS_TEMPLATE.format(body=code)


# ----------------------------------------------------------------- runner --
class Runner:
    def __init__(self):
        self.id = secrets.token_hex(8)
        self.ws = None
        self.offers: dict[str, tuple[str, float]] = {}  # offer id -> (type, valid until)
        self.running: dict[str, dict] = {}  # task id -> state
        self.data_waits: dict[str, asyncio.Future] = {}
        self.digests: dict[str, str] = {}  # sha256(script) -> digest in Zygo's store
        self.zygo = AsyncClient(os.environ.get("ZYGO_API", "http://127.0.0.1:7700"),
                                timeout=TIMEOUT + 10)
        self.can_offer = False

    def grant_token(self) -> str:
        req = urllib.request.Request(
            BROKER.rstrip("/") + "/runners/auth",
            data=json.dumps({"token": AUTH}).encode(),
            headers={"Content-Type": "application/json"}, method="POST")
        with urllib.request.urlopen(req, timeout=10) as r:
            body = json.loads(r.read())
        return (body.get("data") or body)["token"]

    async def send(self, msg: dict):
        await self.ws.send(json.dumps(msg))

    async def run(self):
        # Imported here so that the script templates above can be tested
        # without the one dependency the protocol needs.
        import websockets

        host = urlparse(BROKER).netloc
        while True:
            try:
                token = self.grant_token()
                async with websockets.connect(
                        f"ws://{host}/runners/_ws?id={self.id}",
                        additional_headers={"Authorization": f"Bearer {token}"},
                        max_size=1 << 30) as ws:
                    self.ws = ws
                    log.info("connected to broker")
                    async for raw in ws:
                        await self.on_message(json.loads(raw))
            except Exception as e:  # reconnect, as the stock runners do
                log.warning("broker connection lost: %s", e)
            self.can_offer = False
            self.offers.clear()
            await asyncio.sleep(2)

    # -- messages ------------------------------------------------------------
    async def on_message(self, m: dict):
        t = m["type"]
        if t == "broker:inforequest":
            await self.send({"type": "runner:info", "name": "Zygo Task Runner",
                             "types": list(POOLS)})
        elif t == "broker:runnerregistered":
            self.can_offer = True
            asyncio.create_task(self.offer_loop())
            log.info("registered for %s", ", ".join(POOLS))
        elif t == "broker:taskofferaccept":
            await self.on_accept(m["taskId"], m["offerId"])
        elif t == "broker:tasksettings":
            st = self.running.get(m["taskId"])
            if st is not None:
                st["job"] = asyncio.create_task(self.execute(m["taskId"], st["type"], m["settings"]))
        elif t == "broker:taskdataresponse":
            f = self.data_waits.pop(m["requestId"], None)
            if f and not f.done():
                f.set_result(m["data"])
        elif t == "broker:taskcancel":
            st = self.running.get(m["taskId"])
            if st and st.get("job"):
                st["job"].cancel()  # the SDK turns this into DELETE /requests/<key>
            elif st:
                self.running.pop(m["taskId"], None)
        elif t == "broker:drain":
            self.can_offer = False
        # broker:rpcresponse, broker:nodetypes: nothing asks for them

    async def offer_loop(self):
        while self.can_offer:
            await self.send_offers()
            await asyncio.sleep(OFFER_INTERVAL)

    async def send_offers(self):
        # Also called the moment a task finishes, as @n8n/task-runner does
        # (finishTask -> sendOffers); waiting for the 250 ms tick instead caps
        # a runner at slots / 0.25 s tasks a second.
        if not self.can_offer or self.ws is None:
            return
        now = time.time()
        for oid in [o for o, (_, until) in self.offers.items() if until < now]:
            del self.offers[oid]
        for ttype in POOLS:
            busy = sum(1 for s in self.running.values() if s["type"] == ttype)
            open_ = sum(1 for (ty, _) in self.offers.values() if ty == ttype)
            for _ in range(SLOTS - busy - open_):
                oid = secrets.token_hex(8)
                valid = OFFER_VALIDITY_MS + random.randint(0, 500)
                self.offers[oid] = (ttype, now + valid / 1000 + 0.1)
                await self.send({"type": "runner:taskoffer", "offerId": oid,
                                 "taskType": ttype, "validFor": valid})

    async def on_accept(self, task_id: str, offer_id: str):
        offer = self.offers.pop(offer_id, None)
        if offer is None or offer[1] < time.time():
            await self.send({"type": "runner:taskrejected", "taskId": task_id,
                             "reason": "Offer expired - not accepted within validity window"})
            return
        self.running[task_id] = {"type": offer[0]}
        await self.send({"type": "runner:taskaccepted", "taskId": task_id})

    async def request_items(self, task_id: str) -> list:
        rid = secrets.token_hex(8)
        fut = asyncio.get_running_loop().create_future()
        self.data_waits[rid] = fut
        await self.send({"type": "runner:taskdatarequest", "taskId": task_id, "requestId": rid,
                         "requestParams": {"dataOfNodes": [], "env": False, "prevNode": False,
                                           "input": {"include": True}}})
        data = await asyncio.wait_for(fut, TIMEOUT)
        return ((data.get("inputData") or {}).get("main") or [[]])[0] or []

    async def digest_for(self, source: str) -> str:
        key = hashlib.sha256(source.encode()).hexdigest()
        d = self.digests.get(key)
        if d is None:
            d = (await self.zygo.put_script(source)).sha256
            self.digests[key] = d
        return d

    # -- one task --------------------------------------------------------------
    async def execute(self, task_id: str, ttype: str, s: dict):
        cof = bool(s.get("continueOnFail") or s.get("continue_on_fail"))
        t0 = time.perf_counter()
        t_data = t_zygo = t0
        try:
            mode = s["nodeMode"]
            if ttype == "python":
                event = {"mode": mode, "items": s.get("items") or [], "query": s.get("query")}
            else:
                if s.get("chunk"):
                    raise RuntimeError("Zygo runner: chunked per-item input is not implemented")
                if mode == "runCode":
                    raise RuntimeError("Zygo runner: runCode tasks (AI tools) are not implemented")
                items = await self.request_items(task_id)
                event = {"mode": mode, "items": items}
            t_data = time.perf_counter()
            digest = await self.digest_for(build_script(ttype, s["code"]))
            res = await self.zygo.run_script(POOLS[ttype], digest, event,
                                             timeout=TIMEOUT, key=f"n8n-{task_id}")
            t_zygo = time.perf_counter()
            for line in (res.stdout or "").splitlines()[:100]:
                await self.send({"type": "runner:rpc", "callId": secrets.token_hex(8),
                                 "taskId": task_id, "name": "logNodeOutput", "params": [line]})
            data = {"result": res.result}
            if ttype == "javascript":
                data.update(customData=None, staticData=None)
            await self.send({"type": "runner:taskdone", "taskId": task_id, "data": data})
            if log.isEnabledFor(logging.DEBUG):
                log.debug("task %s %s: data %.1f ms, zygo %.1f ms, reply %.1f ms", task_id, ttype,
                          (t_data - t0) * 1000, (t_zygo - t_data) * 1000, (time.perf_counter() - t_zygo) * 1000)
        except asyncio.CancelledError:
            await self.send({"type": "runner:taskerror", "taskId": task_id,
                             "error": {"message": "Task cancelled"}})
        except Exception as e:
            msg = self.describe(e)
            log.warning("task %s (%s) failed: %r", task_id, ttype, e)
            if cof:
                await self.send({"type": "runner:taskdone", "taskId": task_id,
                                 "data": {"result": [{"json": {"error": msg}}]}})
            else:
                await self.send({"type": "runner:taskerror", "taskId": task_id,
                                 "error": {"message": msg, "description": getattr(e, "detail", "")}})
        finally:
            self.running.pop(task_id, None)
            try:
                await self.send_offers()
            except Exception:
                pass

    @staticmethod
    def describe(e: Exception) -> str:
        if isinstance(e, Timeout):
            return f"Task execution timed out after {int(TIMEOUT)} seconds"
        if isinstance(e, Busy):
            return "Zygo pool is full (429); try again"
        if isinstance(e, HandlerError):
            text = str(e)
            # the traceback's last line is what n8n's own runners show
            last = [ln for ln in text.strip().splitlines() if ln.strip()]
            return last[-1] if last else text
        if isinstance(e, ZygoError):
            return f"Zygo: {e}"
        return str(e)


def main():
    if not AUTH:
        raise SystemExit("N8N_RUNNERS_AUTH_TOKEN is not set: it is the secret n8n's "
                         "broker shares with its runners (N8N_RUNNERS_AUTH_TOKEN on n8n)")
    logging.basicConfig(level=os.environ.get("LOG_LEVEL", "INFO"),
                        format="%(asctime)s %(levelname)s %(message)s")
    asyncio.run(Runner().run())


if __name__ == "__main__":
    main()
