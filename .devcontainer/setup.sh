#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Run once when the dev container is created: the tools the test suites and
# networked sandboxes need, and a first build. `zygo doctor` at the end says
# what this particular host can and cannot do.
set -eu

apt-get update -qq
# python3 and nodejs for the agents and SDK tests; jq for the sh agent;
# passt, nftables and iproute2 for `network = "egress"`; uidmap for a
# subordinate uid range.
apt-get install -y -qq python3 nodejs jq passt nftables iproute2 uidmap >/dev/null

rustup component add clippy rustfmt
cargo build --release
# The binary, behind a wrapper that starts it in the cgroup cgroup.sh made.
install -D -m 0755 target/release/zygo /usr/local/lib/zygo/zygo
install -m 0755 .devcontainer/zygo-wrapper.sh /usr/local/bin/zygo

sh .devcontainer/cgroup.sh
zygo doctor || true
echo
echo "ready: try 'make test', or 'zygo run python:3.12-slim python3 -c \"print(1)\"'"
