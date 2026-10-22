"""Inspect realized lifecycle artifacts without import-from-derivation."""

import json
from pathlib import Path
import shlex
import sys


def exec_arguments(script):
    commands = [
        line.strip()
        for line in script.read_text().splitlines()
        if line.strip().startswith("exec ")
    ]
    if len(commands) != 1:
        raise ValueError(f"expected exactly one exec command in {script}")
    return shlex.split(commands[0])[1:]


def flag_values(arguments, flag):
    values = []
    for index, argument in enumerate(arguments):
        if argument == flag:
            if index + 1 == len(arguments) or arguments[index + 1].startswith("--"):
                raise ValueError(f"missing value for {flag}")
            values.append(arguments[index + 1])
    return values


def main():
    apply_script, restore_script, secret, report = sys.argv[1:]
    apply_arguments = exec_arguments(Path(apply_script))
    manifests = flag_values(apply_arguments, "--manifest")
    if len(manifests) != 1:
        raise ValueError("apply command must select exactly one manifest")
    plan_text = Path(manifests[0]).read_text()
    plan = json.loads(plan_text)
    operations = {operation["id"]: operation for operation in plan["operations"]}
    restore_arguments = exec_arguments(Path(restore_script))
    checks = [
        (
            "credential-value-is-not-in-plan",
            secret not in plan_text,
            "credential contents must not be serialized into the generated plan",
        ),
        (
            "credential-reference-is-a-file-path",
            operations["ensure"]["apply"]["credential_environment"]
            == {"TOKEN_FILE": "token"}
            and operations["ensure"]["check"]["credential_environment"]
            == {"TOKEN_FILE": "token"},
            "apply and check plans must contain only the credential name reference",
        ),
        (
            "restore-targets-only-restore-operations",
            restore_arguments[1] == "restore"
            and flag_values(restore_arguments, "--manifest") == manifests
            and flag_values(restore_arguments, "--operation") == ["restore"]
            and restore_arguments.count("--confirm") == 1,
            "the restore command must select exactly the restore-lifecycle operations",
        ),
    ]
    checked = []
    for name, passed, message in checks:
        if not passed:
            raise AssertionError(f"harbor-db-module-eval: {name}: {message}")
        checked.append({"name": name, "message": message})
    Path(report).write_text(json.dumps(checked, indent=2) + "\n")


if __name__ == "__main__":
    main()
