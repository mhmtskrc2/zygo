#!/usr/bin/env python3
"""A webhook receiver for `--usage-webhook`, for `poc/verify_api.sh`.

Twenty lines rather than a mock, because what is under test is delivery: that
the API posts batches, that they are JSON in the documented shape, and that a
cancelled and a timed-out request each arrive with the right `outcome`. A stand
-in that the driver called directly would test none of those.

Appends one line per batch to the sink file. Writes the port it bound to, so
the script does not have to guess one and race another test.
"""

import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT_FILE, SINK = sys.argv[1], sys.argv[2]


class Handler(BaseHTTPRequestHandler):
    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length", 0) or 0)
        body = self.rfile.read(length)
        with open(SINK, "a") as handle:
            handle.write(body.decode("utf-8", "replace") + "\n")
        # `204`, so the API counts the batch delivered and does not retry it.
        self.send_response(204)
        self.end_headers()

    def log_message(self, *_: object) -> None:
        pass


server = HTTPServer(("127.0.0.1", 0), Handler)
with open(PORT_FILE, "w") as handle:
    handle.write(str(server.server_address[1]))
server.serve_forever()
