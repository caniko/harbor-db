"""Persistent authority for consumer-selected storage roots and backend binding."""

import argparse
import contextlib
import json
import os
import re
import subprocess
import sys
from pathlib import Path

from .durable import lock, read_json, write_json
from .postgres import require_mounts


class AuthorityError(ValueError):
    """Selected storage cannot satisfy its adopted authority contract."""


def state_directory(config):
    require_mounts(config)
    state = Path(config["state_dir"])
    if not state.is_absolute() or not state.is_dir() or str(state.resolve()) != str(state):
        raise AuthorityError(f"authority directory is missing or redirected: {state}")
    return state


def contract(config):
    require_mounts(config)
    if not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", config["resource"]):
        raise AuthorityError("invalid resource name")
    state = state_directory(config)
    consumer = {}
    if config.get("consumer_command"):
        result = subprocess.run(config["consumer_command"], capture_output=True, text=True, check=False)
        if result.returncode:
            raise AuthorityError(f"consumer storage validation failed: {result.stderr.strip()}")
        consumer = json.loads(result.stdout)
        if (not isinstance(consumer, dict)
                or set(consumer) - {"binding", "directories", "required_files", "minimum_counters"}
                or not {"binding", "directories", "required_files"} <= set(consumer)):
            raise AuthorityError("invalid consumer storage contract")
        if not isinstance(consumer["binding"], dict):
            raise AuthorityError("invalid consumer storage binding")
    directories = sorted(set(config["directories"] + consumer.get("directories", [])))
    if not directories:
        raise AuthorityError("authority requires at least one storage directory")
    for path in [state, *map(Path, directories)]:
        if not path.is_absolute() or not path.is_dir() or str(path.resolve()) != str(path):
            raise AuthorityError(f"storage directory is missing or redirected: {path}")
    required_files = sorted(set(config.get("required_files", []) + consumer.get("required_files", [])))
    require_files(required_files)
    counters = consumer.get("minimum_counters", {})
    if (not isinstance(counters, dict) or any(
            not isinstance(key, str) or type(value) is not int or value < 0
            for key, value in counters.items())):
        raise AuthorityError("invalid consumer storage counters")
    if any(state == Path(path) or state.is_relative_to(path) for path in directories):
        raise AuthorityError("authority state must be outside the guarded directories")
    return {
        "version": 1, "resource": config["resource"],
        "binding": {**config["binding"], **({"consumer": consumer["binding"]} if consumer else {})},
        "directories": directories,
        "required_files": required_files,
        **({"minimum_counters": counters} if counters else {}),
    }


def require_files(files):
    for path in map(Path, files):
        if (not path.is_absolute() or not path.is_file()
                or str(path.resolve()) != str(path) or path.stat().st_size == 0):
            raise AuthorityError(f"required storage file is missing, empty or redirected: {path}")


def anchor(config, directory):
    return Path(directory) / f".harbor-db-{config['resource']}-identity.json"


def verify(config, expected):
    path = Path(config["state_dir"]) / "identity.json"
    try:
        record = read_json(path)
        if any(record[key] != expected[key] for key in ("version", "resource", "binding", "directories")):
            raise AuthorityError("storage authority mismatch: backend, schema or paths changed")
        # Files present at adoption remain mandatory, while subsequent saves can
        # add files and monotonic consumer evidence without changing the binding.
        require_files(record["required_files"])
        observed = expected.get("minimum_counters", {})
        if any(observed.get(key, -1) < value for key, value in record.get("minimum_counters", {}).items()):
            raise AuthorityError("consumer storage is older or incomplete compared with adoption")
        marker = {"resource": config["resource"], "identity": record["identity"]}
        for directory in expected["directories"]:
            if read_json(anchor(config, directory)) != marker:
                raise AuthorityError(f"storage identity mismatch at {directory}")
    except FileNotFoundError as error:
        raise AuthorityError("adopted storage identity is missing; refusing initialization") from error
    return record


def check(config):
    with inspection(config):
        pass


def require_stable(config):
    path = Path(config["state_dir"]) / "transition.json"
    if path.exists() and read_json(path)["phase"] not in ("planned", "write-enabled", "complete", "aborted"):
        raise AuthorityError("application backend transition is unfinished; ordinary startup is inhibited")


@contextlib.contextmanager
def inspection(config):
    """Retain existing authority while an additional consumer guard inspects it."""
    state = state_directory(config)
    if not (state / "identity.json").exists():
        raise AuthorityError("storage is not adopted; explicit adoption is required")
    with lock(state / "lock", shared=True):
        require_stable(config)
        yield verify(config, contract(config))


def serve(config, argv, *, inspect=None):
    """Exec the consumer with its validated contract and lifetime authority lease."""
    if not argv or not Path(argv[0]).is_absolute():
        raise AuthorityError("consumer executable must be an absolute path")
    with lock(state_directory(config) / "lock", shared=True) as lease:
        require_stable(config)
        verify(config, contract(config))
        if inspect is not None:
            inspect()
        os.set_inheritable(lease, True)
        os.execv(argv[0], argv)


def adopt(config, identifier):
    with adoption(config, identifier) as publish:
        publish()


@contextlib.contextmanager
def adoption(config, identifier):
    """Keep writers excluded through independent proof and authority publication."""
    if not identifier or len(identifier) > 128:
        raise AuthorityError("a verified nonempty storage identifier is required")
    state = state_directory(config)
    # A registered resource must retain its original lock inode, even if a
    # writer still holds that inode after its pathname has disappeared.
    with lock(state / "lock", create=not (state / "identity.json").exists()):
        yield lambda: publish_adoption(config, identifier)


def publish_adoption(config, identifier):
    """Called only by the publisher yielded under the existing adoption lease."""
    state = state_directory(config)
    expected = contract(config)
    if (state / "identity.json").exists():
        if verify(config, expected)["identity"] != identifier:
            raise AuthorityError("cannot replace adopted storage identity")
        return
    marker = {"resource": config["resource"], "identity": identifier}
    for directory in expected["directories"]:
        path = anchor(config, directory)
        if path.exists() and read_json(path) != marker:
            raise AuthorityError(f"existing storage identity differs at {directory}")
    # Publish the authority only after every guarded root has a durable marker.
    # Retrying interrupted adoption is safe with the same verified identifier.
    for directory in expected["directories"]:
        write_json(anchor(config, directory), marker)
    write_json(state / "identity.json", {**expected, "identity": identifier})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check", help="read-only storage authority check")
    serving = commands.add_parser("serve", help="exec a guarded consumer retaining the authority lease")
    serving.add_argument("argv", nargs=argparse.REMAINDER)
    adoption = commands.add_parser("adopt", help="explicitly adopt verified existing storage")
    adoption.add_argument("--identity", required=True)
    args = parser.parse_args()
    try:
        config = json.loads(args.config.read_text())
        if args.command == "check":
            check(config)
        elif args.command == "serve":
            argv = args.argv[1:] if args.argv[:1] == ["--"] else args.argv
            serve(config, argv)
        else:
            adopt(config, args.identity)
    except (RuntimeError, OSError, ValueError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print(f"harbor-db-resource: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
