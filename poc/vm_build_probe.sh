#!/bin/bash
# V1, asked of a real build: does libkrun link against musl?
#
# Everything about the `vm` backend's distribution story rests on this
# (docs/vm_implementation.md, D8 and M0.1). `dist-linux` ships one static
# binary under 15 MB; if libkrun needs glibc, the vm-capable build is a
# separate target and the README says so. Either answer is the deliverable, so
# this script reports rather than fails: a `RUN` that exits non-zero loses the
# thing being measured.
#
# Run:  make vm-build
set -u

LIBKRUN_REF=${LIBKRUN_REF:-v1.9.7}
LIBKRUNFW_REF=${LIBKRUNFW_REF:-v4.5.1}
OUT=${OUT:-/out}

say()   { printf '%s\n' "$*"; }
head2() { say ""; say "--- $* ---"; }

say "libkrun on musl — the V1 probe"
say "  libkrun   $LIBKRUN_REF"
say "  libkrunfw $LIBKRUNFW_REF"
say "  target    $(uname -m)-unknown-linux-musl"
say "  rustc     $(rustc --version)"

VERDICT_MUSL=unknown
VERDICT_STATIC=unknown
NOTES=""

note() { NOTES="$NOTES
  - $*"; }

head2 "fetching"
# The default branch first, then a tag chosen from what actually exists. A
# `--branch` that names a tag nobody published fails with "remote branch not
# found", which reads like a network problem and is not one — the first run of
# this probe guessed `v1.9.7` and spent its time saying so.
#
# Note the status is read from `git` and not from a pipeline: `git clone |
# tail` reports `tail`'s success, which is how a failed clone reached the next
# step.
clone_log=$(git clone --quiet https://github.com/containers/libkrun.git /build/libkrun 2>&1)
if [ $? -ne 0 ]; then
    say "$clone_log" | tail -5
    say ""
    say "VERDICT: unknown — libkrun could not be fetched, so nothing below was"
    say "         measured. Check this build's network access."
    exit 0
fi
cd /build/libkrun || exit 0

say "published tags, newest last:"
git tag --sort=version:refname | tail -8 | sed 's/^/  /'
say ""
if git rev-parse --verify --quiet "refs/tags/$LIBKRUN_REF" >/dev/null; then
    say "checking out $LIBKRUN_REF"
else
    LIBKRUN_REF=$(git tag --sort=version:refname | tail -1)
    say "the requested ref was not published; using the newest tag instead: $LIBKRUN_REF"
    note "the pinned ref did not exist; this probe measured $LIBKRUN_REF"
fi
git checkout --quiet "$LIBKRUN_REF" || exit 0
say "at $(git rev-parse --short HEAD)"

head2 "what the build expects"
# Read the Makefile rather than guessing: the two things that decide the
# answer are whether it links against libkrunfw and whether it can produce a
# static archive at all.
say "targets:"
grep -oE '^[a-zA-Z0-9_./-]+:' Makefile 2>/dev/null | tr -d ':' | sort -u | head -20 | sed 's/^/  /'
say ""
say "libkrunfw referenced in the Makefile:"
grep -n 'krunfw' Makefile 2>/dev/null | head -10 | sed 's/^/  /' || say "  (not mentioned)"
say ""
say "crate type:"
grep -rn 'crate-type' src/libkrun/Cargo.toml Cargo.toml 2>/dev/null | head -5 | sed 's/^/  /' || say "  (not declared)"

if grep -rq 'crate-type.*staticlib' src/libkrun/Cargo.toml Cargo.toml 2>/dev/null; then
    VERDICT_STATIC="yes (staticlib)"
    note "the crate declares a staticlib target, so a \`.a\` is producible"
else
    VERDICT_STATIC="not needed"
    note "the crate declares cdylib and rlib, not staticlib — and that is fine: Zygo links Rust to Rust, so the rlib cargo produces is the artefact, and no \`.a\` or \`.so\` is involved"
fi

if grep -q 'krunfw' Makefile 2>/dev/null; then
    note "the build links against libkrunfw, so D8's 'libkrunfw is not linked at all' needs \`krun_set_kernel\` to exist in this version *and* the link to be made optional"
fi

head2 "does krun_set_kernel exist in this version?"
# D8 rests on it: it is what keeps the GPL kernel out of an Apache-2.0 binary
# and the 15 MB budget intact.
if grep -rn 'krun_set_kernel' include/ src/ 2>/dev/null | head -5 | sed 's/^/  /'; then
    note "krun_set_kernel is present, so the kernel can be a downloaded artefact (D8)"
else
    say "  not found"
    note "krun_set_kernel is ABSENT in $LIBKRUN_REF: D8 does not hold, the kernel would have to be linked in, and both the licence and the size question reopen"
fi

head2 "building"
say "this is the measurement"
say ""
say "`cargo build -p libkrun`, not `make`. The Makefile builds the whole"
say "workspace into a `cdylib`, and musl supports neither: the workspace"
say "drags in `krun-input`, whose build script wants a static libclang, and"
say "cargo says outright that a cdylib is an unsupported crate type for a"
say "musl target. Neither is what Zygo needs. D1 links libkrun *into* the"
say "binary, which for a Rust library means a cargo dependency and an rlib —"
say "no shared object, no C static library, no Makefile."
say ""
cargo build --release -p libkrun > /tmp/build.log 2>&1
build_rc=$?
tail -20 /tmp/build.log
# `PIPESTATUS`, not the pipeline's status: `make | tail` reports `tail`'s
# success and would call a failed build a pass. This script has now made that
# mistake twice — once on the clone above — which is why both say so.
if [ "$build_rc" -eq 0 ]; then
    VERDICT_MUSL=yes
    note "libkrun compiles cleanly as a Rust library under musl"
    if grep -q 'dropping unsupported crate type .cdylib' /tmp/build.log; then
        note "cargo drops the cdylib on musl, as expected — which is why the project's own \`make\` fails here and why Zygo should depend on the crate rather than link a shared object"
    fi
else
    VERDICT_MUSL=no
    note "the build failed under musl (exit $build_rc); the last 40 lines are above"
    # The failure that matters is the *first* error, not the last line.
    first=$(grep -m1 -E '^error(\[|:)' /tmp/build.log)
    [ -n "$first" ] && note "first error: $first"
fi

head2 "what came out"
found=$(find /build/libkrun/target/release -maxdepth 1 \
    \( -name 'libkrun*.rlib' -o -name 'libkrun*.a' -o -name 'libkrun*.so*' \) 2>/dev/null | head -10)
if [ -n "$found" ]; then
    for f in $found; do
        say "  $(file -b "$f" | cut -c1-90)"
        say "    $f  ($(stat -c %s "$f" 2>/dev/null) bytes)"
    done
    mkdir -p "$OUT" 2>/dev/null && cp $found "$OUT"/ 2>/dev/null && say "  copied to $OUT"
else
    say "  nothing"
fi

head2 "verdict"
say "  builds under musl:        $VERDICT_MUSL"
say "  staticlib target present: $VERDICT_STATIC"
say "$NOTES"
say ""
say "Record this in docs/vm_implementation.md under V1 and in todo.md's M0,"
say "whichever way it went. A negative answer is a decision, not a blocker:"
say "the fallback is a glibc \`dist-linux-vm\` target and a sentence in the"
say "README saying why there are two binaries."
exit 0
