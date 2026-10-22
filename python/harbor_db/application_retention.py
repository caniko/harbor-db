"""Serialized retention of timer-generated attempts; manual/transition names stay pinned."""

import os
import re
import shutil
import time
from datetime import datetime
from pathlib import Path

from . import application_backup, application_publication, cutover
from .durable import lock, read_json, sync_directory


def scheduled_entry(name):
    attempt, _, suffix = name.partition(".")
    if not re.fullmatch(r"timer-[0-9]{8}T[0-9]{6}Z-[0-9]+", attempt):
        return None
    try:
        datetime.strptime(attempt[6:22], "%Y%m%dT%H%M%SZ")
    except ValueError:
        return None
    if suffix not in (
        "",
        "partial",
        "sealed",
        "credentials",
        "restore-workspace",
        "intent.json",
        "pruning",
    ) and not re.fullmatch(r"(?:abandoned|retry-credentials)-[0-9]+", suffix):
        return None
    return attempt


def check_tree(path, device, mounts):
    info = path.lstat()
    if path.is_dir() and not path.is_symlink():
        if info.st_dev != device or os.fsencode(path) in mounts:
            raise ValueError("scheduled pruning cannot cross a mounted directory")
        for child in path.iterdir():
            check_tree(child, device, mounts)


def prune(config, keep, incomplete_age):
    root = application_backup.validate(config)
    cutover.validate_path(config["root"])
    if (
        type(keep) is not int
        or not 1 <= keep <= 4096
        or type(incomplete_age) is not int
        or not config["timeout_seconds"] * 5 + 60 <= incomplete_age <= 90 * 86400
    ):
        raise ValueError(
            "scheduled retention requires bounded keep and an incomplete age beyond all capture stage budgets"
        )
    if config["version"] == 2:
        application_publication.require_namespace(root)
    with lock(root / "lock"):
        now = int(time.time())
        groups = {}
        for entry in root.iterdir():
            attempt = scheduled_entry(entry.name)
            if attempt is not None:
                if entry.is_symlink():
                    raise ValueError("scheduled retention entry is redirected")
                groups.setdefault(attempt, []).append(entry)
        latest = None
        if (root / "LAST_SUCCESS").exists():
            marker = read_json(root / "LAST_SUCCESS")
            latest = marker["attempt"]
            if (
                not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", latest)
                or application_backup.digest(root / latest / "acceptance.json")
                != marker["acceptance_sha256"]
            ):
                raise ValueError("scheduled retention cannot trust LAST_SUCCESS")
        successes, remove = [], []
        for attempt, entries in groups.items():
            destination = root / attempt
            tombstone = root / (attempt + ".pruning")
            if destination in entries:
                if tombstone in entries:
                    raise ValueError(
                        "scheduled retention has conflicting live and retiring output"
                    )
                receipt = read_json(destination / "acceptance.json")
                captured = receipt["captured_at"]
                if type(captured) is not int or not 0 <= captured <= now:
                    raise ValueError("invalid retained capture timestamp")
                if (
                    receipt["status"] != "verified"
                    or receipt["attempt"] != attempt
                    or receipt["resource"] != config["resource"]
                ):
                    raise ValueError(
                        "scheduled retention encountered unbound successful output"
                    )
                successes.append((captured, attempt))
                # Retain the completed point and its retry intent, not worker scratch.
                remove.extend(
                    path
                    for path in entries
                    if path not in (destination, root / (attempt + ".intent.json"))
                )
            elif (
                tombstone in entries
                or now - max(entry.lstat().st_mtime for entry in entries)
                >= incomplete_age
            ):
                remove.extend(entries)
        successes.sort(reverse=True)
        discarded = [attempt for _, attempt in successes[keep:] if attempt != latest]
        for attempt in discarded:
            remove.extend(groups[attempt])
        remove = sorted(set(remove))
        mounts = {
            re.sub(
                rb"\\([0-7]{3})",
                lambda match: bytes([int(match[1], 8)]),
                line.split()[4],
            )
            for line in Path("/proc/self/mountinfo").read_bytes().splitlines()
        }
        for path in remove:
            check_tree(path, root.stat().st_dev, mounts)
        ordinary, tombstones = [], []
        for path in remove:
            if scheduled_entry(path.name) == path.name:
                tombstone = root / (path.name + ".pruning")
                path.rename(tombstone)
                sync_directory(root)
                tombstones.append(tombstone)
            elif path.name.endswith(".pruning"):
                tombstones.append(path)
            else:
                ordinary.append(path)
        for path in ordinary + tombstones:
            if path.is_dir() and not path.is_symlink():
                shutil.rmtree(path)
            else:
                path.unlink()
            sync_directory(root)
        return {
            "status": "pruned",
            "removed": list(map(str, remove)),
            "retained_scheduled": len(successes) - len(discarded),
        }
