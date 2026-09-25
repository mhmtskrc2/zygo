#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""What a CPU-bound handler costs the rest of the protocol.

A warm agent holds one conversation with the supervisor and runs tenant code
at the same time. This measures what the second does to the first: while a
handler is spinning without yielding, does its streamed output still arrive
(proto 1.3), does its progress still arrive, is the request still reported
alive (proto 1.4), and is a bare `PING` still answered?

None of those is decoration. A caller that streams is watching a long job, and
output that arrives in one burst at the end is the same as no streaming; a
heartbeat that stops while a request is working is a supervisor that cannot
tell working from wedged.

    poc/agent_stall.py node    -- agents/node/zygo_agent.js
    poc/agent_stall.py python3 -- agents/python/zygo_agent.py --fd 3

    --bytes N   write N bytes to stdout before the spin, to find out what
                happens when the output pipe fills and draining it needs a
                turn of a loop the handler is not giving back.

The agent is started with **no handler** — the runtime-pool shape — and the
handler below arrives as the request's script, so this exercises the path a
pool serves. Nothing here asserts: it prints what it saw, in milliseconds
from `GO`.
"""

import json
import os
import socket
import struct
import sys
import time

AGENT_FD = 3

# Long enough that a heartbeat is due during the spin (the reference agents
# beat every two seconds), short enough that a run takes a few seconds.
SPIN_MS = 3000

# When the driver asks its own question, as a fraction of the spin.
PING_AT_MS = 1000

# The handlers, one per language. Each writes a line, reports progress,
# optionally floods stdout, then spins in a tight loop with no `await`, no
# yield and no syscall the runtime could schedule around — the worst case.
SCRIPTS = {
    "node": """
'use strict';
module.exports = function handler(event) {
  process.stdout.write('before the spin\\n');
  if (typeof event.progress === 'function') event.progress('starting');
  const line = 'x'.repeat(1023) + '\\n';
  for (let written = 0; written < %(bytes)d; written += line.length) {
    process.stdout.write(line);
  }
  const until = Date.now() + %(spin)d;
  while (Date.now() < until) {}
  process.stdout.write('after the spin\\n');
  return { spun_ms: %(spin)d };
};
""",
    "python": """
import sys, time

def handler(event):
    print("before the spin", flush=True)
    progress = event.get("progress") if isinstance(event, dict) else None
    if callable(progress):
        progress("starting")
    line = "x" * 1023 + "\\n"
    written = 0
    while written < %(bytes)d:
        sys.stdout.write(line)
        written += len(line)
    sys.stdout.flush()
    until = time.monotonic() + %(spin)d / 1000.0
    while time.monotonic() < until:
        pass
    print("after the spin", flush=True)
    return {"spun_ms": %(spin)d}
""",
}


def frame(message):
    body = json.dumps(message).encode()
    return struct.pack(">I", len(body)) + body


class Reader:
    """Length-prefixed frames off a socket, with the arrival time of each."""

    def __init__(self, sock):
        self._sock = sock
        self._buf = b""

    def _fill(self, n):
        while len(self._buf) < n:
            chunk = self._sock.recv(65536)
            if not chunk:
                raise EOFError("the agent closed the connection")
            self._buf += chunk

    def next(self):
        self._fill(4)
        (size,) = struct.unpack(">I", self._buf[:4])
        self._fill(4 + size)
        body, self._buf = self._buf[4 : 4 + size], self._buf[4 + size :]
        return json.loads(body), time.monotonic()


def start(binary, args):
    """The agent, with a connected socket on descriptor 3."""
    # `fork` and `exec` by hand rather than `subprocess`: the socket has to
    # land on descriptor 3 exactly, and `subprocess` closes every descriptor
    # above 2 that is not in `pass_fds` *after* `preexec_fn` runs — so a dup2
    # onto 3 there is undone before the exec and the agent finds nothing.
    ours, theirs = socket.socketpair()
    pid = os.fork()
    if pid == 0:
        os.dup2(theirs.fileno(), AGENT_FD)
        os.set_inheritable(AGENT_FD, True)
        os.execvp(binary, [binary] + args)
    theirs.close()
    ours.settimeout(0.05)
    return pid, ours


def main(argv):
    if "--" not in argv:
        sys.exit(__doc__)
    flood = 0
    if "--bytes" in argv:
        at = argv.index("--bytes")
        flood = int(argv[at + 1])
        del argv[at : at + 2]
    split = argv.index("--")
    binary, args = argv[split - 1], argv[split + 1 :]
    language = "python" if "python" in binary else "node"
    script = SCRIPTS[language] % {"spin": SPIN_MS, "bytes": flood}

    pid, sock = start(binary, args)
    reader = Reader(sock)

    sock.settimeout(30)
    ready, _ = reader.next()
    sock.settimeout(0.05)
    if ready.get("type") != "READY":
        sys.exit(f"expected READY, got {ready}")
    print(f"agent: {ready.get('runtime')}, {ready.get('rss_kb', 0) // 1024} MB resident")

    sock.sendall(
        frame(
            {
                "type": "EXEC",
                "id": "stall",
                "event": {},
                "timeout_ms": 60_000,
                "env_overrides": {},
                "stream": True,
                "script": {"source": script},
            }
        )
    )
    forked, _ = reader.next()
    if forked.get("type") != "FORKED":
        sys.exit(f"expected FORKED, got {forked}")

    sock.sendall(frame({"type": "GO", "id": "stall"}))
    go = time.monotonic()
    print(
        f"GO sent; the handler writes {flood} bytes and then spins for "
        f"{SPIN_MS} ms without yielding\n"
    )

    seen = []
    asked = None
    stdout_bytes = 0
    while True:
        # One question of the driver's own, a second into the spin: a bare
        # `PING` is what a supervisor sends when it wants to know whether the
        # *agent* is alive, and it is answered by the agent's loop rather than
        # by anything the handler has a say in.
        if asked is None and time.monotonic() - go > PING_AT_MS / 1000:
            sock.sendall(frame({"type": "PING", "seq": 4242}))
            asked = time.monotonic()
            seen.append(((asked - go) * 1000, "-> PING seq=4242 (the driver asks)"))

        try:
            # A short timeout rather than a blocking read, so the driver can
            # ask its question *when it meant to*: reading blocks until the
            # agent says something, and an agent that is behaving sends
            # nothing at all during a spin. The reader keeps its partial
            # frame, so a timeout mid-frame costs nothing.
            message, at = reader.next()
        except TimeoutError:
            if time.monotonic() - go > 30:
                sys.exit("the agent never finished the request")
            continue
        ms = (at - go) * 1000
        kind = message.get("type")
        if kind == "CHUNK":
            data = message.get("data", "")
            if message.get("stream") == "stdout" and len(data) > 64:
                stdout_bytes += len(data)
                seen.append((ms, f"CHUNK stdout: {len(data)} bytes"))
            else:
                seen.append((ms, f"CHUNK {message.get('stream')}: {data!r}"))
        elif kind == "PONG":
            waited = (at - asked) * 1000 if asked else float("nan")
            seen.append((ms, f"PONG seq={message.get('seq')} after {waited:.1f} ms"))
        elif kind == "PING":
            seen.append((ms, f"PING id={message.get('id')!r} (the agent's heartbeat)"))
        elif kind == "DONE":
            out = message.get("stdout") or ""
            seen.append(
                (ms, f"DONE exit={message.get('exit_code')}, stdout={len(out)} bytes")
            )
            break
        elif kind == "ERROR":
            seen.append((ms, f"ERROR {message.get('code')}: {message.get('message')}"))
            break
        else:
            seen.append((ms, kind))

    for ms, what in seen:
        print(f"  {ms:8.1f} ms  {what}")

    during = [w for ms, w in seen if ms < SPIN_MS - 200 and not w.startswith("->")]
    print()
    print(f"arrived while the handler was spinning: {len(during)}")
    for what in during:
        print(f"    {what}")
    if flood:
        print(f"stdout forwarded in chunks larger than 64 bytes: {stdout_bytes} bytes")

    sock.sendall(frame({"type": "SHUTDOWN"}))
    for _ in range(100):
        if os.waitpid(pid, os.WNOHANG)[0]:
            break
        time.sleep(0.05)
    else:
        os.kill(pid, 9)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
