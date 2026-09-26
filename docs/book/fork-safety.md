# Fork safety, question by question

A warm function answers each request with a `fork()` of a process that has
already done its imports. That is where Zygo's speed comes from, and it is
the part people ask about first. This page answers those questions one at a
time, and says plainly where the answer is "not fully". Each answer links to
the chapter with the detail.

```text
  zygote: interpreter + your imports, never runs a request
     │
     ├── fork ──▶ child 1: GO ─▶ child filter ─▶ reseed ─▶ own TMPDIR ─▶ handler ─▶ _exit
     ├── fork ──▶ child 2: GO ─▶ …
     └── fork ──▶ child 3: GO ─▶ …
          ▲
          └── each child starts as a copy of the zygote, not of child 1 or 2
```

## Is a forked child really clean?

Its **memory** is. A child starts as a copy of the zygote, and the zygote has
never run a request. Whatever child 1 changes — a global, a monkeypatch, a
cache — is in child 1's own pages and is gone when it exits. Child 2 is a new
copy of the zygote, not of child 1. A test holds this line for every agent
(rule 4 of [the protocol](../../spec/protocol.md)).

Its **files** need more than a fork, because every request in one sandbox
sees the same filesystem. Each request gets its own temporary folder, named
by `TMPDIR` and removed when it ends. But a literal `/tmp/...` path is one
folder for the whole sandbox, and so is any writable mount you add. Write
temporary files through `tempfile` or `os.tmpdir()`, never to a fixed path.
[Warm functions](13-warm-functions.md#what-a-request-sees) lists what a
request sees.

## What does a child share with the zygote?

Everything the zygote had when it was forked: the loaded modules, anything
built at import time, and open descriptors. The pages are shared
*copy-on-write*: they are only copied when one side writes. That is the
point. Your import-time work is paid once and every request starts with it.
It is also the rule to remember: **what you build at import time, every
request sees**. Build clients, compiled templates and models there. Do not
put per-user data there.

## Does every child have the same memory layout (ASLR)?

Yes. *ASLR* places a program's memory at random addresses, so an attacker
cannot know where things are. A fork copies the layout rather than drawing a
new one. So every child of one zygote has the same addresses, and an address
leaked by one request holds for the next. The layout changes only when the
zygote starts again, such as on a `zygo up` that replaces it or a `serve`
after a `stop`. This is a real cost of the fork model, and it is
why the `ns` backend is described as a wall for semi-trusted code. The Node
agent is different: each of its workers is a new process with its own
layout.

## Can a secret end up in the zygote?

No. A secret is never in the environment, never in the `EXEC` message and
never in the zygote. The supervisor writes it as a file, `/run/secrets/NAME`
(mode 0400), from outside the sandbox, after the child exists and before it
is told to start. The file goes when the function's last request in flight
finishes. Requests of the same function that run at the same moment can read
it too: they have the same value, the same uid and the same folder. A request
of another function cannot, because that function has its own sandbox. In a
runtime pool, which many tenants share, a request that receives secrets has
its zygote to itself while the files exist, so no other tenant's child is
forked beside them; the values are the calling tenant's and are written the
same way, never through the zygote.
[Chapter 14](14-limits-network-secrets.md#a-secret-lives-for-one-request) has
the details.

## What about threads and locks?

A fork copies only the thread that called it. If another thread held a lock
at that moment, the lock stays locked in every child. At warm-up the Python
agent checks for threads that would survive a fork, Python threads and native
ones. If it finds any, it stops forking and starts a fresh interpreter for
each request instead: much slower, but correct, and `zygo logs` says it did.
Start threads inside the handler, or lazily on first use.
[Chapter 13](13-warm-functions.md#when-a-handler-is-not-safe-to-fork).

## Do children draw the same random numbers?

Not from the usual places. The agent reseeds Python's `random`, numpy's
global generator and torch's generator in every child, before your code runs.
`os.urandom`, `secrets` and `uuid.uuid4()` ask the kernel on every call, so a
fork does not repeat them. What the agent **cannot** reseed is a generator
you created at import time, such as `RNG = random.Random()`. Every request
draws the same numbers from it. The agent names such a generator in
`zygo logs` at warm-up. Create it inside the handler.

## What about a connection opened at import time?

Every child inherits the same socket. Two requests running at once would then
write into one connection, and the replies would mix. The agent does not
detect this. Build the *client* at import time, which is where the cost is,
and let it connect on first use in the child. Most clients, `boto3` and
`requests.Session` among them, connect lazily.

## Do `atexit` handlers run?

No. A child leaves with `os._exit`: no `atexit` handlers, no interpreter
shutdown, and no flushing of buffers that the zygote also owns. Anything your
handler must save, it saves before it returns.

## Does the seccomp filter survive the fork?

Yes, and it cannot be removed. The kernel copies a process's filters into its
children, and nothing can take one away. The zygote runs under the sandbox's
filter, so every child does too. Under `strict`, each child also installs a
second filter after `GO` and before any request code: no `execve`, no new
process. Filters stack, and the strictest answer wins. That second filter is
installed by the agent, so it is only as good as the agent.
[Chapter 24](24-seccomp-profiles.md#the-child-filter).

## Can one tenant's data reach another through shared pages?

Not through a function's zygote. An agent function's zygote holds that
function's code and nothing else, and the multi-tenant pattern is one zygote
per script version ([ADR 0005](adr/0005-one-warm-zygote-per-script-version.md)).
A *runtime pool* is shared by tenants, so its zygote holds no tenant code at
all. The script arrives with the request and loads in the child. Zygo sends
it as a path to a read-only mount, which never passes through the zygote. The
fallback, the script's text in the message, does pass through the zygote's
memory, where the next tenant's child inherits a copy. Zygo logs when it has
to use that fallback. What a child writes stays in its own pages.

## Why not snapshot and restore a microVM instead?

It is the same idea one level down: restore a whole VM from a snapshot
instead of copying a process. It gives each call a hardware boundary, which
`ns` does not. It also costs more: the projects that do it report around
10–20 ms per restore, against Zygo's 1.4 ms fork. It has the same problems
too: every restore of one snapshot starts with the same memory, random state
included. Zygo's `vm` backend runs one-shot sandboxes, and warm functions stay
on `ns` ([ADR 0002](adr/0002-warm-paths-stay-on-ns.md) says why, and what would
change that). [Similar projects](10-similar-projects.md#firecracker-cloud-hypervisor-and-platforms-on-them)
compares the two.

## Is the Node agent any different?

Yes. Node cannot be forked safely, so its agent keeps a few *workers* ready.
Each worker is a new Node process that loads the handler itself, serves one
request and exits, and a replacement starts off the request path. Nothing is
inherited copy-on-write. So the thread, random-number and layout questions
above do not arise. The cost is a little higher than a fork. The handler's
import-time code runs when the worker starts, before the request it will
serve has its own cgroup.
[Chapter 13](13-warm-functions.md#a-node-handler).

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [23. Security: the threat model](23-security.md) · [Contents](README.md) · **Next: [24. Seccomp profiles](24-seccomp-profiles.md) →**
