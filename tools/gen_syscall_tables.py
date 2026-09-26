#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Regenerate `crates/zygo-core/src/backend/ns/syscalls.rs`.

Run after adding a syscall name to any profile in `seccomp.rs`. A name with no
number is silently dropped from the generated filter, which means either a hole
(a denied syscall that is never actually compared against) or a package that
mysteriously stops working — neither visible by reading the code.

The table holds **every** syscall the architecture's header defines, not only
the ones a profile names. That completeness is what the filter's last rule
rests on: a number above the table's highest answers `ENOSYS` ("this build has
never heard of it"), and one at or below it answers `EPERM` ("known, refused").
A table with gaps would answer `EPERM` for a syscall nobody ever decided on —
which is how `fchmodat2`, the syscall glibc's `chmod` tries first, would break
`python3 -m venv` the moment anything numbered above it joined the table.

The profile names are still read out of `seccomp.rs`, to check that every one
of them resolves on at least one architecture.

Numbers come from each architecture's own `<sys/syscall.h>`, **compiled** rather
than preprocessed — arm64 defines several as `__NR3264_*` aliases that only a
compiler resolves.

    make syscall-tables

Needs Docker with binfmt for the non-native architecture.
"""

from __future__ import annotations

import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SECCOMP = ROOT / "crates/zygo-core/src/backend/ns/seccomp.rs"
TARGET = ROOT / "crates/zygo-core/src/backend/ns/syscalls.rs"

# Every constant whose names must be resolvable. `SPECIAL_CASED` matters as much
# as the rest: `clone` appears in no allowlist because it is filtered on its
# arguments, and leaving it out produced a filter that denied every fork.
NAME_CONSTANTS = [
    "BASE_ALLOWLIST",
    "PERMISSIVE_EXTRA",
    "STRICT_REMOVED",
    "STRICT_CHILD_REMOVED",
    "NEVER_ALLOWED",
    "NEWER_REFUSED",
    "SPECIAL_CASED",
]

# The compiler image, and the kernel headers the numbers come from: gcc:13 is
# Debian bookworm, and bookworm-backports carries linux-libc-dev 6.12.
# `ZYGO_GEN_IMAGE` overrides the image, with `{arch}` standing for aarch64 or
# x86_64 — for a host that already has one per architecture and cannot pull.
IMAGE = os.environ.get("ZYGO_GEN_IMAGE", "gcc:13")
HEADERS = (
    'echo "deb http://deb.debian.org/debian bookworm-backports main" '
    "> /etc/apt/sources.list.d/backports.list && apt-get update -qq && "
    "apt-get install -qq -t bookworm-backports linux-libc-dev >/dev/null 2>&1 && "
)

ARCHES = [("linux/arm64", "aarch64", "0xc000_00b7"), ("linux/amd64", "x86_64", "0xc000_003e")]


def syscall_names() -> list[str]:
    source = SECCOMP.read_text()
    names: set[str] = set()
    for const in NAME_CONSTANTS:
        match = re.search(rf"pub const {const}: &\[&str\] = &\[(.*?)\];", source, re.S)
        if match is None:
            sys.exit(f"{const} not found in {SECCOMP.name}")
        found = re.findall(r'"([a-z0-9_]+)"', match.group(1))
        print(f"  {const:22} {len(found)} names")
        names.update(found)
    return sorted(names)


def probe(platform: str, arch: str, names: list[str]) -> list[tuple[str, int]]:
    """Compile a program that prints each syscall's number on `platform`."""
    lines = ["#include <sys/syscall.h>", "#include <stdio.h>", "int main(void){"]
    # Not syscalls: bookkeeping macros that share the prefix.
    names = [n for n in names if n not in ("syscalls", "arch_specific_syscall")]
    for name in names:
        lines += [
            f"#ifdef __NR_{name}",
            f'  printf("{name} %d\\n", (int)__NR_{name});',
            "#endif",
        ]
    lines.append("  return 0;}")

    program = "\n".join(lines)
    result = subprocess.run(
        ["docker", "run", "--rm", "-i", "--platform", platform, IMAGE.format(arch=arch),
         "sh", "-c",
         HEADERS + "cat > /tmp/gen.c && gcc -O0 -o /tmp/gen /tmp/gen.c && /tmp/gen"],
        input=program.encode(),
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        sys.exit(f"{platform}: {result.stderr.decode()[:400]}")

    table = []
    for line in result.stdout.decode().splitlines():
        parts = line.split()
        if len(parts) == 2:
            table.append((parts[0], int(parts[1])))
    return sorted(table)


def header_names(platform: str, arch: str) -> list[str]:
    """Every `__NR_<name>` the platform's `<sys/syscall.h>` defines."""
    result = subprocess.run(
        ["docker", "run", "--rm", "--platform", platform, IMAGE.format(arch=arch), "sh", "-c",
         HEADERS + "echo '#include <sys/syscall.h>' | gcc -dM -E -"],
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        sys.exit(f"{platform}: {result.stderr.decode()[:400]}")
    return sorted(set(re.findall(r"#define __NR_([a-z0-9_]+) ", result.stdout.decode())))


def render(entries: list[tuple[str, int]]) -> str:
    return "\n".join(f'    ("{name}", {number}),' for name, number in entries)


def main() -> int:
    print(f"reading profiles from {SECCOMP.relative_to(ROOT)}")
    wanted = syscall_names()
    print(f"  union: {len(wanted)} names\n")

    source = TARGET.read_text()
    resolved: set[str] = set()
    for platform, arch, _audit in ARCHES:
        print(f"probing {platform}…")
        table = probe(platform, arch, header_names(platform, arch))
        resolved.update(name for name, _ in table)
        print(f"  {arch}: {len(table)} syscalls, highest {max(nr for _, nr in table)}")

        pattern = (
            rf'#\[cfg\(target_arch = "{arch}"\)\]\n'
            r"pub const TABLE: &\[\(&str, u32\)\] = &\[.*?\n\];"
        )
        replacement = (
            f'#[cfg(target_arch = "{arch}")]\n'
            f"pub const TABLE: &[(&str, u32)] = &[\n{render(table)}\n];"
        )
        source, count = re.subn(pattern, replacement, source, flags=re.S)
        if count != 1:
            sys.exit(f"could not find the {arch} table in {TARGET.name}")

    missing = [n for n in wanted if n not in resolved]
    if missing:
        sys.exit(f"named in a profile but on neither architecture: {missing}")

    TARGET.write_text(source)
    print(f"\nwrote {TARGET.relative_to(ROOT)}")
    print("now run: make test-linux")
    return 0


if __name__ == "__main__":
    sys.exit(main())
