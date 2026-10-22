"""Keep a consumer's authority lease through external SSH commands and Git hooks."""

import os
import pwd
import socket
import sys
from pathlib import Path

from . import cutover, resource
from .durable import read_json


def serve_login_shell(path, argv, *, host):
    manifest = cutover.validate_manifest(read_json(path), host)
    user = pwd.getpwuid(os.geteuid()).pw_name
    selected = [
        entry
        for entry in manifest["resources"].values()
        if entry["kind"] == "filesystem"
        and entry["user"] == user
        and entry.get("login_shell")
    ]
    if len(selected) != 1:
        raise ValueError(
            "login user must select exactly one declared filesystem authority"
        )
    config = selected[0]
    resource.serve(
        config["authority"],
        [config["login_shell"], *argv],
        inspect=lambda: cutover.check_resource(config, phase="startup"),
    )


def main():
    try:
        serve_login_shell(
            Path("/etc/harbor-db/cutover.json"), sys.argv[1:], host=socket.gethostname()
        )
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"harbor-db-cutover-shell: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
