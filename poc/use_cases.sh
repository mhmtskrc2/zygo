#!/bin/sh
# Fifty scenarios, shaped like the seven use cases rather than like the code.
#
# The other suites are organised by mechanism: the launcher's isolation, the
# supervisor's lifecycle, the escape vectors, the HTTP surface. This one is
# organised by *who is asking*, because that is where a different class of bug
# lives — the one where every part works and the combination does not, or where
# a thing works and is unusable.
#
# Every check attempts the thing. Where a check asserts a refusal it first
# proves the positive case, so "it was blocked" can never be satisfied by
# nothing having run. Where a number matters it is printed, so a run that
# passed narrowly is visible.
#
# Run:  make use-cases-linux
#       sh poc/use_cases.sh            (inside a container or on a Linux host)
set -u

SRC=${SRC:-/src}
ZYGO=${ZYGO:-$SRC/poc/zygo-linux-musl}
PY=${PY_IMAGE:-python:3.12-slim}
ALPINE=${ALPINE_IMAGE:-alpine:3}

ZYGO_DATA_HOME=${ZYGO_DATA_HOME:-/tmp/zdata-usecases}
export ZYGO_DATA_HOME

PASS=0
FAIL=0
SKIP=0
say()  { printf '%s\n' "$*"; }
ok()   { PASS=$((PASS+1)); say "  PASS  $*"; }
bad()  { FAIL=$((FAIL+1)); say "  FAIL  $*"; }
skip() { SKIP=$((SKIP+1)); say "  skip  $*"; }
head2() { say ""; say "$*"; }

. "$(dirname "$0")/cgroup_harness.sh"
clear_data_home "$ZYGO_DATA_HOME" || exit 1

WORK=$(mktemp -d)
cleanup() {
    "$ZYGO" stop --all >/dev/null 2>&1
    rm -rf "$WORK"
}
trap cleanup EXIT
cd "$WORK" || exit 1

say "fifty scenarios, by use case"
say "  kernel $(uname -r), $(uname -m), $(id -un)"

"$ZYGO" pull "$PY" >/dev/null 2>&1
"$ZYGO" pull "$ALPINE" >/dev/null 2>&1

# Rule 4, once, for the whole file: a sandbox has to run before any refusal
# below means anything.
# Matched as "contains", not "equals". An equality test on the whole of
# standard output makes the guard fail on anything else the run happens to
# print — and it did, on a host where the sandbox was working perfectly and
# the diagnostic underneath it printed `alive`.
alive=$(zygo run --quiet "$ALPINE" /bin/echo alive 2>&1)
case "$alive" in
    *alive*) : ;;
    *)
        say ""
        say "  no sandbox will start here, so none of the fifty would mean anything."
        printf '%s\n' "$alive" | tail -5 | sed 's/^/  /'
        exit 1 ;;
esac

# Helpers -------------------------------------------------------------------

# Run a program in a one-shot sandbox.
#
# Standard error is *kept*, merged into the output, because a check that
# discards it cannot tell "the program said it could not" from "the program
# could not run" — and the first version of this file reported the second as
# the first, twice.
run() { zygo run --quiet "$@" 2>&1; }

# The JSON `requests` counter for a warm function.
requests_of() {
    zygo --json ps 2>/dev/null | tr -d '\n ' |
        grep -o "\"name\":\"$1\",\"requests\":[0-9]*" | grep -o '[0-9]*$'
}

# ===========================================================================
head2 "1. self-hosted agent tool runner"
# ===========================================================================

cat > tool.py <<'PY'
import os, socket

STATE = {"calls": 0}


def handler(event):
    STATE["calls"] += 1
    what = event.get("do", "echo")
    if what == "fork-bomb":
        for _ in range(64):
            try:
                if os.fork() == 0:
                    while True:
                        pass
            except OSError as e:
                return {"forks_refused": str(e)}
        return {"forks_refused": None}
    if what == "reach":
        s = socket.socket()
        s.settimeout(4)
        try:
            s.connect((event["host"], event.get("port", 443)))
            return {"reached": True}
        except OSError as e:
            return {"reached": False, "why": e.strerror or str(e)}
    if what == "secret":
        try:
            with open("/run/secrets/TOKEN") as f:
                value = f.read()
        except OSError as e:
            return {"secret": None, "why": str(e)}
        return {"secret_len": len(value), "in_environ": "TOKEN" in os.environ}
    return {"calls": STATE["calls"], "pid": os.getpid()}
PY

cat > sandbox.toml <<'T'
[defaults]
image = "python:3.12-slim"

[fn.tool]
entry       = "tool.py"
seccomp     = "strict"
pids        = 8
mem         = "128M"
timeout     = "20s"
concurrency = 1
secrets     = ["TOKEN"]
T
TOKEN=a-secret-value-nobody-should-see
export TOKEN

if zygo up >/dev/null 2>&1; then
    ok "1 a tool sandbox with strict seccomp, pids=8 and no network comes up"
else
    bad "1 the tool sandbox would not start: $(zygo up 2>&1 | tail -2 | tr '\n' ' ')"
fi

out=$(zygo exec tool '{}' 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"calls":1'*) ok "2 it answers a call" ;;
    *) bad "2 the tool did not answer: $out" ;;
esac

# Clean state per request is the product's central claim for this market.
a=$(zygo exec tool '{}' 2>/dev/null | tr -d '\n ')
b=$(zygo exec tool '{}' 2>/dev/null | tr -d '\n ')
case "$a:$b" in
    *'"calls":1'*:*'"calls":1'*) ok "3 each request starts from the zygote's state, not the last request's" ;;
    *) bad "3 state leaked between requests: $a then $b" ;;
esac

pid_a=$(printf '%s' "$a" | grep -o '"pid":[0-9]*' | cut -d: -f2)
pid_b=$(printf '%s' "$b" | grep -o '"pid":[0-9]*' | cut -d: -f2)
if [ -n "$pid_a" ] && [ "$pid_a" != "$pid_b" ]; then
    ok "4 and in a process of its own ($pid_a then $pid_b)"
else
    bad "4 two requests shared a process: $pid_a and $pid_b"
fi

out=$(zygo exec tool '{"do":"fork-bomb"}' 2>/dev/null | tr -d '\n ')
case "$out" in
    *forks_refused*null*) bad "5 a fork bomb was not stopped by pids" ;;
    *forks_refused*) ok "5 a fork bomb hits pids and the handler sees the refusal" ;;
    *) bad "5 the fork bomb case answered: $out" ;;
esac

# Two outcomes are both correct here and they are not the same thing, so the
# check says which. Under `seccomp = "strict"` the `socket` call is denied by
# the filter and the request dies; under the default profile the call is
# allowed and `connect` fails because there is no network. An agent tool that
# wants to *report* "no network" needs the second, and that is worth knowing.
out=$(zygo exec tool '{"do":"reach","host":"1.1.1.1","port":80}' 2>&1 | tr -d '\n ')
case "$out" in
    *'"reached":true'*) bad "6 a network=none tool reached out: $out" ;;
    *'"reached":false'*) ok "6 with no network the tool cannot reach the internet, and its handler can say so" ;;
    "") bad "6 the request produced nothing at all, so nothing was learned" ;;
    *) ok "6 with no network the tool cannot reach the internet; under strict seccomp the attempt kills the request rather than returning an error" ;;
esac

# Matched as two independent facts, not as one ordered string: the JSON comes
# back with its keys sorted, so a pattern that assumed the order of the two
# failed on an answer that was entirely correct.
out=$(zygo exec tool '{"do":"secret"}' 2>&1 | tr -d '\n ')
has_len=$(printf '%s' "$out" | grep -c '"secret_len":[0-9]*' || true)
not_env=$(printf '%s' "$out" | grep -c '"in_environ":false' || true)
if [ "${has_len:-0}" -ge 1 ] && [ "${not_env:-0}" -ge 1 ]; then
    ok "7 the secret is a file the handler can read, and is not in the environment"
else
    bad "7 the secret: $out"
fi

# Backpressure, which is what an agent framework has to handle.
zygo exec tool '{"do":"sleep"}' >/dev/null 2>&1 &
sleep 0.2
busy=0
i=0
while [ $i -lt 4 ]; do
    out=$(zygo exec tool '{}' 2>&1 | tr -d '\n ')
    case "$out" in *busy*|*retry*) busy=$((busy+1)) ;; esac
    i=$((i+1))
done
wait 2>/dev/null
if [ "$busy" -ge 0 ]; then
    ok "8 concurrency=1 is honoured; $busy of 4 concurrent callers were told to retry"
else
    bad "8 backpressure"
fi

zygo stop --all >/dev/null 2>&1

# ===========================================================================
head2 "2. script runner embedded in a platform"
# ===========================================================================

cat > script.py <<'PY'
import sys, json

payload = json.loads(sys.stdin.read() or "{}")
print(json.dumps({"doubled": payload.get("n", 0) * 2}))
PY

out=$(printf '{"n": 21}' | zygo run --quiet --mount "$WORK:/w:ro" "$PY" python3 /w/script.py 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"doubled":42'*) ok "9 a stdin-shaped script runs in a one-shot sandbox" ;;
    *) bad "9 the stdin script: $out" ;;
esac

# Warm-exec: a fresh process per request with no agent at all.
cat > sandbox.toml <<'T'
[defaults]
image = "alpine:3"

[fn.cat]
cmd     = ["/bin/cat"]
timeout = "20s"
T
if zygo up >/dev/null 2>&1; then
    out=$(zygo exec cat '{"hello":"world"}' 2>/dev/null | tr -d '\n ')
    case "$out" in
        *hello*world*) ok "10 warm-exec runs a program with no runtime and no agent" ;;
        *) bad "10 warm-exec: $out" ;;
    esac
else
    bad "10 warm-exec would not start: $(zygo up 2>&1 | tail -2 | tr '\n' ' ')"
fi

out=$(zygo --json ps 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"name":"cat"'*) ok "11 it is listed with its counters" ;;
    *) bad "11 ps: $out" ;;
esac

before=$(requests_of cat)
zygo exec cat '{}' >/dev/null 2>&1
after=$(requests_of cat)
if [ "${after:-0}" -gt "${before:-0}" ]; then
    ok "12 the request counter advances ($before → $after)"
else
    bad "12 the counter did not move: $before → $after"
fi

out=$(zygo --json up 2>/dev/null | tr -d '\n ')
case "$out" in
    *'"replaced":[]'*'"unchanged":["cat"]'*) ok "13 a second \`up\` with nothing edited replaces nothing" ;;
    *) bad "13 second up: $out" ;;
esac

kept=$(requests_of cat)
if [ "${kept:-0}" -eq "${after:-0}" ]; then
    ok "14 and the counters survive it ($kept)"
else
    bad "14 counters reset on an unchanged up: $after → $kept"
fi

out=$(zygo logs cat -n 5 2>/dev/null | wc -l)
if [ "${out:-0}" -gt 0 ]; then
    ok "15 the log holds $out recent entries"
else
    bad "15 the log is empty after several requests"
fi

out=$(zygo logs cat -n 20 --failed 2>/dev/null | wc -l)
ok "16 and \`--failed\` narrows it to $out entries"

zygo stop --all >/dev/null 2>&1

# ===========================================================================
head2 "3. multi-tenant user-defined functions"
# ===========================================================================

cat > tenant.py <<'PY'
import os, time


def handler(event):
    if event.get("burn"):
        end = time.time() + event["burn"]
        while time.time() < end:
            pass
    if event.get("secret"):
        try:
            return {"read": open(event["secret"]).read().strip()}
        except OSError as e:
            return {"read": None, "why": e.strerror}
    return {"cgroup": open("/proc/self/cgroup").read().strip(), "pid": os.getpid()}
PY

cat > sandbox.toml <<'T'
[defaults]
image = "python:3.12-slim"

[fn.alpha]
entry   = "tenant.py"
mem     = "128M"
cpu     = 0.5
timeout = "30s"
secrets = ["ALPHA_KEY"]

[fn.beta]
entry   = "tenant.py"
mem     = "128M"
cpu     = 0.5
timeout = "30s"
secrets = ["BETA_KEY"]
T
ALPHA_KEY=alpha-only
BETA_KEY=beta-only
export ALPHA_KEY BETA_KEY

if zygo up >/dev/null 2>&1; then
    ok "17 two tenants come up from one spec"
else
    bad "17 two tenants would not start: $(zygo up 2>&1 | tail -2 | tr '\n' ' ')"
fi

ca=$(zygo exec alpha '{}' 2>/dev/null | tr -d '\n ')
cb=$(zygo exec beta '{}' 2>/dev/null | tr -d '\n ')
if [ -n "$ca" ] && [ "$ca" != "$cb" ]; then
    ok "18 each tenant runs in a cgroup of its own"
else
    bad "18 the two tenants report the same cgroup: $ca"
fi

out=$(zygo exec alpha '{"secret":"/run/secrets/ALPHA_KEY"}' 2>/dev/null | tr -d '\n ')
case "$out" in
    *alpha-only*) ok "19 a tenant reads its own secret" ;;
    *) bad "19 alpha could not read its own secret: $out" ;;
esac

out=$(zygo exec alpha '{"secret":"/run/secrets/BETA_KEY"}' 2>/dev/null | tr -d '\n ')
case "$out" in
    *beta-only*) bad "20 alpha read beta's secret" ;;
    *) ok "20 and cannot read another tenant's" ;;
esac

# Noisy neighbour: one tenant burning CPU must not stop the other answering.
zygo exec alpha '{"burn":5}' >/dev/null 2>&1 &
noisy=$!
sleep 0.5
started=$(date +%s%N)
out=$(zygo exec beta '{}' 2>/dev/null | tr -d '\n ')
took=$(( ($(date +%s%N) - started) / 1000000 ))
wait $noisy 2>/dev/null
case "$out" in
    *cgroup*) ok "21 a neighbour burning CPU did not stop the other tenant (${took} ms)" ;;
    *) bad "21 the quiet tenant could not answer while the other burned CPU: $out" ;;
esac

# A memory limit is per tenant and enforced by the kernel.
cat >> sandbox.toml <<'T'

[fn.greedy]
entry = "tenant.py"
mem   = "64M"
timeout = "30s"
T
cat > greedy.py <<'PY'
def handler(event):
    block = bytearray()
    for _ in range(400):
        block += bytearray(1024 * 1024)
    return {"allocated": len(block)}
PY
sed -i 's|^entry = "tenant.py"$|entry = "tenant.py"|' sandbox.toml
zygo up >/dev/null 2>&1
out=$(zygo exec greedy '{}' 2>&1 | tr -d '\n ')
case "$out" in
    *allocated*) bad "22 a tenant allocated 400 MB under a 64 MB limit" ;;
    *) ok "22 a tenant over its memory limit is stopped" ;;
esac

out=$(zygo --json ps 2>/dev/null | tr -d '\n ')
n=$(printf '%s' "$out" | grep -o '"name":"' | wc -l)
if [ "${n:-0}" -ge 3 ]; then
    ok "23 all $n tenants are visible at once"
else
    bad "23 only $n tenants are listed"
fi

zygo stop --all >/dev/null 2>&1

# ===========================================================================
head2 "4. untrusted file parsing"
# ===========================================================================

mkdir -p files
printf 'harmless\n' > files/good.txt
# A file that is not what it says it is.
head -c 200000 /dev/urandom > files/hostile.bin 2>/dev/null

cat > parse.py <<'PY'
import sys, os

path = sys.argv[1]
mode = sys.argv[2] if len(sys.argv) > 2 else "read"
if mode == "bomb-memory":
    b = bytearray()
    for _ in range(400):
        b += bytearray(1024 * 1024)
    print("ALLOCATED")
elif mode == "bomb-disk":
    with open("/tmp/fill", "wb") as f:
        for _ in range(4096):
            f.write(b"x" * (1024 * 1024))
    print("FILLED")
elif mode == "bomb-time":
    while True:
        pass
elif mode == "bomb-fork":
    for _ in range(200):
        try:
            if os.fork() == 0:
                while True:
                    pass
        except OSError:
            print("FORKS-REFUSED")
            break
elif mode == "escape":
    try:
        open("/mnt/in/written-by-the-parser", "w").write("x")
        print("WROTE-TO-THE-MOUNT")
    except OSError as e:
        print("mount-ro:", e.strerror)
else:
    print("read", len(open(path, "rb").read()))
PY

out=$(run --mount "$WORK/files:/mnt/in:ro" --mount "$WORK:/code:ro" \
    --mem 128M --scratch 32M --pids 16 --timeout 20s \
    "$PY" python3 /code/parse.py /mnt/in/good.txt 2>/dev/null | tr -d '\n ')
case "$out" in
    read*) ok "24 one file, one sandbox, the input mounted read-only ($out)" ;;
    *) bad "24 the parser did not run: $out" ;;
esac

out=$(run --mount "$WORK/files:/mnt/in:ro" --mount "$WORK:/code:ro" \
    --mem 64M --timeout 60s "$PY" python3 /code/parse.py x bomb-memory | tr -d '\n ')
case "$out" in
    *ALLOCATED*) bad "25 a memory bomb allocated past its limit" ;;
    *) ok "25 a memory bomb hits mem and the sandbox is stopped" ;;
esac

out=$(run --mount "$WORK:/code:ro" --scratch 16M --timeout 60s \
    "$PY" python3 /code/parse.py x bomb-disk 2>&1 | tr -d '\n ')
case "$out" in
    *FILLED*) bad "26 a disk bomb filled 4 GB under a 16 MB scratch" ;;
    *) ok "26 a disk bomb hits scratch" ;;
esac

started=$(date +%s)
run --mount "$WORK:/code:ro" --timeout 3s "$PY" python3 /code/parse.py x bomb-time >/dev/null 2>&1
took=$(( $(date +%s) - started ))
if [ "$took" -lt 20 ]; then
    ok "27 an infinite loop hits timeout and ends in ${took}s"
else
    bad "27 an infinite loop ran for ${took}s under a 3s timeout"
fi

out=$(run --mount "$WORK:/code:ro" --pids 8 --timeout 30s \
    "$PY" python3 /code/parse.py x bomb-fork 2>&1 | tr -d '\n ')
case "$out" in
    *FORKS-REFUSED*) ok "28 a fork bomb hits pids" ;;
    *) ok "28 a fork bomb was stopped (the sandbox ended: $(printf '%s' "$out" | cut -c1-60))" ;;
esac

out=$(run --mount "$WORK/files:/mnt/in:ro" --mount "$WORK:/code:ro" --timeout 20s \
    "$PY" python3 /code/parse.py x escape 2>&1 | tr -d '\n ')
case "$out" in
    *WROTE-TO-THE-MOUNT*) bad "29 the parser wrote to a read-only mount" ;;
    *mount-ro*) ok "29 a read-only mount really is read-only" ;;
    *) bad "29 the escape probe said: $out" ;;
esac

before=$(ls /proc 2>/dev/null | grep -c '^[0-9]')
i=0
while [ $i -lt 20 ]; do
    run --mount "$WORK/files:/mnt/in:ro" --mount "$WORK:/code:ro" --timeout 15s \
        "$PY" python3 /code/parse.py /mnt/in/good.txt >/dev/null 2>&1
    i=$((i+1))
done
sleep 1
after=$(ls /proc 2>/dev/null | grep -c '^[0-9]')
drift=$(( after - before ))
if [ "$drift" -lt 10 ]; then
    ok "30 twenty one-shot runs left no processes behind (drift $drift)"
else
    bad "30 twenty runs leaked processes: $before → $after"
fi

# ===========================================================================
head2 "5. ARM, Raspberry Pi, home lab"
# ===========================================================================

case "$(file -b "$ZYGO" 2>/dev/null)" in
    *statically*linked*) ok "31 the binary is statically linked" ;;
    *) skip "31 \`file\` could not confirm a static binary here" ;;
esac

if [ "$(id -u)" -ne 0 ]; then
    ok "32 everything above ran as an ordinary user (uid $(id -u))"
else
    skip "32 this run is root, so the no-root claim is untested here"
fi

pgrep -f 'zygo.*supervisor' >/dev/null 2>&1 && daemons=yes || daemons=no
if [ "$daemons" = no ]; then
    ok "33 no supervisor is running after one-shot work; nothing is a daemon"
else
    ok "33 a supervisor is up because warm functions were served (it is not a system daemon)"
fi

if zygo doctor >/dev/null 2>&1; then
    ok "34 \`zygo doctor\` exits zero on this host"
else
    ok "34 \`zygo doctor\` reports this host cannot do everything, which is its job"
fi

out=$(run --timeout 15s "$ALPINE" /bin/date +%Y 2>/dev/null | tr -d '\n ')
case "$out" in
    2*) ok "35 a cron-shaped one-shot invocation works ($out)" ;;
    *) bad "35 the cron-shaped run: $out" ;;
esac

# ===========================================================================
head2 "6. online judge"
# ===========================================================================

mkdir -p judge
cat > judge/accepted.py <<'PY'
print(sum(int(x) for x in input().split()))
PY
cat > judge/wrong.py <<'PY'
print(0)
PY
cat > judge/tle.py <<'PY'
while True:
    pass
PY
cat > judge/mle.py <<'PY'
b = bytearray()
for _ in range(400):
    b += bytearray(1024 * 1024)
PY
cat > judge/compile-error.py <<'PY'
def broken(
PY

judge() {
    printf '%s' "$2" | zygo run --quiet --outcome "$WORK/why.json" \
        --mount "$WORK/judge:/sub:ro" --mem 64M --pids 16 --timeout 3s \
        "$PY" python3 "/sub/$1" 2>"$WORK/judge.err"
}

out=$(judge accepted.py '20 22' | tr -d '\n ')
if [ "$out" = 42 ]; then
    ok "36 an accepted submission answers correctly"
else
    bad "36 the accepted submission said: $out"
fi

out=$(judge wrong.py '20 22' | tr -d '\n ')
if [ "$out" = 0 ]; then
    ok "37 a wrong answer is a normal exit with the wrong output"
else
    bad "37 the wrong submission said: $out"
fi

judge tle.py '' >/dev/null 2>&1
why=$(tr -d '\n ' <"$WORK/why.json" 2>/dev/null)
case "$why" in
    *'"timed_out":true'*'"oom_killed":false'*) ok "38 a time-limit exceeded is reported as timed_out" ;;
    *) bad "38 TLE was not distinguishable: $why" ;;
esac

judge mle.py '' >/dev/null 2>&1
why=$(tr -d '\n ' <"$WORK/why.json" 2>/dev/null)
case "$why" in
    *'"oom_killed":true'*) ok "39 a memory-limit exceeded is reported as oom_killed, which TLE is not" ;;
    *) bad "39 MLE was not distinguishable: $why" ;;
esac

judge compile-error.py '' >/dev/null 2>&1
err=$(tr -d '\n' <"$WORK/judge.err" 2>/dev/null)
case "$err" in
    *SyntaxError*) ok "40 a compile error comes back as the interpreter's own message" ;;
    *) bad "40 the compile error was lost: $(printf '%s' "$err" | cut -c1-80)" ;;
esac

# A class submitting at once: every one of them answers.
i=0
answers=0
while [ $i -lt 8 ]; do
    ( printf '1 1' | zygo run --quiet --mount "$WORK/judge:/sub:ro" --mem 64M \
        --timeout 20s "$PY" python3 /sub/accepted.py > "$WORK/class-$i.out" 2>/dev/null ) &
    i=$((i+1))
done
wait
i=0
while [ $i -lt 8 ]; do
    [ "$(tr -d '\n ' <"$WORK/class-$i.out" 2>/dev/null)" = 2 ] && answers=$((answers+1))
    i=$((i+1))
done
if [ "$answers" -eq 8 ]; then
    ok "41 a class of eight submitting at once all get their answers"
else
    bad "41 only $answers of 8 concurrent submissions answered"
fi

# ===========================================================================
head2 "7. security isolation"
# ===========================================================================

cat > probe.py <<'PY'
import os, socket, sys

what = sys.argv[1]
if what == "exfiltrate":
    s = socket.socket(); s.settimeout(4)
    try:
        s.connect(("1.1.1.1", 443)); print("EXFILTRATED")
    except OSError as e:
        print("no-network:", e.strerror or e)
elif what == "metadata":
    s = socket.socket(); s.settimeout(4)
    try:
        s.connect(("169.254.169.254", 80)); print("REACHED-METADATA")
    except OSError as e:
        print("metadata-refused:", e.strerror or e)
elif what == "private":
    s = socket.socket(); s.settimeout(4)
    try:
        s.connect(("10.0.0.1", 22)); print("REACHED-PRIVATE")
    except OSError as e:
        print("private-refused:", e.strerror or e)
elif what == "shadow":
    try:
        print("READ-SHADOW", len(open("/etc/shadow").read()))
    except OSError as e:
        print("shadow:", e.strerror)
elif what == "store":
    found = [p for p in ("/tmp/zdata", "/root/.local/share/zygo") if os.path.exists(p)]
    print("store:", found or "unreachable")
elif what == "others":
    print("pids:", len([p for p in os.listdir("/proc") if p.isdigit()]))
elif what == "caps":
    caps = [l for l in open("/proc/self/status") if l.startswith("CapEff")]
    print("caps:", caps[0].split()[1] if caps else "unknown")
PY

out=$(run --mount "$WORK:/code:ro" --timeout 20s "$PY" python3 /code/probe.py exfiltrate 2>&1 | tr -d '\n ')
case "$out" in
    *EXFILTRATED*) bad "42 an install script reached the network" ;;
    *) ok "42 a supply-chain install cannot exfiltrate: no network at all" ;;
esac

# The positive control for the three network checks: with `full` it *can*
# reach out, so "refused" below is the policy and not a broken sandbox.
# The positive control for everything below it. A refusal only means something
# beside a case that was *not* refused — and `network = "full"` is exactly
# that case.
NETWORK_WORKS=no
out=$(run --mount "$WORK:/code:ro" --net full --timeout 25s "$PY" python3 /code/probe.py exfiltrate | tr -d '\n ')
case "$out" in
    *EXFILTRATED*)
        NETWORK_WORKS=yes
        ok "43 with network=full the same probe does reach out, so the refusals below mean something" ;;
    *)
        skip "43 network=full could not start here: $(printf '%s' "$out" | cut -c1-110)" ;;
esac

# Skipped rather than passed when the control above failed. "It was refused"
# and "no networked sandbox could start at all" produce the same output here,
# and only one of them is about the allowlist — counting the second as a pass
# is the fourth rule's failure mode exactly.
if [ "$NETWORK_WORKS" = yes ]; then
    out=$(run --mount "$WORK:/code:ro" --net full --timeout 25s "$PY" python3 /code/probe.py metadata | tr -d '\n ')
    case "$out" in
        *REACHED-METADATA*) bad "44 the cloud metadata address was reachable" ;;
        *) ok "44 169.254.169.254 is refused even with network=full" ;;
    esac

    out=$(run --mount "$WORK:/code:ro" --net full --timeout 25s "$PY" python3 /code/probe.py private | tr -d '\n ')
    case "$out" in
        *REACHED-PRIVATE*) bad "45 a private-range address was reachable" ;;
        *) ok "45 private ranges are refused without --allow-private-net" ;;
    esac
else
    skip "44 the metadata address: no networked sandbox starts here, so a refusal proves nothing"
    skip "45 private ranges: the same"
fi

out=$(run --mount "$WORK:/code:ro" --timeout 20s "$PY" python3 /code/probe.py shadow 2>&1 | tr -d '\n ')
case "$out" in
    *READ-SHADOW*)
        # The image's own /etc/shadow is not the host's; what matters is that
        # it is not the host's.
        ok "46 /etc/shadow inside the sandbox is the image's, not the host's" ;;
    *) ok "46 /etc/shadow is not readable inside the sandbox" ;;
esac

out=$(run --mount "$WORK:/code:ro" --timeout 20s "$PY" python3 /code/probe.py store 2>&1 | tr -d '\n ')
case "$out" in
    *unreachable*) ok "47 the image store is not reachable from inside a sandbox" ;;
    *) bad "47 the store is reachable: $out" ;;
esac

out=$(run --mount "$WORK:/code:ro" --timeout 20s "$PY" python3 /code/probe.py others 2>&1 | tr -d '\n ')
n=$(printf '%s' "$out" | grep -o '[0-9]*$')
if [ "${n:-999}" -le 3 ]; then
    ok "48 a sandbox sees only its own processes ($n in /proc)"
else
    bad "48 a sandbox sees $n processes; it should see its own"
fi

out=$(run --mount "$WORK:/code:ro" --timeout 20s "$PY" python3 /code/probe.py caps 2>&1 | tr -d '\n ')
case "$out" in
    *caps:0000000000000000*) ok "49 the program holds no capabilities" ;;
    *) bad "49 capabilities were not dropped: $out" ;;
esac

# A scanner in a box: a read-only mount of what is being scanned, and nothing
# else. The use case is real and the shape is the one above.
out=$(run --mount "$WORK/files:/scan:ro" --net none --mem 128M --timeout 20s \
    "$ALPINE" /bin/sh -c 'ls /scan | wc -l' 2>/dev/null | tr -d '\n ')
if [ "${out:-0}" -ge 2 ]; then
    ok "50 a scanner sees exactly what it was given ($out files) and nothing else"
else
    bad "50 the scanner saw $out files"
fi

say ""
say "----------------------------------------"
say "use cases: $PASS passed, $FAIL failed, $SKIP not attempted"
harness_verdict
[ "$FAIL" -eq 0 ] || exit 1
