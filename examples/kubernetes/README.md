# Zygo on Kubernetes

```bash
kubectl apply -f examples/kubernetes/deployment.yaml
kubectl -n zygo port-forward svc/zygo 7700:7700
```

Two replicas, an `emptyDir` for the image store, no `hostPath`, a readiness
probe that goes red while a pod drains, and a `preStop` that drains it. The
CI job `kubernetes` applies exactly this file to a fresh kind cluster on every
change and then runs a sandbox in it — the manifest is checked rather than
illustrated.

There are two manifests, and they differ in one thing: where the writable
cgroup a sandbox needs comes from.

| | `deployment.yaml` | `unprivileged.yaml` |
|---|---|---|
| the pod | `privileged: true`, root on the node | `hostUsers: false`: root of its own user namespace, an unprivileged uid on the node |
| the writable cgroup | from `privileged` | from a RuntimeClass whose containerd handler mounts it read-write; runc hands the pod its own cgroup and nothing else |
| the cluster needs | nothing beyond cgroup v2 | Kubernetes 1.33+, containerd 2.1+ with runc and the systemd cgroup driver, Linux 6.3+, and the handler on the nodes ([below](#without-privileged-true)) |
| checked by | the CI job `kubernetes` (kind) | the CI job `kubernetes-unprivileged` (k3s), `tests/linux/verify_k8s_unprivileged.sh` |

Use `unprivileged.yaml` wherever the cluster can have the handler.

## What `securityContext` is for

Zygo builds sandboxes, so the pod it runs in has to let it. Four things:

| | |
|---|---|
| `seccompProfile: Unconfined` | the default profile denies `unshare(CLONE_NEWUSER)`, which is what a sandbox does first |
| an unmasked `/proc` | a masked `/proc` is not *fully visible*, and the kernel then refuses a fresh `proc` mount inside a user namespace. `privileged: true` gives one; Kubernetes accepts `procMount: Unmasked` only with `hostUsers: false` |
| a writable cgroup v2 subtree | **no pod field expresses this**. A RuntimeClass can, since containerd 2.1 — `unprivileged.yaml`, [below](#without-privileged-true). Without one, it is the whole reason `privileged: true` and `runAsUser: 0` are in `deployment.yaml` |
| a kernel that allows unprivileged user namespaces | a node setting (`kernel.unprivileged_userns_clone`, AppArmor on Ubuntu), not a pod one |

`zygo doctor` names each one when it is missing, so a pod that will not build
sandboxes says which of the four it is rather than failing at the first
request.

**On `privileged: true`.** It is there for the third row, and brings the
second with it. `runAsUser: 0` goes with it: the pod's cgroup belongs to root,
and `privileged` gives capabilities to root only. No sandbox runs as that
root — each one is in a user namespace of its own. What that costs is smaller than it looks, and the reason is what the product is:
the boundary Zygo enforces is the sandbox it builds *inside* this container —
namespaces, seccomp, Landlock, a cgroup per request — not the container
itself. A privileged pod running customer code directly is a different
proposition from a privileged pod whose whole job is building boundaries
around it. Run it on nodes of its own, and read
[the threat model](../../docs/book/23-security.md).

The day Kubernetes could say "give this pod a delegated cgroup subtree", the
line was going to go. That day came from containerd rather than from a pod
field, and `unprivileged.yaml` is the manifest without it.

## Without `privileged: true`

```bash
# on every node that runs Zygo, as root
sh examples/kubernetes/node/base-spec.sh > /etc/containerd/zygo-base-spec.json
cp examples/kubernetes/node/zygo-runtime.toml /etc/containerd/conf.d/zygo.toml
systemctl restart containerd
```

```bash
kubectl apply -f examples/kubernetes/unprivileged.yaml
```

The node half is a containerd runtime handler, `zygo`: the default runc,
with two settings.

* **`cgroup_writable = true`** (containerd 2.1+). The pod's `/sys/fs/cgroup`
  is mounted read-write instead of read-only. In a pod with
  `hostUsers: false`, and with the systemd cgroup driver, runc then hands the
  pod's own cgroup to the uid the pod starts as — `cgroup.procs`,
  `cgroup.subtree_control`, `cgroup.threads` and `memory.oom.group`, the
  files the kernel lists in `/sys/kernel/cgroup/delegate`, and not
  `memory.max`. The pod can make a cgroup per request below its own and
  cannot raise its own limits, which stay the kubelet's. That is systemd's
  `Delegate=yes`, for a pod. **Never use the handler without
  `hostUsers: false`**: outside a user namespace runc hands nothing over, and a
  pod that is root on the node, with a writable cgroupfs, could rewrite its
  own limits.
* **`base_runtime_spec`**, for `/dev/net/tun`. `network = "egress"` needs the
  device, and a pod with its own user namespace cannot take it as a
  `hostPath` volume: the kubelet asks for an idmapped mount, and a device node
  refuses one (`failed to set MOUNT_ATTR_IDMAP on /dev/net/tun`). A device in
  the runtime's base spec is bound in by runc instead. `base-spec.sh` makes
  that spec from the installed containerd's own default, so run it again
  after upgrading containerd. Leave the line out of the handler if every
  sandbox runs with `network = "none"`.

The pod half, in `unprivileged.yaml`, against `deployment.yaml`:

| | |
|---|---|
| `runtimeClassName: zygo`, `hostUsers: false` | the cgroup, as above |
| `runAsUser: 0` | root of the pod's user namespace, not of the node: the cgroup is handed to the uid the container starts as, and writing a range of ids into each sandbox's user namespace takes `CAP_SETUID` in this one |
| `procMount: Unmasked` | the unmasked `/proc` that `privileged` used to bring; accepted only with `hostUsers: false` |
| `seccompProfile` and `appArmorProfile: Unconfined` | as in `deployment.yaml` |
| `capabilities.drop`, `allowPrivilegeEscalation: false`, `readOnlyRootFilesystem: true` | what `privileged` made pointless to say: the default capabilities without raw sockets, device nodes, low ports and audit records, and a root filesystem nothing writes to (`/tmp` is an `emptyDir`) |

Checked on k3s 1.36.5 (containerd 2.3.4, runc 1.4.2) on Linux 6.8, by
`tests/linux/verify_k8s_unprivileged.sh`, which the CI job
`kubernetes-unprivileged` runs: the pod is not privileged and its root is an
unprivileged uid on the node; `zygo doctor` finds the cgroup delegated and
egress possible; a sandbox runs; a request over its memory limit is killed
and the next one is served; an egress allowlist reaches what it names and
nothing else; the pod cannot write its own `memory.max`; an `exec` — the
liveness probe — still gets in after Zygo has built its tree. A Python
runtime pool, the kind an embedder serves, forks a fresh process per request
there with its CPU and peak memory reported.

What `zygo doctor` says when a piece is missing is written for a pod: a
read-only cgroupfs names the RuntimeClass and `hostUsers: false`, a writable
one that was not handed over names the systemd driver, and a missing
`/dev/net/tun` in a pod with its own user namespace names the base spec rather
than a `hostPath` it could not start with.

To run the check against a cluster of your own:

```bash
KUBECTL=kubectl IMAGE=ghcr.io/mhmtskrc2/zygo:latest PULL=IfNotPresent \
  sh tests/linux/verify_k8s_unprivileged.sh
```

## Rolling without dropping a request

`maxUnavailable: 0`, a `preStop` that calls `POST /drain`, and a
`terminationGracePeriodSeconds` longer than the longest request you allow.

The order matters and Kubernetes gets it right: the pod leaves the Service's
endpoints *before* `preStop` runs, so the drain is finishing work that was
already accepted rather than racing new arrivals. `/healthz` answers 503 from
the moment the drain starts, which is what takes it out of any load balancer
that is watching readiness rather than endpoints.

`in_flight: 0` in the drain's answer is a clean drain; anything else is the
grace running out, and the difference is worth logging.

## The image store

An `emptyDir`, because it is a cache: a rescheduled pod re-pulls what it
needs, and the things that are *not* a cache — tenants, tokens, sealed
secrets — belong in whatever database your control plane keeps, with the API
as the way in.

Two alternatives, both fine:

* a **PVC**, if re-pulling on reschedule is worse for you than a volume to
  manage;
* a **worker image** with the images baked in
  ([`packaging/oci/Dockerfile.worker`](../../packaging/oci/Dockerfile.worker)),
  which costs nothing at run time and makes a cold start a container start.

## What is not here

* **An Ingress.** The API is your control plane's, not the internet's; what
  belongs in front of it is a service of yours.
* **Autoscaling.** A warm pool is memory, not CPU. The useful signal is how
  many requests wait, and `GET /metrics` does not publish that yet; until it
  does, scale on `zygo_function_rss_bytes` and on the `429` answers your
  callers see.
* **A PodSecurityPolicy or a Gatekeeper constraint.** A cluster that enforces
  one will need an exception for this namespace, and writing somebody else's
  exception for them is how a manifest becomes wrong.
