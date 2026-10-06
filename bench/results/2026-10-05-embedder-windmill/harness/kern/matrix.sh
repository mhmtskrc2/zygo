#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
B=$HOME/kbench/bench.py
run() { systemd-run --user --scope -p Delegate=yes -q -- python3 $B "$@"; echo; }
run zygo true 64 1 4 8 16 32
run kern true 64 1 4 8 16 32
run zygo harness 64 1 4 8 16 32
run kern harness 64 1 4 8 16 32
run kern-pyc harness 64 1 4 8 16 32
