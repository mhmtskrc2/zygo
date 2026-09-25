# 16. Deploying and running in production

This chapter is about the day after the first demo: putting a project's
functions live, keeping them healthy, and changing them without dropping a
request. It also covers running Zygo inside a container and on Kubernetes,
choosing an isolation backend, and the things Zygo will not do for you.

## From a spec file to warm functions

*Deploying* here means one command, `zygo up`. It reads `sandbox.toml`, asks
the supervisor to warm every `[fn.*]` section, and writes `zygo.lock`. The
*supervisor* is the long-lived Zygo process that keeps the warm functions
([chapter 6](06-how-zygo-works.md#the-supervisor)).

```text
  sandbox.toml ──▶ zygo up ──▶ supervisor ──┬──▶ fn.resize   ✓ warm
  + your shell's                            ├──▶ fn.parse    · unchanged
    secret values                           └──▶ fn.fetch    ✗ failed, reason printed
                     │
                     └──▶ zygo.lock  (image digests, apt versions; commit it)
```

```bash
zygo up        # every [fn.*] warm
zygo down      # stops what this spec declares, and nothing else
```

## What `zygo up` prints

Functions come up in the order the file declares them, and each one gets a
line of its own. `✓` means started or replaced, `·` means unchanged and left
alone, and `✗` means it failed, with the reason. One failure does not stop
the others: a file with ten functions where the sixth cannot start still
brings up the other nine, and says which one failed. If any function failed,
`zygo up` exits with 1. `zygo up --json` prints one document with every
failure and its reason. The lines look like this (the numbers are only an
example):

```text
✓ resize — python, 38 MB, ready in 412 ms
· parse — unchanged (exec, 3 MB)
✗ fetch — fn.fetch.secrets: no value for STRIPE_KEY; set it in the environment …
```

## Run it again: only what changed restarts

Running `up` after an edit is not a second deploy. For each function, Zygo
compares four things with what is already running: the resolved spec, the
secret values, the bytes of the handler file and the bytes of the
requirements file. If all four are the same, the function is **unchanged**
and left alone — warm memory, request counters and all. If it was paused or
cold since the last deploy, `up` brings it back to warm. If anything
differs, it is replaced.

## Blue/green replacement

A changed function is replaced *blue/green*: the new version is started next
to the old one, and traffic moves only when the new one is ready. Requests
the old sandbox already accepted finish on it. Requests waiting in its queue
go to the new one. No request is dropped, and none sees a half-started
function.

```text
  time ──────────────────────────────────────────────────────────▶

  old (blue)   ████████████████████████▓▓▓▓▓ finishes what it accepted, then stops
  new (green)            ░░░░░░ warming ░░░░████████████████████████████████
                                           ▲
                                  switch: the new one is warm;
                                  queued requests go to it
```

## `zygo.lock` in a deploy

`up` records the image digest, apt versions and requirements hash for each
function in `zygo.lock`. Commit it. If a tag has moved under a spec nobody
edited, `up` refuses that function and prints both digests, so a deploy never
quietly runs different code; `zygo up --relock` accepts the move.
[Chapter 15](15-images-and-dependencies.md#zygolock-the-same-image-next-time)
explains it with a diagram. Remember too that `up` never pulls: pull the
images first.

## `zygo up` has no escape flags

`--allow-host-net`, `--allow-private-net` and `--allow-unlimited` do not exist
on `zygo up`. A deploy from a file should never loosen a wall just because
the file says so. A function that really needs one must be served by hand
with `zygo serve` and the flag typed by a person
([chapter 14](14-limits-network-secrets.md#loosening-by-name)).

## The supervisor

You rarely start the supervisor yourself. `zygo serve`, `zygo up`,
`zygo token`, `zygo secrets` and `zygo api` start it when none is running,
and give up if it is not ready within 10 seconds. It runs as your own user,
never as root. When it gets `SIGTERM` — the normal "please stop" signal from
systemd, Docker or Kubernetes — it stops taking new requests and gives the
running ones up to 25 seconds to finish. That is less than the 30 seconds
most process managers wait before they force a kill.

```bash
zygo supervisor status    # what it is, and where its socket lives
zygo supervisor stop      # drain, exit; the next serve or up starts a new one
zygo supervisor run       # run it in the foreground, to see why it will not start
```

`zygo supervisor` is a hidden command: it does not appear in `zygo --help`,
because you seldom need it.

## Watching it

```bash
zygo ps                      # what is warm, and its counters
zygo top                     # ps on a timer, plus rates
zygo stats resize            # latencies over the log window
zygo logs resize -f          # the zygote's output and every request
zygo logs resize --failed -n 20
```

`zygo stats` keeps two kinds of number apart and labels each: counters since
the function warmed up, and latencies over the log window. It refuses to
report a 99th percentile from fewer than a hundred requests, rather than
invent one. Over the API, `GET /metrics` gives counters in Prometheus
format ([chapter 17](17-api-sdk-mcp.md#metrics-otlp-and-usage-events)).

## Idle functions

A warm function holds memory. So Zygo lets unused ones step down, in two
stages. After `idle_timeout` (default ten minutes) with no request, the
function is **paused**: frozen in place, still in memory, and thawed by the
next request in a few milliseconds. After `cold_after` (default one hour) it
is **cold**: the sandbox is dropped, only the spec is kept, and the next
request pays the full warm-up again.

```toml
idle_timeout = "10m"
cold_after   = "1h"
```

```text
  request ──▶ WARM ──(idle_timeout: 10m)──▶ PAUSED ──(cold_after: 1h)──▶ COLD
               ▲                              │                            │
               └────── next request: ~ms ─────┘                            │
               └────── next request: pays the whole warm-up again ─────────┘
```

`zygo ps` shows the state of each function. To bring one back before a real
request arrives, call `POST /fn/<name>/warm` over the API, or
`client.warm(name)` in the SDKs.

## Capacity

Zygo's capacity is a budget for **one machine**. Each function runs
`concurrency` requests at once (default 4). Up to four times that many more
wait in a queue, for at most five seconds. Past that, Zygo answers *busy*:
HTTP `429` with the numbers, or exit code 75 from `zygo exec`. This is
*backpressure* — a polite "not now" — and not a failure. The request never
ran, so retrying it is safe and correct.

```text
  concurrency = 4

  running  [■][■][■][■]                         4 at once
  queue    [·][·][·][·][·][·][·][·]…[·]         up to 16 more, 5 s at most
  more     ──▶ 429 busy (exit 75): never ran, retry later
```

## Upgrading Zygo

A warm function cannot be carried across to a new Zygo binary. An upgrade is
always: drain, exit, start the new version, warm up again.
[ADR 0004](adr/0004-no-supervisor-reexec.md) is the study that explains why.
To drain over the API:

```bash
curl -X POST -H "Authorization: Bearer $ZYGO_API_TOKEN" \
    "http://127.0.0.1:7700/drain?grace_ms=60000"
```

This stops taking new requests, finishes the running ones, and exits. The
answer carries the number still running: `in_flight: 0` is a clean drain,
and anything else means the grace time ran out. The cost is about half a
second of warm-up per function afterwards. No request is dropped, as long as
something else is ready to take them — which is what two replicas and
`min_warm` are for.

```text
  replica A: ███ serving ███ drain ▓▓ exit │ start new ░ warm ░ ███ serving ███
  replica B: ███████████████ serving ███████████████████████████ drain ▓▓ …
                            ▲ B takes all the traffic while A upgrades
```

## Debugging a live function

```bash
zygo shell resize
zygo shell resize -- cat /proc/1/cgroup
```

`zygo shell` starts a new process inside the function's namespaces. The warm
zygote is not touched: it keeps its memory and keeps serving. The shell sees
the sandbox's files, processes, network and host name, and holds no
capabilities. It is on purpose **not** under the seccomp filter, the Landlock
rules or the tenant's cgroup: a debug shell that the memory limit kills is
no use to anyone.

## Calling it from a program

```bash
zygo api                     # listens on 127.0.0.1:7700, bearer-token auth
```

```python
import zygo
client = zygo.connect()
out = client.fn("resize")({"url": "..."}).result   # what the handler returned
```

The API starts **call-only**: a token can call the functions somebody
declared in a spec file, and nothing else. `--allow-deploy` adds serving,
stopping and one-shot runs, which together amount to a shell, not an API.
`ZYGO_API_TOKEN` is the operator's token. For a platform with customers,
`zygo token mint --tenant acme` prints a token that registers scripts and
calls functions **for that customer only**; the tenant comes from the token,
never from anything the caller sends. [Chapter 17](17-api-sdk-mcp.md) covers
the API and SDKs in full.

## Giving it to an agent

An AI agent can use Zygo through MCP, the Model Context Protocol, a standard
way for agents to call tools. The whole installation is one entry in the
agent's configuration:

```json
{ "mcpServers": { "zygo": { "command": "zygo", "args": ["mcp"] } } }
```

[Chapter 17](17-api-sdk-mcp.md#part-three-the-mcp-server) describes the tools it offers.

## Running Zygo inside a container

Zygo builds sandboxes, so a container it runs in must let it. It needs **no
privileges and no capabilities**, and `--privileged` is not the answer. The
container image asks for three things. A host with AppArmor needs a fourth,
and sandboxes with a network need a fifth. `zygo doctor` names each one that
is missing.

```bash
docker run \
  --security-opt seccomp=unconfined \
  --security-opt systempaths=unconfined \
  --security-opt apparmor=unconfined \
  --cgroupns=host --cgroup-parent=/zygo \
  -v /sys/fs/cgroup/zygo:/sys/fs/cgroup/zygo:rw \
  --device /dev/net/tun \
  -v zygo-data:/var/lib/zygo \
  -p 7700:7700 -e ZYGO_API_TOKEN=... \
  ghcr.io/mhmtskrc2/zygo
```

With Docker's `systemd` cgroup driver — the default on Ubuntu and Debian —
the parent must be a slice: `--cgroup-parent=zygo.slice` and
`/sys/fs/cgroup/zygo.slice`.

## The three things, and two more

```text
  ┌─ the container Zygo runs in ──────────────────────────────────────────┐
  │                                                                       │
  │  1. seccomp=unconfined      ─▶ may call unshare(CLONE_NEWUSER)        │
  │  2. systempaths=unconfined  ─▶ /proc not masked, so a fresh /proc     │
  │                                can be mounted in a user namespace     │
  │  3. its own cgroup subtree  ─▶ somewhere to put each sandbox          │
  │  4. apparmor=unconfined     ─▶ (AppArmor hosts) mounts not refused    │
  │  5. /dev/net/tun            ─▶ (egress / full) pasta's network card   │
  │                                                                       │
  │   ┌──────────┐ ┌──────────┐ ┌──────────┐                              │
  │   │ sandbox  │ │ sandbox  │ │ sandbox  │  ◀── the real boundary       │
  │   └──────────┘ └──────────┘ └──────────┘                              │
  └───────────────────────────────────────────────────────────────────────┘
```

| Need | Why | In Kubernetes |
|---|---|---|
| A seccomp profile that allows `unshare(CLONE_NEWUSER)` | Docker's default profile denies it, and it is the first thing a sandbox does | `securityContext.seccompProfile: {type: Unconfined}` |
| An unmasked `/proc` | runtimes cover parts of `/proc` (`kcore`, `acpi` …); the kernel then refuses a new `proc` mount inside a user namespace, because the old one is not *fully visible*. Sandboxes die on "mounting /proc failed: Operation not permitted" | `securityContext.procMount: Unmasked` |
| A writable cgroup v2 subtree of its own | every sandbox goes in a cgroup, and the container's `/sys/fs/cgroup` is read-only | no field exists; see below |
| No AppArmor profile, on AppArmor hosts | Docker's `docker-default` profile denies `mount`; `zygo doctor` reports "the mount tree could not be made private" | `securityContext.appArmorProfile: {type: Unconfined}` |
| `/dev/net/tun`, for `egress` and `full` | `pasta` gives a sandbox its network card through it, and runtimes leave the device node out | a `hostPath` of type `CharDevice` |

## Give it its own cgroup, not the host's

The usual advice is to mount all of `/sys/fs/cgroup` read-write. That works,
but it hands the container the **host's whole cgroup tree**: it could then
change limits on any cgroup on the machine, including other containers'.
That is worse than the privilege you were trying to avoid. Give Zygo a
subtree of its own instead, as the command above does with `/zygo`. Sealed
sandboxes — `network = "none"`, the default — do not need `/dev/net/tun`.

## The host's AppArmor still applies

A host's own AppArmor rules still reach into a container. Ubuntu's `passt`
profile attaches to the path `/usr/bin/pasta` and refuses the pid file Zygo
asks for. An image that installs `pasta` somewhere else on `PATH` is not
affected, and the Zygo image puts nothing at `/usr/bin/pasta` for exactly
this reason ([chapter 22](22-troubleshooting.md) has the error and the fix).

## Why this is safe enough

A container set up this way is no easier to escape than the host it runs on.
That is the point: the wall Zygo enforces is the one it builds *inside* — the
sandboxes — not the container around it. The test scripts
`poc/verify_supervisor.sh` and `poc/verify_api.sh` run in exactly this shape
in CI, unprivileged, on every change. `make verify-oci` builds the image and
runs a sandbox inside it the same way.

## The container image

```bash
make oci-image      # build it from the binary make already checked
make verify-oci     # build it and run a sandbox inside it, unprivileged
```

The published image is `ghcr.io/mhmtskrc2/zygo:<version>`, for `linux/amd64` and
`linux/arm64`. It is signed with [cosign](https://docs.sigstore.dev/) and
listed in the release's `SHA256SUMS`. It is Alpine plus a few programs:

| In the image | Why |
|---|---|
| `zygo` | the static musl binary, the one `poc/check_dist.sh` checked for size |
| `pasta`, `nft`, `tc` | what `network = "egress"` needs; without them egress is refused with a reason, never quietly opened |
| `newuidmap`, `newgidmap` | how a non-root user maps a *range* of user ids; without them every tenant maps to one host uid and that separation is lost |
| user 65532 | `nonroot`, the number distroless images use, with a 65536-wide range of sub-ids |
| **no Python, no Node** | each sandbox runs in an image of its own; what a handler can import comes from the image *it* names |

The data folder is `/var/lib/zygo` (`ZYGO_DATA_HOME`), a volume. The image
has a health check on `/healthz`.

## No Python in the Zygo image

That last row surprises people. The Zygo image does not need Python, because
your function never runs in it: it runs in `python:3.12-slim` or whatever
image the spec names, pulled into Zygo's store. If you want those images
already on the node, so a cold start is not a trip to the registry, build a
*worker image*: [`Dockerfile.worker`](../../packaging/oci/Dockerfile.worker)
runs `zygo pull` at build time and is a few lines long. It trades a bigger
image for no waiting; pull only the images you really serve.

## The image is call-only by default

The image's default command is `api --listen 0.0.0.0:7700`, with **no**
`--allow-deploy`. A token can call the functions somebody declared and
nothing else. Deploy rights over HTTP are a shell, and a default that hands
them out is one nobody reads the flag for. Pass `--allow-deploy` when the
thing in front of the API is your own control plane. The worker image does.

## Verifying a published image

```bash
cosign verify ghcr.io/mhmtskrc2/zygo:0.1.0 \
  --certificate-identity-regexp '^https://github\.com/.*/\.github/workflows/release\.yml@refs/tags/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

This is *keyless* signing: the signature is tied to the GitHub workflow that
built the image and the tag it was built from. There is no private key to
keep safe, and none to leak.

## Zygo on Kubernetes

[`examples/kubernetes/`](../../examples/kubernetes/) has a working manifest.
A CI job named `kubernetes` applies exactly this file to a fresh kind
cluster on every change and runs a sandbox in it, so the file is tested, not
just an illustration.

```bash
kubectl apply -f examples/kubernetes/deployment.yaml
kubectl -n zygo port-forward svc/zygo 7700:7700
```

```text
  namespace zygo
  ┌─────────────────────────────────────────────────────────────────────┐
  │ Secret zygo: api-token, secrets-key                                 │
  │ Service zygo :7700 ─────────┬──────────────────────┐                │
  │                             ▼                      ▼                │
  │  ┌─ pod 1 ─────────────────────────┐  ┌─ pod 2 ─────────────────┐   │
  │  │ zygo api --allow-deploy         │  │ (same)                  │   │
  │  │ securityContext: see below      │  │                         │   │
  │  │ /var/lib/zygo  emptyDir 20Gi    │  │                         │   │
  │  │ /run/zygo      emptyDir, memory │  │                         │   │
  │  │ readiness: /healthz             │  │                         │   │
  │  │ liveness:  zygo doctor --json   │  │                         │   │
  │  │ preStop:   POST /drain          │  │                         │   │
  │  └─────────────────────────────────┘  └─────────────────────────┘   │
  └─────────────────────────────────────────────────────────────────────┘
     replicas: 2 · maxUnavailable: 0 · maxSurge: 1 · no hostPath anywhere
```

The Secret holds two values: the operator's API token, from which every
customer token is minted, and the key that seals per-tenant secrets
([chapter 14](14-limits-network-secrets.md#the-tenant-secret-store)). The pod
asks for 1 CPU and 2 GiB, and sets a memory limit of 8 GiB but **no CPU
limit**: each tenant's CPU is already capped by a cgroup Zygo sets per
request, and a pod-wide CPU ceiling would also slow the supervisor itself.

## What `securityContext` is for, and `privileged: true`

A pod needs the same things as the container above. Three have fields of
their own; one is a node setting:

| | |
|---|---|
| `seccompProfile: Unconfined` | the default profile denies `unshare(CLONE_NEWUSER)` |
| `procMount: Unmasked` | a masked `/proc` stops a fresh `proc` mount in a user namespace |
| a writable cgroup v2 subtree | **no field says this**, and it is the only reason `privileged: true` is there |
| unprivileged user namespaces | a setting on the node (`kernel.unprivileged_userns_clone`, AppArmor on Ubuntu), not the pod |

`privileged: true` buys the third row and nothing else. It also happens to
bring `/dev/net/tun` and no AppArmor profile, which a pod without it would
have to ask for as the table above describes. The cost is smaller than it
looks, because the wall Zygo enforces is the sandbox it builds inside the pod
— namespaces, seccomp, Landlock, a cgroup per request — not the pod itself.
Still, run it on nodes of its own, and read [chapter 23](23-security.md).
The day Kubernetes can say "give this pod its own cgroup subtree", that line
goes, and nothing else changes.

## Rolling out without dropping a request

Three settings together: `maxUnavailable: 0`, so no pod stops before its
replacement is ready; a `preStop` hook that calls `POST /drain`; and a
`terminationGracePeriodSeconds` (120 in the example) longer than your longest
request. Kubernetes does these in the right order: it takes the pod out of the
Service *before* `preStop` runs, so the drain only finishes work already
accepted. `/healthz` answers 503 from the moment a drain starts, which also
takes the pod out of any load balancer that watches readiness.

```text
  kubectl rollout
     │
     ├─ start new pod ─▶ startupProbe /healthz passes (warm) ─▶ ready
     ├─ old pod removed from the Service's endpoints
     ├─ preStop: POST /drain?grace_ms=60000
     │      stop admitting · finish in-flight · reply {in_flight: 0} · exit
     └─ old pod gone          (terminationGracePeriodSeconds: 120)
```

Log the drain's answer: `in_flight: 0` is clean, and anything else means the
grace time ran out.

## Three probes, three questions

| Probe | Checks | Why |
|---|---|---|
| `startupProbe` | `GET /healthz`, every 2 s, up to 30 times | a warm pool takes as long as its interpreter; until this passes, the others cannot fail the pod |
| `readinessProbe` | `GET /healthz` | "should this pod get work?" It says no while draining, without killing the pod |
| `livenessProbe` | `zygo doctor --json` | "is this pod broken?" A liveness check on `/healthz` would restart a pod that is draining on purpose; `doctor` asks whether the *host* can still build sandboxes |

## The image store on Kubernetes

The store is an `emptyDir`, because it is a cache: a pod that moves to
another node pulls again what it needs. The things that are *not* a cache —
tenants, tokens, sealed secrets — belong in your control plane's database,
with the API as the way in. Two alternatives are both fine: a *PVC* (a
persistent volume), if re-pulling costs you more than a volume to manage, or
a worker image with the images baked in, which costs nothing at run time. No
`hostPath` appears anywhere, so pods can be scheduled freely.

## Trying it on kind

[`kind.yaml`](../../examples/kubernetes/kind.yaml) creates a one-node *kind*
cluster (Kubernetes in Docker) that can run the manifest. It needs cgroup v2
on the host, and it turns on the `ProcMountType` feature gate. Without that
gate the API server removes `procMount: Unmasked`, and every sandbox dies on
"mounting /proc". A real cluster does not need this file, but it needs the
same two things.

## What the Kubernetes example leaves out

- **An Ingress.** The API belongs to your control plane, not the internet.
  Put a service of your own in front of it.
- **Autoscaling.** A warm pool costs memory, not CPU, and the useful signal
  is `zygo_gate_queued` from `GET /metrics`, not CPU use. Scale on it once
  you know your workload.
- **A PodSecurityPolicy or Gatekeeper rule.** A cluster that enforces one
  will need an exception for this namespace, and that exception is yours to
  write.

## Choosing an isolation backend

```toml
isolation = "ns"     # ns | gvisor | vm
```

A *backend* is the kind of wall between the sandbox and the host. You change
it with one field; nothing else in the spec changes.

```text
  ns       your code ─▶ seccomp, Landlock, namespaces ─▶ host kernel
  gvisor   your code ─▶ gVisor (a kernel in user space) ─▶ host kernel
  vm       your code ─▶ guest kernel ─▶ KVM (hardware) ─▶ host kernel
           ─────────────────────────────────────────────────────────▶
           cheaper, and warm functions               stronger wall
```

| | `ns` | `gvisor` | `vm` |
|---|---|---|---|
| Wall | namespaces, cgroups, seccomp, Landlock; the host's kernel | a user-space kernel between you and the host's | a guest kernel under KVM |
| One-shot runs | yes | yes | yes |
| Warm functions | **yes, the only one** | refused | refused |
| Networking | yes | refused | refused |
| Setup | none | `zygo backend install gvisor` | a `--features vm` build and a guest kernel |

## The three backends in detail

**`ns`** uses one kernel, shared with the host, with every lock the kernel
offers turned on. Every number in this book is measured on it.
**`gvisor`** puts a kernel written in user space between the sandbox and
yours: a smaller attack surface, at a cost on every syscall. Warm functions
and networked sandboxes on it are refused with a reason, never weakened.
**`vm`** is a hardware wall: libkrun and KVM, with a guest kernel of its own.
On the Raspberry Pi 5 a one-shot run took about 420 ms, against about 73 ms
for `ns` on the same machine. The guest writes to a private layer, bounded by
`scratch`, and nothing it writes reaches the shared image or the next
sandbox. It has no networking and no warm functions yet.

## Installing a backend

```bash
zygo backend list        # what this host can actually use
zygo backend install gvisor
zygo run --isolation gvisor python:3.12-slim python3 -c 'import platform; print(platform.release())'
```

`zygo backend install gvisor` downloads gVisor's `runsc` from its official
release bucket on `storage.googleapis.com` and checks its sha512 before
unpacking it. The `vm` backend needs a Zygo binary built with
`--features vm` (or `make vm-build`) and a guest kernel file at
`<data>/backends/krun/Image`. There is no published kernel to download yet:
`zygo backend install vm` tells you to build one with `make vm-kernel` and
where to copy it. `zygo doctor` reports it once it is in place.

## Which backend to pick

Use `ns` for code you chose, or code you half trust. For code you did not
choose, use the strongest wall you can get. Read
[chapter 23](23-security.md) before you trust any of them with something
that matters — in particular the part about where the wall is weaker than it
looks. [ADR 0002](adr/0002-warm-paths-stay-on-ns.md) explains why warm
functions stay on `ns`.

## What Zygo will not do

This list is plain on purpose: a tool that is vague about its limits is worse
than one that lacks a feature.

- **Run on macOS or Windows natively.** Sandboxes are a Linux feature. On a
  Mac, Zygo manages a Linux VM for you.
- **Accept connections.** No mode of Zygo serves your traffic. A function is
  called through the CLI, the SDKs or Zygo's own HTTP API; you put your own
  ingress in front of that.
- **Scale past one machine.** Capacity is a budget per host, and a `429`
  past it.
- **Replace Docker.** Zygo uses OCI images and none of Docker's runtime. If
  you need `docker compose`, long-running services or published ports, you
  need Docker.
- **Analyse the code it runs.** Zygo contains hostile code. It does not tell
  you the code *was* hostile: there is no audit mode, no network log and no
  verdict.
- **Hide the kernel.** The `ns` backend shares one kernel with the host, and
  this book says so everywhere.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [15. Images and dependencies](15-images-and-dependencies.md) · [Contents](README.md) · **Next: [17. The HTTP API, the SDKs and MCP](17-api-sdk-mcp.md) →**
