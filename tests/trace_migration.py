"""Bind the frozen Python scenarios to executed source functions and lines.

This is migration evidence, not a Rust-parity verdict. Child interpreter execs
are not traced; their owning scenario remains in the original test inventory.
"""

import argparse
import ast
import hashlib
import json
import sys
import threading
import unittest
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=Path("python/harbor_db"))
    parser.add_argument("--tests", type=Path, default=Path("tests"))
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    source = args.source.resolve()
    files = {}
    functions = {}
    for path in sorted(source.glob("*.py")):
        data = path.read_bytes()
        name = path.name
        files[str(path)] = name
        for node in ast.walk(ast.parse(data)):
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                key = f"{name}:{node.lineno}:{node.name}"
                functions[key] = {
                    "file": name,
                    "name": node.name,
                    "start": node.lineno,
                    "end": node.end_lineno,
                    "source_sha256": hashlib.sha256(data).hexdigest(),
                    "tests": {},
                }

    active = None
    executed = {}

    def trace(frame, event, arg):
        if event == "line" and active is not None:
            name = files.get(frame.f_code.co_filename)
            if name is not None:
                key = f"{name}:{frame.f_code.co_firstlineno}:{frame.f_code.co_name}"
                # Decorators can precede the AST definition's line number.
                if key not in functions:
                    matches = [
                        k for k, value in functions.items()
                        if value["file"] == name
                        and value["name"] == frame.f_code.co_name
                        and value["start"] <= frame.f_lineno <= value["end"]
                    ]
                    if len(matches) == 1:
                        key = matches[0]
                if key in functions:
                    lines = functions[key]["tests"].setdefault(active, set())
                    lines.add(frame.f_lineno)
        return trace

    class Result(unittest.TextTestResult):
        def startTest(self, test):
            nonlocal active
            active = test.id()
            executed[active] = set()
            super().startTest(test)

        def stopTest(self, test):
            nonlocal active
            active = None
            super().stopTest(test)

    suite = unittest.defaultTestLoader.discover(str(args.tests), pattern="test_*.py")
    sys.settrace(trace)
    threading.settrace(trace)
    try:
        result = unittest.TextTestRunner(resultclass=Result, verbosity=2).run(suite)
    finally:
        sys.settrace(None)
        threading.settrace(None)
    for key, value in functions.items():
        for test, lines in value["tests"].items():
            value["tests"][test] = sorted(lines)
            executed[test].add(key)
    accepted = result.wasSuccessful() and result.testsRun == 174 and not result.skipped
    report = {
        "version": 1,
        "status": "trace_generated" if accepted else "failed",
        "tests_run": result.testsRun,
        "skipped": len(result.skipped),
        "limitations": ["Child interpreter execs are not traced", "Executed lines do not imply assertions or Rust parity"],
        "functions": functions,
        "tests": {key: sorted(values) for key, values in executed.items()},
        "uncovered": [key for key, value in functions.items() if not value["tests"]],
    }
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
    return 0 if accepted else 1


if __name__ == "__main__":
    sys.exit(main())
