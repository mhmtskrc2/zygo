# Roadmap

**Goal.** Be the runtime a workflow engine, a webhook platform or a SaaS with
customer-written plugins reaches for when it has to run someone else's script
— many times, cheaply, with a boundary it can explain.
[ADR 0001](docs/book/adr/0001-embedded-runtime.md) says who that is for, what
it costs, and the exit criterion each phase is held to.

What an embedder asks for, in the order they ask:

1. Run a script that lives in *my* database, not in a file on *your* disk.
2. Thousands of scripts, most of them idle: warm cost per runtime, not per
   script.
3. Tenant A must not see tenant B — files, secrets, network, memory, uid.
4. Cancel, stream output, run for minutes, hand files in and out.
5. Deploy inside my worker container on Kubernetes, unprivileged.
6. An API that does not move under me, and a boundary I can put in my own
   security page.

## Where it stands

| Phase | Unlocks | Status |
|---|---|---|
| 0 Prove the wedge | the decision to continue | done — [chapter 25](docs/book/25-performance.md) |
| 1 Runtime zygotes | thousands of scripts per host | done — [ADR 0005](docs/book/adr/0005-one-warm-zygote-per-script-version.md) |
| 2 The embedder API | building on Zygo without touching its disk | done — [chapter 17](docs/book/17-api-sdk-mcp.md), [`examples/plugin-host`](examples/plugin-host) |
| 3 Runtimes people embed | JavaScript embedders | done — Node agent; [ADR 0003](docs/book/adr/0003-no-deno-or-bun-agent.md) |
| 4 Runs where embedders deploy | running inside a worker pod | done — [`packaging/oci`](packaging/oci), [`examples/kubernetes`](examples/kubernetes), [ADR 0004](docs/book/adr/0004-no-supervisor-reexec.md) |
| 5 Multi-tenant hardening | passing a customer's security review | next |
| 6 Reference integrations | the first external production user | next |

## Next

**Phase 5 — multi-tenant hardening.** Done when the threat model has no
"untested" row for a multi-tenant claim.

- ~~Refuse to start multi-tenant without uid separation, instead of running
  "degraded".~~ Done: a tenant cannot be registered on a host without a
  subordinate uid range unless `ZYGO_ALLOW_SHARED_UID=1` says so.
- Tenant-against-tenant escape cases: A tries B's script, secret file,
  scratch, `/proc/<pid>` and loopback, each proving the positive path first.
- Resource isolation with a stated bound: a tenant at its pid limit, with a
  full scratch or a spinning CPU moves another tenant's p99 by no more than
  that.
- The seccomp matrix widened to what plugin authors import.
- The threat model rewritten as trusted operator / untrusted tenant / hostile
  tenant.
- An **external review**, scoped to the multi-tenant claims so it is
  affordable. None has been done yet.

**Phase 6 — reference integrations.** Done when one external project runs
Zygo in production for user scripts and says so.

- Windmill (a worker backend; today it uses nsjail), n8n (a Code-node
  runner), Temporal (an activity worker), and an adapter for the agent
  frameworks that let you bring your own execution.
- An embedding guide in the order an embedder meets things: install in a
  worker image, create a runtime, register a tenant, run, stream, cancel,
  bill, upgrade.
- Stability written down: semver for the HTTP API and the SDKs, protocol v1
  frozen, a deprecation window of two minor versions.

**Open source.** A contributor guide, a changelog, templates, supply-chain
checks, the book as a website, a `.devcontainer`, a WSL2 page
([chapter 11](docs/book/11-getting-started.md#on-windows-through-wsl2)), an
AppArmor profile for Ubuntu (`packaging/apparmor/`) and the raw benchmark
records (`bench/results/`) are in; still to come is the benchmark load
generator itself.

## Not planned

Services, ports and compose; warm functions on `vm` or `gvisor`; Windows; a
faster macOS shim; a hosted service. [ADR 0001](docs/book/adr/0001-embedded-runtime.md#what-is-deprioritised-and-why)
says why for each, and [ADR 0002](docs/book/adr/0002-warm-paths-stay-on-ns.md)
what would reopen the second.

Something you need that is not here? Open an issue: the order above follows
what embedders ask for.
