#!/bin/sh
# Fill `zygo.rb.in` in with the digests of the archives a release just built.
#
#   sh packaging/homebrew/render.sh <version> <directory of artifacts> > zygo.rb
#
# The directory is searched recursively for `*.tar.gz.sha256` files, which is
# the shape `actions/download-artifact` leaves behind when it is given every
# artifact at once. Every digest the template asks for has to be found, or this
# exits non-zero: a formula that installs three architectures and has a
# placeholder in the fourth is worse than no formula, because the placeholder
# is only discovered by whoever has that machine.
set -u

VERSION=${1:-}
ARTIFACTS=${2:-}
REPO=${ZYGO_REPO:-mhmtskrc2/zygo}
TEMPLATE=$(dirname "$0")/zygo.rb.in

if [ -z "$VERSION" ] || [ -z "$ARTIFACTS" ]; then
    echo "usage: render.sh <version> <artifact directory>" >&2
    exit 2
fi

# The digest of the archive for one target, from whichever `.sha256` names it.
digest_for() {
    target=$1
    file=$(find "$ARTIFACTS" -name "zygo-$target.tar.gz.sha256" | head -n 1)
    if [ -z "$file" ]; then
        echo "no checksum for zygo-$target.tar.gz under $ARTIFACTS" >&2
        return 1
    fi
    # `sha256sum` and `shasum -a 256` both print "<digest>  <name>".
    awk '{ print $1; exit }' "$file"
}

DARWIN_ARM=$(digest_for aarch64-apple-darwin) || exit 1
DARWIN_INTEL=$(digest_for x86_64-apple-darwin) || exit 1
LINUX_ARM=$(digest_for aarch64-unknown-linux-musl) || exit 1
LINUX_INTEL=$(digest_for x86_64-unknown-linux-musl) || exit 1

sed \
    -e "s|@@REPO@@|$REPO|g" \
    -e "s|@@VERSION@@|$VERSION|g" \
    -e "s|@@DARWIN_ARM_SHA@@|$DARWIN_ARM|g" \
    -e "s|@@DARWIN_INTEL_SHA@@|$DARWIN_INTEL|g" \
    -e "s|@@LINUX_ARM_SHA@@|$LINUX_ARM|g" \
    -e "s|@@LINUX_INTEL_SHA@@|$LINUX_INTEL|g" \
    "$TEMPLATE"

# A placeholder that survived means the template grew a field this script does
# not fill.
if sed -n '/@@/p' "$TEMPLATE" | grep -q '@@'; then
    remaining=$(sed \
        -e "s|@@REPO@@||g" -e "s|@@VERSION@@||g" \
        -e "s|@@DARWIN_ARM_SHA@@||g" -e "s|@@DARWIN_INTEL_SHA@@||g" \
        -e "s|@@LINUX_ARM_SHA@@||g" -e "s|@@LINUX_INTEL_SHA@@||g" \
        "$TEMPLATE" | grep -c '@@' || true)
    if [ "$remaining" -gt 0 ]; then
        echo "$remaining placeholder(s) in the template are not filled by this script" >&2
        exit 1
    fi
fi
