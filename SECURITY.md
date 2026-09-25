# Security policy

Zygo runs other people's code on purpose. A bug that lets that code out of a
sandbox is the most serious kind of bug this project can have, and it is treated
that way.

## Reporting a vulnerability

**Do not open a public issue.** Use either channel:

* GitHub's private vulnerability reporting on this repository
  (*Security → Report a vulnerability*), which creates a private advisory
  only the maintainers can see;
* or e-mail **mhmtskrc2@gmail.com** with `[zygo security]` in the
  subject, for reporters who will not use GitHub.

Please include, as far as you have it:

* what you ran — the spec, the command, the image;
* the host: kernel version (`uname -r`), whether rootless, and the output of
  `zygo doctor`;
* what you got, and what you expected instead;
* a proof of concept, ideally as a shell script that fails on a fixed build.

**What to expect.** An acknowledgement within 3 working days, and an assessment
within 10. If it is a confirmed escape or privilege escalation, we will agree a
disclosure date with you, fix it, and publish an advisory naming you unless you
prefer otherwise. If we disagree that it is a vulnerability we will say why, in
writing, rather than closing it silently.

Zygo is pre-1.0 and has not had an external audit; that audit is
planned. **There is no bug bounty**: reports are answered, fixed and
credited, not paid for.

## Supported versions

Pre-1.0: only the latest release is supported. Fixes land on `main` and in the
next release; there are no backport branches yet.

## What is in scope

Anything that crosses one of the boundaries the project claims to hold:

| In scope | |
|---|---|
| Sandbox escape | tenant code reaching the host filesystem, host processes, or the supervisor |
| Cross-tenant leakage | one function reading another's memory, files, secrets or network |
| Privilege escalation | gaining capabilities, uids or cgroup control the spec did not grant |
| Policy bypass | reaching a network destination the `allow` list excludes; writing a read-only mount; exceeding a cgroup limit |
| Secret exposure | a secret value appearing in the zygote's memory, in `EXEC`, in logs, in `zygo spec explain`, or after the request that used it finished |
| Supervisor takeover | another user reaching the control socket or the HTTP API |
| Image handling | a crafted image writing outside its layer directory, or a digest being accepted without verification |
| Denial of the host | tenant code exhausting host memory, pids or disk past its cgroup |

### Out of scope, and why

* **Timing and microarchitectural side channels** (Spectre and relatives).
  Documented as out of scope in the design's threat model. Tenants that need
  this isolation belong on the `vm` backend and separate hosts.
* **`--allow-host-net`, `--allow-private-net`, `--allow-unlimited`, and
  `network = "host"`.** Each removes a guarantee, deliberately, and each has to
  be typed. Reports that these do what they say are not vulnerabilities.
* **`zygo shell`.** It is a debug tool that enters a sandbox you already own,
  without the seccomp filter, and it says so. It grants nothing the owner of
  the function does not already have.
* **Kernel vulnerabilities themselves.** Report those upstream. We do want to
  hear if Zygo's configuration makes an existing kernel bug reachable from a
  sandbox when a stricter default would not.
* **Anything requiring root on the host already.** Zygo is rootless; a root
  attacker has won before Zygo is involved.

## What the project already does about this

Every escape vector in the [threat model](docs/book/23-security.md) is **actually
attempted** by `make escape-linux` — 19 vectors in 27 checks, currently 26
blocked, 0 escaped, and 1 skipped where the test image has no setuid binary
to try. The rule the whole suite is
built on is that a test must attempt the thing rather than read a setting: a
test that checks a flag also passes on a kernel that ignores that flag.

`zygo doctor` reports what this host can and cannot enforce, rather than
failing obscurely — including when Landlock, `cgroup.kill` or a subordinate uid
range is missing, each of which weakens a boundary.

## Hardening the deployment

* Prefer `isolation = "vm"` for code you did not write. The `ns` backend leans
  on one kernel and does not hide it.
* Keep `network = "none"` unless a function genuinely needs egress, and keep the
  `allow` list to the hosts it needs.
* Do not run Zygo as root. It does not need it, and `pasta`, `newuidmap` and the
  cgroup delegation all behave better without it.
* Bind the HTTP API to loopback or a unix socket. It refuses to start
  unauthenticated on a reachable address, but the default is worth keeping.
* Install `uidmap` so tenants get distinct subordinate uid ranges rather than
  sharing one identity map.
