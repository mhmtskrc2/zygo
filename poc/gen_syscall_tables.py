#!/usr/bin/env python3
"""Regenerate `crates/zygo-core/src/backend/ns/syscalls.rs`.

Run after adding a syscall name to any profile in `seccomp.rs`. A name with no
number is silently dropped from the generated filter, which means either a hole
(a denied syscall that is never actually compared against) or a package that
mysteriously stops working — neither visible by reading the code.

The names are read back out of `seccomp.rs`, so the two files cannot drift: the
profiles are the source of truth and this only resolves them to numbers.

Numbers come from each architecture's own `<sys/syscall.h>`, **compiled** rather
than preprocessed — arm64 defines several as `__NR3264_*` aliases that only a
compiler resolves.

    make syscall-tables

Needs Docker with binfmt for the non-native architecture.
"""

from __future__ import annotations

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
    "SPECIAL_CASED",
]

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


def probe(platform: str, names: list[str]) -> list[tuple[str, int]]:
    """Compile a program that prints each syscall's number on `platform`."""
    lines = ["#include <sys/syscall.h>", "#include <stdio.h>", "int main(void){"]
    for name in names:
        lines += [
            f"#ifdef __NR_{name}",
            f'  printf("{name} %d\\n", (int)__NR_{name});',
            "#endif",
        ]
    lines.append("  return 0;}")

    program = "\n".join(lines)
    result = subprocess.run(
        ["docker", "run", "--rm", "-i", "--platform", platform, "gcc:13",
         "sh", "-c", "cat > /tmp/gen.c && gcc -O0 -o /tmp/gen /tmp/gen.c && /tmp/gen"],
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


def render(entries: list[tuple[str, int]]) -> str:
    return "\n".join(f'    ("{name}", {number}),' for name, number in entries)


def main() -> int:
    print(f"reading profiles from {SECCOMP.relative_to(ROOT)}")
    names = syscall_names()
    print(f"  union: {len(names)} names\n")

    source = TARGET.read_text()
    for platform, arch, _audit in ARCHES:
        print(f"probing {platform}…")
        table = probe(platform, names)
        print(f"  {arch}: {len(table)} resolved, {len(names) - len(table)} absent here")

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

    TARGET.write_text(source)
    print(f"\nwrote {TARGET.relative_to(ROOT)}")
    print("now run: make test-linux")
    return 0


if __name__ == "__main__":
    sys.exit(main())
