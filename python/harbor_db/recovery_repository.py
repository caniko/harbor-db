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
    if "repository_protocol" not in settings or settings["repository_protocol"] == "legacy":
        return False
    if settings["repository_protocol"] == "source-local-v1":
        return True
    raise ValueError("invalid recovery repository protocol")


def valid_identifier(value):
    return isinstance(value, str) and re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", value) is not None and not value.endswith(".partial")


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


@dataclass(frozen=True)
class SelectedCapture:
    directory: object
    metadata_path: object
    snapshot_path: object
    metadata: object
    binding: object

    def validate_snapshot(self, token, records):
        if token != self.metadata["writer_fence_token"] or records != self.metadata["record_hashes"]:
            raise ValueError("source-local capture differs from the live fenced records")


def select(config, settings, now):
    from . import recovery
    if not source_local(settings):
        raise ValueError("source-local capture selection requires source-local-v1")
    root = recovery.absolute(settings["backup_root"])
    if _carrier(root / "recovery/PROTOCOL", 256).decode("utf-8").strip() != "source-local-v1":
        raise ValueError("invalid recovery repository protocol")
    selector = recovery.absolute(root / "recovery/SELECTED")
    with os.fdopen(_regular(selector), "rb") as stream:
        content = stream.read(257)
    if len(content) > 256:
        raise ValueError("capture selector exceeds 256 bytes")
    identifier = content.decode("utf-8").strip()
    if not valid_identifier(identifier):
        raise ValueError("invalid capture identifier")
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
    ranges = manifest_value.get("WAL-Ranges") if isinstance(manifest_value, dict) else None
    timeline = meta.get("timeline")
    if not isinstance(ranges, list) or not ranges:
        raise ValueError("source-local capture requires manifest WAL ranges")
    if type(timeline) is not int or not 1 <= timeline <= 2**32 - 1:
        raise ValueError("invalid capture timeline")
    stop = 0
    for item in ranges:
        if not isinstance(item, dict):
            raise ValueError("invalid manifest WAL range")
        start, end = recovery.lsn(item.get("Start-LSN")), recovery.lsn(item.get("End-LSN"))
        if type(item.get("Timeline")) is not int or item["Timeline"] != timeline or start > end:
            raise ValueError("capture requires one consistent manifest WAL timeline and valid ranges")
        stop = max(stop, end)
    if recovery.lsn(meta.get("backup_stop_lsn")) != stop or recovery.lsn(meta.get("post_backup_lsn")) <= stop:
        raise ValueError("capture recovery point does not follow the actual manifest stop LSN")
    hashes = meta.get("record_hashes")
    segment = meta.get("wal_segment_bytes")
    if (type(meta.get("version")) is not int or meta["version"] != 1
            or meta.get("capture_id") != identifier or meta.get("manifest_sha256") != manifest_digest
            or meta.get("record_contract_sha256") != recovery.contract(settings)
            or not isinstance(meta.get("writer_fence_token"), str) or not re.fullmatch(r"[0-9a-f]{32}", meta["writer_fence_token"])
            or meta.get("epoch_id") != meta["writer_fence_token"]
            or type(segment) is not int or not 2**20 <= segment <= 2**30 or segment & (segment - 1)
            or not isinstance(hashes, dict) or not hashes or set(hashes) != {item["name"] for item in settings["record_checks"]}
            or any(not isinstance(value, str) or not re.fullmatch(r"[0-9a-f]{64}", value) for value in hashes.values())):
        raise ValueError("invalid source-local capture metadata or manifest binding")
    if str(meta["pg_major"]) != str(config["major"]) or meta["system_identifier"] != settings["system_identifier"]:
        raise ValueError("backup identity does not match the declared primary")
    if recovery.lsn(meta["post_backup_lsn"]) <= recovery.lsn(meta["backup_stop_lsn"]):
        raise ValueError("backup recovery point must follow its stop LSN")
    recovery.fresh(meta.get("completed_at"), int(time.time()), settings["max_age_seconds"])
    recovery.fresh(int(manifest.stat().st_mtime), now, settings["max_age_seconds"])
    binding = {"backup_id":meta["backup_id"], "system_identifier":settings["system_identifier"], "major":str(config["major"]),
               "epoch_id":meta["epoch_id"], "manifest_sha256":manifest_digest, "recovery_target_lsn":meta["post_backup_lsn"],
               "metadata_sha256":hashlib.sha256(metadata_bytes).hexdigest()}
    meta["record_hashes"] = MappingProxyType(hashes)
    return SelectedCapture(directory, metadata_path, snapshot_path, MappingProxyType(meta), MappingProxyType(binding))
