#!/bin/sh
# `zygo login` against a real registry that really refuses people.
#
# Run:  make verify-login-linux
#
# The Makefile starts a `registry:2` with htpasswd authentication and runs this
# inside a container that *shares that registry's network namespace*, so the
# registry is at `127.0.0.1:5000` — which is one of the names Zygo treats as
# local and reaches over plain HTTP. Nothing here needs a sandbox, a cgroup or
# a kernel feature: it is the client half of the image store.
#
# What makes these checks worth writing is that every one of them is a thing
# that can be got wrong silently. A `login` that stores a password without
# trying it looks identical to one that works, until a `pull` fails hours
# later; a file written world-readable looks identical to one that is not.
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux}
REGISTRY=${REGISTRY:-127.0.0.1:5000}
USER_NAME=${USER_NAME:-zygotest}
PASSWORD=${PASSWORD:-s3cret}
PASS=0
FAIL=0

say() { printf '%s\n' "$*"; }
ok() { PASS=$((PASS + 1)); say "  PASS  $*"; }
bad() { FAIL=$((FAIL + 1)); say "  FAIL  $*"; }

ZYGO_DATA_HOME=/tmp/zdata-login
export ZYGO_DATA_HOME
rm -rf "$ZYGO_DATA_HOME"
AUTH=$ZYGO_DATA_HOME/auth.json

say "registry credentials"
say "  registry $REGISTRY, user $USER_NAME"
say ""

# ---------------------------------------------------------------------------
say "before logging in"

# The premise of everything below: this registry does refuse people. A suite
# that ran against an open registry would pass every check while proving
# nothing, which is how a "negative" test usually goes wrong.
out=$("$ZYGO" pull "$REGISTRY/nothing:latest" 2>&1)
case $out in
    *"authentication failed"*) ok "a pull is refused, and the error names authentication" ;;
    *) bad "the registry did not refuse an anonymous pull: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac
case $out in
    *"zygo login"*) ok "and the remedy is the command that fixes it" ;;
    *) bad "the error does not say to log in: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac

say ""
# ---------------------------------------------------------------------------
say "a password the registry does not accept"

out=$(printf %s "not-the-password" |
    "$ZYGO" login "$REGISTRY" --username "$USER_NAME" --password-stdin 2>&1)
rc=$?
[ "$rc" -ne 0 ] && ok "a wrong password fails (exit $rc) instead of being stored" ||
    bad "a wrong password was accepted: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)"
case $out in
    *401*) ok "and the message carries what the registry actually answered" ;;
    *) bad "the failure does not say what the registry said: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac
[ ! -e "$AUTH" ] && ok "nothing was written" ||
    bad "a rejected credential was stored anyway: $(cat "$AUTH" | tr '\n' ' ')"

say ""
# ---------------------------------------------------------------------------
say "the password it does accept"

out=$(printf %s "$PASSWORD" |
    "$ZYGO" login "$REGISTRY" --username "$USER_NAME" --password-stdin 2>&1)
rc=$?
[ "$rc" -eq 0 ] && ok "\`login\` succeeds and says where it put it" ||
    bad "login failed: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)"

if [ -f "$AUTH" ]; then
    mode=$(ls -l "$AUTH" | cut -c1-10)
    case $mode in
        -rw-------) ok "the file is readable by its owner alone ($mode)" ;;
        *) bad "the file holding a password is $mode" ;;
    esac
else
    bad "no file at $AUTH"
fi

# Docker's shape, so a person can read it, edit it, or delete a line without
# needing another subcommand.
if grep -q '"auths"' "$AUTH" 2>/dev/null && grep -q "$REGISTRY" "$AUTH" 2>/dev/null; then
    ok "it is Docker's shape, keyed by registry"
else
    bad "unexpected contents: $(tr '\n' ' ' <"$AUTH" 2>/dev/null | cut -c1-120)"
fi

# The password must not be sitting there in the clear *as a field*; base64 is
# not secrecy and is not claimed to be, but the file should be the same
# encoding everything else in the ecosystem reads.
if grep -q '"password"' "$AUTH" 2>/dev/null; then
    bad "the password is stored as its own field rather than in the \`auth\` entry"
else
    ok "stored in the \`auth\` entry, the way every other client reads it"
fi

say ""
# ---------------------------------------------------------------------------
say "and now a pull can authenticate"

# The whole point. A credential that is stored and not *used* is the failure
# this check exists for, and it is invisible from the login side.
out=$("$ZYGO" pull "$REGISTRY/nothing:latest" 2>&1)
case $out in
    *"authentication failed"*) bad "the stored credential was not used: still unauthenticated" ;;
    *"not found"*) ok "the pull authenticates and gets as far as \`not found\`, which is the truth" ;;
    *) bad "unexpected answer: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac

say ""
# ---------------------------------------------------------------------------
say "living beside other entries"

# A second login must not take the first one's line with it.
printf %s "$PASSWORD" | "$ZYGO" login "$REGISTRY" --username "$USER_NAME" --password-stdin >/dev/null 2>&1
python3 - "$AUTH" <<'PY' >/tmp/other-added 2>&1
import json
import sys

path = sys.argv[1]
cfg = json.load(open(path))
cfg["auths"]["example.invalid"] = {"auth": "dXNlcjpwYXNz"}
json.dump(cfg, open(path, "w"))
PY
printf %s "$PASSWORD" | "$ZYGO" login "$REGISTRY" --username "$USER_NAME" --password-stdin >/dev/null 2>&1
if grep -q "example.invalid" "$AUTH" 2>/dev/null; then
    ok "a second login leaves another registry's entry alone"
else
    bad "logging in again dropped the other registry: $(tr '\n' ' ' <"$AUTH" | cut -c1-160)"
fi

# Docker's file is read, not written. A user who has logged in with Docker
# should not have to again — and should not find Zygo has edited it.
DOCKER_CONFIG=/tmp/docker-cfg
export DOCKER_CONFIG
mkdir -p "$DOCKER_CONFIG"
printf '{"auths":{"%s":{"auth":"%s"}}}\n' "$REGISTRY" \
    "$(printf '%s:%s' "$USER_NAME" "$PASSWORD" | base64 | tr -d '\n')" \
    >"$DOCKER_CONFIG/config.json"
before=$(cat "$DOCKER_CONFIG/config.json")
rm -f "$AUTH"
out=$("$ZYGO" pull "$REGISTRY/nothing:latest" 2>&1)
case $out in
    *"not found"*) ok "a credential already in Docker's config.json is used as it is" ;;
    *) bad "Docker's credential was ignored: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac
printf %s "$PASSWORD" | "$ZYGO" login "$REGISTRY" --username "$USER_NAME" --password-stdin >/dev/null 2>&1
[ "$(cat "$DOCKER_CONFIG/config.json")" = "$before" ] &&
    ok "and \`login\` writes its own file, leaving Docker's untouched" ||
    bad "Docker's config.json was modified"
unset DOCKER_CONFIG

say ""
# ---------------------------------------------------------------------------
say "the password never comes from the command line"

# `--password` would put it in `ps` for every process on the machine and in
# the shell's history. The check is that the flag does not exist, which is
# the only way to make sure nobody adds it back for convenience.
if "$ZYGO" login --help 2>&1 | grep -q -- "--password "; then
    bad "there is a \`--password\` flag; a password in argv is visible in \`ps\`"
else
    ok "there is no \`--password\` flag, only \`--password-stdin\` and the terminal"
fi

# And with no terminal and no `--password-stdin`, it must say so rather than
# read a password that the caller is about to log.
out=$("$ZYGO" login "$REGISTRY" --username "$USER_NAME" </dev/null 2>&1)
case $out in
    *"not a terminal"*) ok "with no terminal it explains \`--password-stdin\` instead of reading blind" ;;
    *) bad "unexpected answer with no terminal: $(printf '%s' "$out" | tr '\n' ' ' | cut -c1-160)" ;;
esac

say ""
say "----------------------------------------"
say "registry credentials: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
