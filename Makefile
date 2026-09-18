.PHONY: help build test test-rust test-agent check check-linux test-linux \
        verify-linux verify-supervisor-linux escape-linux dist-linux \
        syscall-tables fmt lint clean

help:
	@echo "build        build the zygo binary"
	@echo "test         run every test suite"
	@echo "check        type-check the workspace"
	@echo "check-linux  type-check the Linux-only code from a non-Linux host"
	@echo "test-linux   run the full suite inside a Linux container"
	@echo "verify-linux run the ns launcher isolation checks against a real kernel"
	@echo "verify-supervisor-linux  end-to-end supervisor lifecycle checks"
	@echo "escape-linux attempt every known escape vector against a real kernel"
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

# The `ns` backend, the cgroup hierarchy and the doctor probes are all behind
# `#[cfg(target_os = "linux")]`, so a macOS `cargo check` never sees them.
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
		sh -c "cargo test --workspace && python3 -m unittest discover -s agents/python || true"

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
	cargo clippy --workspace --all-targets -- -D warnings

clean:
	cargo clean
