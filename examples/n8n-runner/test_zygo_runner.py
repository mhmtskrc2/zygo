# SPDX-License-Identifier: Apache-2.0
"""The part of the runner a mistake in hides best: how a Code node's code is
wrapped into a script a pool can run.

n8n hands the runner a function *body* — code that ends in `return` — and the
names it may use (`$input`, `items`, `$json` in JavaScript; `_items`, `_item`
in Python). The runner turns that into a module whose `handler(event)` the
pool's agent calls. These tests run the generated script, not read it.

    PYTHONPATH=sdk/python/src python3 -m unittest discover -s examples/n8n-runner
"""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from zygo_runner import build_script  # noqa: E402

ITEMS = [{"json": {"id": 1, "name": "a"}}, {"json": {"id": 2, "name": "b"}}]


def run_python(code, mode, items=ITEMS):
    namespace = {}
    exec(compile(build_script("python", code), "<script>", "exec"), namespace)
    return namespace["handler"]({"mode": mode, "items": items})


def run_node(code, mode, items=ITEMS):
    with tempfile.TemporaryDirectory() as d:
        path = os.path.join(d, "script.js")
        with open(path, "w") as f:
            f.write(build_script("javascript", code))
        driver = (
            f"require({json.dumps(path)})"
            f"({json.dumps({'mode': mode, 'items': items})})"
            ".then(r => process.stdout.write(JSON.stringify(r)))"
        )
        out = subprocess.run(["node", "-e", driver], capture_output=True, text=True, check=True)
        return json.loads(out.stdout)


class PythonWrapping(unittest.TestCase):
    def test_all_items_sees_the_items_and_returns_what_the_code_returns(self):
        got = run_python('return [{"json": {"n": len(_items), "first": _items[0]["json"]["name"]}}]',
                         "runOnceForAllItems")
        self.assertEqual(got, [{"json": {"n": 2, "first": "a"}}])

    def test_per_item_runs_once_per_item_and_pairs_each_answer(self):
        got = run_python('return {"id2": _item["json"]["id"] * 2}', "runOnceForEachItem")
        self.assertEqual(got, [
            {"json": {"id2": 2}, "pairedItem": {"item": 0}},
            {"json": {"id2": 4}, "pairedItem": {"item": 1}},
        ])

    def test_per_item_drops_an_item_the_code_returns_nothing_for(self):
        got = run_python('if _item["json"]["id"] == 1:\n    return None\nreturn _item',
                         "runOnceForEachItem")
        self.assertEqual(got, [{"json": {"id": 2, "name": "b"}, "pairedItem": {"item": 1}}])

    def test_imports_and_helper_functions_in_the_body_work(self):
        code = "import hashlib\ndef h(x):\n    return hashlib.sha256(x.encode()).hexdigest()[:8]\n" \
               'return [{"json": {"h": h("a")}}]'
        self.assertEqual(run_python(code, "runOnceForAllItems")[0]["json"]["h"], "ca978112")

    def test_an_exception_reaches_the_caller(self):
        with self.assertRaises(ZeroDivisionError):
            run_python("return 1 / 0", "runOnceForAllItems")


@unittest.skipUnless(shutil.which("node"), "node is not installed")
class JavaScriptWrapping(unittest.TestCase):
    def test_all_items_sees_input_items_and_json(self):
        got = run_node("return [{json: {n: $input.all().length, same: items.length, "
                       "first: $input.first().json.name, j: $json.id}}];", "runOnceForAllItems")
        self.assertEqual(got, [{"json": {"n": 2, "same": 2, "first": "a", "j": 1}}])

    def test_per_item_runs_once_per_item_and_pairs_each_answer(self):
        got = run_node("return {json: {id2: $json.id * 2, via: $input.item.json.name}};",
                       "runOnceForEachItem")
        self.assertEqual(got, [
            {"json": {"id2": 2, "via": "a"}, "pairedItem": {"item": 0}},
            {"json": {"id2": 4, "via": "b"}, "pairedItem": {"item": 1}},
        ])

    def test_the_body_may_declare_the_names_it_is_given(self):
        # `const items = ...` would be a SyntaxError if the body shared a scope
        # with the parameters; n8n's own runner allows it.
        got = run_node("const items = [1, 2, 3];\nreturn [{json: {n: items.length}}];",
                       "runOnceForAllItems")
        self.assertEqual(got, [{"json": {"n": 3}}])

    def test_await_works_at_the_top_of_the_body(self):
        got = run_node("const v = await Promise.resolve(7);\nreturn [{json: {v}}];",
                       "runOnceForAllItems")
        self.assertEqual(got, [{"json": {"v": 7}}])

    def test_null_means_no_items(self):
        self.assertEqual(run_node("return null;", "runOnceForAllItems"), [])


if __name__ == "__main__":
    unittest.main()
