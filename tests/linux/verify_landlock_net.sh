#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Landlock's network rules, run for real.
#
# `crates/zygo-core/src/backend/ns/landlock.rs` builds `bind` and `connect`
# rules for ABI v4 (kernel 6.7), and until this script existed nothing ever ran
# them: the unit tests check the ruleset that is *built*, the supervisor suite
# checks one off-list connect and cannot say which mechanism refused it, and no
# development machine here has a kernel new enough to enforce either.
#
# The difficulty is separating Landlock from nftables. Both refuse, and both
# can refuse with a `PermissionError`, so an errno alone proves nothing. Two
# probes separate them completely:
#
#   * **`bind`** — nftables filters the *output* hook. It cannot refuse a
#     `bind()`, because binding sends no packet. A refused `bind` is Landlock
#     or it is nothing. Since ADR 0008 it is refused only where the namespace
#     is shared between tenants — a runtime pool; a function may listen on its
#     own loopback, where nothing from outside can reach it.
#   * **a connect on loopback** — the first line of Zygo's nftables ruleset is
#     `oifname "lo" accept`, so every loopback connection is permitted by the
#     filter. Landlock's rules are by port and apply to loopback too, so a
#     loopback connect to a port the allowlist does not name is refused by
#     Landlock and by nothing else.
#
# And each has its positive path proved first, because "refused" and "nothing
# happened" look identical from outside:
#
#   * a unix socket binds, so the sandbox can bind at all;
#   * a loopback connect on an *allowed* port gets past Landlock and fails the
#     way an ordinary connection to a closed port fails.
#
# Run:  make landlock-net-linux
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/tests/linux/bin/zygo-linux-musl}
PASS=0
FAIL=0

ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-landlock}
export ZYGO_DATA_HOME

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

say() { printf '%s\n' "$*"; }
ok()  { PASS=$((PASS+1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL+1)); say "  FAIL  $*"; }
skip() { say "  SKIP  $*"; }

# One JSON string field, empty when it is not there.
field() { printf '%s' "$1" | sed -n "s/.*\"$2\":\"\([^\"]*\)\".*/\1/p"; }

say "Landlock network rules"
say "  kernel $(uname -r)"

abi=$("$ZYGO" doctor 2>/dev/null | sed -n 's/.*landlock.*ABI v\([0-9]*\).*/\1/p' | head -1)
abi=${abi:-0}
say "  landlock ABI v$abi"

if [ "$abi" -lt 4 ]; then
    skip "this kernel's Landlock is ABI v$abi; the network rules arrived in v4 (6.7)"
    say ""
    say "----------------------------------------"
    say "landlock network: nothing to run on this kernel"
    harness_verdict
    exit 0
fi

for tool in pasta nft; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        bad "\`$tool\` is not installed, so an egress sandbox cannot start at all"
        say ""
        say "landlock network: $PASS passed, $FAIL failed"
        exit 1
    fi
done

work=/tmp/landlock-net
rm -rf "$work" && mkdir -p "$work" && cd "$work" || exit 1

cat > probe.py <<'PY'
import errno
import os
import socket
import tempfile


def named(exc):
    """The errno's name, which is what distinguishes the two mechanisms."""
    number = getattr(exc, "errno", None)
    return errno.errorcode.get(number, type(exc).__name__) if number else type(exc).__name__


def handler(event):
    what = event["do"]

    if what == "bind_unix":
        # The control for `bind_tcp`: a bind that Landlock's network rules do
        # not cover, so a failure here means the sandbox cannot bind anything
        # and the TCP result below would prove nothing.
        path = os.path.join(tempfile.mkdtemp(), "s.sock")
        s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            s.bind(path)
            return {"result": "ok"}
        except OSError as e:
            return {"result": named(e)}
        finally:
            s.close()

    if what == "bind_tcp":
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            s.bind(("127.0.0.1", 0))
            return {"result": "ok", "port": s.getsockname()[1]}
        except OSError as e:
            return {"result": named(e)}
        finally:
            s.close()

    if what == "listen_self":
        # Bind, listen, and connect to it from the same process: what a
        # program that talks to itself over TCP does. Under `egress` the
        # connect is subject to the allowlist's port rule like any other.
        srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        try:
            srv.bind(("127.0.0.1", 0))
            srv.listen(1)
            port = srv.getsockname()[1]
            cli = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            cli.settimeout(4)
            try:
                cli.connect(("127.0.0.1", port))
                return {"result": "ok", "port": port}
            except OSError as e:
                return {"result": "connect:" + named(e), "port": port}
            finally:
                cli.close()
        except OSError as e:
            return {"result": "bind:" + named(e)}
        finally:
            srv.close()

    if what == "connect":
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(float(event.get("timeout", 4)))
        try:
            s.connect((event["host"], int(event["port"])))
            return {"result": "ok"}
        except OSError as e:
            return {"result": named(e)}
        finally:
            s.close()

    return {"result": "unknown probe"}
PY

cat > sandbox.toml <<'TOML'
[defaults]
image = "python:3.12-slim"
entry = "probe.py"

# One port on the allowlist, so Landlock's rule set is `{443, 53}` — 53 is
# added for DNS over TCP to the forced resolver — and everything else is off
# the list.
[fn.limited]
network = "egress"
allow = ["1.1.1.1:443"]

# No network at all. For a *function* Landlock adds nothing here: the empty
# namespace is the boundary, and the function may talk to itself over TCP.
[fn.sealed]
network = "none"
TOML

# Through the harness wrapper, and with a supervisor of its own: a `zygo`
# started straight from this shell lands in a cgroup with no delegated
# controllers, and every function then fails to start for a reason that has
# nothing to do with Landlock.
zygo pull python:3.12-slim >/dev/null 2>&1
zygo_supervisor /tmp/landlock-supervisor.log
i=0
while [ $i -lt 100 ]; do
    "$ZYGO" supervisor status >/dev/null 2>&1 && break
    i=$((i+1)); sleep 0.1
done
if ! zygo up >/tmp/landlock-up.log 2>&1; then
    bad "the sandboxes did not come up: $(tail -3 /tmp/landlock-up.log | tr '\n' ' ' | cut -c1-200)"
    say ""
    say "landlock network: $PASS passed, $FAIL failed"
    exit 1
fi

probe() {
    out=$(zygo exec "$1" "$2" 2>&1 | tr -d '\n ')
    field "$out" result
}

say ""
say "under network = \"egress\", allow = [\"1.1.1.1:443\"]"

# --- the positive paths, first ----------------------------------------------

r=$(probe limited '{"do": "bind_unix"}')
[ "$r" = ok ] \
    && ok "a unix socket binds, so the sandbox can bind at all" \
    || bad "even a unix socket could not bind ($r); nothing below would mean anything"

# Landlock permits 443, and nothing is listening on loopback there, so the
# connection is refused by the *kernel's TCP stack* rather than by a policy.
# That is the proof that a permitted port really does get through.
r=$(probe limited '{"do": "connect", "host": "127.0.0.1", "port": 443}')
case "$r" in
    ECONNREFUSED|ok)
        ok "a loopback connect on an allowed port gets past Landlock ($r)" ;;
    EACCES|EPERM)
        bad "an *allowed* port was refused with $r — the rule set names 443 and should permit it" ;;
    *)
        bad "a loopback connect on an allowed port answered $r, which is neither of the two expected outcomes" ;;
esac

# --- what has to be refused, and by Landlock --------------------------------

# A function's namespace is one tenant's, and pasta forwards no port into it,
# so a TCP listener there is the function's own business (ADR 0008).
r=$(probe limited '{"do": "bind_tcp"}')
case "$r" in
    ok) ok "a function may bind a TCP port on its own loopback" ;;
    *)  bad "a function's bind() answered $r; only a shared namespace refuses it" ;;
esac

# Talking to itself is still subject to the allowlist's *port* rule, which
# Landlock applies to loopback too: the listener's random port is not on the
# list, so the connect half is refused. A function that needs this names the
# port it listens on in `allow`, with `--allow-private-net`.
r=$(probe limited '{"do": "listen_self"}')
case "$r" in
    connect:EACCES)
        ok "under egress, connecting to its own listener is refused on a port off the list (connect:EACCES)" ;;
    bind:*)
        bad "the listener could not be set up: $r" ;;
    *)
        bad "a self-connect under egress answered $r, not connect:EACCES" ;;
esac

# The first nftables rule is `oifname "lo" accept`, so the filter permits this
# connection. Landlock's port rules apply to loopback too, and 9 is not on the
# list. A refusal here is Landlock's alone.
r=$(probe limited '{"do": "connect", "host": "127.0.0.1", "port": 9}')
case "$r" in
    EACCES)
        ok "connect() to a port off the list is refused by Landlock with EACCES, on loopback, which nftables accepts" ;;
    ok|ECONNREFUSED)
        bad "a loopback connect to an off-list port reached the TCP stack ($r); Landlock's connect rule did not apply" ;;
    *)
        bad "the off-list connect answered $r, not EACCES" ;;
esac

say ""
say "under network = \"none\""

# A sealed function is bounded by its namespace, which has only loopback in
# it. Landlock handles nothing here, so a program that talks to itself over
# TCP works, and there is nobody else to reach.
r=$(probe sealed '{"do": "listen_self"}')
[ "$r" = ok ] \
    && ok "a sealed function listens on its own loopback and connects to itself" \
    || bad "a sealed function's listen-and-connect answered $r, not ok"

r=$(probe sealed '{"do": "connect", "host": "127.0.0.1", "port": 443}')
[ "$r" = ECONNREFUSED ] \
    && ok "a loopback connect to a closed port is the TCP stack's ECONNREFUSED, not a policy's" \
    || bad "a sealed function answered $r to a closed loopback port, not ECONNREFUSED"

# Anything that is not loopback has no route: the namespace has no other
# interface. That, not a rule, is what seals it.
r=$(probe sealed '{"do": "connect", "host": "1.1.1.1", "port": 443}')
[ "$r" = ENETUNREACH ] \
    && ok "an address off loopback is ENETUNREACH: the empty namespace is the boundary" \
    || bad "a sealed function answered $r to 1.1.1.1:443, not ENETUNREACH"

say ""
say "in a runtime pool, whose namespace tenants share"
# The one place `bind` is still refused. The pool is served with
# `seccomp = "default"` on purpose: `strict`, the pool default, removes the
# socket calls outright (EPERM from seccomp), and this script is about the
# layer underneath it.
# Served from a directory with an empty spec of its own: the spec above puts
# `entry` in `[defaults]`, a pool may not carry an entry, and the spec file is
# searched for upwards from the working directory.
mkdir -p "$work/pool" && : > "$work/pool/sandbox.toml"
if (cd "$work/pool" && zygo serve --runtime shared --image python:3.12-slim --agent python --seccomp default) \
        >/tmp/landlock-pool.log 2>&1; then
    r=$(zygo exec --runtime shared --script "$work/probe.py" '{"do": "bind_tcp"}' 2>&1 | tr -d '\n ')
    r=$(field "$r" result)
    [ "$r" = EACCES ] \
        && ok "a pooled script's bind() is refused by Landlock with EACCES: a listener there would be a channel between tenants" \
        || bad "a pooled script answered $r to bind(), not EACCES"
    r=$(zygo exec --runtime shared --script "$work/probe.py" '{"do": "bind_unix"}' 2>&1 | tr -d '\n ')
    r=$(field "$r" result)
    [ "$r" = ok ] \
        && ok "and the same script binds a unix socket, so it is the TCP rule that refused" \
        || bad "a pooled script could not bind a unix socket ($r); the TCP refusal above proves nothing"
else
    bad "the pool did not come up: $(tail -2 /tmp/landlock-pool.log | tr '\n' ' ' | cut -c1-200)"
fi

zygo down >/dev/null 2>&1
zygo stop --all >/dev/null 2>&1

say ""
say "----------------------------------------"
say "landlock network: $PASS passed, $FAIL failed"
harness_verdict
[ "$FAIL" -eq 0 ]
