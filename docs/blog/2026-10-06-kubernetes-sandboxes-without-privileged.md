---
title: "Sandboxes in Kubernetes without privileged: cgroup_writable and hostUsers: false"
published: false
description: "A pod that builds sandboxes needs a cgroup it can write. Kubernetes has no field for that. containerd 2.1 has one, and with a user namespace runc gives the pod its own cgroup and nothing more."
tags: kubernetes, containers, security, linux
canonical_url: https://github.com/mhmtskrc2/zygo/blob/main/docs/blog/2026-10-06-kubernetes-sandboxes-without-privileged.md
---

A pod that builds sandboxes — one that runs other people's code in its own namespaces, with a cgroup per request — usually runs as `privileged: true`. This post shows how that line can go away, and what was tested before it was trusted.

**The short version:** containerd 2.1 has a runtime-handler option called `cgroup_writable`. In a pod with `hostUsers: false`, runc then hands the pod its own cgroup: the files the kernel lists in `/sys/kernel/cgroup/delegate`, but not `memory.max`. The pod can create cgroups below its own. It cannot raise its own limit. This is systemd's `Delegate=yes`, for a pod.

## The problem

A sandbox needs four things from the pod around it:

1. User namespaces. The default seccomp profile blocks `unshare(CLONE_NEWUSER)`.
2. A fully visible `/proc`. A masked one makes the kernel refuse a fresh `proc` mount.
3. No AppArmor profile, on hosts that have one.
4. A cgroup v2 subtree it can write. Every sandbox gets a cgroup with its memory, pid and CPU limits.

Kubernetes has a pod field for the first three: `seccompProfile: Unconfined`, `procMount: Unmasked` (only with `hostUsers: false`), `appArmorProfile: Unconfined`. For the fourth there is no field. The CRI mounts `/sys/fs/cgroup` read-only in every container that is not privileged. That is why `privileged: true` is there, and it gives the pod far more than a cgroup.

## The fix

In December 2024 containerd added `cgroup_writable` (shipped in v2.1.0). For containers started through that handler, `/sys/fs/cgroup` is mounted read-write.

Alone, that would be dangerous: a container that is root on the node could write `max` into its own `memory.max`. What makes it safe is runc. When the container has its own cgroup namespace and a read-write cgroupfs, runc sets the cgroup's owner to the host uid that the container's uid maps to, and its systemd cgroup driver chowns the cgroup directory and the delegate files — `cgroup.procs`, `cgroup.threads`, `cgroup.subtree_control`, `memory.oom.group`, `memory.reclaim` — to that owner. `memory.max` stays root's.

In a pod with `hostUsers: false`, that owner is an unprivileged uid. Three parties, three settings:

| Who | Setting | Why |
|---|---|---|
| containerd handler | `cgroup_writable = true` | the mount is read-write |
| the pod | `hostUsers: false` | the owner is an unprivileged uid; outside a user namespace runc hands nothing over |
| runc | `SystemdCgroup = true` | the chown lives in the systemd driver, not in cgroupfs |

## The proof

Tested on k3s 1.36.5, containerd 2.3.4, runc 1.4.2, Linux 6.8. A user-namespaced pod without the handler:

```
/sys/fs/cgroup  ro   owner 65534 (the host's root, unmapped)
```

The same pod on a RuntimeClass with the handler:

```
/sys/fs/cgroup                rw   owner 0 (the pod's root)
/sys/fs/cgroup/cgroup.procs        owner 0
/sys/fs/cgroup/memory.max          owner 65534
$ mkdir /sys/fs/cgroup/child && echo ok
ok
$ echo max > /sys/fs/cgroup/memory.max
sh: can't create /sys/fs/cgroup/memory.max: Permission denied
```

The last two commands are the whole story. The pod can build below its cgroup. It cannot touch its own limit. The kubelet's memory limit is still the ceiling.

## The pieces

A containerd drop-in on the node (on k3s: `/var/lib/rancher/k3s/agent/etc/containerd/config-v3.toml.d/`):

```toml
[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.zygo]
  runtime_type = "io.containerd.runc.v2"
  cgroup_writable = true
  base_runtime_spec = "/etc/containerd/zygo-base-spec.json"   # for /dev/net/tun

[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.zygo.options]
  SystemdCgroup = true
```

A RuntimeClass named `zygo` with `handler: zygo`, and the pod:

```yaml
spec:
  runtimeClassName: zygo
  hostUsers: false
  containers:
    - securityContext:
        privileged: false
        runAsUser: 0            # root of the pod's user namespace, not the node's
        procMount: Unmasked
        seccompProfile: { type: Unconfined }
        appArmorProfile: { type: Unconfined }
        capabilities: { drop: [NET_RAW, MKNOD, NET_BIND_SERVICE, AUDIT_WRITE] }
        allowPrivilegeEscalation: false
        readOnlyRootFilesystem: true
```

`runAsUser: 0` is root of the pod's user namespace only. runc chowns the cgroup to the uid the container starts as, and mapping ids into each sandbox's namespace needs `CAP_SETUID` there.

## One thing that did not work

Networked sandboxes need `/dev/net/tun`. A `hostPath` volume does not start in a user-namespaced pod: the kubelet asks for an idmapped mount, and a device node refuses it (`failed to set MOUNT_ATTR_IDMAP on /dev/net/tun`). The handler's `base_runtime_spec` does the job instead: containerd's default spec plus the device, generated on the node.

```sh
ctr oci spec | jq '.linux.devices += [{"path":"/dev/net/tun","type":"c","major":10,"minor":200,"fileMode":438,"uid":0,"gid":0}]
  | .linux.resources.devices += [{"allow":true,"type":"c","major":10,"minor":200,"access":"rwm"}]' \
  > /etc/containerd/zygo-base-spec.json
```

## What was checked

A script runs against a fresh k3s in CI. It checks that the pod is not privileged; that a sandbox runs; that **a request allocating 200 MB inside a 64 MB request cgroup is killed and the next request is served**; that a fork bomb stops at the pid limit; that an egress allowlist reaches `example.com:80` and nothing else; that the pod cannot write its own `memory.max`; and that `kubectl exec` still works once the pod has passed controllers down. Fifteen checks, three runs, all green.

## What you need

- **Kubernetes 1.33+** (`hostUsers` and `procMount` on by default), **containerd 2.1+** with runc and the systemd cgroup driver, **Linux 6.3+**. crun was not tested.
- **A node you can configure.** On GKE Autopilot and many managed node pools the containerd config cannot be changed; there, `privileged: true` stays.
- **Never use the handler without `hostUsers: false`.** Outside a user namespace, runc hands nothing over and a root pod could rewrite its own limits.
- **This is not "restricted".** The pod runs seccomp and AppArmor unconfined; the sandboxes inside carry their own. Keep it on its own nodes.

## Where this came from

The manifest is from [Zygo](https://github.com/mhmtskrc2/zygo), a sandbox runtime that forks a warm interpreter into namespaces, seccomp, Landlock and a cgroup per request. Its Kubernetes example carried `privileged: true` with a long comment on why; the comment was out of date. The manifest, the node files and the check script are in [`examples/kubernetes/`](https://github.com/mhmtskrc2/zygo/tree/main/examples/kubernetes). Nothing here is specific to Zygo — a containerd option, a pod field and a chown in runc — so it should work for any pod that builds sandboxes: CI runners, code interpreters, nested containers.

*Sources: containerd [CRI config](https://github.com/containerd/containerd/blob/main/docs/cri/config.md) (`cgroup_writable`, v2.1.0); runc v1.4.2, `libcontainer/specconv/spec_linux.go`; [KEP-127](https://github.com/kubernetes/enhancements/tree/master/keps/sig-node/127-user-namespaces); [KEP-4265](https://github.com/kubernetes/enhancements/blob/master/keps/sig-node/4265-proc-mount/README.md).*
