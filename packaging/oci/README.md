# The container image

```bash
make oci-image      # build it from the binary make already checked
make verify-oci     # build it and run a sandbox inside it, unprivileged
```

`ghcr.io/zygo/zygo:<version>` for `linux/amd64` and `linux/arm64`, signed with
[cosign](https://docs.sigstore.dev/) and listed in the release's
`SHA256SUMS`. It is Alpine plus five things: the static `zygo` binary,
`pasta`, `nft`, `tc` and `newuidmap`.

## Running it

Zygo builds sandboxes, so the container has to let it — **not** by being
privileged, but by three specific things:

```bash
docker run \
  --security-opt seccomp=unconfined \
  --security-opt systempaths=unconfined \
  --cgroupns=host --cgroup-parent=/zygo \
  -v /sys/fs/cgroup/zygo:/sys/fs/cgroup/zygo:rw \
  -v zygo-data:/var/lib/zygo \
  -p 7700:7700 -e ZYGO_API_TOKEN=... \
  ghcr.io/zygo/zygo
```

`zygo doctor` names each one when it is missing, and
[`docs/guide.md`](../../docs/guide.md) explains why each is needed — briefly:
Docker's default seccomp profile refuses `unshare(CLONE_NEWUSER)`, a masked
`/proc` makes the kernel refuse a fresh `proc` mount inside a user namespace,
and a sandbox needs a cgroup to be put in.

**Give it a cgroup subtree of its own.** The common advice is to bind the
whole of `/sys/fs/cgroup` read-write; that works and hands the container every
cgroup on the host, which is worse than the privilege it was avoiding.

## What is in it, and what is not

| | |
|---|---|
| `zygo` | the static musl binary, the one `poc/check_dist.sh` size-checked |
| `pasta`, `nft`, `tc` | what `network = "egress"` needs. Without them egress is refused with a reason rather than quietly opened |
| `newuidmap`, `newgidmap` | how a non-root uid maps a *range*. Without them every tenant maps to one host uid and the uid-level separation is gone |
| uid 65532 | `nonroot`, the number distroless uses, with a 65536-wide subordinate range |
| **no Python, no Node** | a sandbox runs inside an image of its own. What a handler can import comes from the image *it* names, not from this one |

That last row is the one that surprises people. If you want images cached on
the node so a cold start is not a registry round trip, build a worker image:
[`Dockerfile.worker`](Dockerfile.worker) is the pattern, and it is six lines.

## The default is call-only

`CMD` is `api --listen 0.0.0.0:7700` with no `--allow-deploy`: a token reaches
the functions somebody declared and nothing else. Deploy rights over HTTP are
a shell, and a default that hands them out is one nobody reads the flag for.
Pass `--allow-deploy` when the thing in front of the API is your own control
plane.

## Verifying a published image

```bash
cosign verify ghcr.io/zygo/zygo:0.1.0 \
  --certificate-identity-regexp '^https://github\.com/.*/\.github/workflows/release\.yml@refs/tags/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Keyless: the signature is bound to the workflow that built it and the tag it
was built from, so there is no key to keep and none to leak.
