"""Conservative retention of complete base backups and their WAL chains."""

import argparse
import json
import re
import shutil
import sys
import time
from pathlib import Path

from .durable import lock, sync_directory


def prune(root, base_days, wal_days, segment_bytes, *, now=None):
    """Keep two latest backups and all retained WAL from their earliest start.

    Unknown manifests/timelines fail conservatively: no WAL deletion. Partial
    transfers are never complete backups and are preserved for diagnosis.
    """
    if segment_bytes < 1024 * 1024 or segment_bytes > 1024 * 1024 * 1024 or segment_bytes & (segment_bytes - 1):
        raise ValueError("invalid PostgreSQL WAL segment size")
    root = Path(root)
    now = time.time() if now is None else now
    with lock(root / "BACKUP_LOCK", create=True):
        base = root / "base"
        backups = [p for p in base.iterdir() if not p.name.endswith(".partial")]
        if any(p.is_symlink() or not p.is_dir() for p in backups):
            return
        backups.sort(key=lambda p: p.stat().st_mtime, reverse=True)
        retained, expired = [], []
        for index, path in enumerate(backups):
            if index < 2 or path.stat().st_mtime >= now - base_days * 86400:
                retained.append(path)
            else:
                expired.append(path)
        if not retained:
            return
        floors = {}
        try:
            # Classify every completed directory, even one old enough to expire.
            # A filename's presence does not establish a valid recovery chain.
            for path in backups:
                manifest = path / "backup_manifest"
                if manifest.is_symlink():
                    return
                ranges = json.loads(manifest.read_text())["WAL-Ranges"]
                if not isinstance(ranges, list) or not ranges:
                    return
                for entry in ranges:
                    timeline = entry["Timeline"]
                    if type(timeline) is not int or not 1 <= timeline <= 0xFFFFFFFF:
                        return
                    limits = []
                    for key in ("Start-LSN", "End-LSN"):
                        lsn = entry[key]
                        if not isinstance(lsn, str) or not re.fullmatch(r"[0-9A-Fa-f]{1,8}/[0-9A-Fa-f]{1,8}", lsn):
                            return
                        high, low = lsn.split("/")
                        limits.append((int(high, 16) << 32) + int(low, 16))
                    if limits[1] < limits[0]:
                        return
                    if path in retained:
                        start = limits[0] // segment_bytes
                        floors[timeline] = min(floors.get(timeline, start), start)
        except (OSError, ValueError, KeyError, TypeError):
            return
        # Establish the recovery floor before deleting any completed backup.
        for path in expired:
            shutil.rmtree(path)
        sync_directory(base)
        wal = root / "wal"
        segments = [p for p in wal.iterdir() if re.fullmatch(r"[0-9A-F]{24}", p.name)
                    and p.is_file() and not p.is_symlink()]
        # A consumer size mismatch must never translate into unsafe LSN floors.
        if {p.stat().st_size for p in segments} != {segment_bytes}:
            return
        for path in segments:
            timeline = int(path.name[:8], 16)
            segment = int(path.name[8:16], 16) * (2**32 // segment_bytes) + int(path.name[16:], 16)
            if timeline in floors and segment < floors[timeline] and path.stat().st_mtime < now - wal_days * 86400:
                path.unlink()
        sync_directory(wal)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--base-days", type=int, required=True)
    parser.add_argument("--wal-days", type=int, required=True)
    parser.add_argument("--segment-bytes", type=int, required=True)
    args = parser.parse_args()
    try:
        if args.base_days < 1 or args.wal_days < args.base_days + 1:
            raise ValueError("require wal-days >= base-days + 1 > 1")
        prune(args.root, args.base_days, args.wal_days, args.segment_bytes)
    except (OSError, ValueError) as error:
        print(f"harbor-db-backup-prune: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
