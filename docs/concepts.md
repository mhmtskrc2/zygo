# Concepts

Zygo is built on eight principles. Each is a decision with a cost, and the
cost is stated here next to the principle, because a principle whose cost is
hidden is a slogan.

## P1 — A sandbox is a constrained process, not a container

Namespaces, a cgroup, seccomp and Landlock, set up in one process with no RPC.
`clone3` into seven namespaces, `pivot_root` onto the image, capabilities
dropped, a BPF filter installed, `execve`. There is no daemon to talk to, no
shim, no image-service round trip on the request path.

**Cost.** The boundary is the Linux kernel. A kernel privilege-escalation bug
defeats every control at once, which is why the `vm` backend exists for code
you did not write. `zygo doctor` and the [threat model](threat-model.md) say
this plainly.

## P2 — The sandbox waits warm

Nothing is built on the request path. For a compiled program the sandbox is
*held*: namespaces, mounts and hardening set up once, and each request is a
fresh process entered into them — about 2 ms. For an interpreter the sandbox
holds an *agent* that has already imported the handler, and each request is a
`fork()` of it — copy-on-write, so no copying, but a fresh process, so no
state carried over. Measured: p50 1.7 ms at 250 requests/s, on the machines
named in [what Zygo costs](performance.md#the-machines).

**Cost.** A warm function is resident memory — a Python zygote is ~35 MB. The
idle policy pauses it (frozen, still resident) after `idle_timeout` and drops
it after `cold_after`; the next request then pays the warm-up again.

## P3 — Isolation is one flag

`isolation = "ns" | "gvisor" | "vm"`. Same spec, same command, same protocol;
the backend decides where the boundary is drawn. `ns` is the kernel; `vm` is
KVM with a guest kernel; `gvisor` is a userspace kernel.

**Cost.** `ns` and `gvisor` are built; `vm` boots a guest and runs one-shot
sandboxes, at about ten times `ns`'s setup cost and without scratch, network
or warm functions. `zygo backend install gvisor` downloads the runtime, and the same
command run on both backends gives the same answer while `uname -r` inside
reports `4.19.0-gvisor` rather than the host's kernel — which is the point.

`gvisor` is one-shot only so far: a warm function needs to be *entered* per
request, which on `ns` is `setns` into namespaces the supervisor holds, and
gVisor's boundary is not those namespaces. It refuses rather than quietly
running something weaker, and so does a networked sandbox on it. Rootless
`runsc` also cannot write cgroups, so limits there are advisory, and it says
so on every start. For a warm function today, that means `ns`.

## P4 — OCI images, no Dockerfile

Any image from any registry is the filesystem. What a Dockerfile would add —
Python packages, apt packages — is expressed in the spec instead and built
*once*, as a venv or as a derived layer, inside the image, and shared by every
function that names the same thing. The image itself is never modified.

**Cost.** A derived layer is a copy of the image, an install, and a diff;
about five seconds the first time. Overlayfs would be faster, and rootless
overlayfs needs kernel 5.11, which not every host this runs on has.

## P5 — Rootless, daemonless

No root at any point, and no system service: the supervisor is a process in
the user's own session that `zygo serve` starts when it needs one. Egress
networking is `pasta` in userspace and nftables inside the sandbox's own
namespace — which works because entering a user namespace you created grants
a full capability set inside it, and nowhere else.

**Cost.** Some things need a program the host has to have: `pasta` and `nft`
for egress, `newuidmap` for distinct uid ranges per tenant. A missing program
is a refusal to start with the package named, never a silent downgrade.

## P6 — Secure by default

The network is off, the root is read-only, the capability set is empty, the
seccomp allowlist is on, and every limit has a value. Loosening any of it is a
flag you have to type — `--allow-host-net`, `--allow-private-net`,
`--allow-unlimited` — and the flag is not available to `zygo up`, so a spec
that needs one has to be served deliberately.

**Cost.** The first thing a new user hits is a limit. The errors name the field
and the fix.

## P7 — Every limit is mandatory

Memory, CPU, pids, wall-clock time, scratch space, open files, and for a
networked function connections and bandwidth: each has a default, and there
is no "unlimited" without the flag. The deadline kills the request's whole
process tree, not one pid — the per-request cgroup is what makes that one
write.

**Cost.** Disk I/O and bandwidth have no default limit, because a sensible
one depends on the device; both warn when absent.

## P8 — Honest about what is unverified

Every claim in the README is either measured or marked. The test suites
attempt the thing rather than reading a setting; a negative check first proves
the thing ran; a latency number says whether it hit a limit.
[What Zygo costs](performance.md) records what was measured and on what
hardware; several checks passed — or failed — for the wrong reason before
those rules existed.

**Cost.** The status paragraph is long, and it says "not built" more often
than a launch page would.

## The two warm modes, side by side

| | warm-exec (`cmd`) | agent (`entry`) |
|---|---|---|
| What is warm | the sandbox | the sandbox *and* a loaded interpreter |
| A request is | a fresh process entered into the sandbox | a `fork()` of the agent |
| Overhead | ~2 ms + the program's start | ~1.7 ms |
| Needs | nothing: any image, any language | an agent that speaks the [protocol](../spec/protocol.md); Python ships, Node and sh are in `examples/` |
| Use when | the runtime starts fast: Go, Rust, C, sh | starting the runtime is the cost: Python with imports, a JVM, Node with a dependency tree |
