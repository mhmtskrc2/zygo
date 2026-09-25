# SPDX-License-Identifier: Apache-2.0
"""A tool an LLM agent can call: evaluate an arithmetic expression.

The model produces the expression; the model is not trusted. So the tool runs
in a sandbox with no network, a small memory limit and a short deadline, and
the expression is parsed rather than `eval`-ed. The sandbox is what makes the
parser's mistakes survivable, not the other way round.
"""

import ast
import operator

_OPS = {
    ast.Add: operator.add,
    ast.Sub: operator.sub,
    ast.Mult: operator.mul,
    ast.Div: operator.truediv,
    ast.Pow: operator.pow,
    ast.Mod: operator.mod,
    ast.USub: operator.neg,
}


def _evaluate(node):
    if isinstance(node, ast.Constant) and isinstance(node.value, (int, float)):
        return node.value
    if isinstance(node, ast.BinOp) and type(node.op) in _OPS:
        return _OPS[type(node.op)](_evaluate(node.left), _evaluate(node.right))
    if isinstance(node, ast.UnaryOp) and type(node.op) in _OPS:
        return _OPS[type(node.op)](_evaluate(node.operand))
    raise ValueError(f"unsupported: {ast.dump(node)}")


def handler(event: dict) -> dict:
    expression = str(event.get("expression", ""))
    if len(expression) > 200:
        return {"error": "expression too long"}
    try:
        tree = ast.parse(expression, mode="eval")
        return {"result": _evaluate(tree.body)}
    except (ValueError, SyntaxError, ZeroDivisionError, OverflowError) as e:
        return {"error": f"{type(e).__name__}: {e}"}
