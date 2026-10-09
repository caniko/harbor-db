"""Real borrowed-fence import and whole-primary recovery, with independent app restore."""

import json
import shlex


def publish(host, path, value):
    host.succeed("printf '%s' " + shlex.quote(json.dumps(value)) + " > " + shlex.quote(path))


def align(receiver, sender):
    seconds = max(int(host.succeed("date +%s").strip()) for host in (receiver, sender))
    receiver.succeed(f"date --set=@{seconds}")


start_all()
primary.wait_for_unit("postgresql.service")
certifier.wait_for_unit("multi-user.target")
control = "runuser -u postgres -- psql -XwqAt -v ON_ERROR_STOP=1"
primary.succeed(control + " -c \"CREATE ROLE demo_owner NOLOGIN; CREATE ROLE demo_runtime LOGIN; CREATE TABLE documents(id int PRIMARY KEY, body text); ALTER TABLE documents OWNER TO demo_owner; GRANT SELECT ON documents TO demo_runtime\"")
client = "runuser -u demo -- psql -Xw -U demo_runtime -d postgres -c 'SELECT * FROM documents'"
primary.succeed(client)

# Actual primary identity is runtime-created. Add the resolved contracts to the
# guest's real Nix store rather than treating a mutable template as immutable.
identifier = primary.succeed(control + " -c 'SELECT system_identifier FROM pg_control_system()'").strip()
template = primary.succeed("readlink -f /etc/harbor-db/demo-transition.json").strip()
selected = json.loads(primary.succeed("cat " + template))
source, target = selected["source_manifest"], selected["target_manifest"]
database = json.loads(primary.succeed("cat " + selected["postgres_manifest"]))
database["recovery"]["system_identifier"] = identifier
publish(primary, "/run/demo-primary.json", database)
database_path = primary.succeed("nix-store --add /run/demo-primary.json").strip()
selected["postgres_manifest"] = database_path
publish(primary, "/run/demo-transition.json", selected)
contract = primary.succeed("nix-store --add /run/demo-transition.json").strip()
pg = "runuser -u postgres -- harbor-db-postgres --config " + database_path
transition = "harbor-db-transition --config " + contract

primary.succeed("runuser -u demo -- sh -c 'printf source-revision-seven > /var/lib/demo-old/records'")
primary.succeed("runuser -u demo -- harbor-db-resource --config " + source + " adopt --identity retained-resource")
primary.succeed("systemctl start demo.service")
primary.wait_for_unit("demo.service")
primary.succeed("systemctl stop postgresql.service")
token = json.loads(primary.succeed(pg + " fence-open --system-identifier " + identifier))["token"]
primary.fail(pg + " inspect-fence --token " + token)
primary.succeed("systemctl start postgresql.service")
primary.wait_for_unit("postgresql.service")
primary.succeed(pg + " inspect-fence --token " + token)
primary.fail(client)
primary.succeed(transition + " plan --candidate " + contract + " --writer-fence-token " + token)
result = json.loads(primary.succeed(transition + " prepare"))
assert result["status"] == "awaiting-independent-restore"
primary.fail("systemctl is-active demo.service")
primary.succeed("systemctl start demo.service")
primary.fail("systemctl is-active demo.service")
primary.fail("runuser -u demo -- harbor-db-resource --config " + source + " check")

point = result["backup"].split("/")[-1]
archive = primary.succeed(f"tar -C /var/lib/demo-backups -czf - {point} | base64 -w0").strip()
certifier.succeed(f"printf '%s' '{archive}' | base64 -d | tar -xzf - -C /var/lib/demo-backups")
align(certifier, primary)
backup_contract = certifier.succeed("readlink -f /etc/harbor-db/demo-backup.json").strip()
proof = json.loads(certifier.succeed(f"runuser -u demo -- harbor-db-application-backup --config {backup_contract} certify /var/lib/demo-backups/{point} --state /var/lib/demo-certifier"))
encoded = certifier.succeed(f"base64 -w0 /var/lib/demo-certifier/{proof['source_acceptance_sha256']}.json").strip()
primary.succeed(f"printf '%s' '{encoded}' | base64 -d > /var/lib/demo-authority/independent.json")
align(primary, certifier)

# Import is retained while whole-primary recovery acceptance is absent. The old
# authority remains published and ordinary clients remain inhibited.
primary.fail(transition + " prepare")
assert json.loads(primary.succeed(transition + " status"))["phase"] == "imported"
primary.succeed("test $(cat /var/lib/demo-new/import-count) = 1")
primary.succeed("grep filesystem-source /var/lib/demo-authority/identity.json")
primary.fail("runuser -u demo -- harbor-db-resource --config " + target + " check")
primary.fail(client)

# Execute the existing physical recovery owner after import, at this same token.
base = "/var/lib/demo-primary-backup"
primary.succeed(f"runuser -u postgres -- pg_basebackup -h /run/postgresql -U postgres -D {base}/base/base-1 -X stream --checkpoint=fast")
stop_lsn = json.loads(primary.succeed(f"cat {base}/base/base-1/backup_manifest"))["WAL-Ranges"][0]["End-LSN"]
target_lsn = primary.succeed(control + " -c \"SELECT pg_create_restore_point('transition_import')\"").strip()
primary.succeed(control + " -c 'SELECT pg_switch_wal()'")
meta = {"backup_id": "base-1", "system_identifier": identifier, "pg_major": 18,
        "epoch_id": "import", "backup_stop_lsn": stop_lsn, "post_backup_lsn": target_lsn}
publish(primary, base + "/base/base-1.meta.json", meta)
primary.succeed(f"printf base-1 > {base}/LAST_SUCCESS; chown -R postgres:postgres {base}")
primary.succeed(pg + " snapshot-records --socket-dir /run/postgresql --port 5432")
primary.succeed(f"runuser -u postgres -- cp -a {base}/base/base-1 /var/lib/demo-recovered/18; cp /var/lib/postgres/18/pg_wal/0000000* /var/lib/demo-restore-wal/; chown -R postgres:postgres /var/lib/demo-recovered /var/lib/demo-restore-wal")
restored = ("listen_addresses = ''\nunix_socket_directories = '/var/lib/demo-recovery-socket'\nport = 55432\n"
            "restore_command = '/run/current-system/sw/bin/cp /var/lib/demo-restore-wal/%f %p'\n"
            "recovery_target_lsn = '" + target_lsn + "'\nrecovery_target_action = 'promote'\ndefault_transaction_read_only = on\n")
primary.succeed("printf '%s' " + shlex.quote(restored) + " > /var/lib/demo-recovered/18/postgresql.conf; touch /var/lib/demo-recovered/18/recovery.signal; chown postgres:postgres /var/lib/demo-recovered/18/postgresql.conf /var/lib/demo-recovered/18/recovery.signal")
primary.succeed("runuser -u postgres -- pg_ctl -D /var/lib/demo-recovered/18 -l /dev/null -w start")
primary.wait_until_succeeds("runuser -u postgres -- psql -h /var/lib/demo-recovery-socket -p 55432 -Atqc 'SELECT NOT pg_is_in_recovery()' | grep -qx t")
primary.succeed(pg + " certify-recovery --data-dir /var/lib/demo-recovered/18 --socket-dir /var/lib/demo-recovery-socket --port 55432")
assert json.loads(primary.succeed(transition + " prepare"))["phase"] == "prepared"
primary.succeed("test $(cat /var/lib/demo-new/import-count) = 1")
primary.succeed(pg + " inspect-recovery --socket-dir /run/postgresql --port 5432")

# Equal table counts cannot hide changed records; successful import is not rerun.
primary.succeed(control + " -c \"UPDATE documents SET body='lost-review'\"")
primary.fail(pg + " inspect-recovery --socket-dir /run/postgresql --port 5432")
primary.fail(transition + " prepare")
primary.succeed(control + " -c \"UPDATE documents SET body='source-revision-seven'\"")
primary.succeed(transition + " prepare")
primary.succeed("test $(cat /var/lib/demo-new/import-count) = 1")
primary.succeed(transition + " abort")
primary.succeed("runuser -u demo -- harbor-db-resource --config " + source + " check")
primary.succeed(pg + " inspect-fence --token " + token)
primary.fail(client)
primary.succeed(transition + " retire")
primary.succeed("runuser -u postgres -- pg_ctl -D /var/lib/demo-recovered/18 -w stop")
primary.succeed("systemctl stop postgresql.service")
primary.succeed(pg + " fence-close --token " + token)
primary.succeed("systemctl start postgresql.service")
primary.wait_for_unit("postgresql.service")
primary.succeed(client)
