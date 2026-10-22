"""Read-only source-local selection. Callers retain the repository lease."""

import hashlib
import json
import os
import re
import stat
import time
from dataclasses import dataclass
from types import MappingProxyType


def source_local(settings):
    if (
        "repository_protocol" not in settings
        or settings["repository_protocol"] == "legacy"
    ):
        return False
    if settings["repository_protocol"] == "source-local-v1":
        return True
    raise ValueError("invalid recovery repository protocol")


def valid_identifier(value):
    return (
        isinstance(value, str)
        and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", value) is not None
        and not value.endswith(".partial")
    )


def _regular(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    if not stat.S_ISREG(os.fstat(fd).st_mode):
        os.close(fd)
        raise ValueError("storage is not a regular file")
    return fd


def _carrier(path, limit):
    from . import recovery

    with os.fdopen(_regular(recovery.absolute(path)), "rb") as stream:
        content = stream.read(limit + 1)
    if len(content) > limit:
        raise ValueError("recovery repository carrier exceeds its byte limit")
    return content


def reject_retired(root, identifier):
    path = root / "recovery/captures" / f"{identifier}.retired"
    try:
        path.lstat()
    except FileNotFoundError:
        return
    raise ValueError("capture identifier is permanently retired; use a new identifier")


def _optional_carrier(path, limit):
    try:
        path.lstat()
    except FileNotFoundError:
        return None
    return _carrier(path, limit)


def retire(config, identifier):
    """Explicit retirement under the exclusive reader/producer repository lease."""
    from . import recovery
    from .durable import lock, sync_directory, write_json

    settings = config["recovery"]
    if not source_local(settings) or not valid_identifier(identifier):
        raise ValueError(
            "retirement requires source-local-v1 and a valid capture identifier"
        )
    root = recovery.absolute(settings["backup_root"])
    major = str(config["major"])
    if (
        not re.fullmatch(r"[0-9]+", major)
        or not isinstance(settings["system_identifier"], str)
        or not re.fullmatch(r"[1-9][0-9]*", settings["system_identifier"])
    ):
        raise ValueError("invalid retirement repository identity")
    local = root / "recovery"
    for name in ("pins", "captures", "snapshots"):
        path = recovery.absolute(local / name)
        if not stat.S_ISDIR(path.lstat().st_mode):
            raise ValueError("retirement requires existing repository directories")
    with lock(recovery.absolute(root / "locks/mutate")):
        if (
            _carrier(local / "PROTOCOL", 256).decode("utf-8").strip()
            != "source-local-v1"
        ):
            raise ValueError("invalid recovery repository protocol")
        selected = _optional_carrier(local / "SELECTED", 256)
        if selected is not None:
            selected = selected.decode("utf-8").strip()
            if not valid_identifier(selected) or selected == identifier:
                raise ValueError(
                    "cannot retire the selected capture or an uncertain selection"
                )
        names = (
            f"captures/{identifier}.json",
            f"captures/{identifier}.wal-inventory",
            f"snapshots/{identifier}.json",
            f"pins/{identifier}.json",
        )
        carriers = {name: _optional_carrier(local / name, 16 << 20) for name in names}
        journal = local / "captures" / f"{identifier}.retired"
        header = {
            "version": 1,
            "capture_id": identifier,
            "backup_root": str(root),
            "major": major,
            "system_identifier": settings["system_identifier"],
        }
        saved = _optional_carrier(journal, 16 << 20)
        if saved is not None:
            record = json.loads(saved)
            hashes = record.get("carriers") if isinstance(record, dict) else None
            if (
                not isinstance(hashes, dict)
                or set(hashes) != set(names)
                or hashes[names[3]] is None
                or {key: value for key, value in record.items() if key != "carriers"}
                != header
                or type(record.get("version")) is not int
            ):
                raise ValueError(
                    "capture retirement journal differs from repository identity"
                )
            for name, current in carriers.items():
                expected = hashes[name]
                if (
                    expected is not None
                    and (
                        not isinstance(expected, str)
                        or not re.fullmatch(r"[0-9a-f]{64}", expected)
                    )
                ) or (
                    current is not None
                    and expected != hashlib.sha256(current).hexdigest()
                ):
                    raise ValueError(
                        "capture retirement carrier changed; preserving remaining evidence"
                    )
        else:
            pin = carriers[names[3]]
            if pin is None:
                raise ValueError("capture retirement requires an existing pin")
            metadata = json.loads(pin)
            if (
                not isinstance(metadata, dict)
                or type(metadata.get("version")) is not int
                or metadata["version"] != 1
                or metadata.get("capture_id") != identifier
                or metadata.get("system_identifier") != settings["system_identifier"]
                or str(metadata.get("pg_major")) != major
                or not valid_identifier(metadata.get("backup_id"))
                or (carriers[names[0]] is not None and carriers[names[0]] != pin)
            ):
                raise ValueError(
                    "capture retirement pin or metadata differs from repository identity"
                )
            if carriers[names[2]] is not None:
                snapshot = json.loads(carriers[names[2]])
                if (
                    not isinstance(snapshot, dict)
                    or snapshot.get("metadata_sha256")
                    != hashlib.sha256(pin).hexdigest()
                ):
                    raise ValueError("capture retirement snapshot differs from its pin")
            record = {
                **header,
                "carriers": {
                    name: None
                    if content is None
                    else hashlib.sha256(content).hexdigest()
                    for name, content in carriers.items()
                },
            }
            write_json(journal, record)
        # Producer contention leaves the acknowledged retirement journal resumable.
        with lock(recovery.absolute(root / "BACKUP_LOCK")):
            # Keep the pin until obsolete metadata/snapshot unlinks are durable.
            for name, content in carriers.items():
                path = local / name
                if content is not None:
                    path.unlink()
                # Resume the flush even when a prior attempt already unlinked it.
                sync_directory(path.parent)
        return record


@dataclass(frozen=True)
class SelectedCapture:
    directory: object
    metadata_path: object
    snapshot_path: object
    metadata: object
    binding: object

    def validate_snapshot(self, token, records):
        if (
            token != self.metadata["writer_fence_token"]
            or records != self.metadata["record_hashes"]
        ):
            raise ValueError(
                "source-local capture differs from the live fenced records"
            )


def select(config, settings, now):
    from . import recovery

    if not source_local(settings):
        raise ValueError("source-local capture selection requires source-local-v1")
    root = recovery.absolute(settings["backup_root"])
    if (
        _carrier(root / "recovery/PROTOCOL", 256).decode("utf-8").strip()
        != "source-local-v1"
    ):
        raise ValueError("invalid recovery repository protocol")
    selector = recovery.absolute(root / "recovery/SELECTED")
    with os.fdopen(_regular(selector), "rb") as stream:
        content = stream.read(257)
    if len(content) > 256:
        raise ValueError("capture selector exceeds 256 bytes")
    identifier = content.decode("utf-8").strip()
    if not valid_identifier(identifier):
        raise ValueError("invalid capture identifier")
    reject_retired(root, identifier)
    metadata_path = recovery.absolute(root / "recovery/captures" / f"{identifier}.json")
    snapshots = recovery.absolute(root / "recovery/snapshots")
    if not snapshots.is_dir():
        raise ValueError("capture snapshots namespace is not a directory")
    snapshot_path = recovery.absolute(snapshots / f"{identifier}.json")
    metadata_bytes = _carrier(metadata_path, 16 << 20)
    pin_bytes = _carrier(root / "recovery/pins" / f"{identifier}.json", 16 << 20)
    if pin_bytes != metadata_bytes:
        raise ValueError("capture metadata differs from its immutable retention pin")
    meta = json.loads(metadata_bytes)
    if not isinstance(meta, dict):
        raise ValueError("capture metadata must be an object")
    if not valid_identifier(meta.get("backup_id")):
        raise ValueError("invalid completed backup identifier")
    directory = recovery.absolute(root / "base" / meta["backup_id"])
    manifest = recovery.absolute(directory / "backup_manifest")
    with os.fdopen(_regular(manifest), "rb") as stream:
        manifest_bytes = stream.read()
    manifest_digest = hashlib.sha256(manifest_bytes).hexdigest()
    manifest_value = json.loads(manifest_bytes)
    ranges = (
        manifest_value.get("WAL-Ranges") if isinstance(manifest_value, dict) else None
    )
    timeline = meta.get("timeline")
    if not isinstance(ranges, list) or not ranges:
        raise ValueError("source-local capture requires manifest WAL ranges")
    if type(timeline) is not int or not 1 <= timeline <= 2**32 - 1:
        raise ValueError("invalid capture timeline")
    stop = 0
    for item in ranges:
        if not isinstance(item, dict):
            raise ValueError("invalid manifest WAL range")
        start, end = (
            recovery.lsn(item.get("Start-LSN")),
            recovery.lsn(item.get("End-LSN")),
        )
        if (
            type(item.get("Timeline")) is not int
            or item["Timeline"] != timeline
            or start > end
        ):
            raise ValueError(
                "capture requires one consistent manifest WAL timeline and valid ranges"
            )
        stop = max(stop, end)
    if (
        recovery.lsn(meta.get("backup_stop_lsn")) != stop
        or recovery.lsn(meta.get("post_backup_lsn")) <= stop
    ):
        raise ValueError(
            "capture recovery point does not follow the actual manifest stop LSN"
        )
    hashes = meta.get("record_hashes")
    segment = meta.get("wal_segment_bytes")
    if (
        type(meta.get("version")) is not int
        or meta["version"] != 1
        or meta.get("capture_id") != identifier
        or meta.get("manifest_sha256") != manifest_digest
        or meta.get("record_contract_sha256") != recovery.contract(settings)
        or not isinstance(meta.get("source_machine_sha256"), str)
        or not re.fullmatch(r"[0-9a-f]{64}", meta["source_machine_sha256"])
        or not isinstance(meta.get("writer_fence_token"), str)
        or not re.fullmatch(r"[0-9a-f]{32}", meta["writer_fence_token"])
        or meta.get("epoch_id") != meta["writer_fence_token"]
        or type(segment) is not int
        or not 2**20 <= segment <= 2**30
        or segment & (segment - 1)
        or not isinstance(hashes, dict)
        or not hashes
        or set(hashes) != {item["name"] for item in settings["record_checks"]}
        or any(
            not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{64}", value)
            for value in hashes.values()
        )
    ):
        raise ValueError("invalid source-local capture metadata or manifest binding")
    if (
        str(meta["pg_major"]) != str(config["major"])
        or meta["system_identifier"] != settings["system_identifier"]
    ):
        raise ValueError("backup identity does not match the declared primary")
    if recovery.lsn(meta["post_backup_lsn"]) <= recovery.lsn(meta["backup_stop_lsn"]):
        raise ValueError("backup recovery point must follow its stop LSN")
    first = min(recovery.lsn(entry["Start-LSN"]) // segment for entry in ranges)
    last = (recovery.lsn(meta["post_backup_lsn"]) - 1) // segment + 1
    if not 1 <= last - first + 1 <= 1_000_000:
        raise ValueError("capture WAL range exceeds supported segment limit")
    per_log = 2**32 // segment
    wal_bytes = _carrier(
        root / "recovery" / "captures" / f"{identifier}.wal-inventory", 16 << 20
    )
    inventory = {}
    for number in range(first, last + 1):
        name = f"{timeline:08X}{number // per_log:08X}{number % per_log:08X}"
        carrier = recovery.absolute(root / "wal" / name)
        with os.fdopen(_regular(carrier), "rb") as stream:
            before = os.fstat(stream.fileno())
            if before.st_size != segment:
                raise ValueError(
                    "retained WAL carrier is not a complete regular segment"
                )
            inventory[name] = hashlib.file_digest(stream, "sha256").hexdigest()
            after = os.fstat(stream.fileno())
            if after.st_size != segment or (before.st_mtime_ns, before.st_ctime_ns) != (
                after.st_mtime_ns,
                after.st_ctime_ns,
            ):
                raise ValueError("retained WAL carrier changed during inventory")
    if json.loads(wal_bytes) != inventory:
        raise ValueError(
            "retained WAL bytes differ from the immutable capture inventory"
        )
    recovery.fresh(
        meta.get("completed_at"), int(time.time()), settings["max_age_seconds"]
    )
    recovery.fresh(int(manifest.stat().st_mtime), now, settings["max_age_seconds"])
    binding = {
        "backup_id": meta["backup_id"],
        "system_identifier": settings["system_identifier"],
        "major": str(config["major"]),
        "epoch_id": meta["epoch_id"],
        "manifest_sha256": manifest_digest,
        "recovery_target_lsn": meta["post_backup_lsn"],
        "metadata_sha256": hashlib.sha256(metadata_bytes).hexdigest(),
        "wal_inventory_sha256": hashlib.sha256(wal_bytes).hexdigest(),
    }
    meta["record_hashes"] = MappingProxyType(hashes)
    return SelectedCapture(
        directory,
        metadata_path,
        snapshot_path,
        MappingProxyType(meta),
        MappingProxyType(binding),
    )
