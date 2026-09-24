# 5. Docker

Docker did not invent any of the parts in the last three chapters. What it
did was put them behind one simple command, and add a way to ship software
that the whole industry now uses.

## The problem Docker solved

Before 2013, putting a program on a server meant installing its language,
its libraries and its settings on that server, and hoping they matched the
developer's machine. Docker's answer was to ship the program *with* its whole
file system, as one thing you can copy, and to run it in namespaces so it did
not see or break the rest of the server. "It works on my machine" became
"it works in this image". The isolation was a side benefit; the packaging was
the revolution.

## Images and layers

An *image* is a file system plus a little metadata: which command to run,
which user, which environment. It is stored as a stack of *layers*, each a
tar file of changes on top of the one below. Two images based on the same
`python:3.12` share those lower layers on disk and on the network. A layer is
named by the hash of its content, so it can never change without getting a
new name. That is why an image pulled today by digest is the same, byte for
byte, as the one pulled next year.

```text
        myapp:1.0                  otherapp:2.3
    ┌─────────────────┐        ┌─────────────────┐
    │ COPY . /app     │        │ COPY . /app     │   each layer is named by
    ├─────────────────┤        ├─────────────────┤   the hash of its content:
    │ pip install …   │        │ pip install …   │   sha256:9f…, sha256:77…
    └────────┬────────┘        └────────┬────────┘
             └────────────┬─────────────┘
                 ┌────────▼────────┐
                 │ python:3.12     │   shared: stored and
                 ├─────────────────┤   downloaded once
                 │ debian:bookworm │
                 └─────────────────┘
```

## The Dockerfile

A Dockerfile is a recipe for building an image, one step per line:
`FROM python:3.12`, `RUN pip install requests`, `COPY . /app`. Each step runs
in a temporary container and its changes become a new layer. It is simple and
very widely known. The cost is that every change of a dependency means a new
build, a new image and a new push. [Chapter 6](06-how-zygo-works.md#images-without-a-dockerfile)
shows how Zygo avoids this.

## OCI: the standards

The *Open Container Initiative* turned Docker's formats into open standards.
The *image spec* says how an image and its layers are stored; the
*distribution spec* says how a registry serves them; the *runtime spec* says
how a folder plus a `config.json` becomes a running container. Because of
OCI, an image built by Docker runs under Podman, Kubernetes, or Zygo, and
comes from any registry: Docker Hub, GitHub, your own. Zygo reads OCI images
and uses none of Docker's own code.

## A container is a process

Run `docker run -d nginx` and then `ps -ef` on the host: nginx is there, as a
normal process with a normal pid. There is no box and no small machine. The
kernel sees a process with its own namespaces, in a cgroup, with a seccomp
filter and an AppArmor profile — exactly the list at the end of
[chapter 4](04-other-locks.md#putting-it-together). "Container" is a word for
that process plus the metadata Docker keeps about it.

## The chain behind `docker run`

`docker` itself does almost nothing. It sends your request over a socket to
`dockerd`, the daemon, which runs as root. `dockerd` asks `containerd` to
create the container. `containerd` starts a `containerd-shim`, which runs
`runc`. `runc` does the real work — namespaces, cgroup, mounts, filters — then
execs your program and exits. The shim stays, for as long as the container
lives, to hold its input, output and exit code.

```text
docker ─▶ dockerd ─▶ containerd ─▶ containerd-shim ─▶ runc ─▶ your program
 (CLI)    (root)      (root)        (stays)           (exits)
```

Five programs and three hand-overs for one command. Each exists for a good
reason — upgrades without stopping containers, Kubernetes, many clients — and
each costs time.

## Where Docker's time goes

`docker run` of a small Python script takes about 300 to 1000 milliseconds.
The isolation itself — the part `runc` sets up — is around one millisecond of
that. The rest is the chain above, creating the container's record and its
writable layer, setting up the network bridge, and then starting Python and
its imports from nothing. `docker exec` into a running container skips some
of this and still costs 50 to 100 ms. For a service that runs for weeks,
none of it matters. For a function that runs for 10 ms, it is almost all of
the cost.

```text
  one `docker run` of a 10 ms Python function, roughly
  0 ms                                                                ~500 ms
  ┌────────────────────────────────┬─┬───────────────────────────┬──┐
  │ docker → dockerd → containerd  │▒│ Python starts and imports │██│
  │ → shim; record, layer, network │▒│ its modules from nothing  │██│
  └────────────────────────────────┴─┴───────────────────────────┴──┘
                                    ▒ the isolation itself, ~1 ms
                                                       ██ your code, ~10 ms
```

## What a container leaves behind

A container is an *object*, not only a process. When the program exits, the
container stays in `docker ps -a`, with its writable layer on disk and its
logs in the daemon, until someone runs `docker rm`, or passed `--rm`. This is
right for services: you can restart them, read their logs, step inside. For
a job that runs once, it is something to clean up.

## Docker's defaults

Docker's defaults are made for running software you chose. The root file
system is writable, fourteen capabilities are kept, the process runs as root
inside, the network is a bridge that can reach your local network, and there
is no memory, CPU or process limit unless you ask. You can make a container
very tight — `--read-only --cap-drop ALL --network none --memory ...` — but
you have to know every flag. For running code you did *not* write, the
defaults point the wrong way. [The comparison](../comparison.md#the-defaults)
lists them side by side with Zygo's.

## Rootless Docker and Podman

The root daemon has always been Docker's weak spot: access to its socket is
the same as root on the host. *Rootless Docker* runs the whole chain as a
normal user inside a user namespace. *Podman* goes further: no daemon at all,
each container started by the `podman` command itself, with a small helper,
`conmon`, staying behind like the shim. Both are real steps forward. Both
still create container objects and still start your program from nothing, so
the per-run cost stays in the hundreds of milliseconds.

## What Docker is for

Docker is the right tool for building images, running long-lived services,
connecting them with networks, publishing ports and restarting them when they
fail. Zygo does none of that, and uses Docker's images happily. The question
this book cares about is narrower: what if the thing you run is a short
function, called thousands of times, written by someone you do not fully
trust? The next chapter is the answer Zygo gives.
