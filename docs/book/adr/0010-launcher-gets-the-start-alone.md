# ADR 0010 — The launcher is given only the start, and a bounded line

*A record of the decision as it was taken; the numbers in it are as of its date. Today's numbers are in [chapter 25](../25-performance.md).*

**Status:** accepted, 2026-10-07. Prompted by a supervisor that warmed
nothing for an hour, answered `/healthz` with `200` throughout, and left no
log of what it was doing.

## Context

Every sandbox the supervisor makes is created on one thread, the launcher.
That is deliberate and stays: `PR_SET_PDEATHSIG`, which kills the sandboxes
of a supervisor that crashed, is delivered when the *thread* that created
the sandbox exits, and a sandbox created on a connection thread died when
that CLI command returned (measured on 5.10/aarch64: killed inside 300 ms).

What ran on that thread had grown. A warm-up did everything there: checked
the handler file, built the `system` layer (up to 900 s), the venv (600 s),
the bytecode layer (300 s), resolved the allowlist's names, built the root,
and only then started the sandbox (60 s to `execve`, 120 s to `READY`).
Every `POST /runtimes` zygote, every pool growth, every rewarm and every
`zygo run` through the supervisor queued behind whatever was on it, with no
bound on the wait. A client's budget for a `serve` was 20 minutes; two
builds in line could exceed it, and the client then reported a supervisor
that "did not answer" when it was merely busy.

The incident that prompted this was worse than busy. Another process on the
host ran `find /` into a virtiofs mount that had stopped answering. The
launcher's next start touched a path on that mount — a check that a file
exists — and sat in the kernel, in a wait no timeout in user space can end.
Every start after it waited for ever. `/healthz` asks the supervisor for its
pools over a control call that does not pass through the launcher, so it
said `ok`. And the supervisor's log did not exist: the client that started
it had given it a pipe for stderr, read one line of it, and dropped it. With
`SIGPIPE` restored to its default — so that `zygo ps | head` ends quietly —
the supervisor's first warning after that killed the supervisor outright.
Every background supervisor had been living under that rule since the
pipe was introduced; this one happened to be killed by it at a bad time.

## Decision

**The launcher is given only the start.** The `clone3`, the mounts and the
handshake until the child has `execve`d, with the 60 s deadline it already
had, so a turn is milliseconds and bounded at about 70 s whatever the
sandbox does. Everything else a start needs — layers, venv, bytecode,
allowlist, root, the sandbox's configuration — is made first, on the thread
that asked, by `Pool::prepare`. A build there takes the supervisor's one
build lock, which `PUT /deps` builds already took, so "one build at a time"
is now true of every build rather than of some. And the wait for `READY`,
which is the interpreter and the imports and where a warm-up's time goes,
is that thread's too (`Pool::await_ready`): the kernel's rule is about the
thread that *created* the sandbox, and nothing says who reads its socket.
A pool's `min_warm` zygotes are launched one after another and awaited
together, so eight of them import side by side rather than in turn.

**A stuck launcher says so.** A start it has held for three minutes is past
every deadline a start has and is parked inside the kernel, where nothing
in user space can reach it. The idle thread names it in the log once;
`RUNTIMES` carries what the launcher is doing (control v15); and
`GET /healthz` answers `degraded` with `launcher_stuck`, the start's name
and its age — `degraded`, not `503`, because what is warm still serves and
the remedy is a person's restart, not a balancer's.

**The line is bounded both ways.** A warm-up waits at most 60 s for its
turn, a `zygo run` 20 s (under the 30 s its client allows before `STARTED`),
and no more than 32 starts may wait at once. A start refused is never run:
the caller has gone, and a sandbox made for nobody is a leak. The refusal
names what the launcher was doing and for how long, and the same line goes
to the log. A start that has already begun is waited for past the budget;
the budget is for the line, not the work.

**The supervisor's stderr is a file.** The client that starts a supervisor
opens `supervisor.log` in the data folder and hands it over as stderr. A
file has no reader to lose. A supervisor that dies on startup is still
quoted from it, from the point its lifetime began. The supervisor logs at
`info` by default, which is the level its warm-ups, refusals and idle tiers
are written at, and the file is moved aside past 8 MiB at the next start.

**A pool of launcher threads was considered and not built now.** It is
possible — any number of threads that never exit keep the `PDEATHSIG`
property, and the child side is already written for a multi-threaded
parent — and it would let several starts run at once. With the start alone
on the launcher a turn is milliseconds to a few seconds, and nothing has yet
been measured to show that one thread limits a real host. The bound and the
split come first because they fix a hang; the pool would only add
throughput, and its default size should come from a measurement.

## Consequences

* Chapter 6 explains the launcher; chapters 21 and 22 the log file and the
  refusal; chapter 17 the `503` it becomes over the API.
* A `pip install` no longer holds every `zygo run` on the host. A second
  start of another image proceeds while the first builds.
* A start stuck in the kernel still sticks the launcher — nothing in user
  space can unstick a thread in a filesystem wait — but every later caller
  is now told so within a minute, with the name of the stuck start, instead
  of waiting for ever; the log and `/healthz` say it without being asked.
  `zygo supervisor stop` is the remedy, as before.
* A `POST /runtimes` with `min_warm` above one warms in about the time of
  one zygote rather than `min_warm` times that.
* Control protocol v15: a `launcher` on `RUNTIMES`. A client of v14 is
  refused by a v15 supervisor, as every version step is, and
  `zygo supervisor stop` is the remedy ([ADR 0004](0004-no-supervisor-reexec.md)).
* A supervisor started in the background no longer dies at its first
  warning, and what it did is on disk afterwards.

## What would reopen this

* A measurement showing start-up of many pools, or growth under a burst,
  limited by the one launcher thread. That is the case for a pool of
  launcher threads, sized by a setting, with the same bounded line in front
  of it.
* A way to end a start stuck in the kernel without restarting the
  supervisor. There is none today: a thread in an uninterruptible wait
  cannot be cancelled, and the sandbox's creating thread has to be *that*
  thread. A pool of launcher threads would let the others carry on, which
  is one more argument for it if the case recurs.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [ADR 0009 — The API may allow private addresses](0009-api-private-net.md) · [Contents](../README.md) · **Next: [Glossary](../glossary.md) →**
