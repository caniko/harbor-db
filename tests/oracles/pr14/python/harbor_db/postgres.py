"""Explicit adoption, fail-closed startup and staged PostgreSQL upgrades.

No command implicitly adopts or initializes a missing cluster. Upgrade journals
are published before initdb, and remain a startup barrier until publication has
been verified. Old clusters are historical snapshots, never writable fallbacks.
"""

import argparse
import contextlib
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

from .durable import lock, read_json, sync_directory, sync_tree, write_json


class LifecycleError(RuntimeError):
    """Storage cannot safely satisfy the declared lifecycle contract."""


def run(argv, **kwargs):
    return subprocess.run(
        [str(arg) for arg in argv], check=True,
        env={**kwargs.pop("env", os.environ), "LC_ALL": "C"},
        **kwargs,
    )


def require_mounts(config):
    def unescape(value):
        return re.sub(r"\\([0-7]{3})", lambda m: chr(int(m[1], 8)), value)

    mounts = {
        unescape(line.split()[4])
        for line in Path("/proc/self/mountinfo").read_text().splitlines()
    }
    for mount in config.get("required_mounts", []):
        if mount not in mounts:
            raise LifecycleError(f"required mount is absent: {mount}")


def validate_config(config):
    for key in ("data_dir", "state_dir", "package"):
        path = Path(config[key])
        if not path.is_absolute() or str(path.resolve()) != str(path):
            raise LifecycleError(f"{key} must be an absolute non-symlink path: {path}")
    if not re.fullmatch(r"[0-9]+", str(config["major"])):
        raise LifecycleError("invalid PostgreSQL major")
    if not config["resource"]:
        raise LifecycleError("resource name is empty")
    data, state = Path(config["data_dir"]), Path(config["state_dir"])
    if state == data or state.is_relative_to(data):
        raise LifecycleError("state_dir must be outside the cluster")
    require_mounts(config)


def inspect_cluster(package, data_dir, major):
    data = Path(data_dir)
    try:
        version = (data / "PG_VERSION").read_text().strip()
    except FileNotFoundError as error:
        raise LifecycleError(f"cluster is missing at {data}; initialization is forbidden") from error
    if data.is_symlink() or version != str(major):
        raise LifecycleError(f"cluster major mismatch at {data}")
    output = run(
        [Path(package) / "bin/pg_controldata", data],
        capture_output=True, text=True,
    ).stdout
    match = re.search(r"^Database system identifier:\s*([0-9]+)$", output, re.MULTILINE)
    if not match:
        raise LifecycleError(f"cannot inspect cluster identifier at {data}")
    return match[1]


def inspect_live(config, expected_identifier, socket_dir, port):
    """Verify the authoritative local endpoint independently of its control file.

    Called as the PostgreSQL service user, including by a NixOS pre-switch check.
    It creates no authority state and never connects through ambient PG routing.
    """
    validate_config(config)
    if (not re.fullmatch(r"[1-9][0-9]*", expected_identifier)
            or not Path(socket_dir).is_absolute()
            or "," in socket_dir
            or not 1 <= port <= 65535):
        raise LifecycleError("live inspection requires an identifier and local socket/port")
    sql = """
        SELECT json_build_object(
            'data_dir', current_setting('data_directory'),
            'major', (current_setting('server_version_num')::int / 10000)::text,
            'system_identifier', system_identifier::text,
            'fsync', current_setting('fsync'),
            'full_page_writes', current_setting('full_page_writes'),
            'synchronous_commit', current_setting('synchronous_commit'),
            'in_recovery', pg_is_in_recovery())
        FROM pg_control_system();
    """
    env = {key: value for key, value in os.environ.items() if not key.startswith("PG")}
    env["PGCONNECT_TIMEOUT"] = "5"
    output = run([
        Path(config["package"]) / "bin/psql", "--no-psqlrc", "--no-password",
        f"--host={socket_dir}", f"--port={port}", "--username=postgres",
        "--dbname=postgres", "--set=ON_ERROR_STOP=1", "--tuples-only", "--no-align",
        "--command", sql,
    ], env=env, capture_output=True, text=True, timeout=15).stdout
    observed = json.loads(output)
    expected = {
        "data_dir": config["data_dir"], "major": str(config["major"]),
        "system_identifier": expected_identifier, "fsync": "on",
        "full_page_writes": "on", "synchronous_commit": "on", "in_recovery": False,
    }
    if observed != expected:
        raise LifecycleError("live endpoint differs from the declared durable primary identity")
    if inspect_cluster(config["package"], config["data_dir"], config["major"]) != expected_identifier:
        raise LifecycleError("live endpoint and physical cluster identifiers differ")
    return observed


def identity(config, identifier):
    return {
        "version": 1,
        "resource": config["resource"],
        "data_dir": config["data_dir"],
        "major": str(config["major"]),
        "system_identifier": identifier,
    }


def registered(config):
    try:
        record = read_json(Path(config["state_dir"]) / "identity.json")
    except FileNotFoundError as error:
        raise LifecycleError("cluster is not adopted; explicit adoption is required") from error
    expected = identity(config, record.get("system_identifier"))
    if record != expected:
        raise LifecycleError("registered cluster identity mismatch (including rollback target)")
    return record


def verify_identity(config):
    record = registered(config)
    observed = inspect_cluster(config["package"], config["data_dir"], config["major"])
    if observed != record["system_identifier"]:
        raise LifecycleError("cluster identity mismatch: replacement or stale storage")
    return record


def reject_upgrade(config):
    if (Path(config["state_dir"]) / "upgrade.json").exists():
        raise LifecycleError("unfinished upgrade; explicit upgrade resume is required")


def recovery_admission(config, **kwargs):
    if config.get("recovery") is None:
        return contextlib.nullcontext()
    from .recovery import admission
    return admission(config, **kwargs)


def adopt(config, expected_identifier):
    validate_config(config)
    reject_upgrade(config)
    # State storage must be provisioned by the consumer; do not recreate it.
    state = Path(config["state_dir"])
    path = state / "identity.json"
    # Only first adoption may create the anchor. A surviving writer can still
    # hold an unlinked inode; replacing it would bypass its authority lease.
    with recovery_admission(config), lock(state / "lock", create=not path.exists()):
        reject_upgrade(config)
        observed = inspect_cluster(config["package"], config["data_dir"], config["major"])
        if observed != expected_identifier:
            raise LifecycleError("independently supplied system identifier does not match")
        if path.exists():
            verify_identity(config)
        else:
            write_json(path, identity(config, observed))


def adopt_live(config, expected_identifier, socket_dir, port):
    """Explicit switch-time adoption; an already guarded writer needs no mutation."""
    validate_config(config)
    reject_upgrade(config)
    path = Path(config["state_dir"]) / "identity.json"
    # Later activations must coexist with the writer's shared lifetime lease.
    already_adopted = path.exists()
    with recovery_admission(config, socket_dir=socket_dir, port=port), lock(Path(config["state_dir"]) / "lock", shared=already_adopted, create=not already_adopted):
        reject_upgrade(config)
        if already_adopted or path.exists():
            verify_identity(config)
            changed = False
        else:
            changed = True
        observed = inspect_live(config, expected_identifier, socket_dir, port)
        if changed:
            write_json(path, identity(config, expected_identifier))
    return {"inspection": observed, "changed": changed}


def check(config):
    validate_config(config)
    # A read-only check never creates adoption state or the lock inode.
    if not (Path(config["state_dir"]) / "identity.json").exists():
        raise LifecycleError("cluster is not adopted; explicit adoption is required")
    with lock(Path(config["state_dir"]) / "lock", shared=True):
        reject_upgrade(config)
        verify_identity(config)
        from . import writer_fence
        writer_fence.startup(config)


def serve(config):
    """Become the postmaster, retaining its authority lease across exec.

    Linux flock is attached to the open file description. PostgreSQL and its
    forked children retain it; there is no separate supervisor to die and unlock
    a surviving writer. The VM gate verifies retention in the actual postmaster.
    """
    validate_config(config)
    with lock(Path(config["state_dir"]) / "lock", shared=True) as lease:
        reject_upgrade(config)
        verify_identity(config)
        os.set_inheritable(lease, True)
        executable = str(Path(config["package"]) / "bin/postgres")
        from . import writer_fence
        fence = writer_fence.startup(config)
        # PGDATA or data_directory in postgresql.conf must never select a
        # different cluster from the one just checked under the authority lease.
        os.execv(executable, [
            executable, "-D", config["data_dir"], "-c", f"data_directory={config['data_dir']}",
            # ALTER SYSTEM settings survive generations and override the generated
            # configuration. Server-wide durability must outrank those settings.
            "-c", "fsync=on", "-c", "full_page_writes=on",
            "-c", "synchronous_commit=on",
            *(["-c", f"hba_file={fence['hba_file']}"] if fence is not None else []),
        ])


def require_stopped(package, data):
    if (Path(data) / "postmaster.pid").exists():
        raise LifecycleError(f"cluster has a postmaster.pid; verify it is stopped: {data}")
    result = subprocess.run(
        [str(Path(package) / "bin/pg_ctl"), "-D", str(data), "status"],
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
    )
    if result.returncode == 0:
        raise LifecycleError(f"cluster is running: {data}")
    if result.returncode != 3:
        raise LifecycleError(f"cluster status is indeterminate (code {result.returncode}): {data}")


def control_digest(data):
    fd = os.open(Path(data) / "global/pg_control", os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def upgrade(config, *, retry_incomplete=False):
    """Upgrade offline by copy, preserving all interrupted output for diagnosis.

    Publication is ordered: validated staging -> ready journal -> rename and
    directory fsync -> identity fsync -> journal removal and directory fsync.
    Any crash before journal removal blocks startup, including an old generation.
    """
    validate_config(config)
    settings = config["upgrade"]
    source = dict(config, data_dir=settings["data_dir"], major=str(settings["major"]),
                  package=settings["package"])
    validate_config(source)
    if int(source["major"]) >= int(config["major"]):
        raise LifecycleError("upgrade requires a newer PostgreSQL major")
    state = Path(config["state_dir"])
    target = Path(config["data_dir"])
    staging = target.with_name(target.name + ".harbor-staging")
    source_copy = target.with_name(target.name + ".harbor-source")
    if Path(source["data_dir"]) in (target, staging, source_copy):
        raise LifecycleError("source and destination must differ")
    journal_path = state / "upgrade.json"
    # Upgrades always operate on adopted authority, including journal resumes.
    with lock(state / "lock") as lease:
        if not journal_path.exists() and (state / "identity.json").exists():
            current = read_json(state / "identity.json")
            if current.get("data_dir") == str(target):
                verify_identity(config)
                return
        require_stopped(source["package"], source["data_dir"])
        journal = read_json(journal_path) if journal_path.exists() else None
        if journal is None:
            source_record = verify_identity(source)
        else:
            source_record = identity(source, inspect_cluster(
                source["package"], source["data_dir"], source["major"],
            ))
            current = read_json(state / "identity.json")
            if current != source_record and current != journal.get("identity"):
                raise LifecycleError("registered identity changed during upgrade")
        digest = control_digest(source["data_dir"])
        if any((Path(source["data_dir"]) / "pg_tblspc").iterdir()):
            raise LifecycleError("external tablespaces are unsupported for staged upgrades")
        if (Path(source["data_dir"]) / "pg_wal").is_symlink():
            raise LifecycleError("external WAL storage is unsupported for staged upgrades")
        intent = {
            "version": 1, "source": source_record, "source_control": digest,
            "target": {k: config[k] for k in ("resource", "data_dir", "major", "package")},
            "staging": str(staging),
            "source_copy": str(source_copy),
        }
        if journal is not None:
            if any(journal.get(k) != v for k, v in intent.items()):
                raise LifecycleError("upgrade source or contract changed; cannot resume")
            if journal["phase"] == "ready":
                publish(config, journal)
                return
            if journal["phase"] != "building" or not retry_incomplete:
                raise LifecycleError("incomplete upgrade; use --retry-incomplete after inspection")
            if staging.exists():
                # Preserve interrupted output. Never remove a cluster automatically.
                abandoned = staging.with_name(staging.name + ".interrupted")
                if abandoned.exists():
                    raise LifecycleError(f"inspect preserved incomplete cluster: {abandoned}")
                require_stopped(config["package"], staging)
                staging.rename(abandoned)
                sync_directory(staging.parent)
            if source_copy.exists():
                abandoned = source_copy.with_name(source_copy.name + ".interrupted")
                if abandoned.exists() or (source_copy / "postmaster.pid").exists():
                    raise LifecycleError(f"inspect preserved source copy: {source_copy}")
                source_copy.rename(abandoned)
                sync_directory(source_copy.parent)
        # NixOS tmpfiles may provision an empty data directory. Removing only an
        # empty directory is safe; any initialized or partially written DB fails.
        if target.exists() and not target.is_symlink() and not any(target.iterdir()):
            target.rmdir()
            sync_directory(target.parent)
        if target.exists() or staging.exists() or source_copy.exists():
            raise LifecycleError("unregistered destination exists; explicit inspection required")
        journal = {**intent, "phase": "building"}
        write_json(journal_path, journal)
        # pg_upgrade starts/stops the old server and changes its control file.
        # Give it a disposable offline copy; never modify the registered source.
        if settings.get("copy_command"):
            run([*settings["copy_command"], source["data_dir"], source_copy], pass_fds=(lease,))
        else:
            shutil.copytree(source["data_dir"], source_copy)
        # Resolve NixOS' configuration symlinks inside the copy; external database
        # storage (tablespaces or WAL) was rejected above.
        run([Path(config["package"]) / "bin/initdb", "-D", staging,
             *settings.get("initdb_args", [])], pass_fds=(lease,))
        with (staging / "postgresql.conf").open("a") as stream:
            stream.write("\n" + settings.get("extra_config", "") + "\n")
        run([
            Path(config["package"]) / "bin/pg_upgrade",
            f"--old-bindir={source['package']}/bin", f"--new-bindir={config['package']}/bin",
            f"--old-datadir={source_copy}", f"--new-datadir={staging}",
            # Socket directory is private and cannot contact the production instance.
            f"--socketdir={state}", "--old-port=55438", "--new-port=55439",
            # A copied config (including postgresql.auto.conf) may explicitly
            # point data_directory at the registered source. Temporary servers
            # must use only the disposable trees, just like the normal launcher.
            f"--old-options=-c listen_addresses='' -c data_directory={shlex.quote(str(source_copy))}",
            f"--new-options=-c listen_addresses='' -c data_directory={shlex.quote(str(staging))}",
        ], cwd=staging, pass_fds=(lease,))
        require_stopped(config["package"], staging)
        validator = settings["validate_command"]
        if not validator:
            raise LifecycleError("upgrade requires a consumer validation command")
        run([*validator, staging], pass_fds=(lease,))
        require_stopped(config["package"], staging)
        if control_digest(source["data_dir"]) != digest:
            raise LifecycleError("registered source changed during offline upgrade")
        journal["identity"] = identity(
            config, inspect_cluster(config["package"], staging, config["major"]),
        )
        sync_tree(staging)
        journal["phase"] = "ready"
        write_json(journal_path, journal)
        publish(config, journal)


def publish(config, journal):
    target = Path(config["data_dir"])
    staging = Path(journal["staging"])
    if target.exists() and staging.exists():
        raise LifecycleError("ambiguous publication: both target and staging exist")
    location = staging if staging.exists() else target
    require_stopped(config["package"], location)
    observed = inspect_cluster(config["package"], location, config["major"])
    if identity(config, observed) != journal["identity"]:
        raise LifecycleError("validated upgrade identity mismatch")
    if staging.exists():
        staging.rename(target)
    # A previous invocation may have died after rename but before this flush.
    # Resuming must complete publication before removing the startup barrier.
    sync_directory(target.parent)
    state = Path(config["state_dir"])
    write_json(state / "previous-identity.json", journal["source"])
    write_json(state / "identity.json", journal["identity"])
    (state / "upgrade.json").unlink()
    sync_directory(state)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("check", help="read-only startup identity check")
    commands.add_parser("serve", help="exec PostgreSQL with its authority lease")
    adoption = commands.add_parser("adopt", help="explicitly register an existing cluster")
    adoption.add_argument("--system-identifier", required=True)
    for command in ("inspect-live", "adopt-live"):
        live = commands.add_parser(command, help="verify the live local primary" + (
            " and explicitly adopt it" if command == "adopt-live" else " without writes"
        ))
        live.add_argument("--system-identifier", required=True)
        live.add_argument("--socket-dir", default="/run/postgresql")
        live.add_argument("--port", type=int, default=5432)
    recovery_inspection = commands.add_parser("inspect-recovery", help="read-only backup and record-level recovery admission")
    recovery_inspection.add_argument("--socket-dir")
    recovery_inspection.add_argument("--port", type=int)
    fence_open = commands.add_parser("fence-open", help="prepare a durable writer fence while the primary is stopped")
    fence_open.add_argument("--system-identifier", required=True)
    fence_close = commands.add_parser("fence-close", help="explicitly thaw a stopped primary; never starts PostgreSQL")
    fence_close.add_argument("--token", required=True)
    offline_fence = commands.add_parser("inspect-offline-fence", help="verify a stopped fence transition before startup release")
    offline_fence.add_argument("--token", required=True)
    offline_fence.add_argument("--phase", choices=["prepared", "closed"], required=True)
    inhibit = commands.add_parser("inhibit-startup", help="root-owned persistent systemd gate before offline fence transitions")
    inhibit.add_argument("--system-identifier", required=True)
    release = commands.add_parser("release-startup", help="explicitly release startup after a verified stopped transition; never starts PostgreSQL")
    release.add_argument("--token", required=True)
    release.add_argument("--fence-token", required=True)
    release.add_argument("--phase", choices=["prepared", "closed"], required=True)
    fence_live = commands.add_parser("inspect-fence", help="verify the restarted primary excludes application SQL writers")
    fence_live.add_argument("--token", required=True)
    fence_live.add_argument("--socket-dir", default="/run/postgresql")
    fence_live.add_argument("--port", type=int, default=5432)
    preparation = commands.add_parser("prepare-recovery", help="explicit managed backup/snapshot/restore preparation before adoption")
    preparation.add_argument("--preparation-config", type=Path, required=True)
    preparation.add_argument("--socket-dir", required=True)
    preparation.add_argument("--port", type=int, required=True)
    for command in ("snapshot-records", "certify-recovery"):
        recovery_parser = commands.add_parser(command, help="execute record checks and publish bound recovery evidence")
        recovery_parser.add_argument("--socket-dir", required=True)
        recovery_parser.add_argument("--port", type=int, required=True)
        if command == "certify-recovery":
            recovery_parser.add_argument("--data-dir", required=True)
    migration = commands.add_parser("upgrade", help="explicit offline staged upgrade")
    migration.add_argument("--retry-incomplete", action="store_true")
    args = parser.parse_args()
    try:
        config = json.loads(args.config.read_text())
        if args.command == "adopt":
            adopt(config, args.system_identifier)
        elif args.command in ("inspect-live", "adopt-live"):
            operation = inspect_live if args.command == "inspect-live" else adopt_live
            print(json.dumps(operation(config, args.system_identifier, args.socket_dir, args.port)))
        elif args.command == "check":
            check(config)
        elif args.command == "serve":
            return serve(config)
        elif args.command in ("inhibit-startup", "release-startup"):
            from . import startup_inhibition
            # The privileged adapter/runuser argv must come from immutable,
            # root-owned policy, never a service-user-writable manifest.
            path = args.config.resolve()
            info = path.stat()
            if not str(path).startswith("/nix/store/") or info.st_uid != 0 or info.st_mode & 0o022:
                raise LifecycleError("startup inhibition requires immutable root-owned policy")
            config = json.loads(path.read_text())
            if args.command == "inhibit-startup":
                result = startup_inhibition.inhibit(config, args.system_identifier)
            else:
                result = startup_inhibition.release(config, path, args.token, args.fence_token, args.phase)
            print(json.dumps(result, sort_keys=True))
        elif args.command in ("fence-open", "fence-close", "inspect-fence", "inspect-offline-fence"):
            from . import writer_fence
            if args.command == "fence-open":
                result = writer_fence.open_fence(config, args.system_identifier)
            elif args.command == "fence-close":
                result = writer_fence.close_fence(config, args.token)
            elif args.command == "inspect-offline-fence":
                result = writer_fence.inspect_offline(config, args.token, args.phase)
            else:
                result = writer_fence.inspect_live(config, args.token, args.socket_dir, args.port)
            print(json.dumps(result, sort_keys=True))
        elif args.command in ("inspect-recovery", "snapshot-records", "certify-recovery", "prepare-recovery"):
            from . import recovery
            if args.command == "inspect-recovery":
                if (args.socket_dir is None) != (args.port is None):
                    raise LifecycleError("live recovery inspection requires both local socket and port")
                result = recovery.check(config) if args.socket_dir is None else recovery.live_check(config, args.socket_dir, args.port)
            elif args.command == "prepare-recovery":
                result = recovery.prepare(config, json.loads(args.preparation_config.read_text()), args.socket_dir, args.port)
            elif args.command == "snapshot-records":
                result = recovery.snapshot(config, args.socket_dir, args.port)
            else:
                result = recovery.certify(config, args.data_dir, args.socket_dir, args.port)
            # Receipts expose digests, never private query results or records.
            print(json.dumps(result, sort_keys=True))
        else:
            upgrade(config, retry_incomplete=args.retry_incomplete)
    except (LifecycleError, OSError, ValueError, TypeError, KeyError, subprocess.SubprocessError) as error:
        print(f"harbor-db-postgres: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
