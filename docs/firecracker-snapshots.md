# Firecracker snapshot/restore, as a v2 candidate

Phase 4 asks for *research*, not an implementation: the `vm` backend is
libkrun and is not built yet, and snapshots are the thing that would make a
second VM backend worth having. This is the reading, written down so the next
person starts where this stopped rather than at the beginning.

Nothing here has been measured on this project's hardware. Every number is
from Firecracker's own published material, and is labelled as such.

## What the feature is

A running Firecracker microVM can be paused, and its guest memory and device
state written out as two files: a memory image and a VM state file. A fresh
Firecracker process can then be started **from** that pair and resume the
guest exactly where it stopped — same processes, same open files, same heap.

The interesting consequence for Zygo: the snapshot can be taken once, after
the expensive part, and restored many times. A Python interpreter with numpy
imported, snapshotted at the moment it would have sent `READY`, is a file that
can be turned back into a running interpreter.

## Why it is interesting here, and why it is not phase 4

Zygo's warm path is `fork()` from a zygote: a request costs a page-table copy
and nothing else, measured at p50 1.9 ms on this project's own hardware. A
snapshot restore is a different shape of the same idea — pay the expensive
start once, reuse it many times — but at a coarser grain: it restores a whole
machine rather than forking a process.

| | zygote `fork()` | snapshot restore |
|---|---|---|
| unit | a process | a whole VM |
| measured here | p50 1.9 ms | — |
| published | — | Firecracker documents restore in the low hundreds of milliseconds, dominated by how much guest memory has to be faulted in |
| isolation | the host kernel | a VMM and KVM |
| state between uses | none: a fresh child each time | the snapshot's, which is the problem below |

So a restore is not a faster fork; it is a *safer* fork, for workloads where
the host kernel is not an acceptable boundary. That is the T3 column of the
threat model, and it is the same argument the `vm` backend already makes. The
case for building it is therefore "a hardware boundary at a start-up cost that
is not a cold boot", not "speed".

## The three problems to solve before it is a backend

**1. Every restore of one snapshot shares its entropy.** This is the one that
turns a performance feature into a security bug, and Firecracker's own
documentation is explicit about it: a guest restored twice from the same
snapshot has the same random state, the same session keys, the same
`/dev/urandom` pool. Firecracker exposes a virtio-rng device and a
`PATCH /machine-config` path for post-restore entropy; a Zygo backend would
have to reseed on every restore *before* any tenant code runs, and prove it
the way the Python agent's `_reseed_random` is proven — two requests, two
different values, asserted.

This is the same bug the reference agent had (see the fork-fallback tests) at
a different layer, which is a good sign the fix is well understood and a bad
sign for anyone who assumes it is handled.

**2. Time does not advance in a snapshot.** A restored guest believes it is
the moment the snapshot was taken. TLS certificate validation, token expiry
and anything that logs a timestamp are all wrong until the clock is corrected.
Firecracker documents a post-restore clock adjustment; it has to be part of
the restore path, not an afterthought.

**3. Snapshots are not portable, and Zygo's cache would have to know it.**
A snapshot is tied to the CPU model, the microVM configuration and the
Firecracker version that wrote it. The image store's cache key would need all
three, in the same spirit as the derived-layer key
(`base digest + arch + packages`). A snapshot restored on a host with a
different CPU feature set is undefined behaviour, not a slow path.

## What Zygo would have to build

Roughly in order, none of it started:

* a `vm` backend that works at all — phase 2.5, libkrun, blocked on KVM;
* a guest image carrying the agent, so the protocol runs over vsock inside the
  VM (the design's §3.9 already specifies vsock for this);
* snapshot-after-`READY`: take the snapshot at the moment the agent would
  announce itself, which is the equivalent of the zygote's post-import state;
* a snapshot cache keyed on CPU model, VM configuration and Firecracker
  version, with the same content-addressed shape the layer store uses;
* a restore path that reseeds entropy and corrects the clock **before** the
  guest runs tenant code, with tests that assert both rather than assuming;
* and a measurement, on real hardware, of restore against this project's own
  1.9 ms fork — because if restore lands where the published figures suggest,
  the backend is for isolation and not for latency, and the docs should say so
  rather than implying a choice that does not exist.

## The honest summary

Snapshots would give Zygo a hardware boundary without a cold boot per request,
which is worth having for untrusted code. They would not beat `fork()`, and
the three correctness problems above — entropy, time, portability — are the
kind that produce a system that works in a demo and leaks in production.
Recorded as a v2 candidate, behind a `vm` backend that has to exist first.
