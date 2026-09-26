# SPDX-License-Identifier: Apache-2.0
"""What the package looks like from outside: its name, and its examples.

The module is ``zygo_sdk`` — another project owns ``zygo`` on PyPI — and every
example writes ``import zygo_sdk as zygo``. An example that said ``import
zygo.aio`` looked right and did not run. These read the README and the
package docstring and check each Python block compiles and its imports
resolve, so the next such slip fails here rather than on a reader's machine.
"""

from __future__ import annotations

import ast
import re
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "src"))

import zygo_sdk as zygo  # noqa: E402


def python_blocks(text: str) -> list[str]:
    return re.findall(r"```python\n(.*?)```", text, flags=re.S)


class PackageTests(unittest.TestCase):
    def test_aio_is_reachable_from_the_package(self) -> None:
        # The README's form: one import, then `zygo.aio.connect()`.
        self.assertIs(zygo.aio, sys.modules["zygo_sdk.aio"])
        self.assertTrue(callable(zygo.aio.connect))
        with self.assertRaises(AttributeError):
            zygo.no_such_thing  # noqa: B018 - the lazy attribute is `aio` only

    def test_every_readme_example_compiles_and_imports(self) -> None:
        blocks = python_blocks((ROOT / "README.md").read_text())
        self.assertGreaterEqual(len(blocks), 3, "the README lost its examples")
        for block in blocks:
            tree = compile(block, "README.md", "exec", flags=ast.PyCF_ONLY_AST)
            for node in ast.walk(tree):
                if isinstance(node, ast.Import):
                    for alias in node.names:
                        __import__(alias.name)
                elif isinstance(node, ast.ImportFrom):
                    module = __import__(node.module, fromlist=[a.name for a in node.names])
                    for alias in node.names:
                        self.assertTrue(hasattr(module, alias.name), f"{node.module}.{alias.name}")
                elif isinstance(node, ast.Attribute):
                    # `zygo.<name>`: the alias the README uses for the package.
                    if isinstance(node.value, ast.Name) and node.value.id == "zygo":
                        self.assertTrue(hasattr(zygo, node.attr), f"zygo.{node.attr} does not exist")

    def test_no_example_imports_the_name_another_project_owns(self) -> None:
        sources = [ROOT / "README.md", *sorted((ROOT / "src" / "zygo_sdk").glob("*.py"))]
        for source in sources:
            text = source.read_text()
            self.assertNotRegex(text, r"^\s*import zygo\b(?!_sdk)", source.name)
            self.assertNotRegex(text, r"^\s*from zygo\b(?!_sdk)", source.name)
            self.assertNotIn("import zygo.aio", text, source.name)

    def test_the_docstrings_cross_reference_the_real_module(self) -> None:
        # A Sphinx role has to name the module that exists; `~zygo.Busy`
        # renders as a broken link and reads as a package that is not this one.
        for source in sorted((ROOT / "src" / "zygo_sdk").glob("*.py")):
            self.assertNotRegex(source.read_text(), r":(class|meth|mod|func):`~?zygo\.", source.name)


if __name__ == "__main__":
    unittest.main()
