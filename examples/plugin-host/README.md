# A plugin host, on the API alone

The exit criterion for Zygo's embedder API, written as a program rather than a
checklist. The question it answers is not "does each route work" — the suites
answer that — but **can somebody build the thing the API is for without
reaching past it**.

So: no `sandbox.toml`, no file written on the Zygo machine, no shelling out to
`zygo`. If this had needed one of those, the API was missing a route and Phase
2 was not finished.

```bash
make verify-plugin-host
```

## What a plugin host is

Software with customers who supply code: a CI service, a notebook backend, a
webhook platform, an agent framework with tools. Each customer's code is
theirs, runs under their own limits, and must not be able to reach anybody
else's.

`host.py` is about a hundred lines and does all of it:

| What the host does | The API it uses |
|---|---|
| Onboard a customer | `POST /tenants`, `POST /tenants/<id>/tokens` |
| Decide what they may use | `PATCH /tenants/<id>/limits` |
| Give them a key | `PUT /tenants/<id>/secrets/<name>` |
| Declare one runtime for everybody | `POST /runtimes` |
| Install a plugin | `PUT /scripts`, as that customer |
| Run one, with files in and out | `POST /runtimes/<r>/call`, `?out=1` |
| Watch a long one | the same, `?stream=1` |
| Stop one | `DELETE /requests/<key>` |
| Bill for it | `--usage-webhook` on the server |
| Offboard | `DELETE /tenants/<id>` |

## The shape that makes it work

**One runtime, many customers.** The pool holds an interpreter and a dependency
set and *no code at all* — the script arrives with the request and is loaded in
the forked child, after its seccomp filter. That is what makes it safe for
several customers to share one, and it is why ten thousand plugins do not mean
ten thousand warm processes.

**A token is the whole of a customer's authority.** `onboard` returns one, and
everything that customer's code can reach follows from it. Nothing else the
host holds is needed to keep them apart.

**A digest is not a capability.** A customer who learns another's script digest
is told the script does not exist — the same answer an unregistered digest
gets, because "it exists but is not yours" is a fact about somebody else.
`demo.py` tries it.

**Limits are the customer's, not the runtime's.** The pool is declared once
with generous limits; each tenant's own narrow them, on the request's cgroup.
`demo.py` has a plugin that allocates 96 MiB and a customer capped at 64.
