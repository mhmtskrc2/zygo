# 15. Images and dependencies

A sandbox needs files to run: an interpreter, libraries, and your own
packages. This chapter explains where those files come from — images, which
Zygo pulls from normal registries, and the layers it builds on top of them —
and how to keep the store tidy. There is no Dockerfile and no build step for
you to run.

## The big picture

A function's file system is an *image* from a registry, plus up to three
layers Zygo builds for you. The image is never changed. Each extra layer is
built once and then shared.

```text
   python:3.12-slim            from the registry; never changed
          │
          ├── + bytecode layer   .pyc files for the standard library     built on first pull
          ├── + system layer     apt packages: system = ["libwebp7"]     key: image + list
          └── + /venv            pip packages: requirements = "…"        key: image + file
                     │
                     ▼
   what the function sees as /   (read-only, except /tmp)
   shared by every function that names the same image and the same lists
```

## Images, layers, tags and digests

An *OCI image* is the standard package format for containers, the same one
Docker uses. It is a stack of *layers*: each layer is an archive of files,
and stacking them gives one file tree. A *registry* is a server that stores
images, such as Docker Hub or `ghcr.io`. A *tag* is a friendly name, like
`python:3.12-slim`, that the owner can move to a new version at any time. A
*digest* is the image's fingerprint, like `sha256:4f1b…`, and it never
changes: the same digest always means exactly the same bytes.

## Pulling an image

To *pull* is to download an image into Zygo's local store. Any OCI image from
any registry works.

```bash
zygo pull python:3.12-slim                 # into the local store
zygo pull --platform linux/amd64 alpine:3  # another CPU type than this host's
zygo images                                # reference, digest, layers, size, when pulled
zygo images --json                         # the same, for a program: match `reference` exactly
```

`--platform` takes `os/arch`. Without it, Zygo pulls the version built for
the host it runs on.

## When Zygo pulls for you

`zygo run` pulls an image the first time you use it, like `docker run`. Two
flags change that. `--pull never` refuses a missing image instead; the run
exits 1, and `--outcome` records `phase: plan`. `--pull always` pulls again,
for a tag that may have moved. `zygo serve` and `zygo up` **never pull**. A
deploy should not quietly depend on a registry being up, so they stop and ask
you to pull first.

| Command | Image missing | Image present |
|---|---|---|
| `zygo run` | pulls it | uses it |
| `zygo run --pull never` | refuses, exit 1 | uses it |
| `zygo run --pull always` | pulls it | pulls it again |
| `zygo serve`, `zygo up` | refuses: "pull it first" | uses it |

## The store

Zygo unpacks each layer **once**, into a *content-addressed* store: every
layer is saved under its own digest, so the same layer is never stored twice,
even when many images share it. A sandbox does not get a copy. The layers are
mounted read-only into it, stacked with overlayfs
([chapter 4](04-other-locks.md#layers-overlayfs-bind-mounts-and-tmpfs)). The
store lives in Zygo's data folder, `~/.local/share/zygo` by default
([chapter 21](21-environment-files-exit-codes.md) says how to move it).

```text
  store (on disk, once)                     sandboxes (no copies)
  ─────────────────────                     ─────────────────────
  layers/sha256:aa… ─┐
  layers/sha256:bb… ─┼── read-only mounts ─▶ sandbox 1   sandbox 2   sandbox 3
  layers/sha256:cc… ─┘                       (each stacks the same three layers)
  + the compressed download of each layer, kept beside it
```

## Private registries: `zygo login`

A private registry needs a user name and a password or token. `zygo login`
stores them:

```bash
zygo login ghcr.io -u you                                   # prompts, echo off
echo "$TOKEN" | zygo login ghcr.io -u you --password-stdin  # for CI
```

There is **no `--password` flag**, on purpose. A command-line argument is
visible to every process on the machine through `ps`, and it lands in your
shell history. Zygo checks the login against the registry *before* it saves
it, so a typo fails now, not at the next deploy. The credential goes into
Zygo's own `auth.json` in its data folder, readable only by you (mode 0600).

## Docker's logins are read too

If you already logged in with Docker, Zygo can use that. It reads
`~/.docker/config.json`, or the folder named by `$DOCKER_CONFIG`, and never
writes to it. When both have a login for the same registry, Zygo's own
`auth.json` wins.

```text
  a pull from ghcr.io needs a login
      │
      ├─ 1. <data>/auth.json          (written by `zygo login`)    ← wins
      └─ 2. ~/.docker/config.json     (or $DOCKER_CONFIG; read only)
```

## Dependencies without touching the image

What you would write in a Dockerfile goes in `sandbox.toml` instead. There are
two kinds of dependency, and neither changes the image:

```toml
requirements = "./requirements.txt"   # Python packages, into a venv
system       = ["libwebp7"]           # apt packages, into a layer
```

## Python packages: `requirements`

A *venv* (virtual environment) is a folder that holds a set of Python
packages apart from the system's. Zygo builds one from your
`requirements.txt`, **inside a sandbox**, with the image's own `pip`. That is
the only way to be sure the packages match the Python that will run them. The
venv is mounted read-only at `/venv`, with `/venv/bin` first on `PATH`. On the
Docker Desktop VM used for the measurements, the first build took about
4 seconds and a second function reusing it about 0.1 seconds
([chapter 25](25-performance.md) has the numbers and the machine).

## System packages: `system`

Some Python packages need a C library from the operating system, such as
`libwebp7` for WebP images. `system` lists Debian (apt) packages, optionally
with a version: `"libpq5=16.4-1"`. Zygo installs them inside a writable copy
of the image, compares the result with the original, and saves the
difference as a new OCI layer of its own. There is no Dockerfile, and nothing
else is rebuilt.

```text
  image layers (read-only) ──▶ writable copy ──▶ apt-get install libwebp7
                                                        │
                                  diff: only the files apt added or changed
                                                        ▼
                                          a new layer, "<image>+system.<key>"
                                          stacked on the image for this function
```

## How the caches are keyed

Both kinds are built once and shared. Each is saved under a *key*: the image's
digest plus the list or the file's bytes. Every function that names the same
image and the same list uses the same build. Edit the list and you get a new
key, so a new build. A different image is also a new key, because a package
built for one Python often fails with a confusing error on another.

| Built | Keyed on | Shared by |
|---|---|---|
| the venv | image digest + the bytes of `requirements.txt` | every function and every run with that pair |
| the system layer | image digest + the `system` list | every function with that pair |
| the bytecode layer | image digest | everything that uses the image |

## One-shot runs share the same cache

`zygo run` can use a requirements file too:

```bash
zygo run --requirements ./requirements.txt python:3.12-slim python3 -m pytest
```

The first job with a given image and file builds the venv. Every job after
that, and every warm function with the same pair, reuses it.

## The Python bytecode layer

Python turns each `.py` file it imports into *bytecode* — a faster form it
can run — and normally saves it as a `.pyc` file for next time. The official
`python:*-slim` images ship no `.pyc` files at all: `python:3.12-slim` has
1097 `.py` files in its standard library and none compiled. A sandbox's root
is read-only, so Python cannot save them either. So every run compiled every
module again; `import re` alone took 34 ms.

## How the bytecode layer works

The first time Zygo pulls or runs such an image, it compiles the standard
library once, inside a sandbox, into a layer of its own. For
`python:3.12-slim` that took about 3.2 s and made 18.6 MB. The
layer is stacked on the image, so the `.pyc` files sit next to the sources.
They are marked *unchecked*: a layer never changes, so there is nothing to
check them against. On the Lima VM used for testing, a script that imports ten common modules
(`re`, `json`, `urllib.request` and more) went from 165 ms to 35 ms
([chapter 25](25-performance.md) has the table).

```text
  without the layer                         with the layer
  ─────────────────                         ──────────────
  import re                                 import re
    └─ read re.py, compile it  (every run)    └─ read re.cpython-312.pyc  (ready)
    └─ cannot save .pyc: root is read-only
```

## When there is no bytecode layer

An image that already has bytecode, or has no Python, is used as it is. If
the build fails, Zygo prints a warning and uses the original image; a run
never fails because of it. `ZYGO_BYTECODE=0` turns the layer off.

## Nix: not built

`sandbox.toml` accepts a `nix` field, so the file format is ready for it, but
Zygo does **not** build Nix packages yet. Serving a function that sets `nix`
fails with a clear error. Use `system` or `requirements` for now.

## `zygo.lock`: the same image next time

A tag can move: `python:3.12-slim` next month is not the same bytes as today.
`zygo up` writes `zygo.lock` next to the spec. It records the digest each
`image` resolved to, the versions apt chose for each `system` package, and
the hash of each requirements file. **Commit it** to version control.

```text
  zygo up
     │
     ├─ spec edited for this function?      yes ─▶ serve it, rewrite its lock entry
     │                                       no
     ├─ image digest same as zygo.lock?     yes ─▶ serve it
     │                                       no
     └─ refuse this function, print both digests
          → zygo up --relock   accepts the new image
```

Editing the spec re-locks that function without asking, because you just
asked for the change. [Chapter 20](20-sandbox-toml.md#zygolock) has the rules
in the reference.

## Removing an image: `zygo image rm`

```bash
zygo image rm python:3.11-slim
```

This removes the image, the system-package images derived from it, and then
anything that only they kept alive. It is **refused while a warm function
uses the image**, because that function's root file system *is* those layers,
mounted. Stop the function first.

## Pruning the store

*Pruning* deletes what is no longer needed. Start with `--dry-run`, which
only reports.

```bash
zygo image prune --dry-run
zygo image prune                          # only what nothing can reach any more
zygo image prune --unused-for 30d --blobs
```

| Command | What it deletes |
|---|---|
| `prune` | layers of removed images; venvs, flattened roots and derived layers whose image is gone |
| `--unused-for 30d` | also venvs and flattened roots not used for 30 days, even if their image is still here |
| `--blobs` | also the compressed copy kept next to every unpacked layer |
| `--dry-run` | nothing: it prints what it would delete |

A *flattened root* is all of an image's layers copied into one folder, which
Zygo makes on kernels that cannot stack layers for a normal user. Every use of
a cache is recorded, so a venv used yesterday is safe even at
`--unused-for 7d`. `--blobs` roughly halves the store. The cost is a new
download if an unpacked layer is ever lost.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [14. Limits, networking and secrets](14-limits-network-secrets.md) · [Contents](README.md) · **Next: [16. Deploying and running in production](16-production.md) →**
