# ADR 0002 — Warm functions stay on `ns`; `vm` and `gvisor` stay one-shot

**Status:** accepted, 2026-09-21. Supersedes the "not built yet" wording in
[`docs/comparison.md`](../comparison.md), the backends' own refusal messages
and the `## Status` section of the README.

## Context

Zygo has three isolation backends behind one flag: `ns` (namespaces, cgroups,
seccomp, Landlock), `gvisor` (a userspace kernel), and `vm` (libkrun, a
hardware boundary). All three run one-shot sandboxes from the same spec. Only
`ns` runs warm functions.

Both gaps are structural rather than unfinished:

* **The agent is handed its control socket as an inherited descriptor.** An
  OCI runtime closes everything but stdio, and a guest inherits nothing from
  the host at all. Warm functions on `gvisor` would need `runsc exec` in place
  of `setns` and a different way to reach the agent; on `vm` they would need
  the protocol carried over vsock and a supervisor inside the guest.
* **Networking** on `vm` would need the VMM inside Zygo's own network
  namespace and the nftables allowlist applied to a guest interface, which is
  a second implementation of `net/` with none of the first one's tests.

Each is weeks of work, and each would make a second warm path to keep correct,
to benchmark and to defend — against the one that the entire product rests on.

## Decision

Warm functions are an `ns` feature. `vm` and `gvisor` run one-shot sandboxes
and refuse warm modes with a reason, and that refusal is **the design**, not a
gap. `vm` networking is refused for the same reason.

This is the decision [`script_runtime.md`](../../script_runtime.md) records
under "Explicitly deprioritised", and it is written here so that the backends'
error messages can point at something rather than saying "yet".

## Consequences

* `--isolation vm` is a hardware boundary for work that fits a one-shot
  sandbox: an untrusted build, a single tool call, a job with an input and an
  output. Not for a warm function serving many requests.
* An embedder who needs a hardware boundary *per tenant* rather than per
  request is not served by Zygo today, and should read
  [`docs/comparison.md`](../comparison.md#microsandbox).
* The threat model's statement that `ns` is one kernel away from the host
  stands, and hardening it — uid separation, tenant-versus-tenant escape
  cases, the Landlock network rules — is where the effort goes instead.

## Reopening it

An embedder asking for a hardware boundary per tenant, with a workload where
100 ms of boot per request is affordable. That is a different product shape
from the warm fork and should be reasoned about as one, not added to this
backend because the flag already exists.
