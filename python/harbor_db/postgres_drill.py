"""Restore a logical dump into a private disposable Unix-socket-only cluster."""

import argparse
import os
import subprocess
import sys
import time
from pathlib import Path

from . import process


def run(package, program, arguments, deadline):
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise ValueError("disposable restore exceeded its execution limit")
    leases = tuple(int(value) for value in os.environ.get("HARBOR_DB_LEASE_FDS", "").split(",") if value)
    if any(value < 3 for value in leases):
        raise ValueError("invalid inherited Harbor DB lease")
    for descriptor in leases:
        os.fstat(descriptor)
    try:
        process.execute([str(package / "bin" / program), *map(str, arguments)],
                        timeout=remaining, leases=leases,
                        environment={key: value for key, value in os.environ.items() if not key.startswith("PG")})
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        raise ValueError(f"disposable PostgreSQL {program} failed") from error


def operate(package, command, backup, workspace, dump="database.dump", *, timeout_seconds=None):
    if timeout_seconds is None:
        timeout_seconds = int(os.environ.get("HARBOR_DB_APPLICATION_TIMEOUT_SECONDS", "1800"))
    if type(timeout_seconds) is not int or not 1 <= timeout_seconds <= 86400:
        raise ValueError("invalid disposable restore timeout")
    deadline = time.monotonic() + timeout_seconds
    package, backup, workspace = map(Path, (package, backup, workspace))
    if any(not path.is_absolute() or path.resolve() != path for path in (workspace, backup)):
        raise ValueError("disposable restore paths must be absolute and unredirected")
    if workspace.stat().st_uid != os.getuid() or workspace.stat().st_mode & 0o077:
        raise ValueError("disposable workspace must be owned and private")
    if Path(dump).name != dump:
        raise ValueError("dump must be a direct backup child")
    cluster, socket = workspace / "cluster", workspace / "socket"
    if command == "cleanup":
        if (cluster / "postmaster.pid").exists():
            run(package, "pg_ctl", ["-D", cluster, "-m", "fast", "-w", "stop"], deadline)
        return
    if command != "restore":
        raise ValueError("unsupported disposable restore command")
    if cluster.exists() or socket.exists():
        raise ValueError("disposable restore requires a new workspace")
    socket.mkdir(mode=0o700)
    try:
        run(package, "initdb", ["-D", cluster, "--locale=C", "--encoding=UTF8", "--auth=trust"], deadline)
        run(package, "pg_ctl", ["-D", cluster, "-l", "/dev/null", "-o",
                               f"-k {socket} -p 55439 -c listen_addresses=", "-w", "start"], deadline)
        endpoint = ["-h", socket, "-p", "55439"]
        run(package, "createdb", [*endpoint, "harbor_restore"], deadline)
        run(package, "pg_restore", ["--exit-on-error", "--no-owner", "--no-acl", *endpoint,
                                    "-d", "harbor_restore", backup / dump], deadline)
    except BaseException:
        operate(package, "cleanup", backup, workspace, dump, timeout_seconds=min(timeout_seconds, 120))
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
        operate(args.package, args.command, args.backup, args.workspace, args.dump, timeout_seconds=args.timeout_seconds)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"harbor-db-postgres-drill: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
