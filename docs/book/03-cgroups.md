# 3. Control groups

Namespaces change what a process can see. Control groups, or *cgroups*, limit
what it can use.

## What a cgroup is

A cgroup is a group of processes that the kernel counts and limits together.
You can say "this group may use 256 MB of memory, half a CPU and at most 64
processes", and the kernel holds every process in the group to it. A child
process starts in its parent's group, so a program cannot escape its limit by
forking. Without cgroups, one bad program could use up the whole machine;
with them, it can use up only its own share.

## The tree, as files

Cgroups are shown as folders under `/sys/fs/cgroup`. Each folder is a group,
and a folder inside it is a smaller group inside the bigger one. You create a
group with `mkdir`, move a process into it by writing its pid to
`cgroup.procs`, and set a limit by writing to a file such as `memory.max`.
There is no special tool and no daemon; it is plain file work. Limits nest: a
group can never use more than its parent allows, whatever its own files say.

```text
/sys/fs/cgroup/                                 the whole machine
├── system.slice/                               system services
└── user.slice/
    └── user-1000.slice/
        └── user@1000.service/                  ◀── handed to you (delegation)
            └── zygo.slice/                     memory.max = RAM − reserve
                ├── system/                     the supervisor, protected
                └── tenants/
                    └── acme/                   one customer's budget
                        └── resize/             one function: cpu.max, pids.max
                            └── g4242-1/        one warm sandbox
                                ├── zygote      the warm process: memory.max = mem
                                ├── req-01f3…   one request ─┐ each has its own group and
                                └── req-01f4…   one request ─┘ its own memory.max = mem
```

This is Zygo's real tree; `crates/zygo-core/src/cgroup.rs` describes each level.
The memory limit is on the leaves on purpose. The function's group holds the
warm process and every request at once, so a `memory.max` there would be one
budget for all of them, and a single request going over it would take the
others with it. On its own group, a request that goes over `mem` is killed
alone, and the warm process and the requests beside it go on.

## Controllers

Each kind of resource is handled by a *controller*. The ones a sandbox cares
about most are these:

| Controller | Limits | Example file |
|---|---|---|
| `memory` | RAM, and what happens when it runs out | `memory.max` |
| `cpu` | CPU time, as a share or a hard quota | `cpu.max` |
| `pids` | how many processes and threads | `pids.max` |
| `io` | disk reads and writes | `io.max` |

When a group goes over `memory.max`, the kernel's *OOM killer* ("out of
memory") kills a process in that group — not somewhere else on the machine.
When it hits `pids.max`, `fork` just fails, which is how a fork bomb is
stopped.

## Version 1 and version 2

Linux has two versions of cgroups. Version 1 had a separate tree for each
controller, which was flexible but confusing, and hard to hand to a normal
user safely. Version 2 has one tree for everything, with clearer rules. Most
current distributions use version 2 only. Zygo needs version 2; `zygo doctor`
checks for it and says so if it is missing.

## Delegation: cgroups without root

The cgroup tree belongs to root, so how can a normal user create groups? The
answer is *delegation*: the system gives a user one branch of the tree, and
the user can do what they like inside it. On a systemd machine every login
already has such a branch, under `user@<uid>.service`. Zygo creates its whole
tree inside that branch, which is why it needs no root at all.
[Troubleshooting](22-troubleshooting.md) covers the machines where the
branch is missing or missing a controller.

## Killing a whole group

Killing one process is easy; killing everything it started is not, because a
process can fork faster than you can list its children. Cgroup version 2 has a
file for this, `cgroup.kill`: write `1` to it and the kernel kills every
process in the group at once (Linux 5.14 and newer). Zygo uses it for every
deadline: when a request runs out of time, one file write ends the request
and all its children, and nothing is left over.

```text
  req-01f3/                      echo 1 > req-01f3/cgroup.kill
  ├── python  (the request)
  │   ├── sh                     ─▶  all of them, gone in one step,
  │   │   └── curl                   even ones forked a moment ago
  │   └── python (a worker)
  └── … (a fork bomb in progress)
```

## Counting, not only limiting

A cgroup also keeps numbers. `memory.peak` says the most memory the group ever
used, `memory.events` says whether the OOM killer fired, and `cpu.stat` says
how much CPU was used and how often the group was held back by its quota.
Zygo reads these after each request to report why it ended: out of time, out
of memory, or neither. Its benchmark also reads `cpu.stat`, and refuses to
judge a latency number when the group was held back, because that number
would describe the limit rather than the code.

## Freezing

The `cgroup.freeze` file stops every process in a group without killing it,
and a second write lets them carry on. The processes keep their memory; they
simply get no CPU time. Zygo uses this to *pause* a warm function that has
been idle for a while. Waking it again costs one write, much less than
starting it over.

## Namespaces and cgroups, side by side

| | Namespaces | Cgroups |
|---|---|---|
| Question they answer | What can this process see? | How much can this process use? |
| Unit | one kind of thing: mounts, pids, network… | one group of processes |
| Stop a fork bomb? | no | yes, `pids.max` |
| Hide the host's files? | yes, mount namespace | no |
| Needed for a sandbox | yes | yes |

Neither one limits which *syscalls* a process may call. That is the next
chapter.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [2. Namespaces](02-namespaces.md) · [Contents](README.md) · **Next: [4. The other locks](04-other-locks.md) →**
