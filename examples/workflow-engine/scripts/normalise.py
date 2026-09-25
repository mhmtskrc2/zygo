# SPDX-License-Identifier: Apache-2.0
"""A user script, as a workflow engine's user would write it.

Nothing here knows about Zygo: it is a `handler(event)` that returns a value,
which is what the engine's editor would have shown. The imports at the top are
paid once, when the zygote warms — every run after that inherits them through
copy-on-write and starts with a clean copy of this module.
"""

import decimal
import unicodedata

RATES = {"EUR": decimal.Decimal("1.00"), "USD": decimal.Decimal("0.92")}


def handler(event):
    currency = event.get("currency", "EUR")
    rate = RATES.get(currency)
    if rate is None:
        raise ValueError(f"no rate for {currency!r}; known: {', '.join(sorted(RATES))}")

    items = []
    for item in event.get("items", []):
        items.append(
            {
                "sku": unicodedata.normalize("NFKC", str(item["sku"])).upper(),
                "qty": int(item["qty"]),
            }
        )

    return {
        "id": event.get("id"),
        "items": items,
        "units": sum(i["qty"] for i in items),
        "currency": "EUR",
        "rate": str(rate),
    }
