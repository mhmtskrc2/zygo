# Tests that need a kernel

The Rust unit and integration tests live beside the code, under
`crates/*/tests`, and `cargo test` finds them there. This folder holds what
`cargo test` cannot run: suites that attempt real things against a real Linux
kernel, and the experiments the design was checked on before there was a
`zygo` binary. Cargo does not look here, so nothing collides.

```text
tests/
├── linux/      the suites CI runs: escapes, syscalls, seccomp, the supervisor,
│   │           the API, the VM backend, the benchmarks
│   └── bin/    build outputs (gitignored): zygo-linux-musl, zygo-linux-musl-vm,
│               the guest kernel in vm-out/
└── poc/        the numbered proofs of concept, the record of what was tried first
```

## `tests/linux/`

Every suite needs Linux, cgroup v2 and, for most of them, a privileged
Docker container or a delegated cgroup scope. Each `make` target builds the
static binary first (`make tests/linux/bin/zygo-linux-musl`, in a
`rust:1-alpine` container) and runs the suite in `python:3.12-slim` with the
checkout mounted read-only at `/src`. `tests/linux/cgroup_harness.sh` is
sourced by every suite: it finds a cgroup it may write to, or re-runs the
suite inside a `systemd-run` scope. `ci_cgroup.sh` does the same on a
GitHub runner.

| Suite | `make` target | What it attempts |
|---|---|---|
| `verify_launcher.sh` | `verify-linux` | isolation and limit checks on the `ns` launcher |
| `verify_supervisor.sh` | `verify-supervisor-linux` | 139 end-to-end supervisor lifecycle checks |
| `verify_api.sh`, `api_driver.py`, `usage_sink.py` | `verify-api-linux` | the HTTP API, through the Python client |
| `verify_plugin_host.sh`, `verify_plugin_host_node.sh` | `verify-plugin-host`, `verify-plugin-host-node` | a plugin host on the API alone |
| `verify_mcp.sh`, `mcp_driver.py` | `verify-mcp` | the MCP server over a pipe |
| `escape_suite.sh` | `escape-linux` | every known escape vector, attempted |
| `fuzz_syscalls.sh` | `fuzz-linux` | every syscall number against all three seccomp profiles |
| `seccomp_matrix.sh`, `seccomp_matrix_node.sh` | `seccomp-matrix-linux` | real packages, and Node, under `default` and `strict` |
| `verify_seccomp_profiles.sh` | `verify-seccomp-profiles-linux` | `copy2` and `pip --target` under every profile |
| `verify_landlock_net.sh` | `landlock-net-linux` | Landlock's bind/connect rules, on a 6.7+ kernel |
| `verify_gvisor.sh` | `gvisor-linux` | the `gvisor` backend against a real `runsc` |
| `verify_examples.sh` | `examples-go-linux` | the Go warm-exec example, built and run |
| `verify_login.sh`, `wait_for_registry.sh` | `verify-login-linux` | `zygo login` against a registry that refuses people |
| `verify_oci.sh` | `verify-oci` | the container image, run unprivileged |
| `check_dist.sh` | `dist-linux` | the static binary's size and linking |
| `deps_probe.sh`, `deps_driver.py` | `verify-deps-linux` | a dependency set built from a lockfile |
| `use_cases.sh` | `use-cases-linux` | fifty scenarios, by use case rather than by mechanism |
| `repro_blue_green.sh` | `repro-blue-green-linux` | the one open supervisor question, five times |
| `fork_sweep.sh`, `fork_sweep.py` | `fork-sweep-linux` | popular PyPI packages forked under load |
| `agent_stall.py` | — | a stalled handler against a reference agent |
| `bench_all.sh`, `bench_embed.*`, `bench_density.*` | `bench`, `bench-embed`, `bench-density`, `bench-record` | the numbers in chapter 25; records go to `bench/` |
| `verify_shim.sh`, `verify_shim_concurrency.sh` | `verify-shim`, `verify-shim-concurrency` | the macOS shim against the Linux VM it manages (runs on a Mac) |
| `vm_use_cases.sh`, `verify_vm.sh`, `vm_pi.sh`, `pi_env.sh` | `vm-use-cases-linux`, `verify-vm-pi` | the `vm` backend, with and without a booted guest |
| `Dockerfile.vm`, `vm_kernel.sh`, `vm_build_probe.sh` | `vm-build`, `vm-kernel`, `vm-probe` | the vm-capable binary, the guest kernel, the libkrun probe |
| `build_guest.sh` | `guest-build` | the Linux binary compiled inside the macOS VM, without Docker |
| `cleanup.sh` | — | clears sandboxes an interrupted run left behind |

`make help` lists every target with one line each. On a Mac, `make test-linux`
runs the Rust suite inside a container; the suites above need the Docker
daemon to allow `--privileged`.

## `tests/poc/`

Six numbered experiments from before the first line of the launcher —
namespaces, cgroup limits, copy-on-write pollution, seccomp against real
packages, overlayfs in a user namespace, a network namespace pool — each with
its `docker run` line in its header. They are the record of what was tried
before the design was fixed; nothing in the book depends on them and CI does
not run them. The third, the warm request path, became
`crates/zygo-core/examples/poc3_warm_path.rs`; its old cross-compiled binary
`poc3-linux` is gitignored here.

## `tools/`

Not tests, but kept out of `crates/` for the same reason: `make
syscall-tables` runs `tools/gen_syscall_tables.py` to regenerate the seccomp
syscall number tables, and CI checks the committed tables against a fresh
generation.
