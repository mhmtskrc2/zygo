"""The handler `zygo agent test` expects, for the Python reference agent.

The conformance suite cannot assert anything about an agent's answers unless it
knows what the handler was supposed to do, so the contract is three lines:
return the event unchanged, and honour `stdout` and `stderr` when they are
there. Every agent in this directory ships one of these.
"""

import sys


def handler(event):
    if isinstance(event, dict):
        if isinstance(event.get("stdout"), str):
            print(event["stdout"])
        if isinstance(event.get("stderr"), str):
            print(event["stderr"], file=sys.stderr)
    return event
