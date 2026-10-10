"""Filesystem publication primitives. Callers own locking and resource semantics."""

import contextlib
import argparse
import fcntl
import json
import os
import stat
import tempfile
import sys
from pathlib import Path


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def atomic_write(path, data):
    """Publish bytes, synchronizing the file and containing directory."""
    path = Path(path)
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        sync_directory(path.parent)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def write_json(path, value):
    atomic_write(path, (json.dumps(value, sort_keys=True) + "\n").encode())


def read_json(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd) as stream:
        return json.load(stream)


def sync_tree(path):
    """Flush an offline resource before publication; reject external symlinks."""
    path = Path(path)
    for entry in path.iterdir():
        mode = entry.lstat().st_mode
        if stat.S_ISDIR(mode):
            sync_tree(entry)
        elif stat.S_ISREG(mode):
            fd = os.open(entry, os.O_RDONLY | os.O_NOFOLLOW)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        else:
            raise ValueError(f"external or special storage is unsupported: {entry}")
    sync_directory(path)


def publish_file(source, destination):
    source, destination = Path(source), Path(destination)
    if destination.exists() or destination.is_symlink():
        raise ValueError("publication destination already exists")
    fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            raise ValueError("publication source is not a regular file")
        os.fsync(fd)
    finally:
        os.close(fd)
    source.rename(destination)
    sync_directory(destination.parent)
    if source.parent != destination.parent:
        sync_directory(source.parent)


@contextlib.contextmanager
def lock(path, *, shared=False, create=False):
    """Kernel lock on a persistent inode; never unlink a lock anchor."""
    flags = (os.O_RDONLY if shared else os.O_RDWR) | os.O_NOFOLLOW
    if create:
        flags |= os.O_CREAT
    fd = os.open(path, flags, 0o600)
    try:
        operation = fcntl.LOCK_SH if shared else fcntl.LOCK_EX
        fcntl.flock(fd, operation | fcntl.LOCK_NB)
        yield fd
    finally:
        os.close(fd)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    write = commands.add_parser("write", help="durably publish stdin to a file")
    write.add_argument("destination", type=Path)
    publish = commands.add_parser("publish-tree", help="flush and publish an offline tree")
    publish.add_argument("source", type=Path)
    publish.add_argument("destination", type=Path)
    publish_file_parser = commands.add_parser("publish-file", help="flush and publish an immutable file")
    publish_file_parser.add_argument("source", type=Path)
    publish_file_parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "write":
            atomic_write(args.destination, sys.stdin.buffer.read())
        elif args.command == "publish-file":
            publish_file(args.source, args.destination)
        else:
            if args.destination.exists() or args.destination.is_symlink():
                raise ValueError("publication destination already exists")
            sync_tree(args.source)
            args.source.rename(args.destination)
            sync_directory(args.destination.parent)
            if args.source.parent != args.destination.parent:
                sync_directory(args.source.parent)
    except (OSError, ValueError) as error:
        print(f"harbor-db-durable: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
