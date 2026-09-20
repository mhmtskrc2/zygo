#!/bin/sh
# Run a repository's tests in a sealed one-shot sandbox. Exit code is theirs.
#
#   sh examples/ci-job/run.sh ./repo
set -eu

repo=${1:?usage: run.sh <repository>}

exec zygo run \
    --mount "$repo:/src:ro" \
    --mem 512M --pids 128 --timeout 10m \
    python:3.12-slim \
    python3 -m unittest discover -s /src -v
