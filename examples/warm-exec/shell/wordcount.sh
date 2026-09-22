#!/bin/sh
# One request's script, in a warm-exec pool.
#
# The contract is the whole of the integration: the event arrives on stdin as
# one JSON object, the result goes to stdout as JSON, anything on stderr comes
# back to the caller beside it, and a non-zero exit is a failed request.
#
# There is no agent here and no protocol — `sh` was started with this file as
# its argument, the way `sh script.sh` starts anything. For a runtime that
# begins in under a millisecond that is the whole story, and an agent would
# only be a moving part between the caller and the program.

set -eu

event=$(cat)

# `sh` has no JSON parser, which is the point: a real script in a real pool
# would use `jq`, or be written in something that does. This pulls one string
# field out with `sed` so the example needs nothing but busybox.
text=$(printf '%s' "$event" | sed -n 's/.*"text"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')

if [ -z "$text" ]; then
    echo 'the event needs a "text" field' >&2
    exit 1
fi

words=$(printf '%s\n' "$text" | wc -w | tr -d ' ')
chars=$(printf '%s' "$text" | wc -c | tr -d ' ')

printf '{"words":%s,"chars":%s,"shell":"%s"}\n' "$words" "$chars" "$(readlink /proc/$$/exe || echo sh)"
