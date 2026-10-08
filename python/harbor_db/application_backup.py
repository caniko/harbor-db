"""Executed application capture and restore acceptance with durable publication."""

import argparse
import hashlib
import json
import os
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from .durable import lock, read_json, sync_directory, sync_tree, write_json


def digest(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def identity(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def validate(config, *, check_root=True):
    required = {"version", "resource", "root", "commands", "executable_files", "timeout_seconds", "maximum_age_seconds"}
    if set(config) != required or config["version"] != 1:
        raise ValueError("unsupported application backup manifest")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", config["resource"]):
        raise ValueError("invalid application resource")
    root = Path(config["root"])
    if not root.is_absolute() or (check_root and (root.resolve() != root or not root.is_dir())):
        raise ValueError("backup root is missing or redirected")
    if any(type(config[key]) is not int or not 0 < config[key] <= 86400 for key in ("timeout_seconds", "maximum_age_seconds")):
        raise ValueError("application backup limits must be positive and bounded")
    if set(config["commands"]) != {"capture", "restore", "verify", "cleanup"}:
        raise ValueError("capture, restore and semantic verifier commands are mandatory")
    for stage, argv in config["commands"].items():
        if (not isinstance(argv, list) or not argv or not all(isinstance(arg, str) for arg in argv)
                or not Path(argv[0]).is_absolute() or "{backup}" not in argv
                or (stage != "capture" and "{workspace}" not in argv)):
            raise ValueError("commands require an absolute executable and explicit artifact/workspace arguments")
    return root


def executables(config):
    paths = set(config["executable_files"] + [argv[0] for argv in config["commands"].values()])
    if any(not Path(path).is_absolute() or not Path(path).is_file() for path in paths):
        raise ValueError("backup executable or adapter file is absent")
    return {path: digest(path) for path in sorted(paths)}


def inventory(path):
    result = {}
    if path.is_symlink() or not path.is_dir():
        raise ValueError("backup directory is missing or redirected")
    for entry in sorted(path.rglob("*")):
        mode = entry.lstat().st_mode
        if stat.S_ISDIR(mode):
            continue
        if not stat.S_ISREG(mode):
            raise ValueError("backup contains redirected or special files")
        result[str(entry.relative_to(path))] = digest(entry)
    if not result:
        raise ValueError("application capture produced no artifacts")
    return result


def execute(config, stage, backup, workspace, lease):
    substitutions = {"{backup}": str(backup), "{workspace}": str(workspace)}
    argv = [substitutions.get(arg, arg) for arg in config["commands"][stage]]
    allowed = {"PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "TMPDIR", "CREDENTIALS_DIRECTORY"}
    environment = {key: value for key, value in os.environ.items() if key in allowed}
    # Adapter output is a bounded receipt, never copied into diagnostics. Each
    # child retains the persistent lease if its coordinator dies unexpectedly.
    with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
        process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
                                   env=environment, cwd=workspace, pass_fds=(lease,), start_new_session=True)
        try:
            status = process.wait(timeout=config["timeout_seconds"])
        except BaseException:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
            raise
        if status:
            raise ValueError(f"application {stage} failed; restore point remains unpublished")
        if stdout.tell() > 1024 * 1024:
            raise ValueError("application verifier receipt exceeds the size limit")
        stdout.seek(0)
        return stdout.read()


def certification(config, backup, workspace, lease, artifacts):
    execute(config, "restore", backup, workspace, lease)
    receipt = json.loads(execute(config, "verify", backup, workspace, lease))
    captured = read_json(backup / "capture.json")
    if (set(captured) != {"version", "consistency", "semantic_sha256"} or captured["version"] != 1
            or captured["consistency"] not in {"quiesced", "shared_exported_mvcc_snapshot"}
            or not re.fullmatch(r"[0-9a-f]{64}", captured["semantic_sha256"])):
        raise ValueError("application capture requires complete semantic identity and a declared consistency window")
    if (set(receipt) != {"version", "status", "semantic_sha256"} or receipt["version"] != 1
            or receipt["status"] != "verified" or receipt["semantic_sha256"] != captured["semantic_sha256"]):
        raise ValueError("application restore verifier did not accept the complete captured semantic identity")
    if inventory(backup) != artifacts:
        raise ValueError("backup artifact hash changed during restore verification")
    return {"version": 1, "status": "verified", "executor": socket.gethostname(),
            "executor_machine_sha256": digest("/etc/machine-id"), "semantic_sha256": captured["semantic_sha256"],
            "artifacts": artifacts, "consistency": captured["consistency"]}


def capture(config, attempt):
    root = validate(config)
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", attempt):
        raise ValueError("invalid backup attempt")
    with lock(root / "lock") as lease:
        partial, destination = root / f"{attempt}.partial", root / attempt
        if partial.exists() or destination.exists() or partial.is_symlink() or destination.is_symlink():
            raise ValueError("backup attempt already exists; never overwrite a restore point")
        tools = executables(config)
        captured_at = int(time.time())
        # The capture adapter must create a new destination; its entire output
        # remains as a partial attempt on failure for investigation.
        workspace = root / f"{attempt}.restore-workspace"
        workspace.mkdir(mode=0o700)
        try:
            execute(config, "capture", partial, workspace, lease)
            artifacts = inventory(partial)
            if "acceptance.json" in artifacts:
                raise ValueError("application capture must not supply its own acceptance envelope")
            receipt = certification(config, partial, workspace, lease, artifacts)
        finally:
            # Cleanup is mandatory and idempotent even after failed restore. A
            # cleanup failure retains its private workspace and prevents publish.
            execute(config, "cleanup", partial, workspace, lease)
            shutil.rmtree(workspace)
            sync_directory(root)
        if inventory(partial) != artifacts:
            raise ValueError("backup artifact hash changed during cleanup")
        if executables(config) != tools:
            raise ValueError("backup executable identity changed during capture")
        receipt.update({"resource": config["resource"], "attempt": attempt,
                        "captured_at": captured_at, "manifest_sha256": identity(config), "executables": tools})
        write_json(partial / "acceptance.json", receipt)
        # Readers receive only accepted backup artifacts, never the disposable
        # restore workspace. The service group owns all publications.
        for entry in partial.rglob("*"):
            entry.chmod(0o750 if entry.is_dir() else 0o640)
        partial.chmod(0o750)
        sync_tree(partial)
        partial.rename(destination)
        sync_directory(root)
        write_json(root / "LAST_SUCCESS", {"attempt": attempt, "acceptance_sha256": digest(destination / "acceptance.json")})
        (root / "LAST_SUCCESS").chmod(0o640)
        sync_directory(root)
        return receipt


def inspect(config, backup, *, now=None):
    validate(config)
    backup = Path(backup)
    with lock(Path(config["root"]) / "lock", shared=True):
        return verify_bytes(config, backup, now=now)


def verify_bytes(config, backup, *, now=None):
    receipt = read_json(backup / "acceptance.json")
    if (receipt["status"] != "verified" or receipt["manifest_sha256"] != identity(config)
            or receipt["executables"] != executables(config) or receipt["resource"] != config["resource"]):
        raise ValueError("backup acceptance contract or executable identity changed")
    artifacts = inventory(backup)
    artifacts.pop("acceptance.json")
    if artifacts != receipt["artifacts"]:
        raise ValueError("backup artifact hash does not match acceptance")
    age = (int(time.time()) if now is None else now) - receipt["captured_at"]
    if not 0 <= age <= config["maximum_age_seconds"]:
        raise ValueError("backup acceptance is stale or from the future")
    return receipt


def certify(config, backup, state):
    """Execute restoration of a copied point on an independent machine."""
    validate(config, check_root=False)
    backup, state = Path(backup), Path(state)
    if not state.is_absolute() or state.resolve() != state or not state.is_dir() or state.is_relative_to(backup):
        raise ValueError("certifier state must be an existing private directory outside the copied backup")
    if state.stat().st_uid != os.getuid() or state.stat().st_mode & 0o077:
        raise ValueError("certifier state must be private and owned")
    with lock(state / "lock") as lease:
        source = verify_bytes(config, backup)
        if source["executor_machine_sha256"] == digest("/etc/machine-id") or source["executor"] == socket.gethostname():
            raise ValueError("independent restoration must execute on a different machine")
        workspace = Path(tempfile.mkdtemp(prefix="restore-", dir=state))
        tools = executables(config)
        try:
            artifacts = inventory(backup)
            result = certification(config, backup, workspace, lease, artifacts)
        finally:
            execute(config, "cleanup", backup, workspace, lease)
            shutil.rmtree(workspace)
        verify_bytes(config, backup)
        if executables(config) != tools:
            raise ValueError("certifier executable changed during restoration")
        result.update({"source_acceptance_sha256": digest(backup / "acceptance.json"), "certified_at": int(time.time()),
                       "manifest_sha256": identity(config), "executables": tools, "resource": config["resource"]})
        path = state / f"{result['source_acceptance_sha256']}.json"
        if path.exists() and read_json(path) != result:
            raise ValueError("independent restore receipt already exists; retain its original evidence")
        write_json(path, result)
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    sub = parser.add_subparsers(dest="command", required=True)
    capture_parser = sub.add_parser("capture")
    capture_parser.add_argument("--attempt", default=None)
    inspection = sub.add_parser("inspect")
    inspection.add_argument("backup", type=Path)
    certifier = sub.add_parser("certify")
    certifier.add_argument("backup", type=Path)
    certifier.add_argument("--state", type=Path, required=True)
    args = parser.parse_args()
    try:
        config = read_json(args.config)
        if args.command == "capture":
            result = capture(config, args.attempt or time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()) + f"-{os.getpid()}")
        elif args.command == "certify":
            result = certify(config, args.backup, args.state)
        else:
            result = inspect(config, args.backup)
        print(json.dumps(result, sort_keys=True))
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as error:
        print(f"harbor-db-application-backup: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
