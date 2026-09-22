# The handler `zygo agent test` expects, for the sh agent.
#
# The event arrives on stdin; the result goes to descriptor 4, and stdout and
# stderr are the handler's own. The conformance contract is: return the event
# unchanged, honour `stdout` and `stderr` when they are there, start a program
# when `spawn` is, and sleep for `sleep_ms` when it is a number.
handler() {
    event=$(cat)
    out=$(printf '%s' "$event" | jq -r 'if type == "object" then .stdout // empty else empty end')
    err=$(printf '%s' "$event" | jq -r 'if type == "object" then .stderr // empty else empty end')
    spawn=$(printf '%s' "$event" | jq -r 'if type == "object" then .spawn // empty else empty end')
    nap=$(printf '%s' "$event" | jq -r 'if type == "object" then .sleep_ms // empty else empty end')
    [ -n "$err" ] && printf '%s\n' "$err" >&2
    [ -n "$out" ] && printf '%s\n' "$out"
    # Something for a cancel to arrive during.
    [ -n "$nap" ] && sleep "$(awk "BEGIN{print $nap/1000}")"
    # A *program*, not a shell builtin: this is what the `strict` child filter
    # removes, so it is what the suite has to be able to attempt. A refusal is
    # an outcome, not an error.
    [ -n "$spawn" ] && { /bin/echo "$spawn" || printf 'spawn refused\n' >&2; }
    printf '%s' "$event" >&4
    return 0
}
