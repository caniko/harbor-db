"""Root-coordinated copies isolate accepted bytes from worker-owned inodes."""

import os
import pwd
import re
import shutil
import stat
from collections import deque
from pathlib import Path


def require_namespace(root):
    if os.geteuid() != 0:
        raise ValueError("protected backup publication requires a root coordinator")
    for index, path in enumerate([root, *root.parents]):
        info = path.lstat()
        sticky = index != 0 and info.st_mode & stat.S_ISVTX
        if (
            not stat.S_ISDIR(info.st_mode)
            or info.st_uid != 0
            or (info.st_mode & 0o022 and not sticky)
        ):
            raise ValueError(
                "protected backup namespace has untrusted ownership or permissions"
            )
    info = (root / "lock").lstat()
    if (
        not stat.S_ISREG(info.st_mode)
        or info.st_uid != 0
        or info.st_nlink != 1
        or info.st_mode & 0o022
    ):
        raise ValueError("protected backup lease is not root-owned")


def worker_directory(path, account):
    path.mkdir(mode=0o700)
    os.chown(path, account.pw_uid, account.pw_gid)


def validate_credentials(sources):
    if not isinstance(sources, dict):
        raise ValueError("invalid backup credential sources")
    for name, path in sources.items():
        if not re.fullmatch(r"[A-Za-z0-9_.-]{1,255}", name) or name in (".", ".."):
            raise ValueError("invalid backup credential name")
        if (
            not isinstance(path, str)
            or not Path(path).is_absolute()
            or Path(path).is_relative_to("/nix/store")
        ):
            raise ValueError(
                "backup credentials require runtime paths outside the store"
            )


def credential_source(path):
    """Open one exact trusted inode after descriptor-relative alias admission."""
    path = Path(path)
    if not path.is_absolute():
        raise ValueError("credential source must be absolute")
    current = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    pending = deque(path.parts[1:])
    links = 0
    try:
        while pending:
            parent = os.fstat(current)
            if (
                not stat.S_ISDIR(parent.st_mode)
                or parent.st_uid != 0
                or parent.st_mode & 0o022
            ):
                raise ValueError("untrusted credential source ancestor")
            selected = os.open(
                pending.popleft(),
                os.O_PATH | os.O_NOFOLLOW | os.O_CLOEXEC,
                dir_fd=current,
            )
            try:
                info = os.fstat(selected)
                if info.st_uid != 0 or (
                    not stat.S_ISLNK(info.st_mode) and info.st_mode & 0o022
                ):
                    raise ValueError(
                        "untrusted credential source ownership or permissions"
                    )
                if stat.S_ISLNK(info.st_mode):
                    links += 1
                    if links > 40:
                        raise ValueError("credential source has too many alias hops")
                    target = Path(os.readlink("", dir_fd=selected))
                    if target.is_absolute():
                        replacement = os.open(
                            "/",
                            os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                        )
                        os.close(current)
                        current = replacement
                    # A relative target keeps current at the admitted parent;
                    # selected is only the pinned symlink for readlink.
                    pending.extendleft(
                        reversed(
                            target.parts[1:] if target.is_absolute() else target.parts
                        )
                    )
                else:
                    os.close(current)
                    current, selected = selected, None
            finally:
                if selected is not None:
                    os.close(selected)
        info = os.fstat(current)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
            raise ValueError("credential source must be a trusted regular file")
        descriptor = os.open(
            f"/proc/self/fd/{current}", os.O_RDONLY | os.O_NONBLOCK | os.O_CLOEXEC
        )
        return os.fdopen(descriptor, "rb")
    finally:
        os.close(current)


def credential_inputs(sources):
    inherited = os.environ.get("CREDENTIALS_DIRECTORY")
    if sources is not None:
        validate_credentials(sources)
        inputs = [
            (name, Path(inherited) / name if inherited else Path(path))
            for name, path in sources.items()
        ]
    else:
        inputs = (
            [(path.name, path) for path in Path(inherited).iterdir()]
            if inherited
            else []
        )
    opened = []
    try:
        for name, path in inputs:
            stream = credential_source(path)
            opened.append((name, stream))
            if os.fstat(stream.fileno()).st_size > 1 << 20:
                raise ValueError("backup credential exceeds the byte limit")
        return opened
    except BaseException:
        for _, stream in opened:
            stream.close()
        raise


def check_credential_sources(sources):
    for _, stream in credential_inputs(sources):
        stream.close()


class Worker:
    def __init__(self, user, credentials, sources=None):
        self.account = pwd.getpwnam(user)
        if self.account.pw_uid == 0:
            raise ValueError("backup adapters require an unprivileged worker account")
        self.credentials = None
        opened = credential_inputs(sources)
        if opened:
            try:
                credentials.mkdir(mode=0o700)
                self.credentials = credentials
                for name, input_file in opened:
                    with input_file:
                        if not stat.S_ISREG(os.fstat(input_file.fileno()).st_mode):
                            raise ValueError("credential source is not regular")
                        with (credentials / name).open("xb") as output:
                            data = input_file.read((1 << 20) + 1)
                            if len(data) > 1 << 20:
                                raise ValueError(
                                    "backup credential exceeds the byte limit"
                                )
                            output.write(data)
                            os.fchmod(output.fileno(), 0o400)
                            os.fchown(
                                output.fileno(),
                                self.account.pw_uid,
                                self.account.pw_gid,
                            )
                os.chown(credentials, self.account.pw_uid, self.account.pw_gid)
            except BaseException:
                self.close()
                raise
            finally:
                for _, stream in opened:
                    stream.close()

    def close(self):
        if self.credentials is not None:
            shutil.rmtree(self.credentials)
            self.credentials = None

    def identity(self):
        return {
            "user": self.account.pw_uid,
            "group": self.account.pw_gid,
            "extra_groups": os.getgrouplist(self.account.pw_name, self.account.pw_gid),
        }


def copy_directory(descriptor, destination):
    for name in os.listdir(descriptor):
        source = os.open(
            name,
            os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC,
            dir_fd=descriptor,
        )
        try:
            info = os.fstat(source)
            target = destination / name
            if stat.S_ISDIR(info.st_mode):
                target.mkdir(mode=0o700)
                copy_directory(source, target)
            elif stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
                with (
                    os.fdopen(os.dup(source), "rb") as input_file,
                    target.open("xb") as output,
                ):
                    while chunk := input_file.read(128 * 1024):
                        output.write(chunk)
            else:
                raise ValueError("backup contains redirected, shared or special files")
        finally:
            os.close(source)


def copy_tree(source, destination):
    descriptor = os.open(
        source, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC
    )
    try:
        destination.mkdir(mode=0o700)
        copy_directory(descriptor, destination)
    finally:
        os.close(descriptor)


def reader_permissions(path, gid):
    for item in path.iterdir():
        if item.is_dir():
            reader_permissions(item, gid)
        else:
            os.chown(item, 0, gid)
            item.chmod(0o440)
    os.chown(path, 0, gid)
    path.chmod(0o750)
