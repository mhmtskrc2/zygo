#!/bin/sh
# The `strict` seccomp profile against real packages.
#
# `strict` removes the socket family, `ptrace` and `mount` from the default
# allowlist, and the agent's forked child additionally loses `execve` and
# `fork`. Which popular packages that breaks is not something to reason about
# from a list of syscall names — a C extension may open a socket for a reason
# nobody would guess — so each one is imported and exercised under both
# profiles, in the same venv, and the difference is the matrix.
#
# Every cell is an attempt: the package is imported and made to do something
# representative, and the outcome is what the sandbox returned.
#
# Run:  make seccomp-matrix-linux
set -u

# Where the checkout is: `/src` inside the containers `make` starts, the
# workspace in CI. Everything below is relative to it.
SRC=${SRC:-/src}

ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
ZYGO_DATA_HOME=/tmp/zdata-matrix
export ZYGO_DATA_HOME

# Start from nothing. In a container the data directory is fresh because the
# container is; on a real host `/tmp` survives, and a previous run's images,
# venvs and derived layers are still there — which made this suite count two
# venvs for one requirements file and call it a bug in the cache. Assertions
# about what a cache contains only mean something when the cache started
# empty.
# Sourced below, but needed here: see `clear_data_home`.
. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

# The prelude is already sourced, above, because clearing the data
# directory needs one of its helpers before anything else happens.

say() { printf '%s\n' "$*"; }

say "seccomp compatibility matrix"
say "  kernel $(uname -r)"

zygo_supervisor /tmp/supervisor.log
i=0
while [ $i -lt 100 ]; do
    "$ZYGO" supervisor status >/dev/null 2>&1 && break
    i=$((i+1)); sleep 0.1
done

mkdir -p /tmp/matrix && cd /tmp/matrix || exit 1
"$ZYGO" pull python:3.12-slim >/dev/null 2>&1

cat > requirements.txt <<'REQ'
requests==2.32.3
httpx==0.27.2
pydantic==2.9.2
numpy==2.1.3
pandas==2.2.3
Pillow==11.0.0
REQ

# One handler per package, each doing the thing people use it for.
cat > h_requests.py <<'PY'
import requests


def handler(event):
    # No network in the sandbox: what is under test is the import and the
    # machinery up to the socket, which is where `strict` bites.
    s = requests.Session()
    req = requests.Request("GET", "http://example.invalid/").prepare()
    try:
        s.send(req, timeout=1)
        return {"ok": False, "why": "a request left a sealed sandbox"}
    except requests.exceptions.ConnectionError as e:
        return {"ok": True, "prepared": req.url, "error": type(e).__name__}
PY
cat > h_httpx.py <<'PY'
import httpx


def handler(event):
    # The other half of the HTTP client population, and a different shape:
    # httpx builds its transport — and therefore its socket — when the client
    # is constructed rather than when a request is sent, so `strict` bites in
    # a different place from requests.
    try:
        with httpx.Client(timeout=1.0) as client:
            client.get("http://example.invalid/")
        return {"ok": False, "why": "a request left a sealed sandbox"}
    except Exception as e:
        return {"ok": True, "error": type(e).__name__}
PY
cat > h_sqlite.py <<'PY'
import sqlite3


def handler(event):
    # On disk, not in memory: a file-backed database is the case that uses
    # `fcntl` locking, `fsync` and `ftruncate`, and an in-memory one would
    # exercise none of them. `/tmp` is the sandbox's own writable scratch.
    with sqlite3.connect("/tmp/matrix.db") as db:
        db.execute("CREATE TABLE IF NOT EXISTS t (k TEXT PRIMARY KEY, v INTEGER)")
        db.execute("INSERT OR REPLACE INTO t VALUES ('a', 1), ('b', 2)")
        db.commit()
        total = db.execute("SELECT sum(v) FROM t").fetchone()[0]
    return {"ok": total == 3, "sum": total, "version": sqlite3.sqlite_version}
PY
cat > h_pydantic.py <<'PY'
from pydantic import BaseModel, ValidationError


class Order(BaseModel):
    sku: str
    qty: int


def handler(event):
    try:
        Order(sku="x", qty="not a number")
    except ValidationError as e:
        return {"ok": True, "errors": len(e.errors()), "parsed": Order(sku="a", qty=2).model_dump()}
    return {"ok": False}
PY
cat > h_numpy.py <<'PY'
import numpy as np


def handler(event):
    a = np.arange(1_000_000, dtype=np.float64)
    return {"ok": True, "mean": float(a.mean()), "dot": float(a[:1000] @ a[:1000])}
PY
cat > h_pandas.py <<'PY'
import io

import pandas as pd


def handler(event):
    df = pd.read_csv(io.StringIO("a,b\n1,2\n3,4\n"))
    return {"ok": True, "sum": int(df["a"].sum()), "rows": len(df.groupby("b"))}
PY
cat > h_pillow.py <<'PY'
import io

from PIL import Image


def handler(event):
    img = Image.new("RGB", (64, 64), (200, 30, 30))
    img = img.resize((16, 16))
    out = io.BytesIO()
    img.save(out, "PNG")
    return {"ok": True, "bytes": len(out.getvalue()), "size": list(img.size)}
PY

cat > sandbox.toml <<'TOML'
[defaults]
image = "python:3.12-slim"
requirements = "requirements.txt"
mem = "512M"

[fn.requests_default]
entry = "h_requests.py"
[fn.requests_strict]
entry = "h_requests.py"
seccomp = "strict"

[fn.httpx_default]
entry = "h_httpx.py"
[fn.httpx_strict]
entry = "h_httpx.py"
seccomp = "strict"

[fn.sqlite3_default]
entry = "h_sqlite.py"
[fn.sqlite3_strict]
entry = "h_sqlite.py"
seccomp = "strict"

[fn.pydantic_default]
entry = "h_pydantic.py"
[fn.pydantic_strict]
entry = "h_pydantic.py"
seccomp = "strict"

[fn.numpy_default]
entry = "h_numpy.py"
[fn.numpy_strict]
entry = "h_numpy.py"
seccomp = "strict"

[fn.pandas_default]
entry = "h_pandas.py"
[fn.pandas_strict]
entry = "h_pandas.py"
seccomp = "strict"

[fn.pillow_default]
entry = "h_pillow.py"
[fn.pillow_strict]
entry = "h_pillow.py"
seccomp = "strict"

# The third column: the same packages, in a runtime pool, where the script
# arrives with the request and the child filter is `strict` by default. The
# same venv — it is keyed on the image digest and the requirements bytes, and
# both are the ones above.
[runtime.matrix]
agent = "python"
TOML

say ""
say "building the venv once (six packages, inside the image)…"
started=$(date +%s)
"$ZYGO" up >/tmp/up-matrix.log 2>&1
say "  up finished in $(( $(date +%s) - started )) s"
grep -v "^$" /tmp/up-matrix.log | head -14

say ""
say "warming the runtime pool (the same venv, no handler)…"
"$ZYGO" serve --runtime matrix >>/tmp/up-matrix.log 2>&1 ||
    say "  the pool did not warm; its column will read FAILS"

say ""
printf '  %-10s  %-28s  %-28s  %-28s\n' package default strict "pool (strict)"
printf '  %-10s  %-28s  %-28s  %-28s\n' ------- ------- ------ -------------
for pkg in requests httpx pydantic numpy pandas pillow sqlite3; do
    row="  $(printf '%-10s' "$pkg")"
    # The handler file is named after the module, except sqlite3's.
    case $pkg in
        sqlite3) script=h_sqlite.py ;;
        *) script="h_${pkg}.py" ;;
    esac
    for profile in default strict pool; do
        if [ "$profile" = pool ]; then
            # The same file, sent as a *script* rather than warmed as a
            # handler: one zygote, any package, `strict` by default.
            out=$("$ZYGO" exec --runtime matrix --script "$script" '{}' 2>&1 | tr -d '\n')
        else
            out=$("$ZYGO" exec "${pkg}_${profile}" '{}' 2>&1 | tr -d '\n')
        fi
        case "$out" in
            *'"ok": true'*) cell="works" ;;
            *)
                # The first line of the error, trimmed to the cell.
                cell="FAILS: $(printf '%s' "$out" | sed -n 's/.*\(Error[^"]*\).*/\1/p' | head -1 | cut -c1-20)"
                [ -z "$cell" ] || [ "$cell" = "FAILS: " ] && cell="FAILS: $(printf '%s' "$out" | cut -c1-20)"
                ;;
        esac
        row="$row  $(printf '%-28s' "$cell")"
    done
    say "$row"
done

say ""
say "strict removes: socket connect bind listen accept4 ptrace mount umount2"
say "and, in the forked child only: execve execveat fork vfork, and clone without CLONE_THREAD"
say "(every strict cell above ran its handler under that child filter; see docs/book/24-seccomp-profiles.md)"
say ""
say "the pool column is the same code as a *script*: one warm zygote holding no"
say "tenant code, the script written into the sandbox per request, and \`strict\`"
say "as the pool default rather than something the caller had to ask for."
harness_verdict
"$ZYGO" stop --all >/dev/null 2>&1
