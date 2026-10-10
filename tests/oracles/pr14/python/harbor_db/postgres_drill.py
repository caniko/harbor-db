"""Restore a logical dump into a private disposable Unix-socket-only cluster."""

import argparse
import os
import subprocess
import sys
from pathlib import Path


def run(package, program, arguments):
    result = subprocess.run([str(package / "bin" / program), *map(str, arguments)],
                            env={key: value for key, value in os.environ.items() if not key.startswith("PG")},
                            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=120)
    if result.returncode:
        raise ValueError(f"disposable PostgreSQL {program} failed")


def operate(package, command, backup, workspace, dump="database.dump"):
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
            run(package, "pg_ctl", ["-D", cluster, "-m", "fast", "-w", "stop"])
        return
    if command != "restore":
        raise ValueError("unsupported disposable restore command")
    if cluster.exists() or socket.exists():
        raise ValueError("disposable restore requires a new workspace")
    socket.mkdir(mode=0o700)
    run(package, "initdb", ["-D", cluster, "--locale=C", "--encoding=UTF8", "--auth=trust"])
    try:
        run(package, "pg_ctl", ["-D", cluster, "-l", "/dev/null", "-o",
                               f"-k {socket} -p 55439 -c listen_addresses=", "-w", "start"])
        endpoint = ["-h", socket, "-p", "55439"]
        run(package, "createdb", [*endpoint, "harbor_restore"])
        run(package, "pg_restore", ["--exit-on-error", "--no-owner", "--no-acl", *endpoint,
                                    "-d", "harbor_restore", backup / dump])
    except BaseException:
        operate(package, "cleanup", backup, workspace, dump)
        raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path, required=True)
    parser.add_argument("--dump", default="database.dump")
    parser.add_argument("command", choices=("restore", "cleanup"))
    parser.add_argument("backup", type=Path)
    parser.add_argument("workspace", type=Path)
    args = parser.parse_args()
    try:
        operate(args.package, args.command, args.backup, args.workspace, args.dump)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(f"harbor-db-postgres-drill: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
