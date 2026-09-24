#!/usr/bin/env python3
"""Fork-safety sweep: popular PyPI packages imported in a zygote, then forked.

For each package the harness serves a handler that imports it at module
level — so the import happens once, in the zygote — and then calls it from
many forked children at once. What it looks for is what a fork gets wrong and
a fresh process does not:

* **hangs** — a lock some import-time thread held, copied locked into every
  child;
* **wrong answers** — the same computation giving different results in
  different children;
* **shared randomness** — a generator seeded in the zygote handing every child
  the same "random" numbers;
* **leaked state** — request n seeing what request n-1 wrote to a module;
* **the spawn fallback** — a package whose import starts threads, which makes
  the agent give up forking; not a bug, but a 10–20x cliff worth knowing about;
* **cost** — the zygote's resident size and the per-request latency.

Run:  make fork-sweep-linux            (all packages)
      PACKAGES=numpy,pandas make fork-sweep-linux
"""

import json
import os
import statistics
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
from concurrent.futures import ThreadPoolExecutor

ZYGO = os.environ.get("ZYGO", "/src/poc/zygo-linux-musl")
IMAGE = os.environ.get("IMAGE", "python:3.12-slim")
REQUESTS = int(os.environ.get("REQUESTS", "48"))
PARALLEL = int(os.environ.get("PARALLEL", "8"))
EXEC_TIMEOUT = float(os.environ.get("EXEC_TIMEOUT", "60"))

# name -> (requirement line, body). The body runs inside `handler(event)`
# with the package already imported at module level by `imports`, and must
# set `out` to something JSON-serialisable and deterministic. `rand`, when
# present, is an expression that should differ between children.
PACKAGES = {
    "stdlib": dict(req="", imports="import json, sqlite3, hashlib, ssl, asyncio, logging, concurrent.futures",
                   body="""
db = sqlite3.connect(':memory:'); db.execute('create table t(x)'); db.execute('insert into t values (41)')
async def f(): return db.execute('select x+1 from t').fetchone()[0]
with concurrent.futures.ThreadPoolExecutor(4) as ex: h = list(ex.map(lambda s: hashlib.sha256(s.encode()).hexdigest()[:8], ['a','b']))
out = [asyncio.run(f()), h, ssl.create_default_context().protocol.name]
""", rand="__import__('random').random()"),
    "numpy": dict(req="numpy", imports="import numpy as np",
                  body="a = np.arange(40000, dtype=float).reshape(200, 200); out = float((a @ a.T).trace())",
                  rand="float(np.random.random())"),
    "numpy-rng": dict(req="numpy", imports="import numpy as np\nRNG = np.random.default_rng()",
                      body="out = 1", rand="float(RNG.random())"),
    "pandas": dict(req="pandas", imports="import pandas as pd",
                   body="df = pd.DataFrame({'a': range(1000), 'b': [i % 7 for i in range(1000)]}); out = int(df.groupby('b').a.sum().sum())"),
    "scipy": dict(req="scipy", imports="import scipy.linalg, numpy as np",
                  body="out = round(float(scipy.linalg.det(np.eye(50) * 2) ** (1/50)), 6)"),
    "scikit-learn": dict(req="scikit-learn", imports="from sklearn.linear_model import LinearRegression\nimport numpy as np",
                         body="X = np.arange(100).reshape(-1, 1); out = round(float(LinearRegression(n_jobs=2).fit(X, 3*X.ravel()+1).coef_[0]), 6)"),
    "pillow": dict(req="pillow", imports="from PIL import Image\nimport io",
                   body="im = Image.new('RGB', (300, 300), 'red').resize((64, 64)); b = io.BytesIO(); im.save(b, 'PNG'); out = len(b.getvalue()) > 0"),
    "requests": dict(req="requests", imports="import requests",
                     body="s = requests.Session(); out = requests.Request('GET', 'http://x/?a=1').prepare().url"),
    "httpx": dict(req="httpx", imports="import httpx",
                  body="out = str(httpx.URL('http://x/y', params={'a': 1}))"),
    "aiohttp": dict(req="aiohttp", imports="import aiohttp, asyncio",
                    body="""
async def f():
    async with aiohttp.ClientSession() as s: return str(s.connector.limit)
out = asyncio.run(f())
"""),
    "pydantic": dict(req="pydantic", imports="from pydantic import BaseModel",
                     body="class M(BaseModel):\n    a: int\n    b: str = 'x'\nout = M(a='5').model_dump()"),
    "sqlalchemy": dict(req="sqlalchemy", imports="import sqlalchemy as sa\nENGINE = sa.create_engine('sqlite://')",
                       body="""
with ENGINE.connect() as c: out = c.execute(sa.text('select 1+1')).scalar()
"""),
    "psycopg": dict(req="psycopg[binary]", imports="import psycopg",
                    body="out = psycopg.sql.Identifier('a').as_string(None) if hasattr(psycopg, 'sql') else 1"),
    "redis": dict(req="redis", imports="import redis\nR = redis.Redis(host='127.0.0.1')",
                  body="out = R.connection_pool.max_connections > 0"),
    "boto3": dict(req="boto3", imports="import boto3",
                  body="out = boto3.session.Session(region_name='eu-west-1', aws_access_key_id='x', aws_secret_access_key='y').client('s3').meta.region_name"),
    "grpcio": dict(req="grpcio", imports="import grpc",
                   body="ch = grpc.insecure_channel('127.0.0.1:1'); ch.close(); out = 1"),
    "protobuf": dict(req="protobuf", imports="from google.protobuf import struct_pb2, json_format",
                     body="s = struct_pb2.Struct(); s.update({'a': 1}); out = json_format.MessageToDict(s)"),
    "openai": dict(req="openai", imports="import openai",
                   body="out = type(openai.OpenAI(api_key='x', base_url='http://127.0.0.1:1')).__name__"),
    "anthropic": dict(req="anthropic", imports="import anthropic",
                      body="out = type(anthropic.Anthropic(api_key='x', base_url='http://127.0.0.1:1')).__name__"),
    "lxml": dict(req="lxml", imports="from lxml import etree",
                 body="out = etree.fromstring('<a><b>1</b><b>2</b></a>').xpath('sum(//b)')"),
    "beautifulsoup4": dict(req="beautifulsoup4", imports="from bs4 import BeautifulSoup",
                           body="out = [t.text for t in BeautifulSoup('<p>a</p><p>b</p>', 'html.parser').find_all('p')]"),
    "pyyaml": dict(req="pyyaml", imports="import yaml", body="out = yaml.safe_load('a: [1, 2]')"),
    "orjson": dict(req="orjson", imports="import orjson", body="out = orjson.dumps({'a': [1, 2]}).decode()"),
    "cryptography": dict(req="cryptography", imports="from cryptography.fernet import Fernet\nKEY = Fernet.generate_key()",
                         body="f = Fernet(KEY); out = f.decrypt(f.encrypt(b'hi')).decode()",
                         rand="Fernet.generate_key().decode()"),
    "pyjwt": dict(req="pyjwt", imports="import jwt", body="out = jwt.decode(jwt.encode({'a': 1}, 'k' * 32, 'HS256'), 'k' * 32, ['HS256'])"),
    "bcrypt": dict(req="bcrypt", imports="import bcrypt",
                   body="h = bcrypt.hashpw(b'pw', bcrypt.gensalt(4)); out = bcrypt.checkpw(b'pw', h)",
                   rand="bcrypt.gensalt(4).decode()"),
    "matplotlib": dict(req="matplotlib", imports="import matplotlib\nmatplotlib.use('Agg')\nimport matplotlib.pyplot as plt\nimport io",
                       body="fig, ax = plt.subplots(); ax.plot([1, 2, 3]); b = io.BytesIO(); fig.savefig(b, format='png'); plt.close(fig); out = len(b.getvalue()) > 1000"),
    "jinja2": dict(req="jinja2", imports="import jinja2\nENV = jinja2.Environment()",
                   body="out = ENV.from_string('{{ a|upper }}').render(a='x')"),
    "dateutil": dict(req="python-dateutil", imports="from dateutil import parser", body="out = parser.parse('2026-09-24T10:00Z').isoformat()"),
    "polars": dict(req="polars", imports="import polars as pl",
                   body="out = pl.DataFrame({'a': list(range(1000)), 'b': [i % 7 for i in range(1000)]}).group_by('b').agg(pl.col('a').sum())['a'].sum()"),
    "pyarrow": dict(req="pyarrow", imports="import pyarrow as pa, pyarrow.compute as pc",
                    body="out = pc.sum(pa.array(range(100000))).as_py()"),
    "duckdb": dict(req="duckdb", imports="import duckdb",
                   body="out = duckdb.connect().execute('select sum(range) from range(100000)').fetchone()[0]"),
    "duckdb-shared": dict(req="duckdb", imports="import duckdb\nCON = duckdb.connect()\nCON.execute('select 1').fetchall()",
                          body="out = CON.execute('select sum(range) from range(100000)').fetchone()[0]"),
    "opencv": dict(req="opencv-python-headless", imports="import cv2, numpy as np",
                   body="out = int(cv2.GaussianBlur(np.full((256, 256), 100, np.uint8), (5, 5), 0).sum())"),
    "tiktoken": dict(req="tiktoken", imports="import tiktoken", body="out = 1"),
    "regex": dict(req="regex", imports="import regex", body="out = regex.findall(r'\\p{L}+', 'ab 12 çd')"),
    "rich": dict(req="rich", imports="from rich.console import Console\nimport io",
                 body="c = Console(file=io.StringIO(), width=40); c.print('[bold]x'); out = c.file.getvalue()"),
    "loguru": dict(req="loguru", imports="from loguru import logger\nimport io\nBUF = io.StringIO()\nlogger.remove(); logger.add(BUF, enqueue=False, format='{message}')",
                   body="logger.info('hi'); out = 'hi' in BUF.getvalue()"),
    "loguru-enqueue": dict(req="loguru", imports="from loguru import logger\nimport sys\nlogger.remove(); logger.add(sys.stderr, enqueue=True)",
                           body="logger.info('hi'); logger.complete(); out = 1"),
    "sentry-sdk": dict(req="sentry-sdk", imports="import sentry_sdk\nsentry_sdk.init(dsn='http://k@127.0.0.1:1/1', transport=lambda e: None)",
                       body="sentry_sdk.capture_message('x'); out = 1"),
    "uvloop": dict(req="uvloop", imports="import uvloop, asyncio",
                   body="async def f(): return 7\nout = uvloop.run(f())"),
    "gevent": dict(req="gevent", imports="from gevent import monkey\nmonkey.patch_all()\nimport gevent",
                   body="out = sum(g.value for g in gevent.joinall([gevent.spawn(lambda i=i: i) for i in range(5)]))"),
    "numba": dict(req="numba", imports="import numba\n@numba.njit\ndef F(n):\n    s = 0\n    for i in range(n): s += i\n    return s\nF(10)",
                  body="out = F(1000)"),
    "torch": dict(req="--index-url https://download.pytorch.org/whl/cpu\ntorch", imports="import torch",
                  body="a = torch.arange(40000, dtype=torch.float64).reshape(200, 200); out = float((a @ a.T).trace())",
                  rand="float(torch.rand(1))", heavy=True),
}

HANDLER = """\
import os, json
{imports}

LEAK = []

def handler(event):
    LEAK.append(1)
{body}
    return {{"out": out, "leak": len(LEAK), "pid": os.getpid(), "rand": {rand}}}
"""


def sh(args, **kw):
    return subprocess.run(args, capture_output=True, text=True, **kw)


def rss_of(name):
    r = sh([ZYGO, "ps", "--json"])
    try:
        for f in json.loads(r.stdout)["functions"]:
            if f["name"] == name:
                return f.get("rss_kb"), f.get("imports_ms")
    except Exception:
        pass
    return None, None


def run_one(key, spec, work):
    name = "s-" + key.replace("_", "-")
    d = os.path.join(work, key)
    os.makedirs(d, exist_ok=True)
    body = textwrap.indent(textwrap.dedent(spec["body"]).strip(), "    ")
    with open(os.path.join(d, "handler.py"), "w") as f:
        f.write(HANDLER.format(imports=spec["imports"], body=body, rand=spec.get("rand", "None")))
    args = [ZYGO, "serve", os.path.join(d, "handler.py"), "--name", name,
            "--image", IMAGE, "--mem", spec.get("mem", "768M"), "--cpu", "2",
            "--pids", "256", "--timeout", "30s", "--concurrency", str(PARALLEL)]
    if spec["req"]:
        with open(os.path.join(d, "requirements.txt"), "w") as f:
            f.write(spec["req"] + "\n")
        args += ["--requirements", os.path.join(d, "requirements.txt")]
    res = {"package": key, "problems": []}
    t0 = time.monotonic()
    r = sh(args, timeout=900)
    res["serve_s"] = round(time.monotonic() - t0, 1)
    if r.returncode != 0:
        res["problems"].append("serve failed: " + (r.stderr.strip().splitlines() or ["?"])[-1][:300])
        res["serve_log"] = r.stderr[-3000:]
        return res
    res["rss_mb"], res["imports_ms"] = rss_of(name)
    if res["rss_mb"]:
        res["rss_mb"] = round(res["rss_mb"] / 1024, 1)

    def one(i):
        t = time.monotonic()
        try:
            p = sh([ZYGO, "exec", name, json.dumps({"i": i})], timeout=EXEC_TIMEOUT)
        except subprocess.TimeoutExpired:
            return i, None, "HANG (>%ds)" % EXEC_TIMEOUT, time.monotonic() - t
        dt = time.monotonic() - t
        if p.returncode != 0:
            return i, None, "rc=%d %s" % (p.returncode, (p.stderr.strip().splitlines() or [""])[-1][:200]), dt
        try:
            return i, json.loads(p.stdout), None, dt
        except ValueError:
            return i, None, "bad json: " + p.stdout[:200], dt

    first = one(-1)  # alone, before the storm
    with ThreadPoolExecutor(PARALLEL) as ex:
        results = [first] + list(ex.map(one, range(REQUESTS)))
    errs = [(i, e) for i, _, e, _ in results if e]
    oks = [o for _, o, e, _ in results if not e]
    lat = sorted(dt for _, _, e, dt in results if not e)
    if errs:
        uniq = {}
        for i, e in errs:
            uniq.setdefault(e, 0)
            uniq[e] += 1
        for e, n in uniq.items():
            res["problems"].append("%d/%d failed: %s" % (n, len(results), e))
    if oks:
        outs = {json.dumps(o["out"], sort_keys=True) for o in oks}
        if len(outs) > 1:
            res["problems"].append("children disagree: %s" % sorted(outs)[:3])
        res["out"] = json.loads(sorted(outs)[0])
        leaks = {o["leak"] for o in oks}
        if leaks != {1}:
            res["problems"].append("state leaked between requests: leak counts %s" % sorted(leaks))
        if spec.get("rand"):
            rands = [json.dumps(o["rand"]) for o in oks]
            if len(set(rands)) < len(rands):
                res["problems"].append("shared randomness: %d distinct values in %d children" % (len(set(rands)), len(rands)))
        pids = {o["pid"] for o in oks}
        res["distinct_pids"] = len(pids)
    if lat:
        res["p50_ms"] = round(statistics.median(lat) * 1000, 1)
        res["p99_ms"] = round(lat[min(len(lat) - 1, int(len(lat) * 0.99))] * 1000, 1)
    r = sh([ZYGO, "logs", "-n", "500", name])
    log = r.stdout + r.stderr
    if "falling back to spawn" in log:
        res["problems"].append("spawn fallback: import started threads")
    if "random generator created at import time" in log:
        res["generator_warning"] = True
    res["zygote_log"] = [l for l in log.splitlines() if " zygote " in l][:30]
    res["log_tail"] = log[-1500:]
    sh([ZYGO, "stop", name])
    return res


def main():
    want = os.environ.get("PACKAGES")
    keys = want.split(",") if want else [k for k, v in PACKAGES.items() if not v.get("heavy") or os.environ.get("HEAVY")]
    sh([ZYGO, "pull", IMAGE])
    work = tempfile.mkdtemp(prefix="sweep-")
    out = []
    for k in keys:
        try:
            r = run_one(k, PACKAGES[k], work)
        except Exception as e:  # the harness must not stop at one package
            r = {"package": k, "problems": ["harness: %r" % e]}
            sh([ZYGO, "stop", "s-" + k])
        out.append(r)
        flag = "OK  " if not r["problems"] else "BAD "
        print("%s %-16s serve %5ss  rss %6s MB  p50 %6s ms  p99 %6s ms  %s" % (
            flag, k, r.get("serve_s"), r.get("rss_mb"), r.get("p50_ms"), r.get("p99_ms"),
            "; ".join(r["problems"])), flush=True)
    path = os.environ.get("REPORT", "/tmp/fork_sweep.json")
    with open(path, "w") as f:
        json.dump(out, f, indent=1)
    print("report:", path)
    return 1 if any(r["problems"] for r in out) else 0


if __name__ == "__main__":
    sys.exit(main())
