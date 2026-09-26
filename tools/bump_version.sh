#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Set the release version everywhere it is written, so a tag can be cut.
#
#   tools/bump_version.sh 0.1.4
#
# The release workflow refuses a tag whose version differs from Cargo.toml or
# either SDK manifest, and a unit test refuses a spec/openapi.json whose
# `info.version` is not this build's. Five files and one generated document,
# in one place; CHANGELOG.md is still written by hand.
set -eu

v=${1:?usage: tools/bump_version.sh <version, e.g. 0.1.4>}
case "$v" in
  [0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "not a version: $v" >&2; exit 2 ;;
esac
cd "$(dirname "$0")/.."
old=$(grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)

sub() { # sub <file> <sed expression>
  sed "$2" "$1" > "$1.bump" && mv "$1.bump" "$1"
}
sub Cargo.toml "s/^version = \"$old\"/version = \"$v\"/; s/version = \"$old\" }/version = \"$v\" }/"
sub sdk/python/pyproject.toml "s/^version = \"$old\"/version = \"$v\"/"
sub sdk/python/src/zygo_sdk/__init__.py "s/^__version__ = \"$old\"/__version__ = \"$v\"/"
sub sdk/node/package.json "s/\"version\": \"$old\"/\"version\": \"$v\"/"
sub docs/book/17-api-sdk-mcp.md "s/Both packages are \`$old\`/Both packages are \`$v\`/"

cargo update -w --quiet
# The document carries the version, and the test that guards it compares
# whole documents.
cargo run -q --bin zygo -- api --openapi > spec/openapi.json

for f in Cargo.toml sdk/python/pyproject.toml sdk/python/src/zygo_sdk/__init__.py \
         sdk/node/package.json spec/openapi.json docs/book/17-api-sdk-mcp.md; do
  grep -q "\"$v\"\|\`$v\`" "$f" || { echo "$f does not say $v" >&2; exit 1; }
done
echo "$old -> $v; now write CHANGELOG.md, then: make test && make lint"
