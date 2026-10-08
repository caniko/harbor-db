{pkgs}: let
  tool = import ./postgres-package.nix {inherit pkgs;};
  template = pkgs.writeText "recovery-template.json" (builtins.toJSON {
    resource = "recovery-fixture";
    data_dir = "/var/lib/postgres/18";
    state_dir = "/srv/authority";
    major = "18";
    package = toString pkgs.postgresql_18;
    required_mounts = [];
    recovery = {
      system_identifier = "12345";
      source_hostname = "primary";
      backup_root = "/srv/backup";
      snapshot_file = "/srv/backup/evidence/records.json";
      receipt_file = "/srv/backup/evidence/recovery.json";
      off_host_receipt_file = "/srv/backup/evidence/off-host.json";
      max_age_seconds = 3600;
      verify_timeout_seconds = 60;
      record_checks = [
        {
          name = "saves";
          database = "postgres";
          sql = "SELECT mutation,geometry,review,revision FROM saves ORDER BY mutation";
        }
      ];
    };
  });
  node = {pkgs, ...}: {
    virtualisation.memorySize = 1024;
    environment.systemPackages = [tool pkgs.postgresql_18 pkgs.python3 pkgs.jq];
    environment.etc."recovery-template.json".source = template;
    users.users.postgres = {
      isSystemUser = true;
      group = "postgres";
    };
    users.groups.postgres = {};
    systemd.tmpfiles.rules = [
      "d /srv/backup 0700 postgres postgres -"
      "d /srv/backup/base 0700 postgres postgres -"
      "d /srv/backup/locks 0700 postgres postgres -"
      "f /srv/backup/locks/mutate 0600 postgres postgres -"
      "d /srv/backup/evidence 0700 postgres postgres -"
      "d /srv/recovered 0700 postgres postgres -"
      "d /srv/restore-wal 0700 postgres postgres -"
      "d /srv/recovery-socket 0700 postgres postgres -"
      "d /srv/authority 0700 postgres postgres -"
    ];
  };
in
  pkgs.testers.runNixOSTest {
    name = "harbor-db-postgres-recovery-acceptance";
    nodes = {
      primary = {
        imports = [node];
        # A custom dataDir is consumer-provisioned, unlike the NixOS default.
        # The hardened PostgreSQL unit binds this path before its pre-start code.
        systemd.tmpfiles.rules = ["d /var/lib/postgres/18 0700 postgres postgres -"];
        services.postgresql = {
          enable = true;
          package = pkgs.postgresql_18;
          dataDir = "/var/lib/postgres/18";
          authentication = pkgs.lib.mkBefore "local replication postgres peer\n";
        };
      };
      remote = {
        imports = [node];
        users.users.can = {
          isNormalUser = true;
          uid = 1000;
        };
      };
    };
    testScript = ''
      import json
      import shlex

      start_all()
      try:
          primary.wait_for_unit("postgresql.service")
      except Exception:
          print(primary.execute("systemctl status postgresql.service --no-pager -l; journalctl -u postgresql.service --no-pager -n 80"))
          raise
      remote.wait_for_unit("multi-user.target")
      # Independent VM clocks can advance at different rates under hosted QEMU.
      # Disable time synchronisation and move only the receiving fixture clock
      # forward at each evidence handoff; do not relax production freshness or
      # alter any receipt timestamp.
      for host in (primary, remote):
          host.succeed("systemctl stop systemd-timesyncd.service")
      def align_receiver(receiver, sender):
          seconds = max(int(host.succeed("date +%s").strip()) for host in (receiver, sender))
          receiver.succeed(f"date --set=@{seconds}")
      primary.succeed("runuser -u postgres -- psql -v ON_ERROR_STOP=1 -c \"CREATE TABLE saves (mutation text PRIMARY KEY, geometry jsonb, review text, revision bigint); INSERT INTO saves VALUES ('ack-1', '{\\\"circle\\\":[10,20,30]}', 'reviewed', 42)\"")
      identifier = primary.succeed("runuser -u postgres -- psql -Atqc 'SELECT system_identifier FROM pg_control_system()'").strip()
      for host in (primary, remote):
          host.succeed(f"sed 's/12345/{identifier}/g' /etc/recovery-template.json > /srv/config.json; chown postgres:postgres /srv/config.json; chmod 0600 /srv/config.json")
      command = "runuser -u postgres -- harbor-db-postgres --config /srv/config.json"
      primary.succeed("runuser -u postgres -- pg_basebackup -h /run/postgresql -U postgres -D /srv/backup/base/base-1 -X stream --checkpoint=fast")
      stop_lsn = json.loads(primary.succeed("cat /srv/backup/base/base-1/backup_manifest"))["WAL-Ranges"][0]["End-LSN"]
      target = primary.succeed("runuser -u postgres -- psql -Atqc \"SELECT pg_create_restore_point('recovery_acceptance')\"").strip()
      primary.succeed("runuser -u postgres -- psql -Atqc 'SELECT pg_switch_wal()'")
      meta = {"backup_id": "base-1", "system_identifier": identifier, "pg_major": 18, "epoch_id": "fixture-1", "backup_stop_lsn": stop_lsn, "post_backup_lsn": target}
      primary.succeed("printf '%s\\n' " + shlex.quote(json.dumps(meta)) + " > /srv/backup/base/base-1.meta.json; printf 'base-1\\n' > /srv/backup/LAST_SUCCESS; chown -R postgres:postgres /srv/backup")
      primary.fail(f"{command} inspect-recovery")
      primary.fail(f"{command} adopt-live --system-identifier {identifier}")
      primary.succeed("test ! -e /srv/authority/identity.json; test ! -e /srv/authority/lock")
      primary.succeed(f"{command} snapshot-records --socket-dir /run/postgresql --port 5432")
      # Copy completed WAL into the disposable restore only, never modify the
      # verified base backup. Reaching target is verified through SQL below.
      primary.succeed("runuser -u postgres -- cp -a /srv/backup/base/base-1 /srv/recovered/18; cp /var/lib/postgres/18/pg_wal/0000000* /srv/restore-wal/; chown -R postgres:postgres /srv/recovered /srv/restore-wal")
      recovery_config = "listen_addresses = 'localhost'\nunix_socket_directories = '/srv/recovery-socket'\nport = 55432\nrestore_command = '${pkgs.coreutils}/bin/cp /srv/restore-wal/%f %p'\nrecovery_target_lsn = '" + target + "'\nrecovery_target_action = 'promote'\ndefault_transaction_read_only = on\n"
      primary.succeed("printf '%s' " + shlex.quote(recovery_config) + " > /srv/recovered/18/postgresql.conf; touch /srv/recovered/18/recovery.signal; chown postgres:postgres /srv/recovered/18/postgresql.conf /srv/recovered/18/recovery.signal")
      primary.succeed("tar -C /srv -cf /tmp/recovery.tar backup recovered restore-wal")
      try:
          primary.succeed("runuser -u postgres -- ${pkgs.postgresql_18}/bin/pg_ctl -p ${pkgs.postgresql_18}/bin/postgres -D /srv/recovered/18 -l /srv/recovered/server.log -w start")
      except Exception:
          print(primary.succeed("cat /srv/recovered/server.log"))
          raise
      primary.wait_until_succeeds("runuser -u postgres -- psql -h /srv/recovery-socket -p 55432 -Atqc 'SELECT NOT pg_is_in_recovery()' | grep -qx t")
      primary.succeed(f"{command} certify-recovery --data-dir /srv/recovered/18 --socket-dir /srv/recovery-socket --port 55432")
      primary.fail(f"{command} inspect-recovery")  # Missing independent off-host execution.
      # The explicit managed retry must reuse local evidence and fail before
      # adoption without running the backup/restore commands again.
      preparation = {"readiness_command": ["${pkgs.coreutils}/bin/true"], "backup_command": ["${pkgs.coreutils}/bin/false"], "restore_command": ["${pkgs.coreutils}/bin/false"]}
      primary.succeed("printf '%s\\n' " + shlex.quote(json.dumps(preparation)) + " > /srv/preparation.json; chown postgres:postgres /srv/preparation.json")
      primary.fail(f"{command} prepare-recovery --preparation-config /srv/preparation.json --socket-dir /run/postgresql --port 5432")
      primary.succeed("test ! -e /srv/authority/identity.json; test $(cat /srv/backup/LAST_SUCCESS) = base-1")

      # Execute against the same restored bytes on a second, independently
      # named host, rather than copying the primary's local success receipt.
      primary.succeed("runuser -u postgres -- pg_ctl -D /srv/recovered/18 -w stop")
      # The driver's target directory is relative to its retained output and
      # shared transport, not an arbitrary absolute host temporary directory.
      primary.copy_from_machine("/tmp/recovery.tar", "recovery-transfer")
      remote.copy_from_host(str(primary.out_dir / "recovery-transfer/recovery.tar"), "/tmp/recovery.tar")
      remote.succeed("tar -C /srv -xf /tmp/recovery.tar; chown -R can:users /srv/backup /srv/recovered /srv/restore-wal /srv/recovery-socket")
      align_receiver(remote, primary)
      # A can-owned restore cannot inherit Atlas's peer auth (can -> can), and
      # its socket is private. Keep authentication/config overrides disposable.
      remote.succeed("printf 'local all postgres peer map=recovery\\n' > /srv/recovered/pg_hba.conf; printf 'recovery can postgres\\n' > /srv/recovered/pg_ident.conf; chown can:users /srv/recovered/pg_*.conf; chmod 0600 /srv/recovered/pg_*.conf")
      try:
          remote.succeed("runuser -u can -- ${pkgs.postgresql_18}/bin/pg_ctl -p ${pkgs.postgresql_18}/bin/postgres -D /srv/recovered/18 -o \"-c data_directory=/srv/recovered/18 -c listen_addresses= -c hba_file=/srv/recovered/pg_hba.conf -c ident_file=/srv/recovered/pg_ident.conf\" -l /srv/recovered/remote.log -w start")
      except Exception:
          print(remote.succeed("cat /srv/recovered/remote.log"))
          raise
      remote.wait_until_succeeds("runuser -u can -- psql -U postgres -h /srv/recovery-socket -p 55432 -Atqc 'SELECT NOT pg_is_in_recovery()' | grep -qx t")
      remote.succeed("jq '.recovery.receipt_file = .recovery.off_host_receipt_file' /srv/config.json > /srv/remote-config.json; chown can:users /srv/remote-config.json")
      remote.succeed("runuser -u can -- harbor-db-postgres --config /srv/remote-config.json certify-recovery --data-dir /srv/recovered/18 --socket-dir /srv/recovery-socket --port 55432")
      remote.copy_from_machine("/srv/backup/evidence/off-host.json", "recovery-transfer")
      primary.copy_from_host(str(remote.out_dir / "recovery-transfer/off-host.json"), "/srv/incoming/recovery-off-host")
      primary.succeed("chown -R postgres:postgres /srv/incoming")
      align_receiver(primary, remote)
      primary.succeed("runuser -u postgres -- env CREDENTIALS_DIRECTORY=/srv/incoming harbor-db-postgres --config /srv/config.json prepare-recovery --preparation-config /srv/preparation.json --socket-dir /run/postgresql --port 5432")
      primary.succeed(f"{command} inspect-recovery")
      primary.succeed(f"{command} adopt-live --system-identifier {identifier}")
      primary.succeed("test -s /srv/authority/identity.json")

      # Execute the mandatory dispatcher with real service-user PostgreSQL and
      # the same independently restored database evidence. Corpus initialization
      # below is disposable fixture setup, never an admission behavior.
      primary.succeed("install -d -o postgres -g postgres -m 0700 /srv/corpus /srv/corpus-restore /srv/corpus-authority; printf historical-objects > /srv/corpus/history; cp -p /srv/corpus/history /srv/corpus-restore/history; chown postgres:postgres /srv/corpus/history /srv/corpus-restore/history")
      cutover = {
          "version": 1, "enforced": True, "host": "primary", "timeout_seconds": 30,
          "resources": {
              "postgresql": {"kind": "postgres", "user": "postgres", "config": "/srv/config.json", "compatibility_checks": [{"database": "postgres", "sql": "SELECT count(*) = 1 FROM saves"}], "corpus_checks": {"archive": [{"root": 0, "database": "postgres", "sql": "SELECT jsonb_build_array(jsonb_build_object('path', 'history', 'directory', false))"}]}},
              "archive": {
                  "kind": "filesystem", "user": "postgres", "runtime_units": [], "database_resource": "postgresql",
                  "login_shell": "${pkgs.bash}/bin/bash",
                  "custody_file": "/srv/corpus-authority/custody.json", "max_age_seconds": 3600,
                  "authority": {"resource": "archive", "state_dir": "/srv/corpus-authority", "directories": ["/srv/corpus"], "binding": {"backend": "postgres"}},
              },
          },
      }
      def publish_cutover():
          primary.succeed("printf '%s\\n' " + shlex.quote(json.dumps(cutover)) + " > /srv/cutover.json; chmod 0644 /srv/cutover.json")
      publish_cutover()
      gate = "harbor-db-cutover check --contract /srv/cutover.json --host primary"
      primary.fail(gate)
      primary.succeed("test ! -e /srv/corpus-authority/identity.json")
      primary.succeed("harbor-db-cutover certify --contract /srv/cutover.json --host primary --resource archive --identity accepted-history --restore-root /srv/corpus-restore")
      primary.succeed(gate)
      primary.succeed(gate + " --phase activate")
      # External SSH commands must hold custody even with the application unit
      # stopped. Exercise the actual packaged login shell and inherited lease.
      primary.succeed("install -D -m 0644 /srv/cutover.json /etc/harbor-db/cutover.json")
      primary.succeed("runuser -u postgres -- harbor-db-cutover-shell -c 'test -s /srv/corpus/history'")
      primary.succeed("systemd-run --unit=custody-ssh-writer --property=User=postgres ${tool}/bin/harbor-db-cutover-shell -c '${pkgs.coreutils}/bin/touch /srv/corpus-authority/ssh-ready; exec ${pkgs.coreutils}/bin/sleep infinity'")
      primary.wait_for_unit("custody-ssh-writer.service")
      primary.wait_until_succeeds("test -e /srv/corpus-authority/ssh-ready")
      primary.fail("harbor-db-cutover certify --contract /srv/cutover.json --host primary --resource archive --identity accepted-history --restore-root /srv/corpus-restore")
      primary.succeed("systemctl stop custody-ssh-writer.service")
      cutover["resources"]["postgresql"]["corpus_checks"]["archive"][0]["sql"] = "SELECT jsonb_build_array(jsonb_build_object('path', 'missing-database-repository', 'directory', false))"
      publish_cutover()
      primary.fail("harbor-db-cutover certify --contract /srv/cutover.json --host primary --resource archive --identity accepted-history --restore-root /srv/corpus-restore")
      primary.fail(gate)
      cutover["resources"]["postgresql"]["corpus_checks"]["archive"][0]["sql"] = "SELECT jsonb_build_array(jsonb_build_object('path', 'history', 'directory', false))"
      publish_cutover()
      cutover["resources"]["postgresql"]["compatibility_checks"][0]["sql"] = "SELECT false"
      publish_cutover()
      primary.fail(gate)
      cutover["resources"]["postgresql"]["compatibility_checks"][0]["sql"] = "SELECT count(*) = 1 FROM saves"
      publish_cutover()
      primary.succeed("mv /srv/corpus /srv/corpus-retained")
      primary.fail(gate)
      primary.succeed("test ! -e /srv/corpus; mv /srv/corpus-retained /srv/corpus")
      primary.succeed("printf ordinary-new-state > /srv/corpus/history")
      primary.succeed(gate + " --phase startup")
      primary.fail(gate + " --phase activate")
      primary.succeed("cp -p /srv/corpus-restore/history /srv/corpus/history; chown postgres:postgres /srv/corpus/history")
      primary.succeed("harbor-db-cutover certify --contract /srv/cutover.json --host primary --resource archive --identity accepted-history --restore-root /srv/corpus-restore")
      primary.succeed(gate)

      # Same table count cannot hide a changed review, and a corrupt retained
      # backup cannot reuse a formerly successful recovery receipt.
      remote.succeed("runuser -u can -- psql -U postgres -h /srv/recovery-socket -p 55432 -v ON_ERROR_STOP=1 -c \"BEGIN READ WRITE; UPDATE saves SET review = 'lost-review'; COMMIT\"")
      remote.fail("runuser -u can -- harbor-db-postgres --config /srv/remote-config.json certify-recovery --data-dir /srv/recovered/18 --socket-dir /srv/recovery-socket --port 55432")
      manifest = json.loads(primary.succeed("cat /srv/backup/base/base-1/backup_manifest"))
      corpus_file = next(item["Path"] for item in manifest["Files"] if item["Path"].startswith("base/"))
      primary.succeed("printf corruption >> " + shlex.quote("/srv/backup/base/base-1/" + corpus_file))
      primary.succeed(gate)  # Early filter deliberately defers full backup bytes.
      primary.fail(gate + " --phase activate")
      primary.succeed("printf corrupt >> /srv/backup/base/base-1/PG_VERSION")
      primary.fail(f"{command} inspect-recovery")
    '';
  }
