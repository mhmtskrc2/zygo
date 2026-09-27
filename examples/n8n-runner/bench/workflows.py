#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""The workflows the comparison runs: Webhook -> [Split Out] -> Code, answering
with the last node's first item. One file per workflow, ids fixed so a
re-import replaces rather than duplicates.

    python3 workflows.py OUT_DIR HOST_IP GATEWAY_IP

HOST_IP and GATEWAY_IP are where the probes try to reach this machine and
its router from inside a Code node.
"""
import json
import os
import sys
import uuid

JS = {
    "trivial": "return [{json: {ok: true, n: $input.all().length}}];",
    "cpu": "let s = 0;\nfor (let i = 0; i < 3000000; i++) s += i * i;\nreturn [{json: {s}}];",
    "items": ("return $input.all().map((it, i) => ({json: {id: it.json.id, "
              "name: String(it.json.name).toUpperCase(), total: it.json.qty * it.json.price, "
              "tag: 'r' + i}}));"),
    "deps": ("const c = require('crypto');\n"
             "return [{json: {h: c.createHash('sha256').update(JSON.stringify($input.first().json.body)).digest('hex')}}];"),
}
PY = {
    "trivial": 'return [{"json": {"ok": True, "n": len(_items)}}]',
    "cpu": "s = sum(i * i for i in range(300000))\nreturn [{\"json\": {\"s\": s}}]",
    "items": ('return [{"json": {"id": it["json"]["id"], "name": str(it["json"]["name"]).upper(), '
              '"total": it["json"]["qty"] * it["json"]["price"], "tag": "r" + str(i)}} '
              'for i, it in enumerate(_items)]'),
    "deps": ("import hashlib\n"
             'return [{"json": {"h": hashlib.sha256(str(_items[0]["json"]["body"]).encode()).hexdigest()}}]'),
}

# Probes: what can a Code node reach? Each one is caught and reported, never fatal.
TARGETS = [
    ("n8n_by_name", "http://n8n:5678/healthz"),
    ("broker_by_name", "http://n8n:5679/healthz"),
    ("n8n_host_ip", "http://HOSTIP:5678/healthz"),
    ("broker_vm_loopback", "http://127.0.0.1:5679/healthz"),
    ("internet", "https://example.com/"),
    ("cloud_metadata", "http://169.254.169.254/"),
    ("lan_gateway", "http://GATEWAY/"),
]

JS_PROBE = """const out = {};
const t = async (k, f) => { try { out[k] = String(await f()).slice(0, 160); } catch (e) { out[k] = 'BLOCKED: ' + String((e && e.message) || e).slice(0, 120); } };
await t('env', () => Object.keys(process.env).filter(k => /N8N|TOKEN|KEY|SECRET/i.test(k)).join(',') || '(none of interest)');
await t('read_etc_passwd', () => require('fs').readFileSync('/etc/passwd', 'utf8').split('\\n').length + ' lines');
await t('read_proc1', () => require('fs').readFileSync('/proc/1/cmdline', 'utf8').replace(/\\0/g, ' '));
await t('list_root', () => require('fs').readdirSync('/').join(' '));
await t('spawn_id', () => require('child_process').execSync('id').toString().trim());
const get = (url) => new Promise((res, rej) => { const m = url.startsWith('https') ? require('https') : require('http'); const r = m.get(url, {timeout: 3000}, (x) => { res('REACHED: HTTP ' + x.statusCode); x.resume(); }); r.on('timeout', () => r.destroy(new Error('timeout'))); r.on('error', rej); });
TARGETS_JS
return [{json: out}];"""

PY_PROBE = """import os, subprocess, urllib.request
out = {}
def t(k, f):
    try:
        out[k] = str(f())[:160]
    except BaseException as e:
        out[k] = 'BLOCKED: ' + (type(e).__name__ + ': ' + str(e))[:120]
t('env', lambda: ','.join(k for k in os.environ if any(w in k.upper() for w in ('N8N', 'TOKEN', 'KEY', 'SECRET'))) or '(none of interest)')
t('read_etc_passwd', lambda: str(len(open('/etc/passwd').read().splitlines())) + ' lines')
t('read_proc1', lambda: open('/proc/1/cmdline').read().replace('\\0', ' '))
t('list_root', lambda: ' '.join(os.listdir('/')))
t('spawn_id', lambda: subprocess.run(['id'], capture_output=True, text=True).stdout.strip())
def get(url):
    return 'REACHED: HTTP ' + str(urllib.request.urlopen(url, timeout=3).status)
TARGETS_PY
return [{"json": out}]"""

JS_HOSTILE = {
    "mem": "const a = [];\nfor (let i = 0; i < 40; i++) a.push(Buffer.alloc(50 * 1024 * 1024, 1));\nreturn [{json: {allocated_mb: a.length * 50}}];",
    "loop": "while (true) {}",
    "disk": "const fs = require('fs'); const os = require('os');\nconst b = Buffer.alloc(16 * 1024 * 1024, 1); const p = (process.env.TMPDIR || os.tmpdir()) + '/fill';\nlet n = 0; try { for (; n < 64; n++) fs.appendFileSync(p, b); } catch (e) { return [{json: {written_mb: n * 16, error: String(e.message)}}]; }\nreturn [{json: {written_mb: n * 16}}];",
}
PY_HOSTILE = {
    "mem": "a = [b'x' * (50 * 1024 * 1024) for _ in range(40)]\nreturn [{\"json\": {\"allocated_mb\": len(a) * 50}}]",
    "loop": "while True:\n    pass",
    "disk": "import os, tempfile\nb = b'x' * (16 * 1024 * 1024)\nn = 0\ntry:\n    with open(os.path.join(tempfile.gettempdir(), 'fill'), 'ab') as f:\n        for n in range(1, 65):\n            f.write(b); f.flush()\nexcept BaseException as e:\n    return [{\"json\": {\"written_mb\": (n - 1) * 16, \"error\": str(e)[:120]}}]\nreturn [{\"json\": {\"written_mb\": n * 16}}]",
}


def wid(name):
    return uuid.uuid5(uuid.NAMESPACE_URL, "n8n-bench/" + name).hex[:16]


def workflow(name, code=None, lang=None, split=False):
    nodes = [{
        "id": str(uuid.uuid5(uuid.NAMESPACE_URL, name + "/hook")),
        "name": "Webhook", "type": "n8n-nodes-base.webhook", "typeVersion": 2,
        "position": [0, 0], "webhookId": str(uuid.uuid5(uuid.NAMESPACE_URL, name + "/wh")),
        "parameters": {"httpMethod": "POST", "path": name, "responseMode": "lastNode", "options": {}},
    }]
    conns = {}
    last = "Webhook"
    if split:
        nodes.append({"id": str(uuid.uuid5(uuid.NAMESPACE_URL, name + "/split")), "name": "Split Out",
                      "type": "n8n-nodes-base.splitOut", "typeVersion": 1, "position": [200, 0],
                      "parameters": {"fieldToSplitOut": "body.rows", "options": {}}})
        conns[last] = {"main": [[{"node": "Split Out", "type": "main", "index": 0}]]}
        last = "Split Out"
    if code is None:
        nodes.append({"id": str(uuid.uuid5(uuid.NAMESPACE_URL, name + "/noop")), "name": "No Op",
                      "type": "n8n-nodes-base.noOp", "typeVersion": 1, "position": [400, 0], "parameters": {}})
        conns[last] = {"main": [[{"node": "No Op", "type": "main", "index": 0}]]}
    else:
        params = {"mode": "runOnceForAllItems"}
        if lang == "js":
            params.update(language="javaScript", jsCode=code)
        else:
            params.update(language="pythonNative", pythonCode=code)
        nodes.append({"id": str(uuid.uuid5(uuid.NAMESPACE_URL, name + "/code")), "name": "Code",
                      "type": "n8n-nodes-base.code", "typeVersion": 2, "position": [400, 0],
                      "parameters": params})
        conns[last] = {"main": [[{"node": "Code", "type": "main", "index": 0}]]}
    return {"id": wid(name), "name": name, "nodes": nodes, "connections": conns,
            "settings": {"executionOrder": "v1"}, "active": False, "pinData": {}}


SLOW = {
    "slow-js": ("await new Promise(r => setTimeout(r, 3000));\nreturn [{json: {slept: true}}];", "js"),
    "slow-py": ("import time\ntime.sleep(3)\nreturn [{\"json\": {\"slept\": True}}]", "py"),
}


def main(out, host_ip, gateway):
    def fill(u):
        return u.replace("HOSTIP", host_ip).replace("GATEWAY", gateway)
    js_targets = "\n".join(f"await t('net_{k}', () => get('{fill(u)}'));" for k, u in TARGETS)
    py_targets = "\n".join(f"t('net_{k}', lambda: get('{fill(u)}'))" for k, u in TARGETS)
    flows = [workflow("noop")]
    for w, code in JS.items():
        flows.append(workflow(f"{w}-js", code, "js", split=(w == "items")))
    for w, code in PY.items():
        flows.append(workflow(f"{w}-py", code, "py", split=(w == "items")))
    flows.append(workflow("probe-js", JS_PROBE.replace("TARGETS_JS", js_targets), "js"))
    flows.append(workflow("probe-py", PY_PROBE.replace("TARGETS_PY", py_targets), "py"))
    for w, code in JS_HOSTILE.items():
        flows.append(workflow(f"{w}-js", code, "js"))
    for w, code in PY_HOSTILE.items():
        flows.append(workflow(f"{w}-py", code, "py"))
    for name, (code, lang) in SLOW.items():
        flows.append(workflow(name, code, lang))
    os.makedirs(out, exist_ok=True)
    for f in flows:
        with open(os.path.join(out, f["name"] + ".json"), "w") as fh:
            json.dump(f, fh, indent=1)
    print(len(flows), "workflows in", out)


if __name__ == "__main__":
    main(*sys.argv[1:4])
