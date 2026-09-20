#!/bin/sh
# A Zygo warm agent in POSIX sh. Needs `jq` and nothing else.
#
# This exists to keep `spec/protocol.md` honest. It claims the protocol is
# language independent — "the built-in Python agent, a Node agent, thirty lines
# of bash" — and a claim like that is worth what its smallest implementation
# proves. Run the conformance suite against it:
#
#     zygo agent test /bin/sh -- examples/agents/sh/agent.sh examples/agents/sh/handler.sh
#
# It is a demonstration, not a recommendation: `sh` has no interpreter start
# worth amortising, so a real `sh` function should use warm-exec (`cmd = [...]`)
# instead. It also serves one request at a time and answers a second with
# `overloaded`, which the protocol allows — concurrency is optional, losing a
# request is not.
#
# Handler contract: `handler` reads the event on stdin, writes its **result** to
# descriptor 4, and may write anything it likes to stdout and stderr. Those go
# back as separate fields, which is why the result needs a channel of its own.
#
# Usage: agent.sh <handler.sh>   with the control socket on descriptor 3.
set -u

WIRE=3
HANDLER=${1:?usage: agent.sh <handler.sh>}

# Bytes, not characters — the protocol's length prefix counts bytes, and every
# tool below has to agree. Without this, `gawk` in a UTF-8 locale encodes a
# length byte above 127 as a *two-byte* UTF-8 sequence and every frame longer
# than 127 bytes is silently mis-framed. It cost an afternoon: the conformance
# suite passed in a container (Debian ships `mawk`, which has no multibyte
# notion) and failed on Ubuntu (which ships `gawk`) on exactly the checks whose
# frames were long enough.
export LC_ALL=C

# --- framing ----------------------------------------------------------------
#
# Four bytes of big-endian length, then that many bytes of UTF-8 JSON. `awk`
# writes the header because a length byte may be zero, and a NUL cannot survive
# a shell command substitution.

send() {
    # `jq -c` output is ASCII for our payloads, so a character count is a byte
    # count. `wc -c` would be exact but costs a process per frame.
    len=$(printf '%s' "$1" | wc -c | tr -d ' ')
    awk -v n="$len" 'BEGIN { printf "%c%c%c%c",
        int(n / 16777216) % 256, int(n / 65536) % 256, int(n / 256) % 256, n % 256 }' >&$WIRE
    printf '%s' "$1" >&$WIRE
}

# One frame onto stdout. A non-zero exit means end of stream.
read_frame() {
    len=$(dd bs=1 count=4 2>/dev/null <&$WIRE |
        od -An -tu1 -v |
        awk 'NF == 4 { print $1 * 16777216 + $2 * 65536 + $3 * 256 + $4 }')
    [ -n "${len:-}" ] && [ "$len" -gt 0 ] || return 1
    dd bs=1 count="$len" 2>/dev/null <&$WIRE
}

# `.type`, or empty when the body is not an object at all.
kind_of() {
    printf '%s' "$1" | jq -r 'if type == "object" then .type // "" else "" end' 2>/dev/null
}

error() {
    send "$(jq -cn --arg id "${2:-}" --arg code "$1" --arg msg "$3" \
        '{type:"ERROR", id:(if $id == "" then null else $id end), code:$code, message:$msg}')"
}

pong() {
    send "$(jq -cn --argjson seq "$(printf '%s' "$1" | jq -r '.seq // 0')" \
        '{type:"PONG", seq:$seq}')"
}

# --- warm-up ----------------------------------------------------------------
#
# Everything expensive belongs here, before READY: whatever the agent loads now
# every request inherits through copy-on-write, and whatever it defers it pays
# for per request. `sh` has nothing to load, which is the honest reason this is
# an example rather than a recommendation.

. "$HANDLER"

send "$(jq -cn --argjson pid "$$" \
    '{type:"READY", proto:1, pid:$pid, imports_ms:0, rss_kb:0, runtime:"sh/posix"}')"

# --- serve ------------------------------------------------------------------

while frame=$(read_frame); do
    kind=$(kind_of "$frame")
    case "$kind" in
    "")
        # A whole frame whose body is not a message. The stream is still
        # aligned, so this is reportable rather than fatal.
        error bad_message "" "body is not a JSON object with a type"
        ;;

    PING) pong "$frame" ;;

    GO) ;; # A GO with nothing waiting for it. Not an error, nothing to do.

    SHUTDOWN) break ;;

    EXEC)
        id=$(printf '%s' "$frame" | jq -r '.id')
        event=$(printf '%s' "$frame" | jq -c '.event')

        go=$(mktemp -u) && mkfifo "$go"
        out=$(mktemp) && err=$(mktemp) && res=$(mktemp)

        # One process per request, and it does nothing until GO: until the
        # supervisor has moved it into the request's cgroup, anything it
        # allocates is billed to the agent and escapes the request's limits.
        (
            read -r _ <"$go"
            printf '%s' "$event" | handler >"$out" 2>"$err" 4>"$res"
            echo $? >"$res.code"
        ) &
        child=$!

        send "$(jq -cn --arg id "$id" --argjson pid "$child" \
            '{type:"FORKED", id:$id, pid:$pid}')"

        # Wait for this request's GO. Anything else that arrives is answered in
        # place rather than dropped — including a second EXEC, which gets
        # `overloaded` because this agent serves one request at a time.
        while waiting=$(read_frame); do
            case "$(kind_of "$waiting")" in
            GO) break ;;
            PING) pong "$waiting" ;;
            EXEC)
                error overloaded "$(printf '%s' "$waiting" | jq -r '.id')" \
                    "this agent serves one request at a time"
                ;;
            "") error bad_message "" "body is not a JSON object with a type" ;;
            esac
        done
        printf 'go\n' >"$go"

        wait "$child"
        code=$(cat "$res.code" 2>/dev/null || echo 1)

        if result=$(jq -c . <"$res" 2>/dev/null) && [ -n "$result" ]; then
            send "$(jq -cn --arg id "$id" --argjson code "$code" --argjson result "$result" \
                --rawfile out "$out" --rawfile err "$err" \
                '{type:"DONE", id:$id, exit_code:$code, result:$result,
                  stdout:$out, stderr:$err, wall_ms:0, cpu_ms:0, peak_rss_kb:0}')"
        else
            # The handler wrote nothing usable to descriptor 4. Reported as a
            # failed request rather than passed upwards as a string: the
            # supervisor's caller is expecting JSON either way.
            send "$(jq -cn --arg id "$id" --rawfile out "$out" --rawfile err "$err" \
                '{type:"DONE", id:$id, exit_code:1, result:null, stdout:$out, stderr:$err,
                  error:"the handler wrote no JSON result to descriptor 4",
                  wall_ms:0, cpu_ms:0, peak_rss_kb:0}')"
        fi
        rm -f "$go" "$out" "$err" "$res" "$res.code"
        ;;

    *) error bad_message "" "unexpected message \`$kind\`" ;;
    esac
done
