"""Read-only recovery admission and executed, record-level acceptance receipts.

Snapshots require a consumer-owned consistency window. Certification compares a
disposable read-only restored endpoint with that snapshot; it never restores,
promotes, adopts or changes a database. Remote certification uses the same
copied backup, snapshot and query contract and records its actual executor host.
"""

import contextlib
import hashlib
import json
import os
import re
import socket
import time
from pathlib import Path

from . import postgres, writer_fence, recovery_repository, application_backup
from .durable import lock, read_json, write_json, sync_directory


def absolute(path):
    path = Path(path)
    if not path.is_absolute() or str(path.resolve()) != str(path):
        raise ValueError(f"recovery path must be absolute and not redirected: {path}")
    return path


def digest(path):
    # read_json and all artifact reads reject the final symlink as well as
    # redirected ancestors. Do not silently certify a different backup tree.
    path = absolute(path)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def fresh(timestamp, now, max_age):
    if type(timestamp) is not int or not 0 <= now - timestamp <= max_age:
        raise ValueError("recovery evidence timestamp is stale, future or invalid")


def policy(config):
    return _policy(config)


def validate_evidence_paths(config, settings, source_local):
    declared = [
        settings[key]
        for key in (
            "backup_root",
            "snapshot_file",
            "receipt_file",
            "off_host_receipt_file",
        )
        if key in settings
        and not (key == "off_host_receipt_file" and settings[key] is None)
    ]
    if any(
        not isinstance(raw, str)
        or not raw.startswith("/")
        or any(part in ("", ".", "..") for part in raw.split("/")[1:])
        for raw in declared
    ):
        raise ValueError(
            "recovery evidence must use normalized paths outside repository control paths"
        )
    # Fence-only admission may omit the repository; evidence-producing paths
    # still require an explicit root before locks or publication.
    root = absolute(settings["backup_root"]) if "backup_root" in settings else None
    if source_local and root is None:
        raise ValueError("missing recovery backup root")
    snapshot = absolute(settings["snapshot_file"])
    controls = (
        [
            root / name
            for name in (
                "recovery",
                "base",
                "wal",
                "locks",
                "BACKUP_LOCK",
                "LAST_SUCCESS",
            )
        ]
        if root is not None
        else []
    )
    controls.extend(
        snapshot.parent / name
        for name in ("recovery.lock", "preparation.lock", "preparation.json")
    )
    protected = [
        absolute(config[key]) for key in ("data_dir", "state_dir") if key in config
    ]
    startup = config.get("startup_inhibition")
    if startup is not None:
        if not isinstance(startup, dict) or not isinstance(
            startup.get("state_dir"), str
        ):
            raise ValueError("invalid PostgreSQL storage startup-inhibition path")
        protected.append(absolute(startup["state_dir"]))
    for key in (
        "backup_root",
        "snapshot_file",
        "receipt_file",
        "off_host_receipt_file",
    ):
        if key == "backup_root" and root is None:
            continue
        if key == "off_host_receipt_file" and settings.get(key) is None:
            continue
        evidence = absolute(settings[key])
        if any(
            evidence.is_relative_to(root) or root.is_relative_to(evidence)
            for root in protected
        ):
            raise ValueError(
                "recovery evidence must stay outside PostgreSQL storage and policy trees"
            )
        if key != "backup_root" and any(
            evidence.is_relative_to(control) or control.is_relative_to(evidence)
            for control in controls
        ):
            raise ValueError(
                "recovery evidence must stay outside repository control paths"
            )


def _policy(config, *, certifier=False):
    settings = config["recovery"]
    validate_evidence_paths(
        config, settings, recovery_repository.source_local(settings)
    )
    if type(settings.get("require_writer_fence", False)) is not bool:
        raise ValueError("recovery writer fence requirement must be a boolean")
    paths = [settings["snapshot_file"], settings["receipt_file"]]
    if settings.get("off_host_receipt_file"):
        paths.append(settings["off_host_receipt_file"])
    # A certifier must prove machine independence against the pinned snapshot
    # before queries/publication. Capture/import/admission need distinct paths.
    if (
        settings.get("off_host_receipt_file") == settings["receipt_file"]
        and not certifier
    ):
        raise ValueError("local and off-host recovery receipts must be distinct")
    if settings["snapshot_file"] in paths[1:]:
        raise ValueError("recovery receipts must not overwrite the record snapshot")
    if not re.fullmatch(r"[1-9][0-9]*", settings["system_identifier"]):
        raise ValueError(
            "recovery requires an independently recorded system identifier"
        )
    if type(settings["max_age_seconds"]) is not int or settings["max_age_seconds"] <= 0:
        raise ValueError("recovery evidence age must be positive")
    checks = settings["record_checks"]
    if not checks or len({item["name"] for item in checks}) != len(checks):
        raise ValueError("recovery requires uniquely named record checks")
    for item in checks:
        if any(
            not isinstance(item[key], str) or not item[key].strip() or "\0" in item[key]
            for key in ("name", "database", "sql")
        ):
            raise ValueError("invalid recovery record check")
    return settings


def contract(settings):
    return hashlib.sha256(
        json.dumps(settings["record_checks"], sort_keys=True).encode()
    ).hexdigest()


def lsn(value):
    if not isinstance(value, str) or not re.fullmatch(
        r"[0-9A-Fa-f]{1,8}/[0-9A-Fa-f]{1,8}", value
    ):
        raise ValueError("invalid recovery LSN")
    high, low = value.split("/")
    return (int(high, 16) << 32) + int(low, 16)


def backup(config, settings, now):
    directory, binding, _ = selected_backup(config, settings, now)
    return directory, binding


def selected_backup(config, settings, now):
    if recovery_repository.source_local(settings):
        selected = recovery_repository.select(config, settings, now)
        if (
            postgres.inspect_cluster(
                config["package"], selected.directory, config["major"]
            )
            != settings["system_identifier"]
        ):
            raise ValueError("backup control-file identity differs from the primary")
        return selected.directory, dict(selected.binding), selected
    root = absolute(settings["backup_root"])
    marker = absolute(root / "LAST_SUCCESS")
    fd = os.open(marker, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd) as stream:
        identifier = stream.read(256).strip()
    if not re.fullmatch(
        r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", identifier
    ) or identifier.endswith(".partial"):
        raise ValueError("invalid completed backup identifier")
    directory = absolute(root / "base" / identifier)
    manifest = absolute(directory / "backup_manifest")
    metadata = absolute(root / "base" / f"{identifier}.meta.json")
    meta = read_json(metadata)
    if (
        meta["backup_id"] != identifier
        or str(meta["pg_major"]) != str(config["major"])
        or meta["system_identifier"] != settings["system_identifier"]
    ):
        raise ValueError("backup identity does not match the declared primary")
    if lsn(meta["post_backup_lsn"]) <= lsn(meta["backup_stop_lsn"]):
        raise ValueError("backup recovery point must follow its stop LSN")
    fresh(int(manifest.stat().st_mtime), now, settings["max_age_seconds"])
    if (
        postgres.inspect_cluster(config["package"], directory, config["major"])
        != settings["system_identifier"]
    ):
        raise ValueError("backup control-file identity differs from the primary")
    return (
        directory,
        {
            "backup_id": identifier,
            "system_identifier": settings["system_identifier"],
            "major": str(config["major"]),
            "epoch_id": meta["epoch_id"],
            "manifest_sha256": digest(manifest),
            "recovery_target_lsn": meta["post_backup_lsn"],
            "metadata_sha256": digest(metadata),
        },
        None,
    )


def verify_backup(config, directory):
    postgres.run(
        [Path(config["package"]) / "bin/pg_verifybackup", "--no-parse-wal", directory],
        capture_output=True,
        text=True,
        timeout=config["recovery"]["verify_timeout_seconds"],
    )


def query(config, socket_dir, port, database, sql):
    argv = query_argv(config, socket_dir, port, database, sql)
    env = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
    env["PGCONNECT_TIMEOUT"] = "5"
    return postgres.run(
        argv, env=env, capture_output=True, text=True, timeout=60
    ).stdout


def query_argv(
    config, socket_dir, port, database, sql, *, statement_timeout_seconds=None
):
    """Shared single-statement boundary; callers retain worker/lease policy."""
    if (
        not Path(socket_dir).is_absolute()
        or "," in socket_dir
        or not 1 <= port <= 65535
    ):
        raise ValueError("recovery queries require a local Unix socket and valid port")
    database = database.replace("\\", "\\\\").replace("'", "\\'")
    options = (
        "-c TimeZone=UTC -c DateStyle=ISO,YMD -c bytea_output=hex "
        "-c standard_conforming_strings=on"
    )
    # Keep the endpoint default observable; only the validation transaction is
    # forced read-only. PREPARE does not execute the selected statement.
    if statement_timeout_seconds is not None:
        options += f" -c statement_timeout={statement_timeout_seconds}s"
    # \bind forces a single-statement extended-protocol Parse. PREPARE accepts
    # query/DML grammar, never transaction control or CALL/DO. Only its execution
    # enters the explicit read-only transaction, retaining normalized output.
    return [
        Path(config["package"]) / "bin/psql",
        "--no-psqlrc",
        "--no-password",
        "--quiet",
        f"--host={socket_dir}",
        f"--port={port}",
        "--username=postgres",
        f"--dbname=dbname='{database}' connect_timeout=5 options='{options}'",
        "--set=ON_ERROR_STOP=1",
        "--tuples-only",
        "--no-align",
        "--command",
        "\\bind",
        "--command",
        f"PREPARE harbor_db_recovery_query AS {sql}",
        "--command",
        (
            "BEGIN READ ONLY; SET LOCAL TimeZone = 'UTC'; "
            "SET LOCAL DateStyle = 'ISO,YMD'; SET LOCAL bytea_output = 'hex'; "
            "EXECUTE harbor_db_recovery_query; COMMIT;"
        ),
    ]


def records(config, settings, socket_dir, port):
    return {
        item["name"]: hashlib.sha256(
            query(config, socket_dir, port, item["database"], item["sql"]).encode()
        ).hexdigest()
        for item in settings["record_checks"]
    }


def evidence(path, label, binding, settings, now):
    try:
        value = read_json(absolute(path))
    except FileNotFoundError as error:
        raise ValueError(
            f"missing {label}; execute record snapshot and restore certification"
        ) from error
    if value.get("version") != 1 or any(
        value.get(key) != expected for key, expected in binding.items()
    ):
        raise ValueError(f"{label} is for a different backup")
    if value.get("record_contract_sha256") != contract(settings):
        raise ValueError(f"{label} record-check contract differs")
    fresh(value.get("completed_at"), now, settings["max_age_seconds"])
    return value


def validate_capture_source(config, settings, source, now):
    # The caller retains the repository lease. Independent certifiers use the
    # copied source token, rather than requiring a local primary fence.
    if recovery_repository.source_local(settings):
        selected = recovery_repository.select(config, settings, now)
        if any(source.get(key) != value for key, value in selected.binding.items()):
            raise ValueError("record snapshot is for a different backup")
        selected.validate_snapshot(
            source.get("writer_fence_token"), source.get("records")
        )
        if source.get("completed_at") != selected.metadata["completed_at"]:
            raise ValueError(
                "source-local snapshot completion differs from its immutable capture"
            )
        if (
            source.get("source_machine_sha256")
            != selected.metadata["source_machine_sha256"]
        ):
            raise ValueError(
                "source-local snapshot machine differs from its immutable capture"
            )


def source_snapshot_path(config, settings):
    """Resolve source evidence under the caller's retained repository lease."""
    if recovery_repository.source_local(settings):
        return recovery_repository.select(
            config, settings, int(time.time())
        ).snapshot_path
    return Path(settings["snapshot_file"])


def snapshot_evidence(config, settings, binding, now):
    source = evidence(
        source_snapshot_path(config, settings),
        "record snapshot",
        binding,
        settings,
        now,
    )
    validate_capture_source(config, settings, source, now)
    return source


@contextlib.contextmanager
def evidence_lease(settings, *, inspect=False):
    # Backup mutation and evidence publication have distinct locks. A drill may
    # retain the backup's shared lease while certifying; it cannot race another
    # source snapshot or certifier into publishing mixed evidence.
    anchor = absolute(settings["snapshot_file"]).parent / "recovery.lock"
    if inspect and not anchor.exists():
        raise ValueError(
            "missing record snapshot lease; execute record snapshot and restore certification"
        )
    with lock(anchor, shared=inspect, create=not inspect):
        yield


@contextlib.contextmanager
def writer_exclusion(config, socket_dir=None, port=5432):
    """Pin the selected fence through evidence/authority publication.

    Offline admission verifies the durable selector; primary operations also
    inspect the running endpoint. Disposable certifiers use copied evidence.
    """
    settings = policy(config)
    active = "state_dir" in config and os.path.lexists(writer_fence.marker(config))
    if not settings.get("require_writer_fence", False) and not active:
        yield None
        return
    postgres.validate_config(config)
    anchor = Path(config["state_dir"]) / "writer-fence.lock"
    if not anchor.exists():
        raise postgres.LifecycleError(
            "required writer fence is absent; acquire it before recovery"
        )
    with lock(anchor, shared=True):
        record = writer_fence.startup(config)
        if (
            record is None
            or record["system_identifier"] != settings["system_identifier"]
        ):
            raise postgres.LifecycleError(
                "required writer fence does not bind the declared primary"
            )
        if socket_dir is not None:
            writer_fence.inspect_live(config, record["token"], socket_dir, port)
        yield {key: record[key] for key in ("token", "system_identifier", "hba_sha256")}


def source_fence(settings, source):
    binding = source.get("writer_fence_token")
    if binding is not None and (
        not isinstance(binding, str) or not re.fullmatch(r"[0-9a-f]{32}", binding)
    ):
        raise ValueError("record snapshot writer fence binding is invalid")
    if settings.get("require_writer_fence", False) and binding is None:
        raise ValueError("record snapshot has no required writer fence binding")
    return binding


def snapshot(config, socket_dir, port, *, now=None):
    settings = policy(config)
    if recovery_repository.source_local(settings) and not settings.get(
        "require_writer_fence", False
    ):
        raise ValueError("source-local snapshots require the writer fence")
    now = int(time.time()) if now is None else now
    with (
        writer_exclusion(config, socket_dir, port) as fence,
        lock(absolute(settings["backup_root"]) / "locks/mutate", shared=True),
        evidence_lease(settings),
    ):
        if recovery_repository.source_local(settings):
            directory, binding, selected = selected_backup(config, settings, now)
        else:
            directory, binding = backup(config, settings, now)
            selected = None
        postgres.inspect_live(config, settings["system_identifier"], socket_dir, port)
        verify_backup(config, directory)
        result = {
            "version": 1,
            **binding,
            "completed_at": now,
            "record_contract_sha256": contract(settings),
            "source_machine_sha256": application_backup.machine(),
            "records": records(config, settings, socket_dir, port),
        }
        if fence is not None:
            result["writer_fence_token"] = fence["token"]
        if selected is not None:
            if (
                selected.metadata["source_machine_sha256"]
                != result["source_machine_sha256"]
            ):
                raise ValueError(
                    "source-local snapshot machine differs from its immutable capture"
                )
            selected.validate_snapshot(
                result.get("writer_fence_token"), result["records"]
            )
            return snapshot_evidence(config, settings, binding, now)
        write_json(absolute(settings["snapshot_file"]), result)
    return result


def inspect_restored(config, socket_dir, port):
    return json.loads(
        query(
            config,
            socket_dir,
            port,
            "postgres",
            """
        SELECT json_build_object(
            'data_dir', current_setting('data_directory'),
            'major', (current_setting('server_version_num')::int / 10000)::text,
            'system_identifier', system_identifier::text,
            'read_only', current_setting('default_transaction_read_only'),
            'in_recovery', pg_is_in_recovery(),
            'replay_lsn', pg_last_wal_replay_lsn()::text)
        FROM pg_control_system()
    """,
        )
    )


def storage_identities(path, identities, *, primary=False, deadline=None):
    import stat

    if deadline is not None and time.monotonic() >= deadline:
        raise ValueError("custody namespace inspection budget exhausted")
    info = path.lstat()
    if primary and stat.S_ISLNK(info.st_mode):
        info = path.stat()
        if not stat.S_ISREG(info.st_mode):
            raise ValueError("primary configuration link is not a regular file")
    if not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)):
        raise ValueError("recovery cluster contains redirected or special storage")
    identity = (info.st_dev, info.st_ino)
    if identity in identities:
        return
    identities.add(identity)
    if stat.S_ISDIR(info.st_mode):
        for entry in path.iterdir():
            storage_identities(entry, identities, primary=primary, deadline=deadline)


def isolated_restore(restored, authoritative, backup):
    restored_root = restored.lstat()
    for root in (authoritative, backup):
        try:
            info = root.lstat()
        except FileNotFoundError:
            if root == authoritative:
                continue
            raise
        if (info.st_dev, info.st_ino) == (restored_root.st_dev, restored_root.st_ino):
            raise ValueError(
                "restored cluster aliases shared storage in the authoritative primary or selected backup"
            )
    primary = set()
    try:
        authoritative.lstat()
    except FileNotFoundError:
        pass
    else:
        storage_identities(authoritative, primary, primary=True)
    protected = set()
    storage_identities(backup, protected)
    if primary & protected:
        raise ValueError(
            "selected backup aliases shared storage in the authoritative primary"
        )
    protected.update(primary)
    copy = set()
    storage_identities(restored, copy)
    if not protected.isdisjoint(copy):
        raise ValueError(
            "restored cluster aliases shared storage in the authoritative primary or selected backup"
        )


def certify(config, data_dir, socket_dir, port, *, now=None, hostname=None):
    executor = socket.gethostname() if hostname is None else hostname
    settings = _policy(config, certifier=True)
    executor_machine = application_backup.machine()
    now = int(time.time()) if now is None else now
    restored = absolute(data_dir)
    if restored == absolute(config["data_dir"]):
        raise ValueError("cannot certify the authoritative primary as a restored copy")
    with (
        lock(absolute(settings["backup_root"]) / "locks/mutate", shared=True),
        evidence_lease(settings),
    ):
        directory, binding = backup(config, settings, now)
        isolated_restore(restored, absolute(config["data_dir"]), directory)
        source = snapshot_evidence(config, settings, binding, now)
        source_fence(settings, source)
        machine_acceptance(
            source,
            executor_machine,
            executor != settings.get("source_hostname")
            or settings["receipt_file"] == settings.get("off_host_receipt_file"),
        )
        verify_backup(config, directory)
        observed = inspect_restored(config, socket_dir, port)
        expected = {
            "data_dir": str(restored),
            "major": str(config["major"]),
            "system_identifier": settings["system_identifier"],
            "read_only": "on",
            "in_recovery": False,
        }
        if any(observed.get(key) != value for key, value in expected.items()):
            raise ValueError(
                "restored endpoint is not the declared disposable read-only recovery"
            )
        if lsn(observed["replay_lsn"]) < lsn(binding["recovery_target_lsn"]):
            raise ValueError("restored endpoint did not reach the recovery point")
        actual = records(config, settings, socket_dir, port)
        if actual != source.get("records"):
            raise ValueError(
                "restored application records differ from the source snapshot"
            )
        result = {
            "version": 1,
            "status": "ready",
            **binding,
            "completed_at": now,
            "record_contract_sha256": contract(settings),
            "records": actual,
            "snapshot_sha256": digest(source_snapshot_path(config, settings)),
            "executor_host": executor,
            "executor_machine_sha256": executor_machine,
            "source_machine_sha256": source.get("source_machine_sha256"),
            "restored_data_dir": str(restored),
            "replay_lsn": observed["replay_lsn"],
        }
        write_json(absolute(settings["receipt_file"]), result)
    return result


def check(config, *, now=None):
    with admission(config, now=now) as result:
        return result


def live_check(config, socket_dir, port, *, now=None):
    """Read-only whole-primary acceptance at the current fenced record boundary."""
    settings = policy(config)
    with admission(config, now=now, socket_dir=socket_dir, port=port) as result:
        source = read_json(source_snapshot_path(config, settings))
        if recovery_repository.source_local(settings):
            validate_capture_source(
                config, settings, source, int(time.time()) if now is None else now
            )
        if records(config, settings, socket_dir, port) != source["records"]:
            raise ValueError(
                "live primary records differ from the accepted recovery snapshot"
            )
        return {
            **result,
            "snapshot_sha256": digest(source_snapshot_path(config, settings)),
        }


@contextlib.contextmanager
def admission(config, *, now=None, verify_contents=True, socket_dir=None, port=5432):
    """Keep the accepted backup and evidence stable through authority publication."""
    settings = policy(config)
    now = int(time.time()) if now is None else now
    # Inspection never creates or replaces the persistent backup lock anchor.
    with (
        writer_exclusion(config, socket_dir, port) as fence,
        lock(absolute(settings["backup_root"]) / "locks/mutate", shared=True),
    ):
        directory, binding = backup(config, settings, now)
        with evidence_lease(settings, inspect=True):
            yield check_evidence(
                config,
                settings,
                directory,
                binding,
                now,
                verify_contents=verify_contents,
                fence=fence,
            )


def preflight(config, *, now=None):
    """Early rejection only; never substitutes for byte-verified recovery admission."""
    with admission(config, now=now, verify_contents=False) as result:
        return result


def check_evidence(
    config, settings, directory, binding, now, *, verify_contents=True, fence=None
):
    source = snapshot_evidence(config, settings, binding, now)
    recorded_fence = source_fence(settings, source)
    if settings.get("require_writer_fence", False) and (
        fence is None or recorded_fence != fence["token"]
    ):
        raise ValueError(
            "record snapshot writer fence differs from the retained window"
        )
    expected_names = {item["name"] for item in settings["record_checks"]}
    if set(source.get("records", {})) != expected_names or any(
        not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{64}", value)
        for value in source["records"].values()
    ):
        raise ValueError("record snapshot is incomplete")
    if verify_contents:
        verify_backup(config, directory)
    result = {
        "status": "ready" if verify_contents else "preflight-ready",
        **binding,
        "off_host": None,
    }
    paths = [(settings["receipt_file"], "restore acceptance")]
    if settings.get("off_host_receipt_file"):
        paths.append((settings["off_host_receipt_file"], "off-host restore acceptance"))
    for path, label in paths:
        receipt = evidence(path, label, binding, settings, now)
        validate_receipt(config, settings, source, binding, receipt, label)
        if label.startswith("off-host"):
            result["off_host"] = receipt["executor_host"]
    return result


def machine_acceptance(source, executor, off_host):
    if "source_machine_sha256" not in source and not off_host:
        return
    origin = source.get("source_machine_sha256")
    if (
        not isinstance(origin, str)
        or not re.fullmatch(r"[0-9a-f]{64}", origin)
        or not isinstance(executor, str)
        or not re.fullmatch(r"[0-9a-f]{64}", executor)
        or (origin == executor) == off_host
    ):
        if off_host:
            raise ValueError(
                "off-host recovery must execute on an independent machine with a bound source machine identity"
            )
        raise ValueError("local recovery must execute on the captured source machine")


def validate_receipt(config, settings, source, binding, receipt, label):
    if (
        receipt.get("status") != "ready"
        or receipt.get("records") != source["records"]
        or receipt.get("snapshot_sha256")
        != digest(source_snapshot_path(config, settings))
        or receipt.get("restored_data_dir") == config["data_dir"]
        or not receipt.get("restored_data_dir")
        or receipt["completed_at"] < source["completed_at"]
        or lsn(receipt.get("replay_lsn")) < lsn(binding["recovery_target_lsn"])
    ):
        raise ValueError(f"{label} is not complete record-level recovery evidence")
    if label.startswith("off-host") and (
        not receipt.get("executor_host")
        or receipt["executor_host"] == settings["source_hostname"]
    ):
        raise ValueError("off-host recovery must execute on an independent host")
    if not label.startswith("off-host") and (
        not settings.get("source_hostname")
        or receipt.get("executor_host") != settings["source_hostname"]
    ):
        raise ValueError("local recovery must execute on the declared source host")
    machine_acceptance(
        source, receipt.get("executor_machine_sha256"), label.startswith("off-host")
    )
    if (
        "source_machine_sha256" in source
        and receipt.get("source_machine_sha256") != source["source_machine_sha256"]
    ):
        raise ValueError("recovery receipt differs from the source machine identity")


def stage_off_host_import(incoming, destination):
    """Root pins a trusted receipt into a fresh private systemd import source."""
    from .application_publication import credential_source
    import stat

    destination = Path(destination)
    if os.geteuid() != 0:
        raise ValueError("off-host receipt staging requires root")
    if not destination.is_absolute():
        raise ValueError("off-host staging destination must be absolute")
    parent = destination.parent
    for ancestor in (parent, *parent.parents):
        info = ancestor.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
            raise ValueError("untrusted off-host staging ancestor")
    if parent.stat().st_mode & 0o077:
        raise ValueError("off-host staging parent must be private")
    with credential_source(incoming) as source:
        info = os.fstat(source.fileno())
        if info.st_nlink != 1 or info.st_size > 16 << 20:
            raise ValueError(
                "off-host import receipt must be a bounded single-link regular file"
            )
        data = source.read((16 << 20) + 1)
    if len(data) > 16 << 20:
        raise ValueError("off-host import receipt exceeds the byte limit")
    descriptor = os.open(
        destination,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
        0o400,
    )
    with os.fdopen(descriptor, "wb") as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())
    sync_directory(parent)


def import_off_host(config, path, *, now=None):
    """Validate a transported receipt, then publish it under the evidence lease."""
    settings = policy(config)
    if not settings.get("off_host_receipt_file"):
        raise ValueError(
            "off-host receipt import requires an off-host admission policy"
        )
    now = int(time.time()) if now is None else now
    with (
        writer_exclusion(config) as fence,
        lock(absolute(settings["backup_root"]) / "locks/mutate", shared=True),
        evidence_lease(settings),
    ):
        directory, binding = backup(config, settings, now)
        local = settings | {"off_host_receipt_file": None}
        check_evidence(config, local, directory, binding, now, fence=fence)
        source = snapshot_evidence(config, settings, binding, now)
        receipt = evidence(path, "off-host restore acceptance", binding, settings, now)
        validate_receipt(
            config, settings, source, binding, receipt, "off-host restore acceptance"
        )
        write_json(absolute(settings["off_host_receipt_file"]), receipt)


def prepare(config, preparation, socket_dir, port):
    """Explicit managed bootstrap; ordinary admission/startup never calls this."""
    settings = policy(config)
    for key in (
        "readiness_command",
        "backup_command",
        "restore_command",
        "export_command",
    ):
        if key == "export_command" and not preparation.get(key):
            continue
        argv = preparation[key]
        if (
            not isinstance(argv, list)
            or not argv
            or any(not isinstance(arg, str) or not arg or "\0" in arg for arg in argv)
            or not Path(argv[0]).is_absolute()
        ):
            raise ValueError(f"{key} must be explicit absolute executable argv")
    anchor = absolute(settings["snapshot_file"]).parent / "preparation.lock"
    with writer_exclusion(config, socket_dir, port) as fence:
        if recovery_repository.source_local(settings):
            with lock(absolute(settings["backup_root"]) / "locks/mutate", shared=True):
                if not (
                    absolute(settings["backup_root"]) / "recovery/SELECTED"
                ).exists():
                    raise ValueError(
                        "source-local preparation requires an explicitly published producer capture"
                    )
                with evidence_lease(settings, inspect=True):
                    _, binding = backup(config, settings, int(time.time()))
                    snapshot_evidence(config, settings, binding, int(time.time()))
        return _prepare_fenced(
            config, preparation, socket_dir, port, settings, anchor, fence
        )


def _prepare_fenced(config, preparation, socket_dir, port, settings, anchor, fence):
    with lock(anchor, create=True):
        postgres.inspect_live(config, settings["system_identifier"], socket_dir, port)
        postgres.run(preparation["readiness_command"])
        # A published snapshot binds the selected backup. Never replace that
        # backup on retries, even when off-host evidence is still missing.
        if recovery_repository.source_local(settings):
            with lock(absolute(settings["backup_root"]) / "locks/mutate", shared=True):
                source_snapshot = source_snapshot_path(config, settings)
        else:
            source_snapshot = Path(settings["snapshot_file"])
        if recovery_repository.source_local(settings) and not source_snapshot.exists():
            raise ValueError(
                "source-local preparation requires an explicitly published producer snapshot"
            )
        if not source_snapshot.exists():
            journal = anchor.with_name("preparation.json")
            if not journal.exists():
                if any(
                    Path(settings[key]).exists()
                    for key in ("receipt_file", "off_host_receipt_file")
                    if settings.get(key)
                ):
                    raise ValueError(
                        "recovery receipts exist without their bound source snapshot"
                    )
                postgres.run(preparation["backup_command"])
            with lock(absolute(settings["backup_root"]) / "locks/mutate", shared=True):
                _, binding = backup(config, settings, int(time.time()))
                if journal.exists():
                    if read_json(absolute(journal)) != binding:
                        raise ValueError(
                            "managed preparation backup changed before snapshot; refusing replacement"
                        )
                else:
                    write_json(journal, binding)
                snapshot(config, socket_dir, port)
        else:
            now = int(time.time())
            with (
                lock(absolute(settings["backup_root"]) / "locks/mutate", shared=True),
                evidence_lease(settings, inspect=True),
            ):
                directory, binding = backup(config, settings, now)
                source = snapshot_evidence(config, settings, binding, now)
                if (
                    settings.get("require_writer_fence", False)
                    and source_fence(settings, source) != fence["token"]
                ):
                    raise ValueError(
                        "record snapshot writer fence differs from the retained window"
                    )
                verify_backup(config, directory)
        if not Path(settings["receipt_file"]).exists():
            postgres.run(preparation["restore_command"])
        with (
            lock(absolute(settings["backup_root"]) / "locks/mutate", shared=True),
            evidence_lease(settings, inspect=True),
        ):
            now = int(time.time())
            directory, binding = backup(config, settings, now)
            check_evidence(
                config,
                settings | {"off_host_receipt_file": None},
                directory,
                binding,
                now,
                fence=fence,
            )
        # Transport orchestration belongs to the consumer. Its explicit export
        # command may publish a service-user-owned immutable copy for the
        # independent executor even when final off-host admission will abort.
        if preparation.get("export_command"):
            postgres.run(preparation["export_command"])
        credentials = os.environ.get("CREDENTIALS_DIRECTORY")
        if credentials:
            incoming = Path(credentials) / "recovery-off-host"
            if incoming.exists():
                import_off_host(config, incoming)
        # Missing independent execution must abort the managed activation before
        # authority publication, while retaining the completed local evidence.
        return check(config)
