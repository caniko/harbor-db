"""Executed application capture and restore acceptance with durable publication."""

import argparse
import hashlib
import json
import os
import re
import shutil
import socket
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from .durable import (
    immutable_store_reference,
    lock,
    read_json,
    sync_directory,
    sync_tree,
    write_json,
)
from . import process
from . import application_publication as publication


def digest(path):
    with open(path, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def identity(value):
    return hashlib.sha256(
        json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


def machine():
    # Linux pidfd/nsfs UAPI binds the actual calling thread. Proc namespace
    # aliases and uid_map files are caller-mountable and are not trust anchors.
    import fcntl
    import threading

    descriptors = []
    try:
        task = os.pidfd_open(threading.get_native_id(), os.O_EXCL)
        descriptors.append(task)
        for request, own_user_namespace in (
            (0xFF09, True),
            (0xFF03, False),
            (0xFF0A, False),
        ):
            selected = fcntl.ioctl(task, request)
            descriptors.append(selected)
            owner = selected if own_user_namespace else fcntl.ioctl(selected, 0xB701)
            if not own_user_namespace:
                descriptors.append(owner)
            if (
                fcntl.ioctl(owner, 0xB703) != 0x10000000
                or os.fstat(owner).st_ino != 0xEFFFFFFD
            ):
                raise ValueError(
                    "machine identity rejects caller-controlled namespaces"
                )
    except OSError as error:
        raise ValueError(
            "machine identity requires Linux 6.13+ calling-thread pidfd namespace support"
        ) from error
    finally:
        for descriptor in reversed(descriptors):
            os.close(descriptor)
    return digest("/etc/machine-id")


def capture_receipt(backup):
    fd = os.open(
        backup / "capture.json",
        os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
    )
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode):
            raise ValueError("capture receipt must be a regular file")
        if info.st_size > 4096:
            raise ValueError("capture receipt exceeds the 4096-byte limit")
        data = stream.read(4097)
        if len(data) > 4096:
            raise ValueError("capture receipt exceeds the 4096-byte limit")
        return json.loads(data)


def validate(config, *, check_root=True):
    required = {
        "version",
        "resource",
        "root",
        "commands",
        "executable_files",
        "timeout_seconds",
        "maximum_age_seconds",
    }
    protected = config.get("version") == 2
    if protected:
        required.add("worker_user")
        if "credentials" in config:
            required.add("credentials")
            publication.validate_credentials(config["credentials"])
    if set(config) != required or config["version"] not in (1, 2):
        raise ValueError("unsupported application backup manifest")
    if protected and not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", config["worker_user"]):
        raise ValueError("invalid backup worker account")
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", config["resource"]):
        raise ValueError("invalid application resource")
    root = Path(config["root"])
    if not root.is_absolute() or (
        check_root and (root.resolve() != root or not root.is_dir())
    ):
        raise ValueError("backup root is missing or redirected")
    if any(
        type(config[key]) is not int or not 0 < config[key] <= 86400
        for key in ("timeout_seconds", "maximum_age_seconds")
    ):
        raise ValueError("application backup limits must be positive and bounded")
    if set(config["commands"]) != {"capture", "restore", "verify", "cleanup"}:
        raise ValueError(
            "capture, restore and semantic verifier commands are mandatory"
        )
    for stage, argv in config["commands"].items():
        if (
            not isinstance(argv, list)
            or not argv
            or not all(isinstance(arg, str) for arg in argv)
            or not Path(argv[0]).is_absolute()
            or "{backup}" not in argv
            or (stage != "capture" and "{workspace}" not in argv)
        ):
            raise ValueError(
                "commands require an absolute executable and explicit artifact/workspace arguments"
            )
        if protected:
            immutable_store_reference(argv[0], executable=True)
    if protected:
        for path in config["executable_files"]:
            immutable_store_reference(path)
    return root


def executables(config):
    paths = set(
        config["executable_files"] + [argv[0] for argv in config["commands"].values()]
    )
    if any(not Path(path).is_absolute() or not Path(path).is_file() for path in paths):
        raise ValueError("backup executable or adapter file is absent")
    return {
        path: digest(
            immutable_store_reference(path) if config["version"] == 2 else path
        )
        for path in sorted(paths)
    }


def inventory(path):
    result = {}
    if path.is_symlink() or not path.is_dir():
        raise ValueError("backup directory is missing or redirected")
    for entry in sorted(path.rglob("*")):
        relative = str(entry.relative_to(path))
        try:
            relative.encode("utf-8")
        except UnicodeError:
            raise ValueError("backup artifact path is not UTF-8") from None
        metadata = entry.lstat()
        mode = metadata.st_mode
        if stat.S_ISDIR(mode):
            result[relative] = {"directory": True}
            continue
        if not stat.S_ISREG(mode):
            raise ValueError("backup contains redirected or special files")
        if metadata.st_nlink != 1:
            raise ValueError("backup artifacts must not have hard links")
        if relative in result:
            raise ValueError("backup artifact path is duplicated")
        result[relative] = digest(entry)
    if not result:
        raise ValueError("application capture produced no artifacts")
    return result


def execute(config, stage, backup, workspace, lease, worker=None):
    if config["version"] == 2:
        immutable_store_reference(config["commands"][stage][0], executable=True)
    substitutions = {"{backup}": str(backup), "{workspace}": str(workspace)}
    argv = [substitutions.get(arg, arg) for arg in config["commands"][stage]]
    allowed = {
        "PATH",
        "HOME",
        "USER",
        "LOGNAME",
        "LANG",
        "LC_ALL",
        "TMPDIR",
        "CREDENTIALS_DIRECTORY",
    }
    environment = {key: value for key, value in os.environ.items() if key in allowed}
    inherited = [
        int(value)
        for value in os.environ.get("HARBOR_DB_LEASE_FDS", "").split(",")
        if value
    ]
    if any(value < 3 for value in inherited):
        raise ValueError("invalid inherited Harbor DB lease")
    for descriptor in inherited:
        os.fstat(descriptor)
    retained = tuple(sorted(set([lease, *inherited])))
    environment["HARBOR_DB_LEASE_FDS"] = ",".join(map(str, retained))
    environment["HARBOR_DB_APPLICATION_TIMEOUT_SECONDS"] = str(
        config["timeout_seconds"]
    )
    # Adapter output is a bounded receipt, never copied into diagnostics. Each
    # child retains the persistent lease if its coordinator dies unexpectedly.
    if worker is not None:
        environment.update(
            USER=config["worker_user"],
            LOGNAME=config["worker_user"],
            HOME=str(workspace),
        )
    if worker is not None and worker.credentials is not None:
        environment["CREDENTIALS_DIRECTORY"] = str(worker.credentials)
    return process.execute(
        argv,
        timeout=config["timeout_seconds"],
        environment=environment,
        leases=retained,
        cwd=workspace,
        **({} if worker is None else worker.identity()),
    )


def certification(config, backup, workspace, lease, artifacts, worker=None):
    execute(config, "restore", backup, workspace, lease, worker)
    receipt = json.loads(execute(config, "verify", backup, workspace, lease, worker))
    captured = capture_receipt(backup)
    if (
        set(captured) != {"version", "consistency", "semantic_sha256"}
        or captured["version"] != 1
        or captured["consistency"] not in {"quiesced", "shared_exported_mvcc_snapshot"}
        or not re.fullmatch(r"[0-9a-f]{64}", captured["semantic_sha256"])
    ):
        raise ValueError(
            "application capture requires complete semantic identity and a declared consistency window"
        )
    if (
        set(receipt) != {"version", "status", "semantic_sha256"}
        or receipt["version"] != 1
        or receipt["status"] != "verified"
        or receipt["semantic_sha256"] != captured["semantic_sha256"]
    ):
        raise ValueError(
            "application restore verifier did not accept the complete captured semantic identity"
        )
    if inventory(backup) != artifacts:
        raise ValueError("backup artifact hash changed during restore verification")
    return {
        "version": 1,
        "status": "verified",
        "executor": socket.gethostname(),
        "executor_machine_sha256": machine(),
        "semantic_sha256": captured["semantic_sha256"],
        "artifacts": artifacts,
        "consistency": captured["consistency"],
    }


def capture(config, attempt, *, retry_incomplete=False):
    root = validate(config)
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", attempt) or attempt in (
        "LAST_SUCCESS",
        "lock",
    ):
        raise ValueError("invalid backup attempt")
    protected = config["version"] == 2
    if protected:
        publication.require_namespace(root)
        publication.check_credential_sources(config.get("credentials"))
    with lock(root / "lock") as lease:
        staging, destination = root / f"{attempt}.partial", root / attempt
        partial = staging / "artifacts" if protected else staging
        sealed, credentials = (
            root / f"{attempt}.sealed",
            root / f"{attempt}.credentials",
        )
        if destination.exists() or staging.is_symlink() or destination.is_symlink():
            raise ValueError(
                "backup attempt already exists; never overwrite a restore point"
            )
        tools = executables(config)
        attempt_intent = {"manifest_sha256": identity(config), "executables": tools}
        intent_file = root / f"{attempt}.intent.json"
        workspace = root / f"{attempt}.restore-workspace"
        if any(
            p.exists() or p.is_symlink()
            for p in (
                intent_file,
                staging,
                workspace,
                *([sealed, credentials] if protected else []),
            )
        ):
            if not retry_incomplete:
                raise ValueError(
                    "backup attempt already exists or its immutable intent changed"
                )
            cleanup_tree = capture_cleanup_tree(
                intent_file, attempt_intent, partial, sealed, protected
            )
            if workspace.exists():
                if workspace.is_symlink() or not workspace.is_dir():
                    raise ValueError("incomplete restore workspace is redirected")
                retry_worker = (
                    publication.Worker(
                        config["worker_user"],
                        root / f"{attempt}.retry-credentials-{time.time_ns()}",
                        config.get("credentials"),
                    )
                    if protected
                    else None
                )
                try:
                    execute(
                        config, "cleanup", cleanup_tree, workspace, lease, retry_worker
                    )
                finally:
                    if retry_worker is not None:
                        retry_worker.close()
            if protected and credentials.exists():
                shutil.rmtree(credentials)
            abandoned = root / f"{attempt}.abandoned-{time.time_ns()}"
            abandoned.mkdir(mode=0o700)
            for entry in (
                staging,
                workspace,
                intent_file,
                *([sealed] if protected else []),
            ):
                if entry.exists():
                    entry.rename(abandoned / entry.name)
            sync_directory(abandoned)
            sync_directory(root)
        write_json(
            intent_file,
            attempt_intent | ({"cleanup_origin": "partial"} if protected else {}),
        )
        worker = (
            publication.Worker(
                config["worker_user"], credentials, config.get("credentials")
            )
            if protected
            else None
        )
        try:
            return capture_attempt(
                config,
                attempt,
                root,
                staging,
                partial,
                sealed,
                destination,
                workspace,
                tools,
                lease,
                worker,
            )
        finally:
            if worker is not None:
                worker.close()


def capture_cleanup_tree(intent, expected, partial, sealed, protected):
    """The journal, rather than seal existence, owns the durable cleanup origin."""
    retained = read_json(intent)
    if not isinstance(retained, dict):
        raise ValueError("backup attempt immutable intent changed")
    has_origin = "cleanup_origin" in retained
    origin = retained.pop("cleanup_origin", None) if protected else None
    if retained != expected:
        raise ValueError("backup attempt immutable intent changed")
    if not protected:
        return partial
    info = intent.lstat()
    if (
        not stat.S_ISREG(info.st_mode)
        or info.st_uid != 0
        or info.st_mode & 0o077
        or info.st_nlink != 1
    ):
        raise ValueError("capture cleanup journal is not root-controlled")
    if not has_origin and not os.path.lexists(sealed):
        return partial  # A legacy attempt that never created a seal is unambiguous.
    if origin == "partial":
        return partial
    if origin != "sealed":
        raise ValueError("protected capture lacks an unambiguous cleanup origin")
    info = sealed.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
        raise ValueError("retained certification tree is not root-controlled")
    return sealed


def capture_attempt(
    config,
    attempt,
    root,
    staging,
    partial,
    sealed,
    destination,
    workspace,
    tools,
    lease,
    worker,
):
    captured_at = int(time.time())
    if worker is not None:
        publication.worker_directory(staging, worker.account)
        publication.worker_directory(workspace, worker.account)
    else:
        workspace.mkdir(mode=0o700)
    intent = root / f"{attempt}.intent.json"
    expected = {"manifest_sha256": identity(config), "executables": tools}
    certification_tree = partial
    try:
        execute(config, "capture", partial, workspace, lease, worker)
        capture_receipt(partial)
        artifacts = inventory(partial)
        if "acceptance.json" in artifacts:
            raise ValueError(
                "application capture must not supply its own acceptance envelope"
            )
        if worker is not None:
            publication.copy_tree(partial, sealed)
            publication.reader_permissions(sealed, root.stat().st_gid)
            if inventory(sealed) != artifacts:
                raise ValueError(
                    "backup artifact changed before protected certification"
                )
            sync_tree(sealed)
            write_json(intent, expected | {"cleanup_origin": "sealed"})
            certification_tree = sealed
        receipt = certification(
            config, certification_tree, workspace, lease, artifacts, worker
        )
    finally:
        cleanup_tree = capture_cleanup_tree(
            intent, expected, partial, sealed, worker is not None
        )
        execute(config, "cleanup", cleanup_tree, workspace, lease, worker)
        shutil.rmtree(workspace)
        sync_directory(root)
    if inventory(certification_tree) != artifacts:
        raise ValueError("backup artifact hash changed during cleanup")
    if executables(config) != tools:
        raise ValueError("backup executable identity changed during capture")
    receipt.update(
        {
            "resource": config["resource"],
            "attempt": attempt,
            "captured_at": captured_at,
            "manifest_sha256": identity(config),
            "executables": tools,
        }
    )
    published = certification_tree
    write_json(published / "acceptance.json", receipt)
    for entry in published.rglob("*"):
        metadata = entry.lstat()
        if stat.S_ISDIR(metadata.st_mode):
            entry.chmod(0o750)
        elif stat.S_ISREG(metadata.st_mode) and metadata.st_nlink == 1:
            entry.chmod(0o440)
        else:
            raise ValueError("backup contains redirected, shared or special files")
    published.chmod(0o750)
    if worker is not None:
        publication.reader_permissions(published, root.stat().st_gid)
    sync_tree(published)
    fresh(config, captured_at, int(time.time()))
    published.rename(destination)
    sync_directory(root)
    write_json(
        root / "LAST_SUCCESS",
        {
            "attempt": attempt,
            "acceptance_sha256": digest(destination / "acceptance.json"),
        },
        mode=0o640,
    )
    if worker is not None:
        shutil.rmtree(staging)
    return receipt


def inspect(config, backup, *, now=None):
    validate(config)
    backup = Path(backup)
    with lock(Path(config["root"]) / "lock", shared=True):
        return verify_bytes(config, backup, now=now)


def verify_bytes(config, backup, *, now=None):
    receipt = verify_integrity(config, backup)
    fresh(config, receipt["captured_at"], int(time.time()) if now is None else now)
    return receipt


def verify_integrity(config, backup):
    receipt = read_json(backup / "acceptance.json")
    if (
        receipt["status"] != "verified"
        or receipt["manifest_sha256"] != identity(config)
        or receipt["executables"] != executables(config)
        or receipt["resource"] != config["resource"]
    ):
        raise ValueError("backup acceptance contract or executable identity changed")
    artifacts = inventory(backup)
    artifacts.pop("acceptance.json")
    if artifacts != receipt["artifacts"]:
        raise ValueError("backup artifact hash does not match acceptance")
    return receipt


def inspect_retained(config, backup):
    """Retained rollback captures do not expire; their bytes, tools and contract do."""
    validate(config)
    with lock(Path(config["root"]) / "lock", shared=True):
        receipt = verify_integrity(config, Path(backup))
        if type(receipt["captured_at"]) is not int or not 0 <= receipt[
            "captured_at"
        ] <= int(time.time()):
            raise ValueError(
                "retained backup capture time is invalid or from the future"
            )
        return receipt


def fresh(config, captured_at, now):
    age = now - captured_at
    if not 0 <= age <= config["maximum_age_seconds"]:
        raise ValueError("backup acceptance is stale or from the future")


def certify(config, backup, state):
    """Execute restoration of a copied point on an independent machine."""
    executor_machine = machine()
    validate(config, check_root=False)
    if config["version"] == 2 and os.geteuid() == 0:
        raise ValueError(
            "independent backup certification requires an unprivileged account"
        )
    backup, state = Path(backup), Path(state)
    if (
        not state.is_absolute()
        or state.resolve() != state
        or not state.is_dir()
        or state.is_relative_to(backup)
    ):
        raise ValueError(
            "certifier state must be an existing private directory outside the copied backup"
        )
    if state.stat().st_uid != os.getuid() or state.stat().st_mode & 0o077:
        raise ValueError("certifier state must be private and owned")
    with lock(state / "lock") as lease:
        source = verify_bytes(config, backup)
        if (
            source["executor_machine_sha256"] == executor_machine
            or source["executor"] == socket.gethostname()
        ):
            raise ValueError(
                "independent restoration must execute on a different machine"
            )
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
        acceptance_sha256 = digest(backup / "acceptance.json")
        path = state / f"{acceptance_sha256}.json"
        current = int(time.time())
        previous = None
        if os.path.lexists(path):
            previous = read_json(path)
            if not isinstance(previous, dict):
                raise ValueError(
                    "independent restore receipt already exists; retain its original evidence"
                )
        certified_at = current
        if previous is not None:
            certified_at = previous.get("certified_at")
            if (
                type(certified_at) is not int
                or not source["captured_at"] <= certified_at <= current
            ):
                raise ValueError(
                    "independent restore receipt already exists; retain its original evidence"
                )
        result.update(
            {
                "source_acceptance_sha256": acceptance_sha256,
                "certified_at": certified_at,
                "manifest_sha256": identity(config),
                "executables": tools,
                "resource": config["resource"],
            }
        )
        if previous is not None:
            if previous != result:
                raise ValueError(
                    "independent restore receipt already exists; retain its original evidence"
                )
            return previous
        write_json(path, result)
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    sub = parser.add_subparsers(dest="command", required=True)
    pruning = sub.add_parser(
        "prune",
        help="retain scheduled restore points under the capture lease; preserve transition/manual attempts",
    )
    pruning.add_argument("--keep", type=int, required=True)
    pruning.add_argument("--incomplete-age-seconds", type=int, required=True)
    capture_parser = sub.add_parser("capture")
    origin = capture_parser.add_mutually_exclusive_group()
    origin.add_argument("--attempt", default=None)
    origin.add_argument(
        "--scheduled",
        action="store_true",
        help="generate a timer-retained attempt; automatic manual captures stay pinned",
    )
    capture_parser.add_argument(
        "--retry-incomplete",
        action="store_true",
        help="drain and retain an interrupted attempt with unchanged immutable intent",
    )
    inspection = sub.add_parser("inspect")
    inspection.add_argument("backup", type=Path)
    certifier = sub.add_parser("certify")
    certifier.add_argument("backup", type=Path)
    certifier.add_argument("--state", type=Path, required=True)
    args = parser.parse_args()
    try:
        config = read_json(args.config)
        if args.command == "prune":
            from .application_retention import prune

            result = prune(config, args.keep, args.incomplete_age_seconds)
        elif args.command == "capture":
            if args.scheduled and args.retry_incomplete:
                raise ValueError(
                    "scheduled captures cannot retry an unnamed attempt; select --attempt explicitly"
                )
            prefix = "timer" if args.scheduled else "manual"
            result = capture(
                config,
                args.attempt
                or prefix
                + "-"
                + time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
                + f"-{os.getpid()}",
                retry_incomplete=args.retry_incomplete,
            )
        elif args.command == "certify":
            result = certify(config, args.backup, args.state)
        else:
            result = inspect(config, args.backup)
        print(json.dumps(result, sort_keys=True))
    except (
        OSError,
        ValueError,
        KeyError,
        TypeError,
        subprocess.SubprocessError,
    ) as error:
        print(f"harbor-db-application-backup: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
