# Changelog

Everything a user would notice, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/) — before 1.0, a minor version may
break things and will say so here.

## [Unreleased]

### Changed

- Chapter 10's Windmill comparison is the 6 October 2026 round: Zygo 0.1.6
  as released in Windmill's workers in nsjail's place, without and with a
  network per job, per job and under the whole stack, and what a job can see
  and reach under each — level with nsjail per job without a network, about
  7 ms of CPU more with one, and a cgroup, seccomp, Landlock and a root of its
  own for every job either way. The 24 September round it replaces ran with
  the network off and the chapter did not say so; its stand-in copy and
  results are no longer kept. The stand-in, the probe, the per-job scripts
  and every result line are in `bench/results/2026-10-06-zygo-in-windmill/`.
  The one set its tables leave out — a minute in which every job on every
  worker ran slower — was examined afterwards from Windmill's own job records
  and the VM's logs; nothing in Zygo changed when it stopped, and the record
  says what was checked. The harness gained a per-container CPU sampler.

## [0.1.6] — 2026-10-06

The first release in which `zygo mcp`'s `run_code` runs: every earlier tag
refused it, because the server mounted its workspace where the sandbox keeps
its own files. With it, Zygo on Kubernetes without `privileged: true`,
warm-exec requests that report what they cost, and a `zygo_sdk` on Hex that
will not resolve a mint with the 2026 advisories.

### Added

- Zygo on Kubernetes without `privileged: true`:
  `examples/kubernetes/unprivileged.yaml`, a pod with `hostUsers: false` on a
  RuntimeClass whose containerd handler (`examples/kubernetes/node/`, containerd
  2.1+) mounts its cgroup read-write and carries `/dev/net/tun`. runc then
  hands the pod its own cgroup and not its limits. Checked on k3s 1.36 by
  `tests/linux/verify_k8s_unprivileged.sh` — sandboxes, per-request memory
  limits, an egress allowlist, and a pod that cannot raise its own
  `memory.max` — which a new CI job, `kubernetes-unprivileged`, runs.

### Changed

- The Elixir client, `zygo_sdk`, requires mint 1.10.2 or newer. Every 1.x
  release before it carries three 2026 advisories (EEF-CVE-2026-91043,
  -92103, -94194), and the HTTP/1 chunked-framing one is on the path this
  client uses. A floor, not a pin: an application's own resolution still picks
  the newest it can.

### Fixed

- A warm-exec request reports what it cost. `cpu_ms` and `peak_rss_kb` were
  `0` for every request of an exec pool, in the call's answer, in the usage
  events an embedder bills from and in `zygo stats`; only `wall_ms` was
  measured. They now come from the request's own cgroup — `cpu.stat` and
  `memory.peak`, read after it exits and before the cgroup goes — and, for a
  request that had no cgroup, from the helper's `wait4` usage, which counts
  the request it waited for.
- `zygo doctor` gives a pod a pod's advice. A Kubernetes pod whose cgroup was
  read-only was told to `docker run --cgroup-parent`; it is now told about the
  RuntimeClass and `hostUsers: false`, or about the systemd cgroup driver when
  the cgroup is writable and was not handed over. A pod with its own user
  namespace and no `/dev/net/tun` was told to mount the device as a `hostPath`
  volume, which such a pod does not start with; it is now told to put the
  device in the runtime handler's base spec.

- `zygo mcp`'s `run_code` works. It mounted its workspace at `/work`, which
  every sandbox keeps for per-request files, so every call was refused with
  "`/work` is managed by the sandbox". The workspace is now at `/workspace`,
  which is also the program's working directory.
- `zygo mcp`'s `run_code` marks a sandbox that never started as `isError`,
  and says that changing the code will not help. It used to come back as an
  ordinary result, the launcher's refusal under `--- stderr ---` and
  "Exit code 1", which reads as the program's own failure. A program that
  ran and failed is still an ordinary result, as before.

## [0.1.5] — 2026-09-27

The code is v0.1.4's. This release carries the SDK READMEs that say what has
to be installed and running before their examples work, to the package pages
on PyPI, npm and Hex, which show the README of the version published. It is
also the first tag whose workflow fetches `ex_doc` before publishing to Hex;
v0.1.4's Hex upload failed there and was made by hand.

### Fixed

- The Python, Node and Elixir clients' READMEs, which are their package pages,
  now say the package is only a client and what has to be done before the
  first example runs: install `zygo`, pull the image (the API never pulls
  one), set `ZYGO_API_TOKEN` (`zygo api` will not start without it) and start
  the API with `--allow-deploy`. On a Mac, the API started in the VM answers
  on the Mac at the same address.

## [0.1.4] — 2026-09-27

Four security fixes are the reason to upgrade: a request can no longer write
to the supervisor as its agent, a workspace sent back can no longer carry the
supervisor's files, a sandbox can no longer reach a service the host bound to
loopback only, and a host without uid separation no longer becomes
multi-tenant quietly. The Elixir client, `zygo_sdk`, is released from a tag
for the first time; the 0.1.3 on Hex was published by hand before its code
was committed.

### Security

- A request's own code can no longer write to the supervisor. The Python
  agent forked each child with a copy of its control socket still open, so
  handler code could send a `DONE` of its own for another request's id — in
  a runtime pool, another tenant's — or a broken frame that ended the agent
  and every request in it. The child now closes its copy before anything
  else runs; the example `sh` agent closes it for the handler too. The Node
  agent's workers never had it. The rule is now §3.13 of `spec/protocol.md`
  and a question in "Fork safety", and the agent's tests try a forged
  `DONE` from a handler.
- A workspace sent back with `?out=1` can no longer carry the supervisor's own
  files. It was packed by path through `/proc/<pid>/root`, where an absolute
  symlink resolves against the supervisor's root, not the sandbox's: a
  handler that left a process running could swap a folder, or the workspace
  itself, for a link between the check and the read. The workspace is now
  walked by descriptor with `O_NOFOLLOW` at every step, a name is packed
  only if it is still a folder or a regular file when opened, and a file
  contributes no more bytes than were counted against the cap (chapter 17).
- An `egress` or `full` sandbox can no longer reach a service its host
  bound to `127.0.0.1` only. `pasta`, at its defaults, forwarded a connection
  to the sandbox's own loopback on to the same port on the host's loopback,
  and answered for the gateway address itself and handed those connections
  to the host as well. The firewall inside the namespace passes loopback, so
  the `127.0.0.0/8` rule never saw either. On Linux 6.7+ Landlock still held
  the door to the allowlist's port numbers, so `allow = ["example.com:8443"]`
  reached the host's `127.0.0.1:8443`; below 6.7 every loopback port the
  host had bound was open. `pasta` now runs with `--tcp-ns none --udp-ns
  none --no-map-gw`. A service on the host that a sandbox should reach must
  listen on a second address the host has, such as a Docker bridge's, and be
  allowed with `--allow-private-net`; not the host's main address, which
  `pasta` gives the sandbox too (chapter 14). Escape case 19 attempts it three ways.
  Found by the n8n task-runner round.
- A host whose user has no subordinate uid range no longer becomes
  multi-tenant quietly: `POST /tenants` is refused there, with the fix in
  the message, unless the supervisor runs with `ZYGO_ALLOW_SHARED_UID=1`.
  Before, every tenant's sandbox ran as the same host uid and only `zygo
  doctor` said so.
- The secret store's key is held once, wiped from memory when the supervisor
  drops it, and can no longer be copied.

### Changed

- A function, and a `zygo run` sandbox, may open a TCP listener on its own
  loopback. Landlock's `bind` refusal is kept only where the namespace is
  shared between tenants — a runtime pool — and a sealed function
  (`network = "none"`) is no longer refused `connect` to itself. Nothing from
  outside reaches a port a sandbox opens, on any kernel: `pasta` forwards
  none in. Programs that talk to themselves over `127.0.0.1` — Jupyter, Ray,
  a headless Chrome, n8n's runner launcher with its health-check port — now
  run in a sandbox on 6.7+ as they already did on older kernels. Under
  `egress`, connecting to one's own listener still meets the allowlist's
  port rule (chapters 13, 14, 23; ADR 0008). Escape case 20 attempts a
  listener in a pool.

### Added

- **An n8n Code-node runner**, [`examples/n8n-runner`](examples/n8n-runner):
  it speaks n8n's task-runner protocol for JavaScript and Python and runs each
  task as a fork in a Zygo runtime pool. Beside it, the harness that measures
  it against n8n's own runners behind the same n8n — latency, bursts, CPU and
  memory per part, the first run after idle, and six hostile Code nodes — so
  the tables in chapter 25 can be repeated with `sh run.sh`. `make test` runs
  the runner's script wrapping for real. It lacks n8n's RPC helpers and
  binary data, and says so.

- `zygo doctor` has a `systemd OOM policy` check, and `zygo api` warns at
  start, when the systemd unit holding the sandboxes has `OOMPolicy=stop`
  (systemd's default): one request over its `mem` limit, killed by the kernel
  inside its own cgroup, then stops the whole unit — the API, the supervisor
  and every pool — and every later request is refused. Chapter 16 now has a
  unit file with `OOMPolicy=continue` and `Delegate=yes`, and chapter 22 the
  journal lines this looks like.
- An Elixir client, `zygo_sdk` on Hex, in `sdk/elixir`. Every route the
  Python and Node clients call, as `Zygo.call/4` and the rest: each returns
  `{:ok, value}` or `{:error, %Zygo.Error{}}` and has a `!` twin, and one
  error module with a `kind` (`:busy`, `:handler`, `:timeout` …) stands in
  for the other clients' eleven classes. Pooled keep-alive connections over
  TCP or a unix socket, the same retries of a refused request, NDJSON
  streaming as a lazy `Stream`, and a named pool to put under a supervisor.
  Two dependencies, Mint and NimblePool; Elixir 1.18 or newer. ADR 0007
  says why it lives in this repository.
- `zygo api --allow-private-net`: what deploy callers serve may name private
  and link-local addresses in `allow`, the way `zygo serve
  --allow-private-net` already could. Typed by whoever starts the API and
  never read from a request; it does nothing without `--allow-deploy`, and
  `GET /version` reports it as `private_net`. For an embedder whose pools
  call back to a service of its own on the host's LAN address, which until
  now only a spec file served by hand could reach. ADR 0009.

### Fixed

- `zygo stop --all` says `stopped the supervisor` when it stops one. With no
  function running it printed "nothing to stop" and stopped the supervisor
  anyway. The JSON output is unchanged.
- `spec/openapi.json` no longer says `GET /healthz` needs a token; the router
  answers it before the bearer check, and a test now holds the two together.
  Chapter 17 says what the document leaves out: request bodies, query
  parameters, headers and every status but `200`.
- `zygo doctor --fix --help` names the `favordynmods` repair it already made,
  and the `cgroup moves` check says a request "waits several ms", where a
  number was missing.
- The Node client's type for `version()` has `private_net`.
- The README, the book and the examples agree with the code again. Landlock's
  network rules need Linux 6.7, not the recommended 6.1 (chapter 11); `zygo
  serve` takes `--net`, not `--network` (chapter 13); `GET /metrics` has no
  `zygo_gate_queued` series to scale on (chapter 16, the Kubernetes example);
  a pool lets four times its running requests wait before it answers busy
  (chapter 20); `strict` differs from `default` by the five socket calls
  (chapter 24); a warm function's code is named by path on the Zygo host, so
  one zygote per script version needs a shared disk (chapters 13, 17); the
  headline benchmark run's raw output is not in `bench/results/`, and the
  record that is says so (chapter 25); chapter 26 counts nine ADRs; escape
  cases 16 and 17 have rows in the threat model. The README's `vm` and
  `gvisor` sentence now says why neither is a wall for hostile code yet, and
  the quick start ends with `zygo stop --all`.
- A runtime pool's warnings and errors name its own table: `runtime.py313.io_read`,
  not `fn.py313.io_read`, which pointed at a `[fn.py313]` that does not exist.
  `zygo spec validate` groups the two kinds together.
- A sandbox whose `setgroups` file could not be written no longer fails one
  line later with "the kernel refused the group namespace mapping"; the error
  now names `setgroups`, the file that was refused.
- The book agrees with itself and the code again: chapter 10 counted 19
  escape vectors where the suite attempts 20; chapter 15 quoted venv build
  times from the retired Docker Desktop VM; chapter 13's multi-tenant example
  still imported `zygo` rather than `zygo_sdk`; chapter 11's `doctor --fix`
  table described the machine-wide sysctl where the fix installs an AppArmor
  profile for the `zygo` binary first; chapter 25 listed x86_64 as unmeasured
  under its own x86_64 table; chapter 12 sent `--isolation` to the jails
  chapter; the README's `zygo exec` comment quoted the API's 1.4 ms for a CLI
  call that measures 2.8 ms. SECURITY.md no longer recommends `isolation =
  "vm"` without saying what the `vm` backend is today.
- The README's quick start ran `zygo serve` before anything had pulled its
  image, and `serve` never pulls: on a fresh host the first command stopped
  with "pull it first". The `zygo run` line now comes first and says that it
  pulls. In the same pass: chapter 10 counted 20 escape vectors where the
  suite attempts 21; chapter 26 counted seven ADRs where there are eight;
  chapter 23's hardening list recommended `isolation = "vm"` without the
  caveat SECURITY.md carries; the roadmap's phase 4 row now says the
  Kubernetes pod is still `privileged: true`; chapter 16 said Zygo
  "contains" hostile code where it confines it; the README's benchmark host
  now says it is aarch64.
- A `zygo run` handed to a running supervisor that took longer than 30
  seconds to answer — which a first run of a Python image does on a slow
  disk, while the supervisor compiles the image's bytecode layer — was
  reported as a bare `Resource temporarily unavailable (os error 11)` on the
  control socket. It now says the supervisor did not start the sandbox
  within 30 s, and what to do: `zygo pull IMAGE` builds the layers ahead of
  time, and the build finishes on its own. The budget itself is unchanged
  (chapter 22). Found on a Raspberry Pi 5 with the release's own quick start.
- Chapter 10 answers the first question an embedder asks, "why not a prefork
  pool of my own?", and places WebAssembly runtimes next to Zygo; both are in
  its closing table and the glossary. Its nsjail and kern tables now say at
  the top, not only at the bottom, that they cannot be repeated from this
  repository. Chapter 8 says that 1.4 ms is measured at a steady 250 requests
  a second and that 1,108 a second is the ceiling; chapter 11 says its 29 ms
  one-shot run from a Mac is `true`, not Python; chapter 26 quotes today's
  pool cost rather than the ADR's earlier 0.6 ms. The README has badges.
- Chapter 10 places Sandlock and Zeroboot, two 2026 projects that fork a
  warm process per call, on the map, and chapter 23 lists the
  tenant-against-tenant vectors the escape suite does not attempt yet.
- `POST /run`, and `zygo mcp`'s `run_code`, run their one-shot sandbox against
  the store and supervisor of the process that answers them. An API started
  with `--data-root` spawned the child without it, so the child used the
  default root: a store without the images just pulled, and whatever
  supervisor was there. Found when a `POST /run` to an API on a root of its
  own never answered.
- A single private address in `allow` — `192.168.1.70:8765`, `10.0.0.5` —
  is refused without `--allow-private-net`, as a CIDR inside a private range
  already was. It used to be read as a host name, accepted, and then refused
  by the firewall on every connection with no word about why.
- `network = "egress"` or `"full"` under `seccomp = "strict"` is refused when
  the sandbox is declared. `strict` takes `socket` and `connect` away, so
  every connection failed with `EPERM` before the allowlist was consulted.
  A runtime pool is `strict` unless it names another profile, so this is
  what a pool with an allowlist and no `seccomp` met; the message says to
  set `seccomp = "default"`. The workflow-engine example said to grant
  egress by `network` and `allow` alone, and now says this too.
- **Warm functions and runtime pools:** a request that goes over `mem` is
  now killed alone. The limit and the group kill used to sit on the
  function's cgroup, one budget for the zygote and every request at once, so
  one request allocating past it took the zygote, the Node agent's parked
  workers and every request in flight beside it — in a runtime pool, other
  tenants' — down in one event, and the pool rewarmed. `memory.max` and
  `memory.oom.group` are now written on each request's own cgroup and on the
  zygote's leaf; the function keeps `pids.max` and `cpu.max`. A function may
  therefore use `mem` once per concurrent request plus once for its warm
  process (chapters 3, 13, 20; ADR 0006). `make verify-oom-linux` is the
  check.
- **SDKs:** a call on a pooled connection that `zygo api` had closed while it
  sat idle no longer fails with "the API closed the connection without
  answering". The API hangs up after 30 s idle; all three clients (Python
  sync and async, Node) now drop a pooled connection after 20 s, and send a
  request once more on a fresh connection when a reused one turns out to be
  closed before any of the answer arrived. Seen as about one call in a
  hundred failing under an n8n task runner.
- **Runtime pools:** a pooled script is waited for until its own deadline,
  as a function's request is. The client gave `POST /runtimes/<name>/call`
  and `zygo exec --runtime` the thirty-second budget meant for `ps` and
  `stop`, so every pooled request allowed longer than 30 s failed there with
  "the supervisor did not answer running a script within 30s" and advice to
  restart a supervisor that was fine. A script that really overruns is still
  killed at its deadline and reported as timed out (exit 137, HTTP 408).
  Serving a pool, `POST /fn/<name>/warm` and `zygo shell`, which warm a
  zygote before answering, now get the budget `serve` has.
- `zygo spec explain <name>` knows a `[runtime.<name>]` pool: it prints the
  pool's resolved settings, with `min_warm` and `max_warm`, where it used to
  answer "function `<name>` is not defined". `zygo spec validate` resolves
  pools as well as functions, and counts both. A name that is neither is
  told about the pools among its suggestions.
- `zygo serve --runtime` for a Node pool suggests `--script handler.js`,
  not `handler.py`.

## [0.1.3] — 2026-09-26

The code is v0.1.2's. That release's upload to npm failed after PyPI had
accepted the package (the token asked for a one-time password nobody was there
to type), so the Node package first appears at 0.1.3; the Python package has
both versions.

## [0.1.2] — 2026-09-26

The first release whose `zygo-sdk` packages reach PyPI and npm; the security
fixes below are the reason to upgrade the binary.

### Security

- `GET /metrics` no longer shows a tenant token every function name on the
  host. A tenant token now gets the process-wide counters and the series for
  its own functions only, the same scope as `GET /fn`; an operator token
  still sees everything. Metric names are unchanged.

- A warm request no longer finds files the previous request left in its
  temp folder. `/tmp` is one tmpfs per sandbox — per runtime pool, across
  tenants — and nothing cleared it. Both agents now give each request its own
  folder under `/work`, point `TMPDIR`/`TMP`/`TEMP` (and Python's `tempfile`)
  at it, and remove it afterwards. A literal `/tmp/...` path is still shared.
- A read-only bind mount is now read-only all the way down. Before, a mount
  *below* the source directory stayed writable inside the sandbox; Landlock
  hid that on 5.13 and later, nothing did on older kernels. Uses
  `mount_setattr(AT_RECURSIVE)` on 5.12+, one remount per submount below.
- Every bind mount, `:rw` included, is now `nosuid,nodev`, recursively.
- Under `egress` and `full`, the final reject covers IPv6 as well; an IPv6
  connection outside the allowlist used to meet a silent drop instead of an
  immediate refusal. Multicast, `0.0.0.0/8` and `240.0.0.0/4` joined the
  ranges that stay closed without `--allow-private-net`.
- A launch whose parent dies at the wrong moment is now caught. The old check
  (`getppid() == 1`) could never fire inside a new pid namespace.

### Changed

- The container image is built on Alpine 3.24 (was 3.21).
- The `zygo-sdk` packages for PyPI and npm now carry the LICENSE and NOTICE
  files, as the Apache-2.0 license asks of a distribution.
- The kernel-level test suites moved from `poc/` to `tests/linux/`, the
  Phase 0 proofs of concept to `tests/poc/`, and the syscall table generator
  to `tools/`. The `make` targets are unchanged, except the one named after
  the binary it builds: `make tests/linux/bin/zygo-linux-musl` replaces
  `make poc/zygo-linux-musl`, and every build output now lands in
  `tests/linux/bin/`.

- CI builds with the `rust-version` the manifest declares and treats a broken
  rustdoc link as an error; `make lint` does the same. The lint policy is
  written down in `Cargo.toml`'s `[workspace.lints]`. Every `unsafe` block
  and `unsafe impl` carries a `// SAFETY:` comment naming the invariant it
  relies on, and clippy refuses one without.

- `zygo doctor --fix` on Ubuntu 24.04 installs an AppArmor profile that
  lets the `zygo` binary alone use user namespaces, instead of turning
  `kernel.apparmor_restrict_unprivileged_userns` off for every process. The
  sysctl remains the fallback where no profile can be loaded; the profile is
  also shipped as `packaging/apparmor/zygo`.
- **Breaking, Python SDK:** the module is `zygo_sdk`, not `zygo`. PyPI's
  `zygo` belongs to another project, and two packages that install the same
  module overwrite each other. `import zygo_sdk as zygo` keeps existing code
  working. Both SDKs are 0.1.1, and the release publishes them to PyPI and
  npm as `zygo-sdk`.
- The seccomp syscall table covers every syscall in the Linux 6.10 headers.
  Each syscall Linux 5.11–6.10 added was decided on: `fchmodat2`,
  `epoll_pwait2`, the `futex_*` family, Landlock, `mseal` and
  `map_shadow_stack` are allowed; the new mount API, `pidfd_getfd`,
  `memfd_secret`, `cachestat`, `statmount`, `listmount` and `lsm_*` answer
  `EPERM`. Only syscalls newer than 6.10 answer `ENOSYS`.
- The seccomp compiler no longer has a length limit near 250 instructions.

### Added

- A Node plugin host example, `examples/plugin-host-node/`: the API-only
  plugin host from the Node SDK as a `node:http` server — tenants and tokens
  minted by an operator client, scripts registered and run per customer
  through `forTenant`, one stream route, `retries` on `Busy`/`Unavailable`,
  and Zygo's errors mapped to HTTP status codes. Tested against a fake Zygo
  with `node --test` and end to end with `make verify-plugin-host-node`.
- `zygo stop <name>` stops a runtime pool as well as a function, and
  `zygo stop --all` stops the pools too. A pool used to answer "no function
  named …", and `--all` printed "nothing to stop" while pools ran. A name
  that is both a function and a pool stops both; the output says
  `stopped runtime.<name>` for the pool. `DELETE /fn/{name}` still reaches
  functions only. Control protocol v14.
- Secrets for runtime pools. `[runtime.<name>]` takes `secrets = [...]`, and
  so do `POST /runtimes` (`layer.secrets`) and `serve_runtime(...,
  secrets=[...])` / `serveRuntime(..., { secrets })` in both SDKs — names,
  never values. Each request gets the *calling* tenant's values from the
  tenant secret store, as `/run/secrets/<NAME>`, for that request only, and
  has its zygote to itself while they exist. A tenant without one of the
  names is refused (`400`) before anything runs; a pool naming secrets on a
  host with no store key is refused at `serve`.
- SDKs: an `Unavailable` error (Python and Node) for every `503` —
  dependencies still building, a zygote that failed to warm, or an API that
  is stopping — with `code` and `retry_after`/`retryAfter`. It was a plain
  `ZygoError`; it is still a subclass of it.
- SDKs: opt-in retries. `connect(retries=N, backoff=S)` /
  `connect(url, { retries, backoff })` resend a `Busy` or `Unavailable`
  refusal after the server's `Retry-After` (doubling from `backoff`); a
  handler error, timeout or not-found is never resent. Off by default.
- Python SDK: the async client now has every method of the sync one —
  tenants, tokens, secrets, limits, blobs, `drain`, `for_tenant`, and
  `workspace`/`out` on `call` and `run_script` — with the same names and
  arguments; `zygo_sdk.aio` is reachable after `import zygo_sdk`.
- Node SDK: `index.d.ts` now declares every method and option (`stream`,
  `streamScript`, `cancel`, `drain`, `putBlob`/`blob`/`deleteBlob`,
  `putDeps`/`deps`/`deleteDeps`; `key`, `signal`, `workspace`, `out`, `deps`,
  `tenant`, `retries`, `backoff`), and a test fails when a declaration goes
  missing.

- Chapter 10: where the hosted agent sandboxes — E2B, Modal, Daytona,
  Vercel, Cloudflare, Docker's own — sit on the map, what a session is
  against a call, and when to pick which. The README named three of them
  and the book had one sentence.
- `SUPPORT.md`: where to ask, and what a useful report contains.
  `CONTRIBUTING.md` says who maintains the project, what the risk of one
  maintainer is, and how a second one is added.
- The examples suite (`make examples-go-linux`) now runs the web-api example
  end to end: `zygo up`, `zygo serve --runtime`, `zygo api`, the stdlib web
  server, and the four answers its README prints.
- Chapter 17: adding `zygo mcp` to Claude Code and Codex.
- Chapter 11: Windows through WSL2 — systemd, cgroup v2 only, then the
  Linux install. Not yet tested by the project, and marked so.
- A `.devcontainer` for VS Code and Codespaces in which sandboxes run: it
  arranges the container's cgroup tree at start and wraps `zygo` to begin in
  it.
- `make bench-record` and `bench/`: the raw JSON behind chapter 25's numbers,
  one folder per run, with records from Linux 6.8 and 5.10. Chapter 25 also
  breaks the warm request's 1.4 ms into its phases.
- A book page on fork safety: memory, ASLR, secrets, threads, random
  numbers, inherited connections, seccomp and shared pages, question by
  question. The pages on secrets now say that requests of the same function
  running at once share its secret files.
- `ROADMAP.md`: where the plan stands and what is next. The ADRs point at it
  instead of at planning notes that were never in the repository.
- The book as a searchable website on GitHub Pages, built by mdBook on every
  pull request and published from `main`; `make docs-site` builds it locally.
- Releases carry an SBOM (`zygo-<version>.cdx.json`, CycloneDX) and a
  keyless cosign signature over `SHA256SUMS`, so every tarball is verifiable,
  not only the image.
- `CONTRIBUTING.md`, a code of conduct, issue and pull request templates, and
  this changelog. `NOTICE`, and an SPDX licence line in every source file.
- `cargo deny` in CI and as `make deny`: RustSec advisories, licences and
  sources for the whole dependency tree. Dependabot keeps the pinned actions
  and the crates current.

### Fixed

- The docs disagreed with themselves, a third time: a runtime pool costs
  about 0.5 ms over a warm function everywhere (chapter 25's +0.47 ms on the
  Lima VM; 0.6 and 0.65 ms were older runs), `vm` costs about six times `ns`
  in chapter 6 as in chapters 8 and 25, chapter 26 says the one-shot sandbox
  is about 12 ms rather than 18, and the Phase 1 gate in chapter 26 and ADR
  0001 now quotes the 25 September measurement on both kernels instead of a
  2.92 ms that appears nowhere in chapter 25. Chapter 10's "clean state per
  request" row points at what a fork still shares.
- The book no longer cites a "first adoption report" that is not in the
  repository. What it reported — the network probe against Docker, the
  missing `listxattr`, the `run`-then-`serve` path — stays, as the project's
  own findings.
- The examples suite waited 15 seconds for `zygo api` at `/health`, a path
  the API does not serve; it is `/healthz`.
- The `cargo check` for the musl target warned twice about `libc::time_t`,
  which the libc crate has deprecated on musl; the timestamp cast no longer
  names it.
- Python SDK: the README's async example imported `zygo.aio`, a module that
  does not exist; it now imports `zygo_sdk`. Docstring cross-references named
  `zygo.` instead of `zygo_sdk.`.
- Node SDK: a `Retry-After: 0` header was read as 1 second, and streaming
  calls ignored the header entirely.
- The docs disagreed with themselves, again: warm-exec is 1.4 ms plus the
  program's start everywhere (not 2 ms); the `default` seccomp profile is
  ~215 names, of which 190 exist on aarch64; `vm` has a `scratch`-bounded
  writable layer and costs about six times `ns`; gVisor runs rootless, with
  advisory cgroups; bandwidth and disk I/O are the only limits without a
  default; a re-warm is 150–185 ms, not half a second; `zygo up` never pulls,
  so chapter 11 pulls first; the Landlock network rules are enforced in CI's
  `landlock-network` job; the T3 row says what `vm` cannot do yet; `layer` is
  defined in chapter 17 and the glossary; and "daemonless" says what the
  supervisor is.

- `system = [...]` builds failed with "Release file … is expired" once the
  package index an image shipped with passed its Valid-Until. Flattening an
  image stamped every file with the current time, so apt believed its cached
  index was fresh, the mirror answered "not modified", and apt kept the
  expired one. Flattening now keeps the layers' timestamps, and the build
  drops the image's package lists before `apt-get update`.
- The escape suite's setuid case was always skipped, and when it ran it only
  read `NoNewPrivs`. It now gives a binary a file capability, shows it works
  outside a sandbox, and fails to use it inside.
- `zygo doctor --fix` never offered to take `pasta` out of AppArmor's
  enforce mode unless it ran as root: it read a file only root may read. It
  now falls back to the profile on disk, so the user who needs `egress` on
  Ubuntu is told.
- `spec/openapi.json` said version 0.1.0 in the 0.1.1 release.
- The docs disagreed with themselves: the escape suite's size (16, 17 or 22),
  the container's flags (5, 7 or 9), and the cost of reaching the Mac's VM
  (100 ms or 22 ms). One answer each now, taken from the suite and chapter 25.

## [0.1.1] — 2026-09-25

The first release with every artefact published: static Linux binaries for
x86_64 and aarch64, macOS binaries, a Homebrew formula, a signed multi-arch
image on ghcr.io, and `zygo-core` / `zygo-cli` on crates.io. The code is
v0.1.0's; the release job that built v0.1.0 failed part-way, so v0.1.0 has
no image.

## [0.1.0] — 2026-09-25

The first public version.

- `zygo run`: a one-shot sandbox from an OCI image — namespaces, cgroup v2,
  seccomp and Landlock in one process, rootless, no daemon.
- `zygo serve` and `zygo exec`: warm functions. An interpreter starts once;
  each request is a `fork()` of it, with its own cgroup, deadline and secrets.
- Python and Node reference agents, a POSIX sh agent, and a language-neutral
  agent protocol with a conformance suite (`zygo agent test`).
- An HTTP API, Python (sync and async) and Node clients, and an MCP server.
- `egress` networking with an allowlist, a resolver of its own, and private
  ranges closed by default.
- `gvisor` and `vm` backends for one-shot sandboxes.
- `zygo doctor`, and the book. `doctor` checks whether cgroup2 is mounted
  with `favordynmods` (`cgroup moves`), and `doctor --fix` remounts it so,
  now and at every boot: without it, about 1 warm request in 100 waits
  several milliseconds to enter its cgroup on Linux 6.0 and later.
- A warm-exec request is created inside its cgroup (`CLONE_INTO_CGROUP`,
  Linux 5.7+) rather than moved there, so it never waits on that lock.

[Unreleased]: https://github.com/mhmtskrc2/zygo/compare/v0.1.5...HEAD
[0.1.5]: https://github.com/mhmtskrc2/zygo/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/mhmtskrc2/zygo/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/mhmtskrc2/zygo/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/mhmtskrc2/zygo/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/mhmtskrc2/zygo/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/mhmtskrc2/zygo/releases/tag/v0.1.0
