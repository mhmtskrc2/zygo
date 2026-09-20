# The handler `zygo agent test` expects, for the sh agent.
#
# The event arrives on stdin; the result goes to descriptor 4, and stdout and
# stderr are the handler's own. The conformance contract is: return the event
# unchanged, and honour `stdout` and `stderr` when they are there.
handler() {
    event=$(cat)
    out=$(printf '%s' "$event" | jq -r 'if type == "object" then .stdout // empty else empty end')
    err=$(printf '%s' "$event" | jq -r 'if type == "object" then .stderr // empty else empty end')
    [ -n "$err" ] && printf '%s\n' "$err" >&2
    [ -n "$out" ] && printf '%s\n' "$out"
    printf '%s' "$event" >&4
    return 0
}
