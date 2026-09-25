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

## What `securityContext` is for

Zygo builds sandboxes, so the pod it runs in has to let it. Four things:

| | |
|---|---|
| `seccompProfile: Unconfined` | the default profile denies `unshare(CLONE_NEWUSER)`, which is what a sandbox does first |
| an unmasked `/proc` | a masked `/proc` is not *fully visible*, and the kernel then refuses a fresh `proc` mount inside a user namespace. `privileged: true` gives one; Kubernetes accepts `procMount: Unmasked` only with `hostUsers: false` |
| a writable cgroup v2 subtree | **no field expresses this**, which is the whole reason `privileged: true` and `runAsUser: 0` are here |
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

The day Kubernetes can say "give this pod a delegated cgroup subtree", the
line goes. Nothing else in the manifest changes.

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
* **Autoscaling.** A warm pool is memory, not CPU, and the useful signal is
  `zygo_gate_queued` rather than utilisation. `GET /metrics` is Prometheus
  text; scale on what it says once you know what your workload does.
* **A PodSecurityPolicy or a Gatekeeper constraint.** A cluster that enforces
  one will need an exception for this namespace, and writing somebody else's
  exception for them is how a manifest becomes wrong.
