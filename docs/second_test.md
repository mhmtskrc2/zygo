# Second pass — what is still open

Source: a re-test of every item in
[docker-replacement-report.md](docker-replacement-report.md) against the tree
as it stood on 21 September 2026, after the fixes recorded in
[todo.md](../todo.md) under "After test".
Date: 21 September 2026

This is a defect list, not a survey. What closed is recorded at the end in one
line each, so the check is auditable; the body is only what still needs work.

**What was tested.** The working tree, not a commit. Eight files carrying the
prune and disk work were uncommitted at the time
(`cmd/image.rs`, `image/store.rs`, `venv.rs`, `paths.rs`, `derive.rs`,
`pool.rs`, `shim.rs`, `README.md`), so the disk findings below describe work
that may still be in progress. Everything was exercised through the macOS shim
against the Lima VM, kernel 6.8.0, with `python:3.12-slim`.

**Verdict.** Seven of the eight items closed and hold under re-test. One — the
data directory — is half closed: `prune` now walks the right four categories,
but nothing in normal use can ever become collectable, so the symptom is
unchanged. A2 remains open by choice and reproduces at a third of the load it
was first found at. No regressions: all six behaviours from the first pass
still work, 492 Rust tests pass on macOS and the shim suite is 14 of 14.

## Open: the data directory only grows

The fix extended `prune` to four categories — layers, flattened rootfs, venvs
and derived system records — and the code does walk all four. The measured
behaviour did not change, and the reason is upstream of the walk.

A venv's liveness is decided in `crates/zygo-core/src/venv.rs:145` by whether
the **image** it was built against is still in the store. It is not decided by
whether any spec still asks for those requirements. Editing one line of a
`requirements.txt` therefore leaves the old venv behind for ever, because the
image it was built against is obviously still there.

Underneath that is the harder problem: **there is no `zygo image rm`.**
`zygo image --help` offers `prune` and nothing else. An image cannot be
un-referenced by intent, so the layers keyed on it are never unreferenced, and
neither are the venv, flatten and system caches keyed on the same liveness.
`prune` can collect what a crash or an interrupted pull orphaned, and nothing
a user ever decides they are finished with.

Measured after one session of ordinary use, two of those edits being a single
changed version pin:

| | |
|---|---|
| Images as `zygo images` reports them | 49 MB |
| Data directory on disk | 253 MB |
| Of which venv cache | 34 MB |
| Venvs present | 2, one referenced by nothing |
| `zygo image prune --dry-run` | `nothing to prune` |

Two changes close it. Add `zygo image rm <ref>`, which Docker users reach for
by reflex as `rmi` and which is the only thing that can make a layer dead. Key
the venv on its requirements hash as well as its image, so that replacing a
pin retires the venv it replaced. An access-time or LRU sweep would also work
and needs no notion of which specs exist on the machine.

Worth noting so it is not re-reported as a defect: 220 MB of the 253 MB is
extracted layers plus the compressed blobs they came from. Keeping both is what
Docker does too, and the first pass compared the compressed figure from
`zygo images` against the total on disk, which overstated the surprise. The
gap above is the collectable part, not the whole difference.

## Open by choice: A2, per-request cgroup

Reopened with the field numbers recorded on `PoolConfig::per_request_cgroup`,
and left for a decision on bare metal. That is the right call and this section
only adds a confirmation.

The first re-measurement was thrown away: it ran while the test suite was
compiling in the background and reported a p50 of 6716 µs, which measured the
contention and not the runtime. Repeated on a quiet machine, 300 requests at
100 req/s, both rows back to back:

| | per-request cgroup | none |
|---|---|---|
| p50 | 1944 µs | 1762 µs |
| p90 | 10966 µs | 3762 µs |
| p99 | 15296 µs | 4597 µs |
| max | 19099 µs | 7112 µs |
| acceptance | p99 **FAIL** | p99 PASS |

The roadmap's note that it does not take sustained pressure to show is correct.
At a third of the original load the p50 gap narrows to 182 µs, but the p99 gap
is still 3.3× and the acceptance verdict still flips. Nested virtualisation
inflates both columns; they were taken minutes apart on one host, so the ratio
is the part to read.

## Open: two small ones

**`stop --all` against an already-stopped VM announces a start.** Running it
twice prints `starting the Linux VM Zygo runs its sandboxes in` before it
stops anything. The VM has to be up for the supervisor to be told, so the
sequence is correct and only the narration is wrong. A stop that has to start
something first should not say so in the present tense.

**`zygo bench` now enters a scope, but the p99 it reports still fails.** This
is not a defect in the fix, which works; it is a consequence of A2 that will
greet every reader who runs the command the README points them at. Until A2 is
settled, `bench warm` on a default configuration prints `p99 < 10000 µs FAIL`.
Whatever is decided, the command should not be the first thing a newcomer sees
fail.

## Closed, and verified closed

Each was re-run rather than read:

* **`zygo bench` never entered a delegated scope** — now runs from an
  undelegated `session-N.scope` with no wrapper.
* **`zygo run <image>` with no command** — exits 0 exactly as Docker does,
  `--dry-run` resolves `argv: python3` from the image config, and
  `zygo run hello-world` works end to end on a real image.
* **`-v host:guest`** — answers `` `…` is a mount, not an image `` and names
  `--mount`. Does not misfire on `alpine:3.20`.
* **`stop --all` printed raw `limactl` output** — two lines now, from about
  thirty.
* **`--mem 64M` alone was refused** — accepted. An explicit oversized scratch
  still errors and now names a size that would fit.
* **The first traceback frame belonged to Zygo** — a raising handler opens at
  `/zygo/handler.py`, the user's own line.
* **The README did not say the warm path is unreachable from the macOS CLI** —
  it now says it plainly, and says the shim is working rather than failing.

Two items from the first pass were withdrawn rather than fixed, and correctly:
`zygo top` had already landed, and the unqualified drop-in claim is in the
design document rather than the README.

## No regressions

Re-run after the fixes, all still correct: mount, workdir, env and stdin
produce output byte-identical to Docker's; exit 42 passes through; a memory hog
against `--mem 128M` dies with 137 and an infinite loop against `--timeout 3s`
dies with 137; the default `network = "none"` refuses a raw socket; and
`--net egress --allow example.com:443` lets that host answer 200 while
`api.github.com` is blocked.

## Suggested order

1. `zygo image rm`. Nothing else makes `prune` meaningful, and it is the one
   Docker verb with no equivalent that users will reach for without thinking.
2. Key venvs on their requirements hash. Without it, a user who edits a pin
   pays for both copies until the image goes, which is never.
3. Settle A2 on bare metal, and make sure whatever is chosen leaves
   `bench warm` passing its own acceptance line on a default configuration.
4. Fix the `stop --all` narration.
