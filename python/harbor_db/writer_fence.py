"""Explicit, restart-persistent PostgreSQL admission fencing.

Opening and closing require a stopped primary. A prepared fence is not live
acceptance: restart, then inspect it before taking any acknowledged snapshot.
Only local peer-authenticated control SQL and physical replication are admitted.
No exception or cancellation path automatically restores application access.
"""

import contextlib
import hashlib
import json
import os
import re
import secrets
import stat
from pathlib import Path

from . import postgres
from .durable import atomic_write, lock, sync_directory, write_json


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_private(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077:
            raise postgres.LifecycleError("writer fence artifacts must be private owned regular files")
        return stream.read()


def read_auto(config):
    path = Path(config["data_dir"]) / "postgresql.auto.conf"
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o022:
            raise postgres.LifecycleError("automatic configuration is not owned regular non-writable storage")
        return stream.read()


def policy(config):
    settings = config.get("writer_fence", {})
    if not isinstance(settings, dict):
        raise postgres.LifecycleError("invalid writer fence policy")
    control = settings.get("control_role", "postgres")
    roles = settings.get("replication_roles", [])
    libraries = settings.get("allowed_preload_libraries", [])
    if not isinstance(roles, list) or any(not isinstance(role, str) for role in roles) or len(roles) != len(set(roles)):
        raise postgres.LifecycleError("invalid writer fence replication role list")
    if any(not isinstance(role, str) or not re.fullmatch(r"[a-z_][a-z0-9_]*", role)
           for role in [control, *roles]):
        raise postgres.LifecycleError("invalid writer fence role")
    if not isinstance(libraries, list) or any(not isinstance(library, str) or not re.fullmatch(r"[A-Za-z0-9_.-]+", library) for library in libraries):
        raise postgres.LifecycleError("invalid writer fence preload library policy")
    return {"control_role": control, "replication_roles": sorted(roles),
            "allowed_preload_libraries": sorted(set(libraries))}


def hba_contents(settings):
    # Quoting makes reserved HBA names such as `all` literal role identifiers.
    control = '"' + settings["control_role"] + '"'
    lines = [
        "# Harbor DB writer fence: SQL control is local OS-peer only.",
        f"local all {control} peer",
        f"local replication {control} peer",
        "local all all reject",
        "host all all 0.0.0.0/0 reject",
        "host all all ::/0 reject",
    ]
    # PostgreSQL's database keyword `all` excludes physical replication. These
    # exceptions cannot authenticate an ordinary SQL/database connection.
    if settings["replication_roles"]:
        roles = ",".join('"' + role + '"' for role in settings["replication_roles"])
        lines += [f"host replication {roles} 127.0.0.1/32 scram-sha-256",
                  f"host replication {roles} ::1/128 scram-sha-256"]
    lines += ["local replication all reject", "host replication all 0.0.0.0/0 reject",
              "host replication all ::/0 reject"]
    return ("\n".join(lines) + "\n").encode()


def marker(config):
    return Path(config["state_dir"]) / "writer-fence.json"


def journal(config, path=None):
    record = json.loads(read_private(marker(config) if path is None else path))
    token = record.get("token", "")
    if not isinstance(token, str) or not re.fullmatch(r"[0-9a-f]{32}", token):
        raise postgres.LifecycleError("invalid writer fence token")
    directory = Path(config["state_dir"]) / "writer-fences" / token
    for parent in [directory.parent, directory]:
        info = parent.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o077:
            raise postgres.LifecycleError("writer fence history must be private owned directory storage")
    expected = {
        "version": 1, "resource": config["resource"], "data_dir": config["data_dir"],
        "major": str(config["major"]), "policy": policy(config),
        "hba_file": str(directory / "pg_hba.conf"),
        "original_file": str(directory / "original-auto.conf"),
        "selected_file": str(directory / "fenced-auto.conf"),
    }
    if any(record.get(key) != value for key, value in expected.items()):
        raise postgres.LifecycleError("writer fence cluster or policy binding changed")
    for file_key, hash_key in [("original_file", "original_sha256"),
                               ("selected_file", "selected_sha256"),
                               ("hba_file", "hba_sha256")]:
        if digest(read_private(record[file_key])) != record.get(hash_key):
            raise postgres.LifecycleError("writer fence retained artifact changed")
    if read_private(record["hba_file"]) != hba_contents(record["policy"]):
        raise postgres.LifecycleError("writer fence HBA contract changed")
    identifier = postgres.inspect_cluster(config["package"], config["data_dir"], config["major"])
    if identifier != record.get("system_identifier"):
        raise postgres.LifecycleError("writer fence cluster identifier changed")
    if record.get("phase") not in ("prepared", "closing"):
        raise postgres.LifecycleError("unknown writer fence phase")
    return record


def startup(config):
    """Never recreate a missing artifact or infer readiness from a journal."""
    if not os.path.lexists(marker(config)):
        auto = Path(config["data_dir"]) / "postgresql.auto.conf"
        if os.path.lexists(auto) and b"# harbor-db-writer-fence=" in read_auto(config):
            raise postgres.LifecycleError("writer fence selector has no journal")
        return None
    record = journal(config)
    if record["phase"] != "prepared":
        raise postgres.LifecycleError("unfinished writer fence thaw; startup is forbidden")
    if digest(read_auto(config)) != record["selected_sha256"]:
        raise postgres.LifecycleError("writer fence startup selector differs; resume offline preparation")
    return record


@contextlib.contextmanager
def offline(config):
    postgres.validate_config(config)
    state = Path(config["state_dir"])
    # Consumer-provisioned storage is required, including before adoption. Never
    # publish identity or take over a postmaster as a side effect of fencing.
    info = state.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.geteuid() or info.st_mode & 0o022:
        raise postgres.LifecycleError("writer fence state must be owned non-writable directory storage")
    with contextlib.ExitStack() as stack:
        stack.enter_context(lock(state / "writer-fence.lock", create=True))
        if os.path.lexists(state / "identity.json"):
            stack.enter_context(lock(state / "lock"))
            postgres.verify_identity(config)
        postgres.reject_upgrade(config)
        postgres.require_stopped(config["package"], config["data_dir"])
        yield


def open_fence(config, expected_identifier):
    with offline(config):
        observed = postgres.inspect_cluster(config["package"], config["data_dir"], config["major"])
        if not re.fullmatch(r"[1-9][0-9]*", expected_identifier) or observed != expected_identifier:
            raise postgres.LifecycleError("independently supplied fence identifier differs")
        if os.path.lexists(marker(config)):
            record = journal(config)
            if record["phase"] != "prepared":
                raise postgres.LifecycleError("resume explicit writer fence thaw before another preparation")
            current = digest(read_auto(config))
            if current not in (record["original_sha256"], record["selected_sha256"]):
                raise postgres.LifecycleError("automatic configuration changed; retain writer fence")
        else:
            original = read_auto(config)
            if b"# harbor-db-writer-fence=" in original:
                tokens = re.findall(rb"^# harbor-db-writer-fence=([0-9a-f]{32})$", original, re.MULTILINE)
                if len(tokens) != 1:
                    raise postgres.LifecycleError("unbound writer fence selector; retain stopped primary")
                path = Path(config["state_dir"]) / "writer-fences" / tokens[0].decode() / "prepared.json"
                record = journal(config, path)
                if record["phase"] != "prepared" or digest(original) != record["selected_sha256"]:
                    raise postgres.LifecycleError("interrupted writer fence selector changed; retain stopped primary")
                write_json(marker(config), record)
                startup(config)
                return {"status": "prepared-offline", "token": record["token"],
                        "system_identifier": observed, "restart_required": True}
            settings = policy(config)
            token = secrets.token_hex(16)
            history = Path(config["state_dir"]) / "writer-fences"
            history.mkdir(mode=0o700, exist_ok=True)
            if history.is_symlink():
                raise postgres.LifecycleError("writer fence history is redirected")
            # The selector can survive before active-journal publication. Its
            # retained HBA must already have a durable path through this parent.
            sync_directory(Path(config["state_dir"]))
            directory = history / token
            directory.mkdir(mode=0o700)
            sync_directory(history)
            hba = directory / "pg_hba.conf"
            if any(character in str(hba) for character in ["'", "\n", "\r", "\\"]):
                raise postgres.LifecycleError("writer fence storage cannot be represented in PostgreSQL configuration")
            selected = original + (f"\n# harbor-db-writer-fence={token}\nhba_file = '{hba}'\n").encode()
            record = {
                "version": 1, "phase": "prepared", "token": token,
                "resource": config["resource"], "data_dir": config["data_dir"],
                "major": str(config["major"]), "system_identifier": observed,
                "policy": settings, "hba_file": str(hba),
                "original_file": str(directory / "original-auto.conf"),
                "selected_file": str(directory / "fenced-auto.conf"),
                "original_sha256": digest(original), "selected_sha256": digest(selected),
                "hba_sha256": digest(hba_contents(settings)),
            }
            for path, content in [(hba, hba_contents(settings)),
                                  (record["original_file"], original),
                                  (record["selected_file"], selected)]:
                atomic_write(path, content)
            write_json(directory / "prepared.json", record)
            # The startup selector is the durable freeze commit point. Publish
            # it before the active journal: even an older bare postmaster then
            # uses the restrictive HBA on restart. A guarded start with a
            # selector but no journal fails closed until offline resume.
        atomic_write(Path(config["data_dir"]) / "postgresql.auto.conf", read_private(record["selected_file"]))
        write_json(marker(config), record)
        startup(config)
        return {"status": "prepared-offline", "token": record["token"],
                "system_identifier": observed, "restart_required": True}


def close_fence(config, token):
    with offline(config):
        record = journal(config)
        if token != record["token"]:
            raise postgres.LifecycleError("writer fence thaw token differs")
        current = digest(read_auto(config))
        allowed = [record["selected_sha256"]]
        if record["phase"] == "closing":
            allowed.append(record["original_sha256"])
        if current not in allowed:
            raise postgres.LifecycleError("automatic configuration changed; retain writer fence")
        record["phase"] = "closing"
        write_json(marker(config), record)
        atomic_write(Path(config["data_dir"]) / "postgresql.auto.conf", read_private(record["original_file"]))
        receipt = Path(record["hba_file"]).parent / "closed.json"
        write_json(receipt, {"status": "closed-offline", "token": token,
                             "system_identifier": record["system_identifier"],
                             "original_sha256": record["original_sha256"]})
        marker(config).unlink()
        sync_directory(Path(config["state_dir"]))
        return {"status": "closed-offline", "token": token, "receipt": str(receipt),
                "restart_required": True}


def inspect_offline(config, token, phase):
    """Verify a stopped transition boundary before the root startup gate opens."""
    postgres.validate_config(config)
    with lock(Path(config["state_dir"]) / "writer-fence.lock", shared=True):
        postgres.reject_upgrade(config)
        postgres.require_stopped(config["package"], config["data_dir"])
        if phase == "prepared":
            record = startup(config)
            if record is None or record["token"] != token:
                raise postgres.LifecycleError("offline writer fence token differs")
        elif phase == "closed":
            if os.path.lexists(marker(config)) or not re.fullmatch(r"[0-9a-f]{32}", token):
                raise postgres.LifecycleError("unfinished writer fence thaw")
            directory = Path(config["state_dir"]) / "writer-fences" / token
            record = journal(config, directory / "prepared.json")
            receipt = json.loads(read_private(directory / "closed.json"))
            if (receipt != {"status": "closed-offline", "token": token,
                           "system_identifier": record["system_identifier"], "original_sha256": record["original_sha256"]}
                    or digest(read_auto(config)) != record["original_sha256"]):
                raise postgres.LifecycleError("writer fence thaw boundary changed")
        else:
            raise postgres.LifecycleError("unknown offline writer fence phase")
        return {"status": f"{phase}-offline", "token": token, "resource": config["resource"],
                "data_dir": config["data_dir"], "major": str(config["major"]), "system_identifier": record["system_identifier"]}


def inspect_live(config, token, socket_dir, port):
    postgres.validate_config(config)
    if not Path(socket_dir).is_absolute() or "," in socket_dir or not 1 <= port <= 65535:
        raise postgres.LifecycleError("writer fence inspection needs a local socket and port")
    with lock(Path(config["state_dir"]) / "writer-fence.lock", shared=True):
        record = startup(config)
        if record is None or record["token"] != token:
            raise postgres.LifecycleError("writer fence inspection token differs")
        sql = """
            SELECT json_build_object(
                'data_dir', current_setting('data_directory'),
                'major', (current_setting('server_version_num')::int / 10000)::text,
                'system_identifier', system_identifier::text,
                'hba_file', current_setting('hba_file'),
                'control_role', current_user,
                'in_recovery', pg_is_in_recovery(),
                'fsync', current_setting('fsync'),
                'full_page_writes', current_setting('full_page_writes'),
                'synchronous_commit', current_setting('synchronous_commit'),
                'preload_libraries', CASE WHEN current_setting('shared_preload_libraries') = ''
                    THEN '[]'::json ELSE to_json(string_to_array(current_setting('shared_preload_libraries'), ',')) END,
                'logical_subscriptions', (SELECT count(*) FROM pg_subscription WHERE subenabled),
                'prepared_transactions', (SELECT count(*) FROM pg_prepared_xacts),
                'other_writers', (SELECT count(*) FROM pg_stat_activity
                    WHERE pid <> pg_backend_pid() AND backend_type NOT IN (
                        'autovacuum launcher', 'autovacuum worker', 'background writer',
                        'checkpointer', 'walwriter', 'walsender', 'archiver', 'logical replication launcher', 'io worker')))
            FROM pg_control_system();
        """
        env = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
        env.update(PGCONNECT_TIMEOUT="5", PGOPTIONS="-c statement_timeout=10000 -c default_transaction_read_only=on")
        output = postgres.run([
            Path(config["package"]) / "bin/psql", "-X", "-w", "-A", "-t",
            "-v", "ON_ERROR_STOP=1", "-h", socket_dir, "-p", str(port),
            "-U", record["policy"]["control_role"], "-d", "postgres", "-c", sql,
        ], env=env, text=True, capture_output=True, timeout=15).stdout
        observed = json.loads(output)
        expected = {
            "data_dir": config["data_dir"], "major": str(config["major"]),
            "system_identifier": record["system_identifier"], "hba_file": record["hba_file"],
            "control_role": record["policy"]["control_role"], "in_recovery": False,
            "fsync": "on", "full_page_writes": "on", "synchronous_commit": "on",
            "preload_libraries": record["policy"]["allowed_preload_libraries"],
            "logical_subscriptions": 0,
            "prepared_transactions": 0, "other_writers": 0,
        }
        if isinstance(observed.get("preload_libraries"), list):
            observed["preload_libraries"] = sorted(library.strip() for library in observed["preload_libraries"])
        if observed != expected:
            differences = ", ".join(key for key in expected if observed.get(key) != expected[key])
            raise postgres.LifecycleError(f"live writer fence is not exclusive at the bound primary: {differences}")
        return {"status": "ready", "token": token,
                "system_identifier": record["system_identifier"], "hba_sha256": record["hba_sha256"]}
