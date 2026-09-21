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
TOML

say ""
say "building the venv once (five packages, inside the image)…"
started=$(date +%s)
"$ZYGO" up >/tmp/up-matrix.log 2>&1
say "  up finished in $(( $(date +%s) - started )) s"
grep -v "^$" /tmp/up-matrix.log | head -14

say ""
printf '  %-10s  %-28s  %-28s\n' package default strict
printf '  %-10s  %-28s  %-28s\n' ------- ------- ------
for pkg in requests pydantic numpy pandas pillow; do
    row="  $(printf '%-10s' "$pkg")"
    for profile in default strict; do
        name="${pkg}_${profile}"
        out=$("$ZYGO" exec "$name" '{}' 2>&1 | tr -d '\n')
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
say "strict removes: socket socketpair connect bind listen accept4 ptrace mount umount2"
say "and, in the forked child only: execve execveat fork vfork, and clone without CLONE_THREAD"
say "(every strict cell above ran its handler under that child filter; see docs/seccomp-profiles.md)"
harness_verdict
"$ZYGO" stop --all >/dev/null 2>&1
