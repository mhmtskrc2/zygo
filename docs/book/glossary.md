# Glossary

One line each. The chapter in brackets explains the term in full; the numbers follow
[the book's contents](README.md).

| Term | Meaning |
|---|---|
| **ADR** | "Architecture decision record": a short note of one design choice, why it was made, and what it costs. [26] |
| **agent** | The small program inside a warm sandbox that loads your handler, forks per request and speaks Zygo's protocol. [6] |
| **backend** | Where Zygo draws the wall: `ns` (host kernel), `gvisor` (user-space kernel), `vm` (virtual machine). [6] |
| **bind mount** | Showing an existing file or folder at a second place; how your code gets into a sandbox. [4] |
| **blob** | A tar file stored by the API under its hash, sent into a request as its workspace. [17] |
| **blue/green** | Replacing a running thing by warming the new one first, then switching; `zygo up` does this. [16, 19] |
| **BPF** | A tiny, safe program format the kernel can run; seccomp filters are written in it. [4] |
| **capability** | One named piece of root's power, such as `CAP_NET_ADMIN`; a sandbox drops them all. [1, 4] |
| **cgroup** | A group of processes the kernel counts and limits together. [3] |
| **`cgroup.kill`** | A file that kills every process in a cgroup in one write. [3] |
| **chroot** | The old way to change a process's root folder; easy to escape, replaced by `pivot_root`. [4] |
| **clone3** | The syscall that starts a child process, optionally in new namespaces. [2] |
| **container** | A process with namespaces, a cgroup and filters on it, plus a tool's records about it. [5] |
| **controller** | The part of cgroups that handles one resource: memory, CPU, pids, I/O. [3] |
| **copy-on-write** | Sharing memory pages after a fork and copying one only when it is written. [1] |
| **daemon** | A program that runs in the background waiting for requests, such as `dockerd`. [5] |
| **delegation** | Giving a normal user one branch of the cgroup tree to manage. [3] |
| **dependency set** | A venv built once from lock files sent to `POST /deps`, for a runtime pool to use. [17] |
| **deploy rights** | Permission to create, change or destroy sandboxes through the API, not only call them. [17] |
| **digest** | `sha256:…`, the hash that names an image, a layer, a script or a blob exactly. [5, 15] |
| **egress** | Traffic going out of the sandbox; Zygo's `egress` mode allows only listed names. [4, 6] |
| **exec (`execve`)** | Replacing a process's program with a new one from disk. [1] |
| **fork** | Making a copy of the calling process. [1] |
| **gVisor** | A kernel written in Go that runs in user space and answers a sandbox's syscalls. [10] |
| **image** | A file system stored as layers, plus a little metadata; the OCI standard defines it. [5] |
| **jail** | FreeBSD's kernel object that confines a group of processes. [9] |
| **KVM** | The Linux feature that lets it run virtual machines using the CPU's hardware support. [10] |
| **Landlock** | A Linux feature that lets a process limit its own file and network access. [4] |
| **layer** | One tar file of changes in an image, named by the hash of its content. [5] |
| **Lima** | The tool Zygo uses to run a Linux virtual machine on a Mac. [11] |
| **MCP** | Model Context Protocol: how an AI agent host starts and talks to a tool server; `zygo mcp` is one. [17] |
| **microVM** | A very small virtual machine that boots fast, such as Firecracker's. [10] |
| **namespace** | A separate copy of one kind of kernel table — mounts, pids, network — for a group of processes. [2] |
| **`no_new_privs`** | A flag that stops a process and its children from ever gaining power, even through setuid. [4] |
| **OCI** | The Open Container Initiative, and its standards for images, registries and runtimes. [5] |
| **OOM killer** | The part of the kernel that kills a process when memory runs out; with cgroups, only inside the group. [3] |
| **operator** | Whoever runs the Zygo host; an operator token may act for any tenant. [17] |
| **OTLP** | OpenTelemetry's protocol for sending metrics to a collector. [17] |
| **outcome file** | The JSON `zygo run --outcome` writes, saying why a sandbox ended. [21] |
| **overlayfs** | A file system that stacks folders and shows them as one; how image layers become a root. [4] |
| **p50 / p99** | The median time, and the time 99 requests in 100 beat. [25] |
| **pasta** | A program that connects a network namespace to the host's network in user space, without root. [4] |
| **paused / cold** | A warm function after `idle_timeout` (frozen, still in memory) and after `cold_after` (dropped). [13] |
| **pid** | A process's number. [1] |
| **`pivot_root`** | Swapping a mount namespace's root for a new one, so the old root can be removed. [4] |
| **PSS / RSS** | Memory a process uses, with shared pages split between sharers (PSS) or counted in full (RSS). [7, 25] |
| **registry** | A server that stores images, such as Docker Hub or GitHub's. [5, 15] |
| **rlimit** | An old per-process limit, such as the number of open files. [4] |
| **root** | uid 0, the user that passes most permission checks. [1] |
| **rootless** | Running without root at any point, thanks to user namespaces. [2, 6] |
| **runtime pool** | Warm zygotes holding an interpreter and no code; each request brings its own script. [13] |
| **`sandbox.toml`** | The file that describes a project's functions, pools and API. [20] |
| **seccomp** | A filter on which syscalls a process may make. [4] |
| **setns** | The syscall that joins a namespace that already exists. [2] |
| **setuid** | A mark on a program that makes it run as its owner, often root. [4] |
| **supervisor** | Zygo's process that keeps zygotes, hands out requests and enforces deadlines, under your user. [6] |
| **syscall** | A request from a program to the kernel. [1] |
| **tenant** | One customer of whoever embeds Zygo; has its own scripts, secrets, limits and tokens. [14, 17] |
| **tmpfs** | A file system in memory that disappears when no longer used. [4] |
| **token** | A secret a caller sends to the API to prove who it is: operator or tenant. [17] |
| **uid** | A user's number; `uid_map` translates uids between a user namespace and the host. [1, 2] |
| **user space** | Everything outside the kernel: all normal programs. [1] |
| **warm-exec** | Zygo's mode where the sandbox is kept ready and each request is a new process entered into it. [6] |
| **workspace** | A folder of files sent in with one request, and optionally returned with the result. [17] |
| **`zygo.lock`** | The file `zygo up` writes to record which image digests and package versions were used. [20] |
| **zygote** | A process that has done its start-up and is forked for every request; the name comes from Android. [6] |

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [ADR 0005 — One warm zygote per script version](adr/0005-one-warm-zygote-per-script-version.md) · [Contents](README.md)
