# SPDX-License-Identifier: Apache-2.0
"""The handler `zygo agent test` expects, for the Python reference agent.

The conformance suite cannot assert anything about an agent's answers unless it
knows what the handler was supposed to do, so the contract is four lines:
return the event unchanged, honour `stdout` and `stderr` when they are there,
start a program when `spawn` is, and sleep for `sleep_ms` when it is a number.
Every agent in this directory ships one of these.
"""

import subprocess
import sys
import time


def handler(event):
    if isinstance(event, dict):
        if isinstance(event.get("stdout"), str):
            print(event["stdout"])
        if isinstance(event.get("stderr"), str):
            print(event["stderr"], file=sys.stderr)
        # Something for a cancel to arrive *during*. Without it every request
        # is over in a millisecond and the suite would be racing the handler
        # rather than testing it.
        if isinstance(event.get("sleep_ms"), (int, float)):
            time.sleep(float(event["sleep_ms"]) / 1000.0)
        if isinstance(event.get("spawn"), str):
            # A *program*, not a function call: this is what the `strict`
            # child filter removes, so it is what the suite has to be able to
            # attempt. A refusal is an outcome, not an error.
            try:
                out = subprocess.run(
                    ["/bin/echo", event["spawn"]], capture_output=True, text=True
                )
                sys.stdout.write(out.stdout)
            except OSError as exc:
                print(f"spawn refused: {exc}", file=sys.stderr)
    return event
