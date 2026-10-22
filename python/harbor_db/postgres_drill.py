"""Restore a logical dump into a private disposable Unix-socket-only cluster."""

import argparse
import os
import select
import signal
import stat
import subprocess
import sys
import time
from pathlib import Path

from . import process
from .durable import sync_directory


def cleanup(cluster, socket, deadline):
    """Inspect a process-bound pidfd; a persisted numeric PID cannot authorize signals."""
    path = cluster / "postmaster.pid"
    try:
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC)
    except FileNotFoundError:
        return
    try:
        info = os.fstat(fd)
        if (
            not stat.S_ISREG(info.st_mode)
            or cluster.resolve() != cluster
            or info.st_uid != os.getuid()
            or info.st_nlink != 1
            or info.st_mode & 0o077
        ):
            raise ValueError(
                "disposable PID carrier must be owned, private and unredirected"
            )
        raw = os.read(fd, 4097)
    finally:
        os.close(fd)
    try:
        lines = raw.decode("utf-8").splitlines()
        pid, started = int(lines[0]), int(lines[2])
        if (
            not 1 < pid < 1 << 31
            or not 0 < started < 1 << 63
            or not lines[0].isascii()
            or not lines[0].isdigit()
            or not lines[2].isascii()
            or not lines[2].isdigit()
        ):
            raise ValueError
    except (ValueError, IndexError) as error:
        raise ValueError("invalid disposable PID carrier") from error
    if (
        len(raw) > 4096
        or len(lines) < 5
        or lines[1] != str(cluster)
        or lines[3] != "55439"
        or lines[4] != str(socket)
    ):
        raise ValueError("disposable PID carrier does not identify its workspace")

    def unchanged():
        current_fd = os.open(
            path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC
        )
        try:
            current = os.fstat(current_fd)
            return (
                stat.S_ISREG(current.st_mode)
                and (current.st_dev, current.st_ino) == (info.st_dev, info.st_ino)
                and os.read(current_fd, 4097) == raw
            )
        finally:
            os.close(current_fd)

    def retire():
        try:
            current = path.lstat()
        except FileNotFoundError:
            sync_directory(cluster)
            return
        if (current.st_dev, current.st_ino) != (
            info.st_dev,
            info.st_ino,
        ) or not unchanged():
            raise ValueError("disposable PID carrier changed during cleanup")
        path.unlink()
        sync_directory(cluster)

    try:
        descriptor = os.pidfd_open(pid)
    except ProcessLookupError:
        retire()
        return
    try:

        def exited():
            return bool(select.select([descriptor], [], [], 0)[0])

        if exited():
            retire()
            return
        try:
            proc = Path("/proc") / str(pid)
            fields = (proc / "stat").read_text().rsplit(") ", 1)[1].split()
            boot = next(
                int(line[6:])
                for line in Path("/proc/stat").read_text().splitlines()
                if line.startswith("btime ")
            )
            frequency = os.sysconf("SC_CLK_TCK")
            executable = Path(os.readlink(proc / "exe"))
            argv = (proc / "cmdline").read_bytes().split(b"\0")
            postgres = executable.name == "postgres" or (
                executable.name.startswith("qemu-")
                and any(Path(os.fsdecode(arg)).name == "postgres" for arg in argv)
            )
            valid = (
                frequency > 0
                and postgres
                and proc.stat().st_uid == info.st_uid
                and os.readlink(proc / "cwd") == str(cluster)
                and int(fields[2]) == pid
                and int(fields[3]) == pid
                and abs(boot + int(fields[19]) // frequency - started) <= 2
            )
        except (OSError, ValueError, IndexError, StopIteration):
            if not exited():
                raise
            valid = False
        if exited():
            retire()
            return
        if not valid:
            raise ValueError(
                "disposable PID references a different process; cleanup refused"
            )
        if not unchanged():
            raise ValueError("disposable PID carrier changed during cleanup")
        try:
            signal.pidfd_send_signal(descriptor, signal.SIGINT)
        except ProcessLookupError:
            pass
        while not exited():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise ValueError("disposable restore exceeded its execution limit")
            select.select([descriptor], [], [], min(remaining, 0.01))
        retire()
    finally:
        os.close(descriptor)


def run(package, program, arguments, deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise ValueError("disposable restore exceeded its execution limit")
    leases = tuple(
        int(value)
        for value in os.environ.get("HARBOR_DB_LEASE_FDS", "").split(",")
        if value
    )
    if any(value < 3 for value in leases):
        raise ValueError("invalid inherited Harbor DB lease")
    for descriptor in leases:
        os.fstat(descriptor)
    try:
        process.execute(
            [str(package / "bin" / program), *map(str, arguments)],
            timeout=remaining,
            leases=leases,
            environment={
                key: value
                for key, value in os.environ.items()
                if not key.startswith("PG")
            },
        )
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        raise ValueError(f"disposable PostgreSQL {program} failed") from error


def operate(
    package, command, backup, workspace, dump="database.dump", *, timeout_seconds=None
):
    if timeout_seconds is None:
        timeout_seconds = int(
            os.environ.get("HARBOR_DB_APPLICATION_TIMEOUT_SECONDS", "1800")
        )
    if type(timeout_seconds) is not int or not 1 <= timeout_seconds <= 86400:
        raise ValueError("invalid disposable restore timeout")
    deadline = time.monotonic() + timeout_seconds
    package, backup, workspace = map(Path, (package, backup, workspace))
    if any(
        not path.is_absolute() or path.resolve() != path for path in (workspace, backup)
    ):
        raise ValueError("disposable restore paths must be absolute and unredirected")
    if workspace.stat().st_uid != os.getuid() or workspace.stat().st_mode & 0o077:
        raise ValueError("disposable workspace must be owned and private")
    if Path(dump).name != dump:
        raise ValueError("dump must be a direct backup child")
    cluster, socket = workspace / "cluster", workspace / "socket"
    if command == "cleanup":
        cleanup(cluster, socket, deadline)
        return
    if command != "restore":
        raise ValueError("unsupported disposable restore command")
    if cluster.exists() or socket.exists():
        raise ValueError("disposable restore requires a new workspace")
    socket.mkdir(mode=0o700)
    try:
        run(
            package,
            "initdb",
            ["-D", cluster, "--locale=C", "--encoding=UTF8", "--auth=trust"],
            deadline,
        )
        run(
            package,
            "pg_ctl",
            [
                "-D",
                cluster,
                "-l",
                "/dev/null",
                "-o",
                f"-k {socket} -p 55439 -c listen_addresses=",
                "-w",
                "start",
            ],
            deadline,
        )
        endpoint = ["-h", socket, "-p", "55439"]
        run(package, "createdb", [*endpoint, "harbor_restore"], deadline)
        run(
            package,
            "pg_restore",
            [
                "--exit-on-error",
                "--no-owner",
                "--no-acl",
                *endpoint,
                "-d",
                "harbor_restore",
                backup / dump,
            ],
            deadline,
        )
    except BaseException:
        operate(
            package,
            "cleanup",
            backup,
            workspace,
            dump,
            timeout_seconds=min(timeout_seconds, 120),
        )
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--dump", default="database.dump")
    parser.add_argument("--timeout-seconds", type=int)
    parser.add_argument("command", choices=("restore", "cleanup"))
    parser.add_argument("backup", type=Path)
    parser.add_argument("workspace", type=Path)
    args = parser.parse_args()
    try:
        operate(
            args.package,
            args.command,
            args.backup,
            args.workspace,
            args.dump,
            timeout_seconds=args.timeout_seconds,
        )
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"harbor-db-postgres-drill: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
