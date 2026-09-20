.PHONY: help build test test-rust test-agent check check-linux test-linux \
        verify-linux verify-supervisor-linux escape-linux dist-linux \
        syscall-tables conformance conformance-node examples-go-linux \
        seccomp-matrix-linux fmt lint clean

help:
	@echo "build        build the zygo binary"
	@echo "test         run every test suite"
	@echo "check        type-check the workspace"
	@echo "conformance  run the agent protocol suite against both reference agents"
	@echo "check-linux  type-check the Linux-only code from a non-Linux host"
	@echo "test-linux   run the full suite inside a Linux container"
	@echo "verify-linux run the ns launcher isolation checks against a real kernel"
	@echo "verify-supervisor-linux  151 end-to-end supervisor lifecycle checks"
	@echo "escape-linux attempt every known escape vector against a real kernel"
	@echo "fuzz-linux   sweep every syscall number against all three seccomp profiles"
	@echo "gvisor-linux the gvisor backend against a real runsc, compared with ns"
	@echo "dist-linux   build the static musl binary and check it against N6"
	@echo "syscall-tables  regenerate the seccomp syscall number tables"
	@echo "fmt / lint   rustfmt / clippy"

build:
	cargo build --release

test: test-rust test-agent

test-rust:
	cargo test --workspace

test-agent:
	python3 -W error::ResourceWarning -m unittest discover -s agents/python

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
examples-go-linux:
	docker run --rm -v "$(PWD)/examples/warm-exec/go:/w" -w /w -e CGO_ENABLED=0 \
		golang:1.23-alpine sh -c 'mkdir -p bin && go build -o bin/parse .'
	docker run --rm --privileged -v "$(PWD):/src:ro" python:3.12-slim \
		sh /src/poc/verify_examples.sh

# The `ns` backend, the cgroup hierarchy and the doctor probes are all behind
# `#[cfg(target_os = "linux")]`, so a macOS `cargo check` never sees them.
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
verify-linux:
	docker run --rm -t --privileged -v "$(PWD):/src:ro" \
		-e ZYGO_DATA_HOME=/tmp/zdata python:3.12-slim \
		sh /src/poc/verify_launcher.sh

# End-to-end supervisor lifecycle: serve, exec, ps, backpressure, stop.
#
# Separate from verify-linux because it is the only suite where the client
# process exits between steps, which is what exposes failures that live in the
# supervisor's threading rather than in the sandbox.
verify-supervisor-linux:
	docker run --rm --privileged -v "$(PWD):/src:ro" \
		python:3.12-slim \
		sh /src/poc/verify_supervisor.sh

# Known escape vectors (design doc §3.10), each actually attempted.
escape-linux:
	docker run --rm --privileged -v "$(PWD):/src:ro" \
		-e ZYGO_DATA_HOME=/tmp/zdata python:3.12-slim \
		sh /src/poc/escape_suite.sh

# The `gvisor` backend against a real `runsc`, including the same command run
# on `ns` and on `gvisor` for comparison (requirement N8). The data directory
# is a volume so the 114 MB runtime is downloaded once, not once per run.
gvisor-linux:
	docker run --rm --privileged -v "$(PWD):/src:ro" \
		-v zygo-gvisor-data:/data -e ZYGO_DATA_HOME=/data python:3.12-slim \
		sh /src/poc/verify_gvisor.sh

# The escape suite attempts the vectors somebody thought of; this attempts
# every syscall number the architecture has, against all three profiles, and
# compares what the kernel answered. `bash` because the comparisons use
# process substitution.
fuzz-linux:
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
seccomp-matrix-linux:
	docker run --rm --privileged -v "$(PWD):/src:ro" python:3.12-slim \
		sh /src/poc/seccomp_matrix.sh
