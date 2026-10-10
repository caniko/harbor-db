"""Run evaluated condition/reload commands and prove their actual authority leases."""

import contextlib
import fcntl
import json
import os
import pwd
import shlex
import subprocess
import sys
import tempfile
import time
from pathlib import Path


@contextlib.contextmanager
def lock(path):
    # Probe the persistent kernel anchor independently of either runtime.
    fd = os.open(path, os.O_RDWR | os.O_NOFOLLOW)
    try:
        fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield fd
    finally:
        os.close(fd)


def main():
    fixture = json.loads(Path(sys.argv[1]).read_text())
    original_manifest = Path(fixture["manifest"])
    manifest = json.loads(original_manifest.read_text())
    with tempfile.TemporaryDirectory(prefix="harbor-db-command-phases-") as temporary:
        root = Path(temporary)
        source, restore, authority = (root / name for name in ("source", "restore", "authority"))
        for path in (source, restore, authority):
            path.mkdir(mode=0o700)
        for path in (source, restore):
            (path / "historical-object").write_bytes(b"preserved historical object")

        resource = manifest["resources"]["history"]
        resource["user"] = pwd.getpwuid(os.geteuid()).pw_name
        # These are disposable processes, not systemd units. Exclusive-lock
        # refusal must prove lease coverage, independently of unit-state checks.
        resource["runtime_units"] = []
        resource["authority"]["state_dir"] = str(authority)
        resource["authority"]["directories"] = [str(source)]
        resource["custody_file"] = str(authority / "custody.json")
        contract = root / "contract.json"
        contract.write_text(json.dumps(manifest))
        certify = [fixture["checker"], "certify", "--contract", str(contract), "--host", "fixture",
                   "--resource", "history", "--identity", "historical-fixture", "--restore-root", str(restore)]
        subprocess.run(certify, check=True, capture_output=True, text=True, timeout=15)
        results = []
        for phase, expected_exit in (("ExecCondition", 17), ("ExecReload", 23)):
            commands = fixture["commands"][phase]
            assert len(commands) == 1, (phase, commands)
            command = shlex.split(commands[0])
            assert command[0] == fixture["checker"] and command[1] == "serve", (phase, command)
            contract_index = command.index("--contract") + 1
            assert command[contract_index] == str(original_manifest)
            command[contract_index] = str(contract)
            ready = root / f"{phase}-ready.json"
            command[command.index("HARBOR_DB_PHASE_READY")] = str(ready)
            with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True) as process:
                try:
                    deadline = time.monotonic() + 5
                    while not ready.exists() and process.poll() is None and time.monotonic() < deadline:
                        time.sleep(0.01)
                    if not ready.exists():
                        stdout, stderr = process.communicate(timeout=10)
                        raise AssertionError((phase, "guarded writer did not reach its ready barrier", stdout, stderr))
                    assert json.loads(ready.read_text()) == {"phase": phase, "arguments": [fixture["literalArgument"]]}
                    try:
                        with lock(authority / "lock"):
                            raise AssertionError(f"{phase} did not retain its authority lease through exec")
                    except BlockingIOError:
                        pass
                    blocked = subprocess.run(certify, check=False, capture_output=True, text=True, timeout=15)
                    assert blocked.returncode != 0, (phase, "certification accepted a live writer")
                    assert process.stdin is not None
                    process.stdin.write("x")
                    process.stdin.flush()
                    stdout, stderr = process.communicate(timeout=10)
                    assert process.returncode == expected_exit, (phase, process.returncode, stdout, stderr)
                finally:
                    if process.poll() is None:
                        process.terminate()
                        process.communicate(timeout=5)
            with lock(authority / "lock"):
                pass
            subprocess.run(certify, check=True, capture_output=True, text=True, timeout=15)
            results.append({"phase": phase, "exit_code": expected_exit, "literal_arguments_preserved": True,
                            "exclusive_lease_blocked_until_exit": True, "certification_resumed_after_exit": True})
        print(json.dumps({"status": "ok", "commands": results}))


if __name__ == "__main__":
    main()
