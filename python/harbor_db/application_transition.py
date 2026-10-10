"""Explicit resumable backend cutover. Publication never starts or thaws writers."""

import argparse
import contextlib
import json
import os
import re
import sys
import time
import pwd
from pathlib import Path

from . import application_backup, resource
from .durable import lock, read_json, sync_directory, write_json
from .transition_manifest import (candidate_path, fence, generation, inspect_barriers, install_barriers,
                                  intent, release_barriers, require_root, stop_units, validate, worker, write_owned)


def journal_path(config):
    source, _ = validate(config)
    return Path(source["state_dir"]) / "transition.json"


def save(config, record, phase=None):
    if phase is not None:
        record["phase"] = phase
    write_owned(journal_path(config), record)


def status(config):
    record = read_json(journal_path(config))
    if record["version"] != 1 or record["intent"] != intent(config):
        raise ValueError("transition intent or executable identity changed; use the original manifest")
    return record


@contextlib.contextmanager
def transaction(config, held=None):
    require_root()
    acquisition = contextlib.nullcontext(held) if held is not None else lock(Path(config["barrier_dir"]) / "lock")
    with acquisition as transition_lease:
        record = status(config)
        source, target = validate(config)
        with lock(Path(source["state_dir"]) / "lock", shared=record["phase"] in ("write-enabled", "complete", "aborted")) as authority_lease:
            with pin_source(config, record, [transition_lease, authority_lease]) as leases:
                yield record, source, target, leases


@contextlib.contextmanager
def pin_source(config, record, leases):
    if "backup" not in record or record["phase"] in ("write-enabled", "complete", "aborted"):
        yield leases
        return
    backup_config = read_json(config["backup_manifest"])
    with lock(Path(backup_config["root"]) / "lock", shared=True) as lease:
        yield [*leases, lease]


def plan(config, candidate, writer_fence_token):
    require_root()
    kind = candidate_path(candidate, config)
    source, target = validate(config)
    if config["postgres_manifest"] is not None and not re.fullmatch(r"[0-9a-f]{32}", writer_fence_token or ""):
        raise ValueError("PostgreSQL transition requires the borrowed capability-3 fence token")
    if config["postgres_manifest"] is None and writer_fence_token is not None:
        raise ValueError("a non-PostgreSQL transition cannot borrow an undeclared fence")
    path = journal_path(config)
    with lock(Path(config["barrier_dir"]) / "lock", create=not path.exists()):
        if path.exists():
            record = status(config)
            if candidate not in (record["candidate"], record["preparation_contract"]) or record["writer_fence_token"] != writer_fence_token:
                raise ValueError("transition resume candidate or fence identity differs")
            return record
        with resource.inspection(source) as authority:
            record = {"version": 1, "phase": "planned", "intent": intent(config),
                      "candidate": candidate if kind == "generation" else None,
                      "preparation_contract": candidate if kind == "contract" else None,
                      "barrier_candidate": candidate if kind == "generation" else None,
                      "writer_fence_token": writer_fence_token, "source_authority": authority,
                      "source_generation": generation(),
                      "created_at": int(time.time()), "fence": None}
            if config["custody_manifest"] is not None:
                entry = read_json(config["custody_manifest"])
                custody = Path(entry["custody_file"])
                if custody.parent != Path(source["state_dir"]):
                    raise ValueError("target custody publication must stay in the established authority directory")
                record["source_custody"] = read_json(custody) if custody.exists() else None
            save(config, record)
            return record


def capture_source(config, record, leases):
    backup_config = read_json(config["backup_manifest"])
    source, _ = validate(config)
    account = Path(source["state_dir"]).stat().st_uid
    user = pwd.getpwuid(account).pw_name
    attempt = "transition-" + application_backup.identity(record["intent"])[:32]
    command = {"user": user, "argv": [str(Path(config["storage_package"]) / "harbor-db-application-backup"),
               "--config", config["backup_manifest"], "capture", "--attempt", attempt, "--retry-incomplete"]}
    backup = Path(backup_config["root"]) / attempt
    if not backup.exists():
        worker(config, command, {}, leases)
    # A completed tree whose pointer publication was interrupted may be reused
    # only after complete byte, tool and freshness verification.
    result = application_backup.inspect(backup_config, backup)
    if result["consistency"] != "quiesced":
        raise ValueError("cutover capture requires quiesced sequence and filesystem state")
    return backup, result


def source_bytes(config, record):
    result = application_backup.inspect(read_json(config["backup_manifest"]), Path(record["backup"]))
    if application_backup.digest(Path(record["backup"]) / "acceptance.json") != record["source_acceptance_sha256"]:
        raise ValueError("source backup evidence changed")
    return result


def run_action(config, stage, record, leases):
    if record["candidate"] is None and "{candidate}" in config["commands"][stage]["argv"]:
        raise ValueError("preparation workers must use the immutable target contract before a generation is bound")
    return worker(config, config["commands"][stage], {"{backup}": record["backup"],
                  "{candidate}": record["candidate"], "{source}": config["source_manifest"],
                  "{target}": config["target_manifest"]}, leases)


def semantic(receipt, expected):
    if (not isinstance(receipt, dict) or set(receipt) != {"version", "status", "semantic_sha256"}
            or receipt["version"] != 1 or receipt["status"] != "verified"
            or receipt["semantic_sha256"] != expected):
        raise ValueError("complete application semantic evidence differs from the captured source")


def evidence(config, record):
    source = source_bytes(config, record)
    receipt = read_json(config["independent_receipt"])
    if (receipt["version"] != 1 or receipt["status"] != "verified" or receipt["resource"] != config["resource"]
            or receipt["source_acceptance_sha256"] != record["source_acceptance_sha256"]
            or receipt["semantic_sha256"] != source["semantic_sha256"]
            or receipt["manifest_sha256"] != source["manifest_sha256"]
            or receipt["executables"] != source["executables"]
            or receipt["executor_machine_sha256"] == source["executor_machine_sha256"]
            or receipt["executor"] == source["executor"]):
        raise ValueError("independent application restore evidence differs or is not independent")
    age = int(time.time()) - receipt["certified_at"]
    if not 0 <= age <= read_json(config["backup_manifest"])["maximum_age_seconds"]:
        raise ValueError("independent restoration is stale or from the future")
    current = application_backup.digest(config["independent_receipt"])
    if record.get("independent_sha256") is not None and record["independent_sha256"] != current:
        raise ValueError("independent restore evidence changed during resume")
    return source, current


def primary_evidence(config, record, leases):
    if config["postgres_manifest"] is None:
        return None
    result = worker(config, {"user": "postgres", "argv": [str(Path(config["storage_package"]) / "harbor-db-postgres"),
                    "--config", config["postgres_manifest"], "inspect-recovery", "--socket-dir", config["postgres_socket"],
                    "--port", str(config["postgres_port"])]}, {}, leases)
    if result["status"] != "ready":
        raise ValueError("the whole primary recovery boundary is not accepted")
    from . import recovery
    database = read_json(config["postgres_manifest"])
    settings = database["recovery"]
    snapshot_path = recovery.source_snapshot_path(database, settings)
    snapshot = read_json(snapshot_path)
    if (snapshot.get("writer_fence_token") != record["writer_fence_token"]
            or application_backup.digest(snapshot_path) != result["snapshot_sha256"]):
        raise ValueError("primary snapshot does not bind the same held writer fence")
    if record.get("primary_snapshot_sha256") is not None and record["primary_snapshot_sha256"] != result["snapshot_sha256"]:
        raise ValueError("primary recovery snapshot changed during transition resume")
    return result["snapshot_sha256"]


def target_custody(config, record, target, leases):
    if config["custody_manifest"] is None:
        return None
    from . import cutover
    entry = read_json(config["custody_manifest"])
    if entry["kind"] != "filesystem" or entry["authority"] != target:
        raise ValueError("target custody manifest does not declare the exact target authority")
    requirement = []
    checks = entry.get("database_inventory_checks", [])
    if checks and config["postgres_manifest"] is None:
        raise ValueError("database-bound custody requires the borrowed PostgreSQL fence")
    for check in checks:
        database = read_json(config["postgres_manifest"])
        command = {"user": "postgres", "argv": [str(Path(database["package"]) / "bin/psql"), "-X", "-w", "-qAt",
                   "-v", "ON_ERROR_STOP=1", "-h", config["postgres_socket"], "-p", str(config["postgres_port"]),
                   "-U", "postgres", "-d", check["database"], "-c", "BEGIN READ ONLY; SET LOCAL statement_timeout = '30s'; " + check["sql"] + "; COMMIT;"]}
        paths = worker(config, command, {}, leases)
        if not isinstance(paths, list):
            raise ValueError("database corpus query did not produce a path array")
        requirement.extend({"root": check["root"], **item} for item in paths)
    contents = cutover.inventory(entry, contents=True)
    cutover.require_database_paths(contents, requirement, roots=target["directories"], git_executable=entry.get("git_executable"))
    return {"version": 1, "resource": target["resource"], "identity": record["source_authority"]["identity"],
            "binding": target["binding"], "directories": target["directories"],
            "root_identities": cutover.root_identities(entry), "inventory": contents,
            "metadata": cutover.inventory(entry, contents=False), "completed_at": int(time.time()),
            "database_snapshot_sha256": record["primary_snapshot_sha256"], "database_requirements": requirement,
            "transition_source_acceptance_sha256": record["source_acceptance_sha256"],
            "independent_restore_sha256": record["independent_sha256"], "semantic_sha256": record["semantic_sha256"]}


def verify_custody(config, record, target, leases):
    observed = target_custody(config, record, target, leases)
    if record["custody"] is not None and any(observed[key] != value for key, value in record["custody"].items() if key != "completed_at"):
        raise ValueError("prepared target corpus evidence changed")


def prepare(config):
    require_root()
    # Install the durable barriers before requesting the lifetime writer lease.
    # Existing processes can drain; a reboot cannot start another generation.
    with lock(Path(config["barrier_dir"]) / "lock") as transition_lease:
        record = status(config)
        if record["phase"] not in ("planned", "quiescing", "quiesced", "captured", "importing", "imported", "prepared"):
            raise ValueError("transition preparation is not permitted at this phase")
        save(config, record, "quiescing" if record["phase"] == "planned" else None)
        install_barriers(config, record)
        stop_units(config)
        return prepare_locked(config, transition_lease)


def prepare_locked(config, transition_lease):
    with transaction(config, transition_lease) as (record, source, target, leases), fence(config, record, leases) as leases:
        inspect_barriers(config)
        if resource.verify(source, resource.contract(source)) != record["source_authority"]:
            raise ValueError("old source authority changed during transition")
        if record["phase"] in ("quiescing", "quiesced"):
            save(config, record, "quiesced")
            backup, accepted = capture_source(config, record, leases)
            record.update({"backup": str(backup), "source_acceptance_sha256": application_backup.digest(backup / "acceptance.json"),
                           "semantic_sha256": accepted["semantic_sha256"]})
            save(config, record, "captured")
        with pin_source(config, record, leases) as pinned:
            return prepare_target(config, record, target, pinned)


def prepare_target(config, record, target, leases):
    if not Path(config["independent_receipt"]).exists():
        return {**record, "status": "awaiting-independent-restore"}
    _, independent_hash = evidence(config, record)
    record["independent_sha256"] = independent_hash
    if record["phase"] not in ("imported", "prepared"):
        save(config, record, "importing")
        run_action(config, "import", record, leases)
    semantic(run_action(config, "verify-target", record, leases), record["semantic_sha256"])
    evidence(config, record)
    if record["phase"] != "prepared":
        save(config, record, "imported")
    record["primary_snapshot_sha256"] = primary_evidence(config, record, leases)
    expected = resource.contract(target)
    prepared = {**expected, "identity": record["source_authority"]["identity"]}
    if record.get("target_authority") is not None and record["target_authority"] != prepared:
        raise ValueError("prepared target authority evidence changed")
    record["target_authority"] = prepared
    custody = target_custody(config, record, target, leases)
    if record.get("custody") is not None and any(custody[key] != value for key, value in record["custody"].items() if key != "completed_at"):
        raise ValueError("prepared target corpus evidence changed")
    record.setdefault("custody", custody)
    save(config, record, "prepared")
    return record


def publish(config, target, expected, old):
    state = resource.state_directory(target)
    current = read_json(state / "identity.json")
    if current not in (old, expected):
        raise ValueError("authority compare-and-swap rejected a different live identity")
    marker = {"resource": target["resource"], "identity": expected["identity"]}
    for directory in expected["directories"]:
        path = resource.anchor(target, directory)
        if path.exists() and read_json(path) != marker:
            raise ValueError("target storage has a conflicting identity")
        write_owned(path, marker)
    write_owned(state / "identity.json", expected)


def commit(config):
    with transaction(config) as (record, source, target, leases), fence(config, record, leases) as leases:
        if record["phase"] not in ("prepared", "committing", "committed"):
            raise ValueError("authority commit requires a prepared transition")
        inspect_barriers(config)
        if generation() != record["candidate"]:
            raise ValueError("the exact accepted generation is not selected")
        evidence(config, record)
        primary_evidence(config, record, leases)
        semantic(run_action(config, "verify-target", record, leases), record["semantic_sha256"])
        verify_custody(config, record, target, leases)
        if record["phase"] != "committed":
            save(config, record, "committing")
            publish(config, target, record["target_authority"], record["source_authority"])
            if record["custody"] is not None:
                entry = read_json(config["custody_manifest"])
                write_owned(entry["custody_file"], record["custody"])
            save(config, record, "committed")
        resource.verify(target, resource.contract(target))
        return record


def bind_candidate(config, candidate):
    with transaction(config) as (record, source, target, leases):
        if record["phase"] != "prepared" or candidate_path(candidate, config) != "generation":
            raise ValueError("bind the realized generation only after transition preparation")
        if record["candidate"] is not None and record["candidate"] != candidate:
            raise ValueError("a different generation is already bound")
        inspect_barriers(config)
        evidence(config, record)
        record["candidate"] = candidate
        save(config, record)
        return record


def admission(config, phase, expected, candidate=None):
    """Target contract admission while authority publication remains pending."""
    record = status(config)
    if record["phase"] not in ("prepared", "committing", "committed") or phase not in ("preflight", "activate"):
        raise ValueError("prepared transition cannot authorize ordinary startup or incomplete preparation")
    _, target = validate(config)
    if target != expected:
        raise ValueError("candidate transition authority differs from the prepared target contract")
    if phase == "activate" and record["candidate"] is None:
        raise ValueError("activation requires an explicitly bound realized generation")
    if phase == "activate" and candidate != record["candidate"]:
        raise ValueError("activation generation differs from the bound candidate")
    require_root()
    with transaction(config) as (record, source, target, leases), fence(config, record, leases) as leases:
        inspect_barriers(config)
        evidence(config, record)
        primary_evidence(config, record, leases)
        semantic(run_action(config, "verify-target", record, leases), record["semantic_sha256"])
        verify_custody(config, record, target, leases)
        if record["phase"] in ("prepared", "committing"):
            current = read_json(Path(source["state_dir"]) / "identity.json")
            if current not in (record["source_authority"], record["target_authority"]):
                raise ValueError("prepared transition authority changed")
        else:
            resource.verify(target, resource.contract(target))
        return {"status": "prepared-with-writers-inhibited", "resource": config["resource"],
                "candidate": record["candidate"], "semantic_sha256": record["semantic_sha256"],
                "database_snapshot_sha256": record["primary_snapshot_sha256"],
                "database_requirements": [] if record["custody"] is None else record["custody"]["database_requirements"]}


def enable_writes(config):
    with transaction(config) as (record, source, target, leases):
        if record["phase"] not in ("committed", "write-enabled"):
            raise ValueError("writer release requires committed authority")
        if generation() != record["candidate"]:
            raise ValueError("writer release generation differs")
        if record["phase"] != "write-enabled":
            with fence(config, record, leases) as pinned:
                inspect_barriers(config)
                evidence(config, record)
                primary_evidence(config, record, pinned)
                semantic(run_action(config, "verify-target", record, pinned), record["semantic_sha256"])
                verify_custody(config, record, target, pinned)
                resource.verify(target, resource.contract(target))
                # Durable point of no return BEFORE any app write is possible.
                save(config, record, "write-enabled")
        release_barriers(config, record)
        return {**record, "status": "write-enabled", "borrowed_fence_release_required": config["postgres_manifest"] is not None}


def complete(config):
    # A running writer owns a shared authority lease. Completion only inspects;
    # it must never require another exclusive lease or old record equality.
    require_root()
    with lock(Path(config["barrier_dir"]) / "lock") as transition_lease:
        record = status(config)
        if record["phase"] not in ("write-enabled", "complete") or generation() != record["candidate"]:
            raise ValueError("completion requires the selected write-enabled generation")
        _, target = validate(config)
        with resource.inspection(target):
            health = run_action(config, "health", record, [transition_lease])
            if health != {"version": 1, "status": "healthy"}:
                raise ValueError("target application health acceptance failed")
            save(config, record, "complete")
        return record


def abort(config):
    with transaction(config) as (record, source, target, leases):
        if record["phase"] == "aborted":
            release_barriers(config, record, aborted=True)
            return record
        if record["phase"] in ("write-enabled", "complete"):
            raise ValueError("writers may have acknowledged changes; a fresh reverse transition is required")
        if generation() != record["source_generation"]:
            raise ValueError("abort requires the exact retained source generation selected with writers inhibited")
        with fence(config, record, leases) as pinned:
            if "backup" in record:
                source_bytes(config, record)
                semantic(run_action(config, "verify-source", record, pinned), record["semantic_sha256"])
            if record["phase"] in ("committing", "committed", "aborting"):
                save(config, record, "aborting")
                publish(config, source, record["source_authority"], record["target_authority"])
                if record["custody"] is not None:
                    path = Path(read_json(config["custody_manifest"])["custody_file"])
                    current = read_json(path) if path.exists() else None
                    if current not in (record["custody"], record["source_custody"]):
                        raise ValueError("custody compare-and-swap rejected changed evidence")
                    if record["source_custody"] is not None:
                        write_owned(path, record["source_custody"])
                    elif path.exists():
                        path.unlink()
                        sync_directory(path.parent)
            resource.verify(source, resource.contract(source))
            save(config, record, "aborted")
            release_barriers(config, record, aborted=True)
            return record


def retire(config):
    """Retain terminal history before allowing a new, independently proven intent."""
    with transaction(config) as (record, source, target, leases):
        if record["phase"] not in ("complete", "aborted"):
            raise ValueError("unfinished transitions cannot be retired")
        active = target if record["phase"] == "complete" else source
        resource.verify(active, resource.contract(active))
        archive = Path(config["barrier_dir"]) / (application_backup.identity(record["intent"]) + ".history.json")
        if archive.exists() and read_json(archive) != record:
            raise ValueError("transition history conflicts")
        write_json(archive, record)
        journal_path(config).unlink()
        sync_directory(journal_path(config).parent)
        return {"status": "retired", "history": str(archive)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    commands = parser.add_subparsers(dest="command", required=True)
    planning = commands.add_parser("plan")
    planning.add_argument("--candidate", required=True)
    planning.add_argument("--writer-fence-token")
    binding = commands.add_parser("bind-candidate")
    binding.add_argument("--candidate", required=True)
    for name in ("status", "prepare", "commit", "enable-writes", "complete", "abort", "retire"):
        commands.add_parser(name)
    args = parser.parse_args()
    try:
        config = read_json(args.config)
        if not str(args.config).startswith("/nix/store/") or any(not str(config[key]).startswith("/nix/store/")
                for key in ("source_manifest", "target_manifest", "backup_manifest")):
            raise ValueError("operator transition manifests must be immutable store files")
        if args.command == "plan":
            result = plan(config, args.candidate, args.writer_fence_token)
        elif args.command == "bind-candidate":
            result = bind_candidate(config, args.candidate)
        else:
            function = {"status": status, "prepare": prepare, "commit": commit, "enable-writes": enable_writes,
                        "complete": complete, "abort": abort, "retire": retire}[args.command]
            result = function(config)
        print(json.dumps(result, sort_keys=True))
    except (OSError, ValueError, KeyError, TypeError, RuntimeError) as error:
        print(f"harbor-db-transition: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
