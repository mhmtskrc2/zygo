.PHONY: help build test test-rust test-agent test-sdk test-sdk-python test-sdk-node \
        verify-mcp check check-linux test-linux \
        verify-linux verify-supervisor-linux escape-linux dist-linux \
        fuzz-linux gvisor-linux verify-login-linux verify-shim \
        repro-blue-green-linux verify-api-linux vm-build vm-probe vm-kernel \
        verify-vm-pi use-cases-linux vm-use-cases-linux \
        syscall-tables conformance conformance-node examples-go-linux \
        seccomp-matrix-linux fmt lint clean

help:
	@echo "build        build the zygo binary"
	@echo "test         run every test suite"
	@echo "test-sdk     the Python and Node clients, against a stand-in API"
	@echo "verify-mcp   drive the MCP server over a pipe, as an agent host does"
	@echo "verify-api-linux  the HTTP API end to end, through the Python client"
	@echo "vm-build     build the vm-capable binary (libkrun linked in)"
	@echo "vm-kernel    build the guest kernel and check its config"
	@echo "vm-probe     ask whether libkrun links against musl (the vm plan V1)"
	@echo "verify-vm-pi the vm backend on the Pi, compared with ns"
	@echo "vm-use-cases-linux  twenty vm cases that need no booted guest"
	@echo "use-cases-linux  fifty scenarios, by use case rather than by mechanism"
	@echo "check        type-check the workspace"
	@echo "conformance  run the agent protocol suite against both reference agents"
	@echo "check-linux  type-check the Linux-only code from a non-Linux host"
	@echo "test-linux   run the full suite inside a Linux container"
	@echo "verify-linux run the ns launcher isolation checks against a real kernel"
	@echo "verify-supervisor-linux  139 end-to-end supervisor lifecycle checks"
	@echo "repro-blue-green-linux   the one open supervisor question, five times"
	@echo "escape-linux attempt every known escape vector against a real kernel"
	@echo "fuzz-linux   sweep every syscall number against all three seccomp profiles"
	@echo "gvisor-linux the gvisor backend against a real runsc, compared with ns"
	@echo "dist-linux   build the static musl binary and check it against N6"
	@echo "verify-login-linux  zygo login against a registry that really refuses people"
	@echo "examples-go-linux   the Go warm-exec example, built and run for real"
	@echo "seccomp-matrix-linux  five reference packages under default and strict"
	@echo "conformance / conformance-node  the agent protocol suite"
	@echo "verify-shim  the macOS shim, against the Linux VM it manages"
	@echo "syscall-tables  regenerate the seccomp syscall number tables"
	@echo "fmt / lint   rustfmt / clippy"

build:
	cargo build --release

test: test-rust test-agent test-sdk

test-rust:
	cargo test --workspace

test-agent:
	python3 -W error::ResourceWarning -m unittest discover -s agents/python

# The two clients. Neither needs Linux, a kernel or a sandbox: what is under
# test is the client — the transport, the error mapping, the connection pool.
# A test that needs a real sandbox belongs in the Rust suites.
test-sdk: test-sdk-python test-sdk-node

test-sdk-python:
	cd sdk/python && python3 -W error::ResourceWarning -m unittest discover -s tests

test-sdk-node:
	cd sdk/node && node --test 'test/*.test.js'

# `zygo mcp` driven over a pipe, the way an agent host drives it: a real
# handshake, a real tool list, and a tool call that really runs a sandbox.
# Needs a kernel, so it runs in a container.
verify-mcp: poc/zygo-linux-musl
	docker run --rm --privileged -v "$(PWD):/src:ro" \
		-e ZYGO_DATA_HOME=/tmp/zdata python:3.12-slim \
		sh /src/poc/verify_mcp.sh

check:
	cargo check --workspace --all-targets

# The protocol conformance suite, against both reference agents. The claim is
# that the wire is language independent; this is what makes it checkable.
# The `sh` agent needs `jq`.
conformance: build
	./target/release/zygo agent test python3 -- agents/python/zygo_agent.py --fd 3 \
		examples/agents/conformance/handler.py
	./target/release/zygo agent test /bin/sh -- examples/agents/sh/agent.sh \
		examples/agents/sh/handler.sh

# The Node agent, against the protocol suite. The binary has to be the static
# musl one: the node image's glibc is older than the build image's.
conformance-node: poc/zygo-linux-musl
	docker run --rm -v "$(PWD):/src:ro" node:22-slim \
		/src/poc/zygo-linux-musl agent test node -- \
		/src/examples/agents/node/agent.js /src/examples/agents/node/handler.js

# The static binary the cross-image checks need. `make dist-linux` checks the
# same build against N6; this one keeps it.
# Phony: the binary has to be rebuilt when the sources change, and make cannot
# see that through a docker build. The target dir and registry are volumes so a
# rebuild is incremental — this one is copied to a real Linux host to test on,
# so it is rebuilt often.
.PHONY: poc/zygo-linux-musl
poc/zygo-linux-musl:
	docker run --rm -v "$(PWD):/w" -w /w \
		-v zygo-musl-target:/target -v zygo-musl-registry:/usr/local/cargo/registry \
		-e CARGO_TARGET_DIR=/target \
		rust:1-alpine sh -c 'apk add --no-cache musl-dev >/dev/null && \
		cargo build --release -p zygo-cli && cp /target/release/zygo /w/poc/zygo-linux-musl'

# The Go warm-exec example, built in a Go container and then run for real:
# `zygo up` on alpine, `zygo exec` through the CLI.
examples-go-linux: poc/zygo-linux-musl
	docker run --rm -v "$(PWD)/examples/warm-exec/go:/w" -w /w -e CGO_ENABLED=0 \
		golang:1.23-alpine sh -c 'mkdir -p bin && go build -o bin/parse .'
	docker run --rm --privileged -v "$(PWD):/src:ro" python:3.12-slim \
		sh /src/poc/verify_examples.sh

# The `ns` backend, the cgroup hierarchy and the doctor probes are all behind
# `#[cfg(target_os = "linux")]`, so a macOS `cargo check` never sees them.
# `zygo login` against a registry that really refuses people.
#
# Two containers: a `registry:2` with htpasswd authentication, and the suite
# sharing its network namespace so the registry is at `127.0.0.1:5000` — one
# of the names Zygo reaches over plain HTTP. Nothing here needs a sandbox, so
# it needs no privileges either.
verify-login-linux: poc/zygo-linux-musl
	@mkdir -p /tmp/zygo-reg-auth
	@docker run --rm httpd:2 htpasswd -Bbn zygotest s3cret > /tmp/zygo-reg-auth/htpasswd
	@docker rm -f zygo-reg >/dev/null 2>&1 || true
	@docker run -d --name zygo-reg -v /tmp/zygo-reg-auth:/auth \
		-e REGISTRY_AUTH=htpasswd -e REGISTRY_AUTH_HTPASSWD_REALM=zygo \
		-e REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd registry:2 >/dev/null
	@poc/wait_for_registry.sh
	@set +e; \
		docker run --rm --network container:zygo-reg -v "$(PWD):/src:ro" \
			python:3.12-slim sh /src/poc/verify_login.sh; \
		status=$$?; \
		docker rm -f zygo-reg >/dev/null 2>&1; \
		exit $$status

# The macOS shim: a real Lima VM, a real Linux kernel, from this Mac. Skips
# itself anywhere else, and where `limactl` is not installed.
verify-shim: build
	sh poc/verify_shim.sh

# Type-checking against a Linux target does, without needing a Linux host.
# `--no-default-features` drops the registry client, whose TLS stack needs a
# cross C toolchain that is not worth installing for a type check.
check-linux:
	rustup target add aarch64-unknown-linux-musl
	cargo check -p zygo-core --no-default-features --target aarch64-unknown-linux-musl

# Behaviour that only a Linux kernel can show — namespaces, cgroups, seccomp —
# is tested in a container. Privileged because the sandbox tests need to create
# namespaces and write cgroup limits.
test-linux:
	docker run --rm --privileged \
		-v "$(PWD):/src" -w /src \
		-e CARGO_TARGET_DIR=/tmp/target \
		rust:1.90 \
		sh -c "cargo test --workspace && python3 -m unittest discover -s agents/python"

# The `ns` launcher can only be exercised on Linux. Runs the isolation and
# limit checks against a real kernel.
# `-t` allocates a terminal, without which the TIOCSTI check cannot run.
verify-linux: poc/zygo-linux-musl
	docker run --rm -t --privileged -v "$(PWD):/src:ro" \
		-e ZYGO_DATA_HOME=/tmp/zdata python:3.12-slim \
		sh /src/poc/verify_launcher.sh

# End-to-end supervisor lifecycle: serve, exec, ps, backpressure, stop.
#
# Separate from verify-linux because it is the only suite where the client
# process exits between steps, which is what exposes failures that live in the
# supervisor's threading rather than in the sandbox.
verify-supervisor-linux: poc/zygo-linux-musl
	docker run --rm --privileged -v "$(PWD):/src:ro" \
		python:3.12-slim \
		sh /src/poc/verify_supervisor.sh

# The one open supervisor question, on its own: a request queued behind a
# function being replaced. `verify-supervisor-linux` checks it once inside a
# thirty-five minute run and it failed once on a Raspberry Pi; this runs only
# that scenario, five times, and prints the timings of each.
repro-blue-green-linux: poc/zygo-linux-musl
	docker run --rm --privileged -v "$(PWD):/src:ro" \
		-e ZYGO_DATA_HOME=/tmp/zdata-bluegreen python:3.12-slim \
		sh /src/poc/repro_blue_green.sh

# The HTTP API end to end, driven by the Python client that ships with it:
# client -> unix socket -> `zygo api` -> the supervisor -> a real sandbox. The
# unit tests on either side of that line cannot reach it.
verify-api-linux: poc/zygo-linux-musl
	docker run --rm --privileged -v "$(PWD):/src:ro" \
		-e ZYGO_DATA_HOME=/tmp/zdata-api python:3.12-slim \
		sh /src/poc/verify_api.sh

# V1, the question the whole `vm` plan rests on: does libkrun build and link
# against musl? `dist-linux` ships one static binary under 15 MB, and if the
# answer is no the vm-capable build is a separate glibc target and the README
# has to say so. The probe reports rather than fails — either answer is the
# deliverable.
vm-build: poc/zygo-linux-musl-vm

# The vm-capable binary: `zygo` with `--features vm`, built natively on musl in
# the same container `make poc/zygo-linux-musl` uses.
#
# Natively, and that is not incidental. A cross-check from a Mac to
# `aarch64-unknown-linux-musl` fails inside libkrun's virtiofs passthrough on
# `libc::statx`, and the same source built natively does not: with `--target`,
# cargo keeps host and target feature resolution apart, and what the native
# build gets from that unification the cross-build does not. The container is
# the host, so the question does not arise.
.PHONY: poc/zygo-linux-musl-vm
poc/zygo-linux-musl-vm: poc/vm-builder
	docker run --rm -v "$(PWD):/w" -w /w \
		-v zygo-musl-target-vm:/target -v zygo-musl-registry:/usr/local/cargo/registry \
		-e CARGO_TARGET_DIR=/target \
		zygo-vm-build sh -c 'cargo build --release -p zygo-cli --features zygo-core/vm && \
		cp /target/release/zygo /w/poc/zygo-linux-musl-vm'
	@file poc/zygo-linux-musl-vm 2>/dev/null || true
	@ls -lh poc/zygo-linux-musl-vm

.PHONY: poc/vm-builder
poc/vm-builder:
	docker build -f poc/Dockerfile.vm -t zygo-vm-build poc/

# The guest kernel: built once, extracted, and its config checked against what
# a Zygo guest needs. The output is what `zygo backend install vm` will
# download, and the digest to pin it by.
vm-kernel: poc/vm-builder
	@mkdir -p poc/vm-out
	@set +e; \
		docker run --rm -v "$(PWD)/poc/vm-out:/out" --entrypoint sh \
			zygo-vm-build /build/vm_kernel.sh > poc/vm-out/kernel.log 2>&1; \
		status=$$?; \
		tail -40 poc/vm-out/kernel.log; \
		exit $$status
	@echo ""
	@echo "the image is in poc/vm-out/Image"

# The V1 probe: libkrun on its own, with a verdict. Kept because the answer is
# a record, and because it is where a newer libkrun gets asked the same
# question.
vm-probe: poc/vm-builder
	@mkdir -p poc/vm-out
	@set +e; \
		docker run --rm --entrypoint /build/vm_build_probe.sh \
			-v "$(PWD)/poc/vm-out:/out" zygo-vm-build > poc/vm-out/v1-probe.log 2>&1; \
		status=$$?; \
		cat poc/vm-out/v1-probe.log; \
		exit $$status
	@echo ""
	@echo "the log is in poc/vm-out/v1-probe.log"

# The `vm` backend on a real KVM, compared with `ns`. Says so rather than
# reporting a pass on a host where no guest ran — see V11.
verify-vm-pi: poc/zygo-linux-musl-vm
	@sh poc/vm_pi.sh

# Twenty use cases for the `vm` backend that do not need the guest to boot:
# what it refuses, what it resolves, what `doctor` says, and what is left
# behind when a guest never comes up. `verify_vm.sh` is the other half — the
# one that needs a working guest — and this is the half that runs today.
vm-use-cases-linux: poc/zygo-linux-musl-vm poc/zygo-linux-musl
	@sh poc/vm_use_cases.sh

# Fifty scenarios organised by who is asking rather than by mechanism: an
# agent tool runner, a platform embedder, multi-tenant functions, untrusted
# file parsing, a home lab, an online judge, security isolation. The class of
# bug it finds is the one where every part works and the combination does not.
# `passt` and `nftables` are installed on purpose: without them the three
# network scenarios skip, and a suite that skips the positive control cannot
# say whether the egress refusals mean anything. `file` is for the
# static-binary check. The Raspberry Pi has all three and confines `pasta`
# with AppArmor instead, so the two hosts test different halves.
use-cases-linux: poc/zygo-linux-musl
	docker run --rm --privileged -v "$(PWD):/src:ro" \
		-e ZYGO_DATA_HOME=/tmp/zdata-usecases python:3.12-slim \
		sh -c 'apt-get -qq update >/dev/null 2>&1 && \
		apt-get -qq install -y passt nftables file >/dev/null 2>&1; \
		sh /src/poc/use_cases.sh' 

# Known escape vectors (design doc §3.10), each actually attempted.
escape-linux: poc/zygo-linux-musl
	docker run --rm --privileged -v "$(PWD):/src:ro" \
		-e ZYGO_DATA_HOME=/tmp/zdata python:3.12-slim \
		sh /src/poc/escape_suite.sh

# The `gvisor` backend against a real `runsc`, including the same command run
# on `ns` and on `gvisor` for comparison (requirement N8). The data directory
# is a volume so the 114 MB runtime is downloaded once, not once per run.
gvisor-linux: poc/zygo-linux-musl
	docker run --rm --privileged -v "$(PWD):/src:ro" \
		-v zygo-gvisor-data:/data -e ZYGO_DATA_HOME=/data python:3.12-slim \
		sh /src/poc/verify_gvisor.sh

# The escape suite attempts the vectors somebody thought of; this attempts
# every syscall number the architecture has, against all three profiles, and
# compares what the kernel answered. `bash` because the comparisons use
# process substitution.
fuzz-linux: poc/zygo-linux-musl
	docker run --rm --privileged -v "$(PWD):/src:ro" \
		-e ZYGO_DATA_HOME=/tmp/zdata python:3.12-slim \
		sh -c 'apt-get -qq update >/dev/null 2>&1 && apt-get -qq install -y bash >/dev/null 2>&1; bash /src/poc/fuzz_syscalls.sh'

# Regenerate crates/zygo-core/src/backend/ns/syscalls.rs.
#
# The numbers come from each architecture's own <sys/syscall.h>, compiled rather
# than preprocessed so arm64's __NR3264_* aliases resolve. Run this after adding
# a syscall to any profile in seccomp.rs — a name with no number is silently
# dropped from the filter.
syscall-tables:
	python3 poc/gen_syscall_tables.py

# Requirement N6: one static binary, no runtime dependencies, small enough to
# `curl | sh`. Fails if it stops being static or grows past the budget.
dist-linux:
	docker run --rm -v "$(PWD):/src" -w /src -e CARGO_TARGET_DIR=/tmp/target \
		rust:1-alpine sh -c '\
		apk add --no-cache musl-dev file >/dev/null && \
		cargo build --release && \
		BIN=/tmp/target/release/zygo && \
		file "$$BIN" && \
		SIZE=$$(stat -c %s "$$BIN") && \
		echo "size: $$((SIZE / 1048576)) MB" && \
		file "$$BIN" | grep -q "statically linked" || { echo "not static"; exit 1; } && \
		[ "$$SIZE" -lt 15728640 ] || { echo "over the 15 MB budget"; exit 1; }'

fmt:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	cargo clippy --workspace --all-targets -- -D warnings

clean:
	cargo clean

# The `strict` seccomp profile against real packages: requests, pydantic,
# numpy, pandas, Pillow — imported and exercised under both profiles in one
# venv. The output is the compatibility matrix in docs/seccomp-profiles.md.
seccomp-matrix-linux: poc/zygo-linux-musl
	docker run --rm --privileged -v "$(PWD):/src:ro" python:3.12-slim \
		sh /src/poc/seccomp_matrix.sh
