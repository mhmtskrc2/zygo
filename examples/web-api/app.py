# SPDX-License-Identifier: Apache-2.0
"""A tiny web API. Each endpoint runs code in Zygo a different way.

    POST /lower   {"text": "Hello WORLD"}              a warm function
    POST /script  {"code": "def handler(e): ...",      a runtime pool
                   "event": {...}}
    POST /cold    {"code": "print(6 * 7)"}             a fresh sandbox

Only the standard library, plus Zygo's Python client (sdk/python), which has
no dependencies of its own. The README says how to start it.
"""

import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import zygo_sdk as zygo

client = zygo.connect()  # finds the API through ZYGO_API_URL and ZYGO_API_TOKEN


def lower(body):
    # A warm function: the handler was loaded once, by `zygo up`.
    return client.fn("lower")({"text": body["text"]}).result


def script(body):
    # A runtime pool: the caller's code arrives with the request and is
    # loaded in a fresh copy of a warm Python, then thrown away.
    result = client.run_script("py312", body["code"], body.get("event"))
    return {"result": result.result, "stdout": result.stdout}


def cold(body):
    # A fresh sandbox for this one request: built, used, removed.
    run = client.run("python:3.12-slim", ["python3", "-c", body["code"]], timeout="10s")
    return {"exit_code": run.exit_code, "stdout": run.stdout, "stderr": run.stderr}


ROUTES = {"/lower": lower, "/script": script, "/cold": cold}


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        route = ROUTES.get(self.path)
        if route is None:
            return self.reply(404, {"error": f"no endpoint {self.path}"})
        try:
            length = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(length) or b"{}")
            self.reply(200, route(body))
        except (KeyError, ValueError) as e:
            self.reply(400, {"error": f"bad request: {e}"})
        except zygo.HandlerError as e:  # the caller's code raised
            self.reply(422, {"error": str(e), "stderr": e.stderr})
        except zygo.Timeout as e:  # it ran past its deadline
            self.reply(504, {"error": str(e)})
        except zygo.Busy as e:  # every warm copy is busy; try again
            self.reply(429, {"error": str(e)})
        except zygo.ZygoError as e:  # anything else from Zygo
            self.reply(502, {"error": str(e)})

    def reply(self, status, data):
        body = json.dumps(data).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    print("listening on http://127.0.0.1:8000")
    ThreadingHTTPServer(("127.0.0.1", 8000), Handler).serve_forever()
