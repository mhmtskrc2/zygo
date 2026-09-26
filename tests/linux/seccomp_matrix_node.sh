#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# The other half of the seccomp compatibility matrix: Node.
#
# A matrix built entirely out of CPython says nothing about Node, and the two
# reach the kernel very differently. libuv builds *every* stdio pipe and every
# IPC channel out of `socketpair()`, which is precisely the kind of call a
# profile written against CPython removes without noticing — `socketpair` was
# in `STRICT_REMOVED` until this ran, and a `strict` Node function could not
# start a single request worker (`spawn EPERM`, before any handler code).
#
# `worker_threads` is the case the open-source review named. A thread is a
# `clone` *with* `CLONE_THREAD`, which the agent's child filter permits, and a
# process is a `clone` without it, which it does not — so a threaded handler
# has to keep working under `strict` and a process-spawning one has to stop.
#
# A separate script from `seccomp_matrix.sh` because it is a separate image and
# a separate spec: `[defaults] requirements` applies to every function in a
# file, and a Node function has no use for a Python venv.
#
# Run:  make seccomp-matrix-linux
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/tests/linux/bin/zygo-linux-musl}
ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-matrix-node}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

say() { printf '%s\n' "$*"; }

say "seccomp compatibility matrix: Node"
say "  kernel $(uname -r)"

zygo_supervisor /tmp/supervisor-node.log
i=0
while [ $i -lt 100 ]; do
    "$ZYGO" supervisor status >/dev/null 2>&1 && break
    i=$((i+1)); sleep 0.1
done

mkdir -p /tmp/matrix-node && cd /tmp/matrix-node || exit 1
zygo pull node:22-slim >/dev/null 2>&1

cat > h_worker.js <<'JS'
const { Worker } = require('worker_threads');

module.exports = async function handler(event) {
  // A real worker thread, doing real work, and joined before the answer. A
  // thread is a `clone` with CLONE_THREAD, which the `strict` child filter
  // permits on purpose: what it removes is a new *process*.
  const answer = await new Promise((resolve, reject) => {
    const w = new Worker(
      "const { parentPort } = require('worker_threads');" +
        'let n = 0; for (let i = 0; i < 1e6; i++) n += i;' +
        'parentPort.postMessage(n);',
      { eval: true }
    );
    w.on('message', resolve);
    w.on('error', reject);
  });
  return { ok: answer === 499999500000, answer };
};
JS

cat > h_stdlib.js <<'JS'
const crypto = require('crypto');
const fs = require('fs');
const zlib = require('zlib');

module.exports = function handler(event) {
  // The three things a Node handler actually does: hash something, compress
  // something, write something. Each reaches the kernel a different way —
  // `getrandom`, a thread-pool task, and ordinary file I/O.
  const key = crypto.randomBytes(16).toString('hex');
  const packed = zlib.gzipSync(Buffer.alloc(4096, 7));
  fs.writeFileSync('/tmp/matrix.bin', packed);
  const back = zlib.gunzipSync(fs.readFileSync('/tmp/matrix.bin'));
  return { ok: back.length === 4096 && key.length === 32, gzipped: packed.length };
};
JS

cat > h_spawn.js <<'JS'
const { execFileSync } = require('child_process');

module.exports = function handler(event) {
  // The negative case, and the reason `strict` exists: a handler that starts
  // a program. Under `default` it works; under `strict` the child filter — or
  // Node's own permission model, where the agent has no way to install the
  // filter — refuses it. A cell that says "works" under strict is a bug.
  try {
    const out = execFileSync('/bin/echo', ['spawned'], { encoding: 'utf8' });
    return { ok: out.trim() === 'spawned', spawned: true };
  } catch (e) {
    return { ok: false, spawned: false, error: e.code || String(e) };
  }
};
JS

cat > sandbox.toml <<'TOML'
[defaults]
image = "node:22-slim"
mem = "512M"

[fn.worker_threads_default]
entry = "h_worker.js"
[fn.worker_threads_strict]
entry = "h_worker.js"
seccomp = "strict"

[fn.stdlib_default]
entry = "h_stdlib.js"
[fn.stdlib_strict]
entry = "h_stdlib.js"
seccomp = "strict"

[fn.spawn_default]
entry = "h_spawn.js"
[fn.spawn_strict]
entry = "h_spawn.js"
seccomp = "strict"

# The third column: the same code in a runtime pool, where the script arrives
# with the request and the child filter is `strict` by default.
[runtime.matrix]
agent = "node"
TOML

say ""
say "warming four Node zygotes…"
zygo up >/tmp/up-matrix-node.log 2>&1
grep -v "^$" /tmp/up-matrix-node.log | head -12

say ""
say "warming the runtime pool (no handler)…"
zygo serve --runtime matrix >>/tmp/up-matrix-node.log 2>&1 ||
    say "  the pool did not warm; its column will read FAILS"

say ""
printf '  %-16s  %-28s  %-28s  %-28s\n' what default strict "pool (strict)"
printf '  %-16s  %-28s  %-28s  %-28s\n' ---- ------- ------ -------------
for case in worker_threads stdlib spawn; do
    row="  $(printf '%-16s' "$case")"
    case $case in
        worker_threads) script=h_worker.js ;;
        *) script="h_${case}.js" ;;
    esac
    for profile in default strict pool; do
        if [ "$profile" = pool ]; then
            out=$(zygo exec --runtime matrix --script "$script" '{}' 2>&1 | tr -d '\n')
        else
            out=$(zygo exec "${case}_${profile}" '{}' 2>&1 | tr -d '\n')
        fi
        case "$out" in
            *'"ok": true'* | *'"ok":true'*) cell="works" ;;
            *'"spawned": false'* | *'"spawned":false'*) cell="refused (as intended)" ;;
            *) cell="FAILS: $(printf '%s' "$out" | cut -c1-20)" ;;
        esac
        row="$row  $(printf '%-28s' "$cell")"
    done
    say "$row"
done

say ""
say 'spawn is expected to be refused under strict and to work under default;'
say 'anything else in that row is the finding. worker_threads must work under'
say 'both: a thread is a clone *with* CLONE_THREAD and the child filter permits'
say 'it. And socketpair is deliberately not removed by strict — it reaches'
say 'nothing, and libuv builds every Node pipe out of one. See STRICT_REMOVED'
say 'in crates/zygo-core/src/backend/ns/seccomp.rs.'
harness_verdict
zygo down >/dev/null 2>&1
"$ZYGO" stop --all >/dev/null 2>&1
