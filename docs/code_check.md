# Code check — 2026-09-21

A read-only review of the whole tree at commit `945e41a`, looking for
improvements that can be made **without changing behaviour that works today**.
Nothing was modified as part of this pass; every item below is a proposal.
Line numbers refer to that commit.

The work was split six ways (pool + supervisor; backend + sandbox + cgroup;
image + oci + derive + venv + lock; spec + net + doctor + tests; the CLI crate;
everything that is not Rust) and the highest-value claims were then verified
by hand against the source. Items marked **verified** were confirmed by
reading the code; the rest come from the review pass and read as plausible but
were not independently re-checked.

## Baseline

Everything the repo already checks is green on this machine (macOS, aarch64):

| Check | Result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | 500 passed (97 + 389 + 13 + 1), 0 failed |
| `make check-linux` (aarch64-musl type-check) | clean |
| Python agent suite (`-W error::ResourceWarning`) | 37 tests, OK (5 skipped on macOS) |
| `TODO` / `FIXME` / `XXX` / `HACK` markers | none |
| `#[allow(...)]` attributes | 3 in total |
| `unwrap`/`expect` in production code | 91, almost all `Mutex::lock().expect(..)` or resolver defaults |

Not green, and not currently checked anywhere:

| Check | Result |
|---|---|
| `cargo test -p zygo-core --no-default-features --no-run` | **fails to compile** (`tests/fuzz_parsers.rs` imports the `registry`-gated `image::auth`) |
| Declared MSRV `rust-version = "1.85"` | **false** — let-chains (`if let .. && ..`) are used, stabilised in 1.88 |
| `clippy::pedantic` (advisory) | ~800 warnings; the useful ones are listed under R-17 |

Overall the code base is in good shape: the security boundaries (tar
extraction, the child-side setup after `clone`, seccomp, the peer check) are
reasoned about in comments and pinned by tests that name a real incident; the
spec surface is strict; the CLI is a thin client. The recurring weaknesses are
(a) a handful of latent bugs on paths no test reaches, (b) the same policy
written in two or three places that have already drifted, (c) a few very long
functions, and (d) comments that describe a previous design.

## Severity and priority

- **P0** — a latent bug or security-hygiene gap worth fixing before the next
  release. Low risk to fix; each has a clear test to add.
- **P1** — robustness, resource ceilings, drift between duplicated policy.
- **P2** — structure, duplication, dead code, stale comments, coverage.

Every fix should land with a test that would have caught it. None of the P0
items changes behaviour for inputs that work today; they only change what
happens on paths that are wrong today.

---

## A. Potential bugs

### P0

**B-01 — Partially failed secret write leaves files behind and breaks every later request** (verified)
`crates/zygo-core/src/pool.rs:1434-1437`, `:1526-1531`. `write_secrets` sets
`secrets.dir` only after every file was written. If the second of three
writes fails, the first file stays, `unlink_all` sees `dir == None` and does
nothing, and the next request's `openat(O_TRUNC)` on a `0400` file fails with
EACCES (unless the supervisor is root). Fix: assign `dir` before the loop so
`unlink_all` can clean a partial write; replace `O_TRUNC` with `unlinkat` +
`O_EXCL`. The `O_TRUNC` comment ("a rewarm may find the file from a previous
generation") is wrong: a rewarm is a fresh tmpfs.

**B-02 — Peer-uid check fails open** (verified)
`crates/zygo-core/src/supervisor/mod.rs:1277`. `reject_foreign_peer` returns
`Option<Response>` with `None` meaning "allowed", and `peer_uid(stream)?`
returns `None` on a `SO_PEERCRED`/`getpeereid` failure. The doc block above
says the consequence of getting this wrong is arbitrary code execution as this
user. Fail closed: treat a failed lookup as `Unauthorised`.

**B-03 — Warm-exec request leaks all 13 helper descriptors into the tenant program** (verified)
`crates/zygo-core/src/backend/ns/enter.rs:252`, `:259`. `F_DUPFD` and `dup2`
both clear `FD_CLOEXEC`; nothing sets it again, so after `execve` the tenant
inherits the seven namespace fds, second copies of its stdio pipes and
`err_w`. The comment at `:91-93` ("the rest vanish at `execve`") predates the
renumbering. `setns` is denied by seccomp, so this is a leak, not an escape.
Fix: `F_DUPFD_CLOEXEC` + `dup3(.., O_CLOEXEC)`; add an escape-suite check that
`/proc/self/fd` inside a request holds exactly 0, 1, 2. Related: `:260`
passes the parked fd being duplicated as `err_fd` to `child::fail`, so a
failure report goes to the wrong descriptor.

**B-04 — Layer flattening writes through symlinks left by a lower layer** (verified)
`crates/zygo-core/src/image/store.rs:813-827` (`copy_tree`) and `:752-757`
(`link_or_copy`). `create_dir_all(&to)` follows a lower-layer symlink;
`to.exists()` is false for a dangling one so `fs::copy` creates the link's
target on the host. The extraction path was hardened (`safe_join`,
`ensure_real_directory`); the flatten path was not. Fix: decide on
`symlink_metadata(&to)`, remove a symlink or kind-mismatched entry first, and
only fall back from `hard_link` to `copy` on the errors that mean "links
unsupported". Add tests mirroring `writing_through_a_planted_symlink_is_refused`.

**B-05 — Requested digest is never compared with what the registry served** (verified)
`crates/zygo-core/src/image/registry.rs:245-266`. `write_blob` verifies the
body against the `Docker-Content-Digest` header (or the body's own hash), but
never against `reference.digest` or the index descriptor's digest. `zygo pull
python@sha256:X` accepts any self-consistent manifest. The index digest
written to `zygo.lock` is copied from the header unparsed. Fix: always hash
the body; if a header is present require equality; if the reference or the
descriptor is pinned require equality with it; run `parse_digest` on every
descriptor digest before it reaches a URL.

**B-06 — `verify-login-linux` can never fail** (verified)
`Makefile:90`. The `-` recipe prefix ignores the suite's exit status, and CI
runs this target verbatim (`.github/workflows/ci.yml:246`), so the "Registry
credentials" step is green regardless. Fix: capture `$?` before the cleanup
`docker rm` and `exit` with it.

**B-07 — Function names reach filesystem paths unsanitised** (verified)
`crates/zygo-core/src/paths.rs:138`, `:151`; `crates/zygo-core/src/net/mod.rs:388`.
`cgroup::sanitise` exists precisely because "tenant names come from callers
that may be passing through end-user input", but `tenant_data(name)`,
`agent_sock(name)` and the pasta pid path join the raw name. `[fn."../x"]` is
legal TOML and `--name` is free text. `resolve_layer` validates env names but
not the function name. Fix: one `is_fn_name` check at the top of
`resolve_layer` so every entry point gets it.

**B-08 — Terminal raw mode is not restored under `panic = "abort"`** (verified)
`crates/zygo-cli/src/tty.rs:104-108`; `Cargo.toml:50`. `RawMode` restores in
`Drop`, and the doc plus the `catch_unwind` test claim it survives a panic. The
release profile aborts, so destructors never run; the test only passes because
the test profile unwinds. Same for `prompt_password` in `cmd/login.rs:143-157`,
which also leaves echo off on Ctrl-C (`ISIG` stays on). Fix: a panic hook that
calls `tcsetattr`, and a Drop guard plus SIGINT handling for the password
prompt.

**B-09 — Python spawn fallback drops requests silently** (verified)
`agents/python/zygo_agent.py:766-770`. After `FORKED`, anything other than
`GO` (a second `EXEC`, a `PING`, a `GO` for another id) kills the worker and
sends neither `DONE` nor `ERROR`. `zygo agent test`'s concurrency check sends
`EXEC a`, `EXEC b` before any `GO`, so any handler that starts an import-time
thread fails conformance. `BadFrame` from that `recv()` is uncaught. Fix:
answer `overloaded`/`PONG` and loop until this request's `GO`, or route the
spawn path through the `serve()` select loop. Add a test.

### P1

**B-10 — Concurrent requests on a crashed function each trigger a full rewarm**
`supervisor/mod.rs:537-571`, `:668-670`. `rewarm` checks `backoff.ready()`
under the lock, releases it, then warms. With `failures == 0` the delay is
zero, so N concurrent callers queue N warm-ups and N-1 immediately retired
sandboxes. Fix: re-check the registry after taking the lock; hold a per-name
"warming" flag across the warm-up.

**B-11 — `tier_idle` can freeze a function between `resume()` and the request running**
`supervisor/mod.rs:594-611` vs `:675-679`. `exec` calls `resume()` before it
holds the gate permit; the idle thread can `pause()` in between and the child
is frozen for the whole deadline. Fix: `resume()` after the permit is held;
`pause()` refuses while `gate.load().0 > 0` under the gate's own lock.

**B-12 — Timed-out request may never be killed when the pid cannot be translated** (verified)
`pool.rs:1188-1196`. The comment says "a pid that cannot be translated still
gets its cgroup directory, so `cgroup.kill` has somewhere to aim"; the code
does `self.host_pid_of(pid)?` and returns early, so no cgroup exists and
`enforce_deadline` has nothing to signal. Implement what the comment says or
refuse the request before `GO`.

**B-13 — `read_status` has no deadline after `collect_request` gave up**
`pool.rs:1894-1900`. Blocking `read_exact` on the status pipe; a wedged helper
hangs the connection thread holding the gate permit. Poll with the remaining
grace.

**B-14 — `wait()` on an unlimited (zero-timeout) sandbox sends SIGKILL after 5 s** (verified)
`backend/ns/mod.rs:375-377` → `wait_within` nudges with SIGKILL at
`budget/2` and errors with "did not exit within 10s of being killed". Zero is
a legal timeout under `--allow-unlimited`. Give `wait_within` a `nudge: bool`
or loop on plain `waitpid` for the zero case.

**B-15 — Two `launch` error paths reap without killing a live child** (verified)
`backend/ns/mod.rs:586-592`, `:603-616` call `reap(true)` on a child that is
still alive (hung pre-exec, or in `hold()`); the other four exits call
`kill()`. Net effect: a 5 s stall on the serialised launcher thread. Use
`kill()` on both.

**B-16 — DNS response ANCOUNT can exceed the records written** (verified)
`net/dns.rs:143-151`. The header is written with `answers.len()` and the loop
then `break`s when the packet is full. A name with ~28+ A records yields a
malformed answer. Truncate `answers` first and set the TC bit.

**B-17 — Bare IPv6 literals in `AllowRule` are mis-split** (verified)
`spec/types.rs:634-645`. `"2001:db8::1"` → host `2001:db8:`, port 1; `"::1"`
→ host `:`. No error is raised; the rule is silently dead. Accept `[v6]:port`
and treat an unbracketed host with more than one `:` as a bare address;
validate `Exact` hosts as LDH labels.

**B-18 — Three definitions of "private range" that already disagree**
`spec/types.rs:580`, `net/dns.rs:246`, `net/mod.rs:132-141`. `100.64.0.0/10`
passes resolve without `--allow-private-net` but the ruleset rejects it above
every accept. One `is_private(IpAddr)`; render the nftables sets from the
same table; a test that walks both directions.

**B-19 — Reserved mount-target check is a string compare on an unnormalised path**
`spec/resolve.rs:563-568`. `/proc/`, `/./proc`, `/proc/sys` and `/tmp`, `/run`
(managed by the launcher, `sandbox/mount.rs:147`) all pass. Normalise first,
compare with `Path::starts_with` against a single exported `MANAGED_TARGETS`.

**B-20 — `write_blob` leaves its temp file behind on read/write errors**
`image/store.rs:135-151`. Only the digest-mismatch branch cleans up; the temp
name is `blob-<hex>-<pid>` so two threads of one process collide; a network
error is reported as an I/O error on the temp path. Guard with a
remove-on-error closure, use a unique name, `sync_all` before rename.

**B-21 — A corrupt `index.json` is treated as empty and then overwritten**
`image/store.rs:449-472`. One unparsable index makes every image "not
pulled" and the next `put` rewrites the file with one entry. Distinguish
NotFound from a parse error; propagate.

**B-22 — Prune can delete a layer another process is mid-way through unpacking**
`image/store.rs:565-588` vs `:190-232`. The layer is not indexed until every
layer is unpacked; prune never checks the `layer-<hex>` lock. Skip locked
digests (`Paths::is_locked`) or take the lock per layer.

**B-23 — `GO` barrier in the Python child accepts EOF as "go"** (verified)
`agents/python/zygo_agent.py:255`. `os.read(go_fd, 1)` is unchecked; if the
zygote dies before `GO` the handler runs outside its cgroup. `if not
os.read(...): os._exit(1)`.

**B-24 — An oversize or unsendable result kills the whole Python agent**
`agents/python/zygo_agent.py:660`, `:796`. `Framing.send` raises `ValueError`
over 32 MiB, `BrokenPipeError` when the supervisor is gone; neither is caught
in `_finish`. The spec's `bad_result` code exists for this and is never
emitted.

**B-25 — The forked Python child can fall back into the parent's loop**
`agents/python/zygo_agent.py:722-723`, `:889`. If `run_request` raises outside
its own `try` (EPIPE on the result pipe), the child unwinds through `serve()`
and exits via interpreter teardown, which contract rule 4 forbids. Wrap the
`pid == 0` branch in `try: ... finally: os._exit(1)`.

**B-26 — Node agent finalises on `'exit'`, not `'close'`** (verified)
`examples/agents/node/agent.js:113`. Stdio may still be open when `'exit'`
fires; a worker's last output chunks can arrive after `DONE`. Also `:134`
assumes every signal is SIGKILL.

**B-27 — Stale guest-binary stamp after `limactl delete`** (verified)
`crates/zygo-cli/src/shim.rs:505-517`, `:653-685`. The stamp is keyed on host
binary len+mtime only; after the instance is recreated the still-matching
stamp short-circuits `ensure_guest_binary` and the forwarded command fails
with exit 127. Remove the stamp when `Vm::Absent`, or key it on instance
creation time.

**B-28 — macOS shim does not forward the environment**
`shim.rs:376-378`. `serve --secret NAME`, `ZYGO_API_TOKEN`,
`OTEL_EXPORTER_OTLP_HEADERS`, `ZYGO_DATA_HOME`, `ZYGO_LOG`, `NO_COLOR` are read
on the client side and never reach the VM, so `--secret` on a Mac always
reports "no value in this environment". Forward an explicit allowlist (not as
`env K=V` in argv: visible in `ps`), or refuse `--secret` in the shim with a
clear message. Also SIGTERM/SIGHUP to the host process do not reach `limactl
shell`; `run.rs` already has `forward_signals`.

**B-29 — Exit-code mapping only inspects the outer error** (verified)
`crates/zygo-cli/src/main.rs:69-72`. Any `.context()` wrapper changes the
outer type, so the library's 2/125 conventions silently degrade to 1. Walk
`e.chain()`.

**B-30 — `zygo_api_errors_total` double-counts** (verified)
`cmd/api.rs:245-256`. The `Err` arm increments, then `is_server_error()` on
the 500 it produced increments again. Count once from the final status.

### P2

**B-31** `pool.rs:1871-1875` — a failed `read(2)` (`n < 0`, including EINTR) is treated as end of file.
**B-32** `supervisor/client.rs:278-306` — the auto-started supervisor's stderr is a pipe nobody drains; two clients racing to start one leave the loser reporting failure without retrying `connect`.
**B-33** `backend/gvisor.rs:352-361` — container id is name + pid, so a restarted function reuses the id and the old sandbox's `cleanup()` deletes the new one. `cgroup.rs:133` already solved this with a counter.
**B-34** `backend/ns/idmap.rs:151-157` — the sandbox-uid entry is not clamped to `range.count`; `user = "70000"` fails with a bare EPERM.
**B-35** `backend/ns/enter.rs:437-447` — parent-side `read_some`/`reap` do not retry on EINTR.
**B-36** `doctor.rs:748-755` — `subuid()` reports "configured" when `USER` is unset and `/etc/subuid` has a blank line.
**B-37** `image/auth.rs:268` — `token_url` appends `?` to a realm that already has a query string.
**B-38** `cmd/bench.rs:291` — `remove_dir_all` on a pivot_root target; `run.rs:207-213` explains why it must be `remove_dir`. Reuse `run.rs`'s `Scratch` guard.
**B-39** `cmd/top.rs:31-33` — `Duration::from_secs_f64` panics for `--interval inf`; use `try_from_secs_f64`.
**B-40** `cmd/api.rs:161-169` — `--listen localhost:7700` is rejected although the help says HOST:PORT.
**B-41** `examples/agents/sh/agent.sh:138` — `printf > "$go"` blocks forever if the subshell died before `GO`; no `trap` so the FIFO and temp files leak on a signal.
**B-42** `examples/webhook/handler.py:31-33` — signature covers a Python `repr`, so no external sender can reproduce it; a non-dict payload is a 500 not a 400.
**B-43** `derive.rs:250-256` — the full flattened image copy under `tmp/` is leaked on the three `?`s between install and success.
**B-44** `pool.rs:447-452` — `tmp()/warm-<name>-<pid>` is created per `serve`/rewarm and never removed (`venv.rs` and `derive.rs` do clean theirs).

## B. Security hygiene (no exploit, but the class of thing the code elsewhere prevents)

**S-01** `image/auth.rs:17-21` — `Credential` and the Docker config structs derive `Debug`; any `{:?}` prints passwords. A manual `impl Debug` with `<redacted>` is cheap. (verified)
**S-02** `image/registry.rs:69-72` — `reqwest::Client` has no `connect_timeout`/`read_timeout`; a stalled registry hangs `pull`/`run` forever. (verified)
**S-03** `backend/ns/mod.rs:462` — the ready pipe is `pipe()`, not `pipe_cloexec()`, and stays open while `newuidmap`, `nft` and the long-lived `pasta` are spawned. (verified)
**S-04** `net/mod.rs:1006` — `recvmsg` without `MSG_CMSG_CLOEXEC`, so the received `/run/secrets` fd is inherited by every later `Command`. (verified)
**S-05** `backend/gvisor.rs:388-399` — non-CLOEXEC `libc::dup` in a multi-threaded process; `try_clone_to_owned()` removes the `unsafe` block.
**S-06** `cmd/api.rs:459-504` — `/fn/<name>/batch` has no element cap or concurrency bound: a 16 MiB body of `[null, ...]` spawns millions of tasks, each opening a supervisor connection. Cap the batch and gate with a `Semaphore`; a panicking element currently fails the whole batch via `.expect`. (verified)
**S-07** `cmd/api.rs:416-435` — the client pool only grows; each idle connection pins a supervisor thread. Drop instead of push above a ceiling.
**S-08** `cmd/api.rs:370-385` — `X-Zygo-Timeout-Ms` is unbounded; clamp and return 400 above a ceiling.
**S-09** `cmd/api.rs:232` — hyper `http1::Builder` without a timer silently disables `header_read_timeout`; a slowloris connection is held forever. Add `TokioTimer` and a connection semaphore.
**S-10** `cmd/api.rs:172-176` — the comment promises the unix socket is 0600 in a 0700 directory; the code chmods after bind and never checks the directory.
**S-11** `backend/ns/child.rs:29-33`, `:588-592`, `seccomp.rs:804-808` — hand-rolled syscall numbers default to x86_64 for every non-aarch64 target; `libc::SYS_*` exist for all Linux targets, or add a `compile_error!` for unsupported arches.
**S-12** `backend/ns/landlock.rs:259` — rule paths built with `to_string_lossy`, so a non-UTF-8 rw mount gets a rule for a different path. `prepare.rs:318` does it right.
**S-13** `.github/workflows/ci.yml` — actions pinned by mutable tag, no top-level `permissions:`, no `timeout-minutes`, no `concurrency:` group. (verified)
**S-14** `image/auth.rs:162-168` — `CredentialStore::save` truncates then writes; a crash loses every registry's credentials. `NamedTempFile` with mode 0600 + persist.

## C. Error handling

**E-01** `supervisor/mod.rs:74-103` — one panicking warm-up job kills the launcher thread for the supervisor's lifetime; every later `serve` gets "restart the supervisor". Wrap `job()` in `catch_unwind`. (verified: no `catch_unwind` in the loop)
**E-02** `derive.rs:308-324`, `venv.rs:237-252` — `apt-get exited 100` for a typo'd package and a bad pip pin become `BackendUnavailable` (exit 125, "the host cannot run this"). Add `Error::Build` (exit 1); the two match blocks are near-identical. Note: changes an exit code.
**E-03** `error.rs:53` — `Bare(#[from] io::Error)` lets any `?` on an `io::Result` compile and yield "No such file or directory" with no path, which the module doc forbids. `ImageError::Unpack(String)`/`Registry(String)` discard `ErrorKind` and the source chain.
**E-04** `net/mod.rs:700` — `format!(..).leak()` on an error path because `Error::primitive` takes `&'static str`; a long-lived supervisor leaks per failure. `Cow<'static, str>`.
**E-05** `net/dns.rs:278-296` — `dns::serve` returns silently on `set_read_timeout` or `recv_from` errors; every later lookup in the sandbox times out with nothing in the log.
**E-06** `supervisor/mod.rs:1168-1176`, `client.rs:171-204` — frame errors are wrapped as `Error::primitive("read", .., io::Error::other(e))`, erasing `FrameError` although `Error::Protocol(ProtocolError::Frame)` exists and is used in `pool.rs`.
**E-07** `supervisor/mod.rs:683-686`, `:1112-1120` — `exec` swallows `NotFound` mid-redirect and reports "shutting down"; a non-EINTR `accept` error returns without setting `stopping`, so the idle thread keeps running.
**E-08** `backend/ns/mod.rs:691-751` — `setgroups` write failure is swallowed (`let _ =`), so the user is told the gid map was refused; only `newuidmap` is checked for, and the operation string is hard-coded for both helpers.
**E-09** `image/store.rs:225`, `:460` — `serde_json::to_vec_pretty(..).unwrap_or_default()` writes an empty sidecar/index on a serialisation failure; for the whiteout sidecar that makes deleted files reappear.
**E-10** `lock.rs:358-366` — the version is checked after a full parse, so a future v2 fails as "not a lock file" and the "upgrade zygo" remedy is unreachable. `save` uses a fixed `zygo.lock.tmp` name.
**E-11** `image/store.rs:104`, `image/reference.rs:206` — `parse_digest` accepts uppercase hex that `write_blob` can never verify (it compares against lowercase). Reject at parse time.
**E-12** `sandbox/mod.rs:164` — `f.user.parse().unwrap_or(1000)`: a non-numeric `user` silently becomes 1000 unless the resolver already rejects it; document or error.
**E-13** `spec/resolve.rs:419-426` — `bandwidth` set with `network = "none"`/`"host"` is accepted and ignored, where `allow` without `egress` is an error.
**E-14** `cmd/supervisor.rs:379-406`, `cmd/image.rs:184-202` — `--json` emits several JSON documents on one stdout (`up` per failed function then a summary; `image rm` then `prune`); drift and missing-secret failures print nothing in JSON mode. Collect into the single summary.
**E-15** `cmd/api.rs:279-283`, `:387-398` — every `control()` failure is a 500 (a supervisor that is down should be 503 + `retry-after`); every `Limited::collect` failure is 413, including a client hanging up mid-body.
**E-16** `cmd/image.rs:518-525` — `remove()` swallows every error and the summary prints "freed N MB" regardless.
**E-17** `agents/python/zygo_agent.py:552-553` — `except OSError: return` skips `_reap_all()`, leaving zombies on a socket error.
**E-18** `backend/ns/seccomp.rs:823-826` — `install` truncates `len` to `u16` silently; `debug_assert!(prog.len() <= 4096)`.

## D. Structure and duplication

**R-01 — Long functions.** `spec/resolve.rs:202` `resolve_layer` (331 lines; six comment-delimited sections that are natural functions), `backend/ns/child.rs:296` `apply` (260), `pool.rs:324` `serve_with_logs` (257; validation, image/venv/derive, mounts, network, seccomp, backend, handshake, `Mode` matched five times), `cmd/supervisor.rs:214` `up` (206; lock policy interleaved with rendering, belongs in `zygo-core::lock`), `backend/ns/mod.rs:428` `launch` (195; `let _ = sandbox.kill(); return Err(e)` ×4 and `reap` ×3 — this is how B-15 slipped in), `cmd/run.rs:20` `run` (185), `pool.rs:937` `call_timed` (178; FORKED/GO/DONE phases).
**R-02 — `WarmFn` and `WarmExec` duplicate nine methods** (`pool.rs:863-1260` vs `:1592-1789`: `status`, `state`, `is_healthy`, `timeout`, `set_secrets`, `pause`, `resume`, `cpu_accounting`, `record`). Extract a `Common` struct embedded in both; the `each!` forwarders call `f.common()`.
**R-03 — `Message::Result` and `Message::Done` carry the same seven fields** (`protocol/mod.rs:79-112`); `result_into_done` exists only to copy them. A shared payload struct; the fixture test pins the wire.
**R-04 — venv/derive build scaffolding is duplicated** (`venv.rs:204-300` vs `derive.rs:294-395`): newroot under `tmp/`, `SandboxConfig::from_resolved`, `run_captured`, cleanup, error mapping, `build_spec` forcing `Network::Host`. One `oneshot::Build` with a single `run()`.
**R-05 — A full `doctor::run()` to learn one boolean, matched by display name** — `pool.rs:440-443`, `venv.rs:208-211`, `cmd/run.rs:111-115`, `cmd/bench.rs:257-260` all run every probe (one forks and `unshare`s) and check `c.name == "overlayfs (userns)"`. Expose a memoised `doctor::overlay_in_userns() -> bool`. Same class: `doctor.rs:130-133` `Report::supports` matches six literal check names, and `egress()` emits two different names for one check (`:346` vs `:364`). Use `pub const` names or a `CheckId` enum.
**R-06 — Small helpers written several times.** `which()` ×4 (`shim.rs:739`, `scope.rs:113`, `doctor.rs:828`, `net/mod.rs:429`); `uname` parsed ×3 in `doctor.rs`; `is_timeout` ×2 (`pool.rs:2320`, `client.rs:213`); hex request-id generator ×2 (`pool.rs:2305`, `supervisor/mod.rs:972`) — and the two counters mean a `zygo logs` id never matches the id in `EXEC`/the cgroup name although the comment says it does; `pipe_cloexec` ×2, `write_all` ×2, errno ×2 in `backend/ns`; exit-code conversion `gvisor.rs:512` vs `ns/mod.rs:345`; `"{}/{}", os, arch` ×4 → `impl Display for Platform`; `sha256:` hex ×4 → `image::digest_of`; `paths.rs:171`/`:193` key-sanitising closure; `reference.rs:27` vs `:57` endpoint; `partial_cmp().expect("no NaN")` ×7 in `bench.rs` vs `total_cmp` in `stats.rs`; `human_duration` vs `human_age`; `DEFAULT_EXEC_TIMEOUT_MS` == `DEFAULT_TIMEOUT_MS`; "no supervisor running → empty output → Ok(0)" ×3 in `cmd/supervisor.rs`; `resolve` vs `resolve_for_serve` share 10 lines; `Cpu` validated twice with `Cpu(pub f64)` bypassing both.
**R-07 — Six ad-hoc tokio runtimes in the CLI** (`run.rs` builds two multi-thread runtimes in one command). One `block_on` helper on a current-thread runtime, or blocking `pull`/`image_config` on the core side. `RegistryClient::image_config` is `async` but only reads the local store (`registry.rs:271-275`), so `run.rs` builds a second runtime and reloads `~/.docker/config.json` to parse a blob already on disk. `check_status` is `async` without an `await`.
**R-08 — Blocking filesystem work inside `async fn pull`** (`registry.rs:124`, `:151`, `:174`, `:266`): `write_blob`, `unpack_layer` (seconds of tar+gzip) and `put` run on the runtime thread; only `download_blob` uses `spawn_blocking`. Hidden by the CLI's `block_on`, visible to any embedder.
**R-09 — `Store::skeleton`** (`store.rs:291-323`): global lock key, no re-check after acquiring, inline cache path unknown to `Paths::ensure` and to prune, key hashing copy-pasted from `:340-352`.
**R-10 — `store.rs` is 1662 lines**; split into store (index, blobs, lock), unpack (`extract_tar`, `safe_join`, `ensure_real_directory`, `link_or_copy` + hostile-tar tests) and flatten (`rootfs_view`, `skeleton`, `copy_tree`) so the security-critical code is one reviewable file.
**R-11 — `WarmExec::call_timed` holds the `sandbox` mutex across `enter::enter`** (`pool.rs:1673-1683`: fork + six `setns`), serialising every warm-exec request, while `secrets_dir` was already duplicated at serve time "because a duplicate is cheaper than reaching through the lock". Duplicate the namespace descriptors too.
**R-12 — `net/mod.rs:532` `configure` carries `#[allow(too_many_arguments)]`**; `cidr_of` returns a `String` and family is sniffed with `contains(':')`; `availability()` is called twice per sandbox.
**R-13 — `supervisor/client.rs:57`** keeps a third socket dup solely for `set_read_timeout`; `reader.get_ref()` does the same.
**R-14 — `image/registry.rs:64`** `Arc<tokio::sync::Mutex<HashMap>>` never held across an `.await` on a type that is never shared; `std::sync::Mutex`.
**R-15 — `backend/gvisor.rs:363-367`** `bundle_dir` ignores the `Paths` an embedder passed to `with_paths`.
**R-16 — poc suites.** The same 30-line prelude (`SRC`, `ZYGO`, `say/ok/bad`, `PASS/FAIL`) is copied into nine scripts and the "wait for `supervisor status`" loop into three; `poc/cgroup_harness.sh` is already the shared prelude. Hard-coded `/tmp/supervisor.log`, `/tmp/sb.err`, `/tmp/escape.err`, `/tmp/fuzz/`, ~30 `/tmp/*.log` shared across suites with no cleanup on failure; `mktemp -d` + `trap`. The six identical `docker run --privileged ... python:3.12-slim sh /src/poc/<x>.sh` recipes in the Makefile; `. poc/ci_cgroup.sh` ×7 and the same six-line comment ×6 in `ci.yml`.
**R-17 — Worth taking from `clippy::pedantic`** (advisory; the rest is noise): 11 × redundant clone, 10 × `format!` appended to a `String`, 9 × temporary with significant `Drop` could be dropped early, 10 × argument passed by value but not consumed, 15 × `u64 as f64` precision, 8 + 7 × `i32 as u8` sign/truncation (check each is intentional), 7 × strict `f64` comparison.

## E. Dead code and stale comments

**D-01 — No callers:** `pool.rs:2417` `agent_failure`, `:2421` `WarmupTiming`, `:917` `default_timeout` (identical to `timeout`); `supervisor/mod.rs:807` `Supervisor::load` and `:996` `FunctionLoad`; `protocol/frame.rs:88` `FrameReader::get_ref`; `protocol/mod.rs:204` `result_into_done` (test only); `spec/mod.rs:309` `locate_field` (documented as the `file:line:col` locator, never wired into error printing); `image/reference.rs:75` `is_pinned`, `:87` `store_key`; `image/auth.rs:328` `TokenResponse.expires_in` never read; `:186` duplicate Docker Hub alias; `agents/python/zygo_agent.py:823` `_read_result`; `:550` `except InterruptedError` unreachable since PEP 475; `cmd/supervisor.rs:725` `absolute()` re-implements `std::path::absolute` (stable since 1.79); `cmd/bench.rs:900` `print_floor` takes a `report` it ignores. (verified for the Rust items in `pool.rs` and `supervisor/mod.rs`)
**D-02 — Dead `Step` variants and an incomplete round-trip test:** `prepare.rs:31/35/40` `MountSys`, `MountDevPts`, `SetHostname` are never sent (`child.rs` ignores those failures); the test at `:742` filters `n != 0` on `1..=26` (a no-op) and never round-trips `27..=30`.
**D-03 — Unreachable instructions:** `seccomp.rs:768-771` `goto(Deny)` after a `jgt` whose both branches leave, plus a redundant `load(OFF_NR)`; `limits.rs:117` `let _ = (&self.io_read, &self.io_write)`; `cgroup.rs:678` `AlreadyExists` arm dead under `create_dir_all`.
**D-04 — Comments that describe a previous design:** `pool.rs:733-735` `Conn` doc (single-lock design), `:249` `rss_kb` "reported at warm-up" (now live from `/proc`), `:1526` `O_TRUNC` rationale, `:2130` `CallTiming::lock` named for the old design; `supervisor/mod.rs:1083-1087` "unblocks `accept`" above `set_nonblocking(false)`; `:970-976` log ids "can be matched to a trace" (they cannot, see R-06); `protocol/frame.rs:83-87` `get_ref` doc; `client.rs:54-57` "no way to reach the socket"; `backend/ns/prepare.rs:224-225` "deliberately not `Send`" directly above `unsafe impl Send + Sync`; `ns/mod.rs:379-381` "the supervisor will replace this with a timerfd in phase 2"; `child.rs:174-177` `getppid() == 1` guard overstated for the init; `enter.rs:91-93` (see B-03); `store.rs:793-799` "recursive copy" doc attached to `is_internal_entry`; `:7-10` names two security-relevant places when there are four; `oci.rs:24-25` "only `write_bundle` touches the filesystem" while `workdir` stats the rootfs; `lib.rs:9-27` module tour omits `derive`, `lock`, `net`, `oci`, `paths`, `error`; `resolve.rs:283` "`unwrap`s below" (they are `expect`s); `cmd/bench.rs:903-912` two fused doc comments; `agents/python/zygo_agent.py:802` "at most one child is ever outstanding" (requests overlap now); `examples/agents/sh/agent.sh:44-45` "`wc -c` would cost a process" on the line that uses `wc -c`; `:13-14` overstates the concurrency design; `examples/warm-exec/go/main.go:37` refers to a `json:"-"` tag that is not there.
**D-05 — Naming:** `cgroup.rs:133` `generation()` is a getter that bumps a global counter (used as a getter in tests); `ns/mod.rs:677` `leak_step` leaks nothing; `cgroup.rs:663` `enable_controllers` returns `Result` but every write is `let _ =`; `backend/ns/enter.rs:301` `Step::Setns` reused for a `fork` failure.
**D-06 — Messages carrying source indentation:** `shim.rs:527`, `cmd/supervisor.rs:216`, `cmd/bench.rs:517-521`, `:895` print a dozen leading spaces per line; everywhere else uses `"\n  → "`.
**D-07** `cli.rs:474-476` `exec --batch` is advertised in `--help` but always fails, while the HTTP API implements batch. Hide until it exists.

## F. Tests

**T-01 — Coverage of the P0 bugs above** (each fix ships with one): partial secret write (B-01); failed peer lookup (B-02); `/proc/self/fd` inside a request (B-03); flatten over a lower-layer dangling symlink and hard link onto one (B-04); pinned digest that does not match the body (B-05, needs a tiny in-process registry, which would also cover the 401 → token → retry path and `check_status`, currently untested); `[fn."../x"]` refused (B-07); spawn path receiving `EXEC`/`PING` before `GO` (B-09).
**T-02 — The reply demultiplexer has no tests** (`pool.rs:745-861`: `Conn::read_replies`, `Conn::fail`, `Reply::drop`, out-of-order and orphan replies). Testable with `UnixStream::pair()` and a fake-agent thread. Also untested: `exec`'s `Attempt::Closed` redirect, `tier_idle` pause/cool transitions, `read_ready` version mismatch, `into_outcome`, `LogRing::since` with `after != 0`. `Function` being a closed enum of real sandboxes blocks the tiering tests; a `#[cfg(test)] Function::Fake` arm or a small trait.
**T-03 — `backend/ns/enter.rs` has no unit tests** and `wait_within` is untested; both are where B-03, B-14, B-35 sit and both are testable on Linux with a pipe and a `fork`+`sleep` child.
**T-04 — `fuzz_parsers.rs` breaks `--no-default-features`** (`:13`); gate the `CredentialStore` sweep on `feature = "registry"`. The DNS wire parser (`dns::parse_query`/`response`), the only parser that reads raw bytes sent by tenant code, has no sweep; the last four sweeps print no seed on failure. `catch_unwind` is moot under `panic = "abort"` in release tests.
**T-05 — Unit tests call the host resolver** (`net/mod.rs:1158`, `:1307` via `resolve_on_host`); inject `&Resolve` as `dns::serve` already does.
**T-06 — Twelve `expect("… has a built-in default")`** in `resolve.rs:285-519` are guarded only by `defaults()` 200 lines away; one test asserting every expected field is `Some` in `defaults()`.
**T-07 — `Limits` test literal copied into five modules** (`gvisor.rs:531`, `landlock.rs:479`, `mount.rs:359`, `limits.rs:179`, `cgroup.rs:802`); `#[cfg(test)] Limits::for_tests()`.
**T-08 — No HTTP-level test of `route`/`authorise`** in `cmd/api.rs` (healthz bypass, 401-before-404 ordering, body limit, batch shape). Making `route` generic over the body type lets tests build `Request<Full<Bytes>>`.
**T-09 — Python agent gaps:** `mode="stdin"`, `env_overrides`, announced frame length past 32 MiB, result over 32 MiB, `spawn_failed`, SHUTDOWN reaping, `_Ring` counting characters not bytes (`:437`, so the "N bytes truncated" note is wrong for non-ASCII).
**T-10 — Other gaps:** `Spec::discover` (walks from `current_dir()`, no test; add `discover_from(dir)`); `write_blob` with a reader that errors mid-stream; `put` on an unparsable index; `ensure_real_directory` replacing a file with a directory; `gvisor` container-id uniqueness across two calls; `wait_within` semantics; `doctor.rs:1144` smoke test forks, writes `uid_map`, opens `/dev/kvm` and runs `systemd-run` ungated.
**T-11 — Sleep-based waits in the suites** (`verify_supervisor.sh:255,261,458,784,1293,1546,1676`; `verify_launcher.sh:293`); poll the state where one can be polled, as `wait_http` already does. `Makefile:89` `sleep 3` for the registry; poll `/v2/`.

## G. Dependencies, build, CI

**C-01 — MSRV.** `rust-version = "1.85"` is false (let-chains, 1.88). Set 1.88 and add a `cargo +1.88 check` CI job so it cannot drift again. (verified)
**C-02 — Lint coverage.** No `cargo-deny`/`cargo-audit`, no `shellcheck` (16 shell scripts, one executed inside sandboxes as a reference agent; shellcheck 0.8 already reports real items: `>&$WIRE` is undefined in POSIX sh, unquoted `$SRC`/`$profile`, `PASS + 1` vs `PASS+1` drift), no Python linter (5 files). One `lint` job.
**C-03 — Redundant manifest entries.** `crates/zygo-cli/Cargo.toml:23` and `:41` list `tempfile` as both a dependency and a dev-dependency; `spec/mod.rs:35` `#[serde(default)]` on an `Option` is a no-op. Duplicate majors in `Cargo.lock` (`syn`, `getrandom`, `cpufeatures`, `windows-sys`) are transitive; `cargo tree -d` to attribute.
**C-04 — Makefile.** `.PHONY` misses `fuzz-linux gvisor-linux verify-login-linux verify-shim`; `help` omits nine targets; `verify-linux` passes `-t` unconditionally so it fails from a non-TTY. Floating image tags (`gcc:13` for the syscall tables matters most: a header change fails the "fresh generation" check with no code change).
**C-05 — CI.** `unit` job compiles debug for `cargo test` then release for `make conformance` on four runners; one profile halves the build.
**C-06 — `poc3_warm_path.rs`** is Unix-only with no `cfg`, so `cargo build --examples` fails on non-unix; `Harness::start` blocks forever if the agent exits before connecting.

## H. Documentation consistency

**X-01 — Numbers that disagree** (verified for the supervisor count): supervisor suite 151 (`README.md:34`, `:321`, `Makefile:14`, `todo.md:473`) vs 157 (`todo.md:48`), while the script has 148 `ok` sites; shim 14 (`README.md:325`, `:362`) vs 15 (`todo.md:1992`); Python tests 27 (`todo.md:32`) vs 37 (`todo.md:46`, actual); "52 scenarios" vs 38 + 19 `ok` sites; kernel floor 5.3 (`README.md:351`) vs 5.15 (`ci.yml:33`, `shim/lima.yaml:12`). Have the suites print their totals and quote those, or drop hard numbers from prose.
**X-02** `spec/protocol.md:139` — "an agent that stops answering is restarted": no non-test code sends `PING`.
**X-03** `docs/SUMMARY.md:11-12` — two entries link outside the book root; mdBook will not render them.
**X-04** `spec/types.rs:529` — ported allow rules are TCP-only (`net/mod.rs:308-315`); undocumented, so `dns.google:53` permits TCP DNS only.
**X-05** `doctor.rs:201-225` — the kernel series table ends at 6.14; add a test that fails when the newest row is older than N months.
**X-06** Python version floor (3.7+) is stated nowhere.
**X-07** `resolve.rs:208` — error field paths render `fn.run.mem` for `zygo run --mem`, a key that exists in no file.

## I. Checked and found clean

- nftables ruleset: every interpolated value is a typed `IpAddr`/`Cidr`/`u16`/`u32` or a constant; no injection surface.
- The child-side sequence after `clone` is allocation-free on every call path; every `unsafe` has a justification.
- `tar` 0.4's `overwrite` unlinks an existing symlink at the final component; `ensure_real_directory` covers the parents.
- HTTP API auth ordering (only `/healthz` unauthenticated; 401 before 404/405; constant-time compare; loopback-only `--no-auth`).
- No blocking calls on the tokio runtime in the CLI (`spawn_blocking` for every supervisor round-trip).
- Lock discipline in pool/supervisor: no lock held across a sandbox teardown; no reentrant lease drop.
- Every command and flag the docs mention exists in `cli.rs`; no dead relative links in README, docs, examples, spec.
- All declared dependencies are used.

## Suggested order

1. P0 bugs B-01..B-09 with their tests (T-01), S-01..S-04, C-01, T-04. Each is a one-to-ten-line change with a clear test.
2. P1 bugs B-10..B-30, the API ceilings S-06..S-09, E-01, E-02 (decide on the exit code first), B-18/B-19 consolidation of duplicated policy.
3. R-01 (split the seven long functions), R-02..R-05, then the rest of D and T as they come up in the files being touched.
4. C-02..C-05, X-01..X-07, R-16 when the suites are next edited.
