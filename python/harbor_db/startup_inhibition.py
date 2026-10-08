"""Root-owned systemd startup barrier; release never starts or thaws PostgreSQL."""

import contextlib
import fcntl
import json
import os
import pwd
import re
import secrets
import shlex
import stat
from pathlib import Path

from . import postgres
from .durable import atomic_write, lock, sync_directory, write_json


def require_root():
    if os.geteuid() != 0:
        raise postgres.LifecycleError("persistent startup inhibition requires root")


def service_uid():
    return pwd.getpwnam("postgres").pw_uid


@contextlib.contextmanager
def fence_boundary_lock(config):
    """Pin the service-owned offline boundary through the root gate commit."""
    descriptor = os.open(Path(config["state_dir"]) / "writer-fence.lock", os.O_RDONLY | os.O_NOFOLLOW)
    try:
        info = os.fstat(descriptor)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != service_uid() or info.st_mode & 0o022:
            raise postgres.LifecycleError("startup release requires the retained service-owned fence anchor")
        # Inspection takes the same shared lock in the child. Offline prepare /
        # thaw take it exclusively and cannot change the boundary after readback.
        fcntl.flock(descriptor, fcntl.LOCK_SH | fcntl.LOCK_NB)
        yield
    finally:
        os.close(descriptor)


def owned(path, *, directory=False):
    info = path.lstat()
    kind = stat.S_ISDIR if directory else stat.S_ISREG
    if not kind(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o022:
        raise postgres.LifecycleError("startup inhibition requires owned non-writable storage")


def owned_ancestors(path):
    for parent in path.parents:
        if parent.exists():
            info = parent.lstat()
            # A root-owned sticky ancestor cannot rename the already checked
            # trusted child. Shared non-sticky storage can replace that child.
            sticky_root = info.st_uid == 0 and bool(info.st_mode & stat.S_ISVTX)
            if (not stat.S_ISDIR(info.st_mode) or info.st_uid not in (0, os.geteuid())
                    or (info.st_mode & 0o022 and not sticky_root)):
                raise postgres.LifecycleError("startup inhibition storage has an untrusted ancestor")


def settings(config):
    postgres.validate_config(config)
    policy = config["startup_inhibition"]
    setup_units = policy.get("setup_units", [])
    if not isinstance(setup_units, list):
        raise postgres.LifecycleError("invalid startup inhibition service units")
    units = [policy["unit"], *setup_units]
    if (any(not isinstance(unit, str) or not re.fullmatch(r"[a-zA-Z0-9_-]+\.service", unit)
            for unit in units) or len(set(units)) != len(units)):
        raise postgres.LifecycleError("invalid startup inhibition service unit")
    for key in ("state_dir", "drop_in_root", "systemctl", "busctl", "runuser", "adapter"):
        path = Path(policy[key])
        if (not path.is_absolute()
                or any(character.isspace() or character in "\\%" for character in str(path))):
            raise postgres.LifecycleError("unsafe startup inhibition path")
        if key in ("state_dir", "drop_in_root"):
            if path.resolve() != path:
                raise postgres.LifecycleError("redirected startup inhibition storage")
            owned_ancestors(path)
    state = Path(policy["state_dir"])
    for protected in (Path(config["data_dir"]), Path(config["state_dir"])):
        if state == protected or state.is_relative_to(protected) or protected.is_relative_to(state):
            raise postgres.LifecycleError("startup inhibition must have separate root-owned storage")
    return policy, state, Path(policy["drop_in_root"]) / (policy["unit"] + ".d") / "zzzz-harbor-db-startup-inhibition.conf"


def drop_ins(policy, primary):
    return [(policy["unit"], primary), *[
        (unit, Path(policy["drop_in_root"]) / (unit + ".d") / primary.name)
        for unit in policy.get("setup_units", [])
    ]]


def content(state):
    return ("# Harbor DB: retained across legacy and guarded generations.\n"
            f"[Unit]\nConditionPathExists=!{state / 'inhibited.json'}\n").encode()


def binding(config, policy, identifier):
    return {"version": 1, "resource": config["resource"], "data_dir": config["data_dir"],
            "major": str(config["major"]), "system_identifier": identifier, "policy": policy}


def record(config, policy, state, identifier):
    path = state / "inhibited.json"
    owned(path)
    value = json.loads(path.read_text())
    expected = binding(config, policy, identifier)
    if (not isinstance(value, dict) or any(value.get(key) != item for key, item in expected.items())
            or not isinstance(value.get("token"), str) or not re.fullmatch(r"[0-9a-f]{32}", value["token"])):
        raise postgres.LifecycleError("startup inhibition binding changed")
    return value


def loaded(policy, drop_in, state, unit=None):
    unit = policy["unit"] if unit is None else unit
    result = postgres.run([policy["systemctl"], "show", unit, "--property=DropInPaths", "--value"],
                          capture_output=True, text=True, timeout=30)
    if str(drop_in) not in shlex.split(result.stdout):
        raise postgres.LifecycleError("startup inhibition drop-in is not loaded by systemd")
    escaped = "".join(character if character.isascii() and character.isalnum() else f"_{ord(character):02x}" for character in unit)
    result = postgres.run([policy["busctl"], "--json=short", "get-property", "org.freedesktop.systemd1",
                           "/org/freedesktop/systemd1/unit/" + escaped, "org.freedesktop.systemd1.Unit", "Conditions"],
                          capture_output=True, text=True, timeout=30)
    observed = json.loads(result.stdout)
    required = ["ConditionPathExists", False, True, str(state / "inhibited.json")]
    if (observed.get("type") != "a(sbbsi)" or not isinstance(observed.get("data"), list)
            or not any(isinstance(condition, list) and condition[:4] == required for condition in observed["data"])):
        raise postgres.LifecycleError("systemd's effective startup inhibition condition is absent")


def check_drop_in(drop_in, state):
    owned(drop_in)
    if drop_in.read_bytes() != content(state):
        raise postgres.LifecycleError("startup inhibition drop-in differs; preserve foreign policy")


def provision_directory(path, mode):
    missing = []
    for parent in [path, *path.parents]:
        if parent.exists():
            break
        missing.append(parent)
    path.mkdir(mode=mode, parents=True, exist_ok=True)
    owned(path, directory=True)
    # Each newly created component needs its own parent entry flushed; syncing
    # only the leaf would lose a first-rollout barrier on an abrupt reboot.
    for parent in missing:
        sync_directory(parent)
        sync_directory(parent.parent)


def inhibit(config, identifier):
    require_root()
    policy, state, drop_in = settings(config)
    if (not re.fullmatch(r"[1-9][0-9]*", identifier)
            or postgres.inspect_cluster(config["package"], config["data_dir"], config["major"]) != identifier):
        raise postgres.LifecycleError("startup inhibition identifier differs")
    provision_directory(state, 0o700)
    with lock(state / "lock", create=True):
        # Publish the persistent drop-in FIRST. A reboot after the later marker
        # commit must already see it, even before this manager has reloaded.
        for _, barrier in drop_ins(policy, drop_in):
            provision_directory(barrier.parent, 0o755)
            owned(barrier.parent.parent, directory=True)
            if os.path.lexists(barrier):
                check_drop_in(barrier, state)
            else:
                atomic_write(barrier, content(state))
        if os.path.lexists(state / "inhibited.json"):
            held = record(config, policy, state, identifier)
        else:
            held = {**binding(config, policy, identifier), "token": secrets.token_hex(16)}
            write_json(state / "inhibited.json", held)
        postgres.run([policy["systemctl"], "daemon-reload"], timeout=30)
        for unit, barrier in drop_ins(policy, drop_in):
            loaded(policy, barrier, state, unit)
        return {"status": "startup-inhibited", "token": held["token"], "unit": policy["unit"]}


def release(config, manifest, token, fence_token, phase):
    require_root()
    policy, state, drop_in = settings(config)
    owned(state, directory=True)
    with lock(state / "lock"), fence_boundary_lock(config):
        identifier = postgres.inspect_cluster(config["package"], config["data_dir"], config["major"])
        held = record(config, policy, state, identifier)
        if held["token"] != token:
            raise postgres.LifecycleError("startup inhibition token differs")
        if phase not in ("prepared", "closed"):
            raise postgres.LifecycleError("unknown startup release boundary")
        for unit, barrier in drop_ins(policy, drop_in):
            check_drop_in(barrier, state)
            loaded(policy, barrier, state, unit)
        # The service identity verifies stoppedness and its own private journal,
        # hashes, receipt and cluster. Root never writes PostgreSQL-owned history.
        result = postgres.run([policy["runuser"], "-u", "postgres", "--", policy["adapter"],
                               "--config", str(manifest), "inspect-offline-fence", "--token", fence_token,
                               "--phase", phase], capture_output=True, text=True, timeout=30)
        boundary = json.loads(result.stdout)
        expected = {"status": f"{phase}-offline", "token": fence_token, "resource": config["resource"],
                    "data_dir": config["data_dir"], "major": str(config["major"]), "system_identifier": identifier}
        if boundary != expected:
            raise postgres.LifecycleError("startup release requires the exact bound offline fence boundary")
        receipt = state / (held["token"] + ".released.json")
        write_json(receipt, {"inhibition": held, "boundary": boundary})
        (state / "inhibited.json").unlink()
        sync_directory(state)
        # The empty condition remains installed; neither daemon-reload nor a
        # service start is needed. Starting the primary is a separate operation.
        return {"status": "startup-released", "token": token, "receipt": str(receipt)}
