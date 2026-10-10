"""Read source-local immutable finalizer intents while owning the mutation lease."""

import contextlib
import hashlib
import json
import os
import re
import stat
from pathlib import Path

from .durable import lock


def canonical(path):
    if not path.is_absolute() or path.resolve(strict=True) != path:
        raise ValueError("redirected source-local storage")
    if any(p.is_symlink() for p in (path, *path.parents)):
        raise ValueError("redirected source-local ancestor")


def directory(path):
    canonical(path)
    if not stat.S_ISDIR(path.lstat().st_mode):
        raise ValueError("source-local storage requires a directory")


def read_bytes(path):
    canonical(path)
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError("source-local metadata requires a regular file")
        data = stream.read(16 * 1024 * 1024 + 1)
        if len(data) > 16 * 1024 * 1024:
            raise ValueError("source-local metadata exceeds size limit")
        return data


def read_json(path):
    return json.loads(read_bytes(path))


def child(value):
    return isinstance(value, str) and bool(re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", value)) and not value.endswith(".partial")


@contextlib.contextmanager
def protection(root, segment_bytes):
    recovery = root / "recovery"
    try:
        recovery.lstat()
    except FileNotFoundError:
        yield set()
        return
    directory(root)
    directory(recovery)
    if read_bytes(recovery / "PROTOCOL").decode().strip() != "source-local-v1":
        raise ValueError("invalid source-local protocol")
    directory(recovery / "pins")
    directory(root / "locks")
    canonical(root / "locks/mutate")
    # Validate the existing inode before durable.lock's exclusive acquisition.
    read_bytes(root / "locks/mutate")
    with lock(root / "locks/mutate"):
        directory(root / "base")
        directory(root / "wal")
        backups = set()
        for path in (recovery / "pins").iterdir():
            capture_id = path.name[:-5] if path.name.endswith(".json") else ""
            if not child(capture_id) or capture_id.startswith(".") or ".partial" in capture_id:
                raise ValueError("uncertain source-local pin entry")
            pin = read_json(path)
            if not isinstance(pin, dict):
                raise ValueError("invalid source-local pin object")
            backup_id = pin.get("backup_id")
            digest = pin.get("manifest_sha256")
            if (type(pin.get("version")) is not int or pin["version"] != 1
                    or pin.get("capture_id") != capture_id or not child(backup_id)
                    or type(pin.get("wal_segment_bytes")) is not int
                    or pin["wal_segment_bytes"] != segment_bytes
                    or not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest)):
                raise ValueError("invalid source-local pin metadata")
            base = root / "base" / backup_id
            directory(base)
            if hashlib.sha256(read_bytes(base / "backup_manifest")).hexdigest() != digest:
                raise ValueError("pinned manifest digest mismatch")
            backups.add(backup_id)
        yield backups
