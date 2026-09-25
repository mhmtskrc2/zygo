# 21. Environment variables, files and exit codes

Everything Zygo reads from its environment, everything it writes to disk, and
every way it can end. Written from the code; if you script around Zygo, this
is the chapter to keep open.

## Environment variables Zygo reads

| Variable | Read by | Meaning | Default |
|---|---|---|---|
| `ZYGO_DATA_HOME` | CLI, supervisor | The data folder (same as `--data-root`). | `$XDG_DATA_HOME/zygo`, else `~/.local/share/zygo` |
| `ZYGO_RUNTIME_DIR` | CLI, supervisor | The runtime folder: sockets and pid files. | `$XDG_RUNTIME_DIR/zygo`, else `/tmp/zygo-<uid>` |
| `ZYGO_LOG` | CLI | Log filter, e.g. `debug` or `zygo=trace`. Overrides `-v`. Logs go to stderr. | `warn` |
| `NO_COLOR` | CLI | Any value turns colour off. Colour is also off when output is not a terminal. | |
| `ZYGO_API_TOKEN` | `zygo api`, SDKs | The bootstrap bearer token. Never a flag, because flags show in `ps`. It is removed from the environment of sandboxes the API starts. | |
| `ZYGO_API_URL` | SDKs | Where the API is: `unix:///path`, `http://host:port` or `host:port`. | `http://127.0.0.1:7700` |
| `ZYGO_SECRETS_KEY` | supervisor | The 32-byte key of the secret store, as 64 hex characters or base64. | none: secret routes refused |
| `ZYGO_SECRETS_KEY_FILE` | supervisor | A file holding that key. Setting both is an error. | |
| `ZYGO_BYTECODE` | image store | `0` turns off the Python bytecode layer. | on |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `zygo api` | Same as `--otlp-endpoint`. | |
| `OTEL_EXPORTER_OTLP_HEADERS` | `zygo api` | Extra headers for OTLP, as `key=value,key=value`. | |
| `DOCKER_CONFIG` | `pull`, `login` | Where Docker's `config.json` is, for registry credentials. | `~/.docker` |
| `ZYGO_KRUN_CONSOLE` | `vm` backend | Write the guest's console to this file, for debugging. | |
| `ZYGO_IN_SCOPE` | CLI | Set by Zygo itself when it re-runs inside a systemd scope, so it does not do it twice. Do not set it by hand. | |

### Only on a Mac

| Variable | Meaning |
|---|---|
| `ZYGO_LINUX_BIN` | The Linux build of Zygo to copy into the VM, before the ones Zygo looks for itself. |
| `ZYGO_LIMA_TEMPLATE` | The Lima template to create the VM from. |
| `LIMA_HOME` | Where Lima keeps its VMs (`~/.lima`). |

Every `ZYGO_*` variable in your shell is passed into the VM; so are the
variables named by `--secret` and by `secrets = [...]` in the spec. Nothing
else from your shell crosses.

## Environment a sandbox receives

| Variable | Who sets it | Meaning |
|---|---|---|
| `ZYGO_FUNCTION` | Zygo | The function's name. (`ZYGO_TENANT` holds the same value; the name is historical.) |
| `HOME` | Zygo | `/tmp`, unless the image or `env` sets one — so programs that write to `~` work on a read-only root. |
| `ZYGO_REQUEST_ID` | the agent, per request | The request's id. |
| `ZYGO_DEADLINE_MS` | the agent, per request | The request's time budget in milliseconds (its `timeout`); `0` means none. |
| `TMPDIR`, `TMP`, `TEMP` | the agent, per request | The request's own temporary folder: the workspace if one was sent, otherwise `/work/tmp-<random>`. Removed when the request ends. |
| `ZYGO_AGENT_TMP_PARENT` | you, for an agent's tests | Where the reference agents make those folders instead of `/work`. Nothing in Zygo sets it. |
| `ZYGO_WORKSPACE` | the agent, per request | The request's workspace folder, when one was sent; the handler starts in it. |
| your `env` | you | Everything in `env` / `--env`. |

Secrets are **not** environment variables: they are files in
`/run/secrets/`.

## Files Zygo writes

```text
~/.local/share/zygo/                    the data folder (ZYGO_DATA_HOME)
├── images/
│   ├── blobs/sha256/…                  compressed layers, as downloaded
│   ├── layers/<digest>/                unpacked layers, shared by every sandbox
│   └── index.json                      image name → manifest
├── cache/
│   ├── venvs/<key>/                    one venv per (image, requirements)
│   ├── flat/<digest>/                  flattened roots, where overlayfs can't be used
│   ├── system/                         records of derived apt layers
│   └── bytecode-failed/                bytecode builds that failed, not retried
├── scripts/  blobs/  deps/             what the API stores: scripts, tars, dependency sets
├── tenants/                            one JSON file per tenant
├── secrets/<tenant>.json               encrypted (ChaCha20-Poly1305); names only readable
├── tokens.json                         token hashes, never secrets        (mode 0600)
├── auth.json                           `zygo login` credentials           (mode 0600)
├── agents/                             the Python and Node agents, kept up to date
├── backends/                           gvisor/runsc · krun/Image (the vm guest kernel)
└── tmp/                                locks, and sandbox roots while they exist

$XDG_RUNTIME_DIR/zygo/                  the runtime folder (ZYGO_RUNTIME_DIR), mode 0700
├── supervisor.sock                     how the CLI talks to the supervisor (mode 0600)
├── supervisor.pid
├── host-report.json                    a short-lived cache of the host checks
└── tenants/<name>/agent.sock …         one socket per warm sandbox
```

Beside your project, `zygo up` writes `zygo.lock`, which you should commit.
`zygo doctor --fix` may write `~/.config/systemd/user/user@.service.d/delegate.conf`,
`/etc/sysctl.d/60-zygo-userns.conf` and
`/etc/systemd/system/zygo-cgroup-favordynmods.service`, and prints each one
before it does. On a
Mac, the VM lives in `~/.lima/zygo/`. There is no Zygo config file: the spec
and the lock are the only configuration.

## Exit codes

```text
  0 ─────── success
  1 ─────── a Zygo error, a failed check, a failed function in `up`, a failed batch line
  2 ─────── the spec is wrong  ·  `bench all`: no verdict, the host was busy
  4 ─────── `exec`: no such function
 75 ─────── `exec`: busy — every slot and the queue are full; retry later
111 ─────── (Mac) the VM could not be reached after three tries
125 ─────── this host cannot run the sandbox, or no supervisor is running
137 ─────── the program was killed: its deadline, or its memory limit
1–255 ───── `run` and `exec`: otherwise, the program's own exit code
```

| Command | Exit |
|---|---|
| `run` | The program's code. `137` for a deadline or memory kill (use `--outcome` to tell which). `2` spec error, `125` host cannot run it, `1` other errors, including `--pull never` with no image. |
| `exec` | The request's code; `137` deadline; `75` busy; `4` unknown function; `125` no supervisor. `--batch`: `0` only if every line succeeded. |
| `up` | `1` if any function failed to start. |
| `doctor` | `0` if no check says `FAIL`. With `--fix`: `1` if you declined or a command failed. |
| `bench` | `0` within budget, `1` a budget missed, `2` (`all` only) no verdict. |
| `agent test` | `1` if any check failed. |
| `supervisor status` · `stop` | `1` if none is running · nothing to stop. |
| `shell` | The shell's own code (`130` if a signal ended it). |
| On a Mac | The Linux command's code; `128 + N` if a signal ended it; `125` if the VM cannot be set up; `111` if it cannot be reached. |

## The outcome file

`zygo run --outcome FILE` writes, when the sandbox ends, why it ended. The
exit code cannot say this alone: a deadline kill and a memory kill are both
`137`, and a judge or a CI step needs to know which. The file is written
atomically — a temporary file, then a rename — so a reader never sees half of
it. If Zygo fails before the program starts, the file is still written, with
`started: false` and the step that failed.

```json
{"exit_code": 137, "timed_out": false, "oom_killed": true, "peak_rss_kb": 65780,
 "wall_ms": 412.7, "plan_ms": 3.1, "start_ms": 9.4, "started": true, "phase": "run"}
```

| Field | Meaning |
|---|---|
| `exit_code` | The same number `zygo run` exits with. |
| `timed_out` | The deadline killed it. |
| `oom_killed` | The memory limit killed it. |
| `peak_rss_kb` | The most memory the sandbox used, in KiB. |
| `wall_ms` | How long the program ran. |
| `plan_ms` · `start_ms` | Time spent planning (spec, pull, venv, network, root) · starting (up to `execve`). |
| `started` | Whether the program ever started. |
| `phase` | Where it ended: `plan`, `start`, or `run`. |

## Ports and sockets

Zygo opens no port unless you run `zygo api`, which listens on
`127.0.0.1:7700` by default. The supervisor listens only on its unix socket,
which only your user can open. No sandbox mode accepts connections from
outside.

<!-- nav: generated by docs/nav.py, do not edit by hand -->

---

← [20. `sandbox.toml`, field by field](20-sandbox-toml.md) · [Contents](README.md) · **Next: [22. Troubleshooting](22-troubleshooting.md) →**
