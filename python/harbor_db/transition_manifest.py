"""Immutable intent, executed workers and borrowed fence leases for transitions."""

import contextlib
import json
import os
import pwd
import re
import stat
import subprocess
from pathlib import Path

from . import application_backup, process, resource, startup_inhibition
from .durable import atomic_write, lock, read_json, sync_directory, write_json


def require_root():
    startup_inhibition.require_root()


def candidate_path(candidate, config):
    if not re.fullmatch(r"/nix/store/[0-9a-z]{32}-[^/\s]+", candidate):
        raise ValueError("transition candidate must be an immutable store contract or generation")
    if Path(candidate).is_file():
        if read_json(candidate) != config:
            raise ValueError("immutable preparation contract differs")
        return "contract"
    if not Path(candidate).is_dir():
        raise ValueError("transition candidate is not realized")
    if generation_contract(candidate, config["resource"]) != config:
        raise ValueError("candidate generation does not declare the exact transition contract")
    return "generation"


def generation_contract(candidate, resource_name, *, store_root="/nix/store"):
    """Resolve NixOS's immutable etc links without admitting a runtime contract."""
    path = (Path(candidate) / "etc/harbor-db" / (resource_name + "-transition.json")).resolve(strict=True)
    if not path.is_relative_to(store_root):
        raise ValueError("candidate transition contract escapes the immutable store")
    return read_json(path)


def generation():
    return str(Path("/run/current-system").resolve())


def validate(config):
    required = {"version", "resource", "source_manifest", "target_manifest", "barrier_dir", "drop_in_root",
                "systemctl", "busctl", "units", "retired_units", "timeout_seconds", "commands", "executable_files",
                "postgres_manifest", "postgres_socket", "postgres_port", "custody_manifest", "backup_manifest", "independent_receipt", "storage_package", "runuser"}
    if set(config) != required or config["version"] != 1:
        raise ValueError("unsupported application transition manifest")
    source, target = map(read_json, (config["source_manifest"], config["target_manifest"]))
    if (source["resource"] != config["resource"] or target["resource"] != config["resource"]
            or source["state_dir"] != target["state_dir"] or source["binding"] == target["binding"]):
        raise ValueError("backend transitions require one authority and different source/target bindings")
    if ("postgresql" in (source["binding"].get("backend"), target["binding"].get("backend"))
            or "postgres" in (source["binding"].get("backend"), target["binding"].get("backend"))) and config["postgres_manifest"] is None:
        raise ValueError("PostgreSQL backend transitions require the existing writer fence")
    for key in ("source_manifest", "target_manifest", "barrier_dir", "drop_in_root", "systemctl", "busctl",
                "backup_manifest", "independent_receipt", "storage_package", "runuser"):
        path = Path(config[key])
        if not path.is_absolute() or any(c.isspace() or c in "\\%" for c in str(path)):
            raise ValueError("transition paths must be explicit, absolute and systemd-safe")
    barrier, authority = Path(config["barrier_dir"]), Path(source["state_dir"])
    if barrier.resolve() != barrier or Path(config["drop_in_root"]).resolve() != Path(config["drop_in_root"]):
        raise ValueError("transition barrier storage is redirected")
    startup_inhibition.owned_ancestors(barrier)
    if barrier.exists():
        startup_inhibition.owned(barrier, directory=True)
    if barrier == authority or barrier.is_relative_to(authority) or authority.is_relative_to(barrier):
        raise ValueError("root startup barriers must be outside application-owned authority")
    if any(barrier == Path(path) or barrier.is_relative_to(path) for path in source["directories"] + target["directories"]):
        raise ValueError("startup barriers must be outside the guarded storage")
    units = config["units"] + config["retired_units"]
    if (not units or len(set(units)) != len(units)
            or any(not re.fullmatch(r"[A-Za-z0-9_@.:-]+\.service", unit) for unit in units)
            or any(unit in ("postgresql.service", "postgresql-setup.service") for unit in units)):
        raise ValueError("application transition units must be unique clients, not PostgreSQL control services")
    if type(config["timeout_seconds"]) is not int or not 0 < config["timeout_seconds"] <= 86400:
        raise ValueError("transition execution limit must be bounded")
    if not Path(config["postgres_socket"]).is_absolute() or type(config["postgres_port"]) is not int or not 1 <= config["postgres_port"] <= 65535:
        raise ValueError("transition requires an explicit local PostgreSQL endpoint")
    if set(config["commands"]) != {"import", "verify-target", "verify-source", "health"}:
        raise ValueError("transition requires import, complete parity, source validation and health commands")
    for command in config["commands"].values():
        if (set(command) != {"user", "argv"} or not isinstance(command["argv"], list) or not command["argv"]
                or not all(isinstance(arg, str) for arg in command["argv"]) or not Path(command["argv"][0]).is_absolute()):
            raise ValueError("transition workers require a declared user and absolute argv")
        pwd.getpwnam(command["user"])
    return source, target


def tools(config):
    files = config["executable_files"] + [c["argv"][0] for c in config["commands"].values()]
    files += [config["systemctl"], config["busctl"], config["runuser"],
              str(Path(config["storage_package"]) / "harbor-db-application-backup")]
    if any(not Path(path).is_absolute() or not Path(path).is_file() for path in files):
        raise ValueError("transition executable is absent")
    return {path: application_backup.digest(path) for path in sorted(set(files))}


def intent(config):
    source, target = validate(config)
    return {"manifest_sha256": application_backup.identity(config),
            "source_sha256": application_backup.digest(config["source_manifest"]),
            "target_sha256": application_backup.digest(config["target_manifest"]),
            "backup_sha256": application_backup.digest(config["backup_manifest"]),
            "custody_sha256": None if config["custody_manifest"] is None else application_backup.digest(config["custody_manifest"]),
            "postgres_sha256": None if config["postgres_manifest"] is None else application_backup.digest(config["postgres_manifest"]),
            "executables": tools(config)}


def write_owned(path, value):
    """Keep authority readable by its established application account."""
    path = Path(path)
    info = path.parent.stat()
    write_json(path, value)
    os.chown(path, info.st_uid, info.st_gid)
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def install_barriers(config, record):
    require_root()
    state = Path(config["barrier_dir"])
    startup_inhibition.owned_ancestors(state)
    startup_inhibition.provision_directory(state, 0o700)
    retired = state / "retired"
    if config["retired_units"]:
        startup_inhibition.provision_directory(retired, 0o700)
    write_json(state / "start-policy.json", {"version": 1, "generation": record["source_generation"],
               "authority_manifest": config["source_manifest"], "units": config["units"] + config["retired_units"]})
    for unit in config["units"] + config["retired_units"]:
        selected = retired if unit in config["retired_units"] else state
        path = Path(config["drop_in_root"]) / (unit + ".d") / "zzzz-harbor-db-backend-transition.conf"
        startup_inhibition.owned_ancestors(path)
        startup_inhibition.provision_directory(path.parent, 0o755)
        if path.exists() or path.is_symlink():
            check_transition_drop_in(config, path, selected, unit)
        else:
            atomic_write(path, transition_content(config, selected, unit))
    if (state / "inhibited.json").exists() and read_json(state / "inhibited.json")["intent"] != record["intent"]:
        raise ValueError("another transition owns the persistent startup barrier")
    write_json(state / "inhibited.json", {"intent": record["intent"], "candidate": record["barrier_candidate"]})
    if config["retired_units"]:
        write_json(retired / "inhibited.json", {"resource": config["resource"], "units": config["retired_units"]})
    subprocess.run([config["systemctl"], "daemon-reload"], check=True, timeout=30)
    inspect_barriers(config)


def inspect_barriers(config):
    state = Path(config["barrier_dir"])
    startup_inhibition.owned(state, directory=True)
    if not (state / "inhibited.json").exists():
        raise ValueError("transition startup barrier is absent")
    for unit in config["units"] + config["retired_units"]:
        selected = state / "retired" if unit in config["retired_units"] else state
        path = Path(config["drop_in_root"]) / (unit + ".d") / "zzzz-harbor-db-backend-transition.conf"
        check_transition_drop_in(config, path, selected, unit)
        startup_inhibition.loaded(config, path, selected, unit)


def transition_content(config, selected, unit):
    checker = Path(config["storage_package"]) / "harbor-db-transition-start"
    return startup_inhibition.content(selected) + (f"\n[Service]\nExecCondition=+{checker} --state {config['barrier_dir']} --unit {unit}\n").encode()


def check_transition_drop_in(config, path, selected, unit):
    startup_inhibition.owned(path)
    if path.read_bytes() != transition_content(config, selected, unit):
        raise ValueError("persistent transition startup policy changed; preserve foreign policy")


def startup_unit(state, unit):
    """Root-owned generation policy also gates legacy units lacking a wrapper."""
    require_root()
    state = Path(state)
    startup_inhibition.owned_ancestors(state)
    startup_inhibition.owned(state, directory=True)
    startup_inhibition.owned(state / "start-policy.json")
    policy = read_json(state / "start-policy.json")
    if policy["version"] != 1 or unit not in policy["units"] or generation() != policy["generation"]:
        raise ValueError("ordinary writer startup requires the explicitly released generation")
    if (state / "inhibited.json").exists():
        raise ValueError("ordinary writer startup is inhibited by a pending transition")
    resource.check(read_json(policy["authority_manifest"]))


def release_barriers(config, record, *, aborted=False):
    state = Path(config["barrier_dir"])
    write_json(state / "start-policy.json", {"version": 1,
               "generation": record["source_generation"] if aborted else record["candidate"],
               "authority_manifest": config["source_manifest"] if aborted else config["target_manifest"],
               "units": config["units"] + (config["retired_units"] if aborted else [])})
    paths = [state / "inhibited.json"]
    if aborted and config["retired_units"]:
        paths.append(state / "retired/inhibited.json")
    for path in paths:
        if path.exists():
            startup_inhibition.owned(path)
            marker = read_json(path)
            expected = ({"resource": config["resource"], "units": config["retired_units"]}
                        if path.parent.name == "retired" else {"intent": record["intent"], "candidate": record["barrier_candidate"]})
            if marker != expected:
                raise ValueError("another transition owns the startup barrier")
            path.unlink()
            sync_directory(path.parent)


def startup_main():
    import argparse
    import sys
    parser = argparse.ArgumentParser(description=startup_unit.__doc__)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--unit", required=True)
    args = parser.parse_args()
    try:
        startup_unit(args.state, args.unit)
    except (OSError, ValueError, KeyError, TypeError, RuntimeError) as error:
        print(f"harbor-db-transition-start: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    import sys
    sys.exit(startup_main())


def stop_units(config):
    units = config["units"] + config["retired_units"]
    subprocess.run([config["systemctl"], "stop", *units], check=True, timeout=config["timeout_seconds"])
    for unit in units:
        result = subprocess.run([config["systemctl"], "show", unit, "--property=ActiveState", "--value"],
                                capture_output=True, text=True, check=True, timeout=30)
        if result.stdout.strip() not in ("inactive", "failed"):
            raise ValueError("an application writer has not stopped")


def worker(config, command, substitutions, leases):
    account = pwd.getpwnam(command["user"])
    argv = [substitutions.get(arg, arg) for arg in command["argv"]]
    kwargs = {"user": account.pw_uid, "group": account.pw_gid,
              "extra_groups": os.getgrouplist(account.pw_name, account.pw_gid)} if os.geteuid() == 0 else {}
    if os.geteuid() != 0 and os.getuid() != account.pw_uid:
        raise ValueError("transition worker identity differs")
    environment = {key: value for key, value in os.environ.items() if key in {"PATH", "HOME", "LANG", "TMPDIR"}}
    environment["HARBOR_DB_LEASE_FDS"] = ",".join(map(str, leases))
    data = process.execute(argv, timeout=config["timeout_seconds"], environment=environment, leases=leases, **kwargs)
    return json.loads(data) if data.strip() else None


@contextlib.contextmanager
def fence(config, record, leases):
    if config["postgres_manifest"] is None:
        yield leases
        return
    database = read_json(config["postgres_manifest"])
    anchor = Path(database["state_dir"]) / "writer-fence.lock"
    with lock(anchor, shared=True) as lease:
        info = os.fstat(lease)
        if info.st_uid != pwd.getpwnam("postgres").pw_uid or info.st_mode & 0o022 or not stat.S_ISREG(info.st_mode):
            raise ValueError("PostgreSQL writer fence anchor is not service-owned")
        observed = worker(config, {"user": "postgres", "argv": [str(Path(config["storage_package"]) / "harbor-db-postgres"),
                          "--config", config["postgres_manifest"], "inspect-fence", "--token", record["writer_fence_token"],
                          "--socket-dir", config["postgres_socket"], "--port", str(config["postgres_port"])]}, {}, [*leases, lease])
        if observed["status"] != "ready" or observed["token"] != record["writer_fence_token"]:
            raise ValueError("PostgreSQL live fence token differs")
        if record.get("fence") is not None and record["fence"] != observed:
            raise ValueError("PostgreSQL fence primary or HBA identity changed")
        record["fence"] = observed
        recovery = database.get("recovery")
        if recovery is None or not recovery.get("require_writer_fence", False):
            raise ValueError("PostgreSQL transitions require token-bound whole-primary recovery acceptance")
        with lock(Path(recovery["backup_root"]) / "locks/mutate", shared=True) as backup_lease, lock(Path(recovery["snapshot_file"]).parent / "recovery.lock", shared=True) as evidence_lease:
            yield [*leases, lease, backup_lease, evidence_lease]
