# SPDX-License-Identifier: Apache-2.0
"""A webhook handler: validate the payload, do the work, answer.

Imports and anything expensive belong at module level — they run once, in the
zygote, and every request inherits them through copy-on-write. The handler
itself runs in a fresh process per request, so nothing it does leaks into the
next one.
"""

import hashlib
import hmac
import os


def _signed(body: bytes, signature: str) -> bool:
    # The secret is a file, not an environment variable: it appears at
    # /run/secrets/WEBHOOK_SECRET for exactly the duration of this request,
    # and the zygote never sees it.
    with open("/run/secrets/WEBHOOK_SECRET", "rb") as f:
        secret = f.read().strip()
    expected = hmac.new(secret, body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, signature)


def handler(event: dict) -> dict:
    """`event` is the JSON body the HTTP API received."""
    if not isinstance(event, dict):
        return {"status": 400, "error": "expected a JSON object"}

    signature = event.get("signature", "")
    payload = event.get("payload", {})
    if os.path.exists("/run/secrets/WEBHOOK_SECRET") and not _signed(
        repr(sorted(payload.items())).encode(), signature
    ):
        return {"status": 401, "error": "bad signature"}

    kind = payload.get("kind", "unknown")
    return {
        "status": 200,
        "received": kind,
        "items": len(payload.get("items", [])),
    }
