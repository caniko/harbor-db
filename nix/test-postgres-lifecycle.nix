{
  pkgs,
  nativePackage ? import ./native-package.nix {inherit pkgs;},
  interop ? false,
}: let
  tool = nativePackage;
  legacyTool = import ./test-python-package.nix {inherit pkgs;};
  sql = pkgs.writeText "acknowledged-save.sql" ''
    CREATE TABLE IF NOT EXISTS saves (
      mutation text PRIMARY KEY, geometry jsonb NOT NULL, review text NOT NULL,
      binding text NOT NULL, receipt text NOT NULL, revision bigint NOT NULL
    );
    BEGIN;
    INSERT INTO saves VALUES ('ack-1', '{"circle":[10,20,30]}', 'reviewed',
      'recording-1', 'receipt-1', 42) ON CONFLICT DO NOTHING;
    COMMIT;
  '';
in
  pkgs.testers.runNixOSTest {
    extraDriverArgs = ["--junit-xml" "junit.xml"];
    name = "harbor-db-postgres-crash-rollback";
    nodes.machine = {
      config,
      lib,
      ...
    }: {
      imports = [./postgres-lifecycle.nix];
      _module.args.harborDbStoragePackage = nativePackage;
      virtualisation.memorySize = 1024;
      services.postgresql = {
        enable = true;
        package = pkgs.postgresql_18;
        dataDir = "/var/lib/postgres/18";
      };
      services.harbor-db.postgresql.enable = true;
      environment.systemPackages = [tool pkgs.postgresql_18 pkgs.python3];
      system.extraDependencies = pkgs.lib.optional interop legacyTool;
      # Model the existing unguarded primary for first-rollout live adoption.
      systemd.services.fixture-existing-postgresql = {
        serviceConfig = {
          User = "postgres";
          Group = "postgres";
          RuntimeDirectory = "postgresql";
          ExecStart = "${pkgs.postgresql_18}/bin/postgres -D /var/lib/postgres/18 -c unix_socket_directories=/run/postgresql";
        };
      };
      specialisation.compat.configuration.services.postgresql.settings.track_io_timing = true;
      specialisation.adoption.configuration = {
        # The test substitutes the independently inspected fixture identifier
        # into this generated hook, then uses the real NixOS switch executable.
        services.harbor-db.postgresql.switchAdoption.systemIdentifier = "12345";
        systemd.services.fixture-existing-postgresql.enable = false;
      };
      environment.etc = lib.mkIf (!config.isSpecialisation) {
        "harbor-db/adoption-check".source = config.specialisation.adoption.configuration.system.preSwitchChecksScript;
      };
    };
    testScript = ''
      start_all()
      machine.wait_for_unit("multi-user.target")
      # First provisioning is explicit. Normal boot cannot adopt an empty DB.
      machine.fail("systemctl start postgresql")
      machine.succeed("test ! -e /var/lib/postgres/18/PG_VERSION")
      machine.succeed("systemctl stop postgresql")
      machine.succeed("install -d -o postgres -g postgres -m 0700 /var/lib/postgres/18")
      machine.succeed("runuser -u postgres -- initdb -D /var/lib/postgres/18")
      identifier = machine.succeed("runuser -u postgres -- pg_controldata /var/lib/postgres/18 | sed -n 's/^Database system identifier: *//p'").strip()
      machine.succeed("systemctl start fixture-existing-postgresql")
      machine.wait_until_succeeds("runuser -u postgres -- pg_isready -h /run/postgresql")
      machine.succeed(f"runuser -u postgres -- harbor-db-postgres --config /etc/harbor-db/postgresql.json inspect-live --system-identifier {identifier}")
      machine.succeed("test ! -e /var/lib/harbor-db/postgresql/identity.json")
      base_system = machine.succeed("readlink -f /run/current-system").strip()
      candidate = f"{base_system}/specialisation/adoption"
      original_hook = machine.succeed("readlink -f /etc/harbor-db/adoption-check").strip()
      # Parameterize only the independently verified identifier in the real
      # generated hook. A wrong identity must abort before stopping the primary.
      machine.succeed(f"cp {original_hook} /run/adoption-check; chmod +x /run/adoption-check")
      machine.succeed(f"sed 's|{original_hook}|/run/adoption-check|g' {candidate}/bin/switch-to-configuration > /run/adoption-switch; chmod +x /run/adoption-switch")
      existing_pid = machine.succeed("systemctl show fixture-existing-postgresql -p MainPID --value").strip()
      machine.fail("/run/adoption-switch test")
      assert machine.succeed("systemctl show fixture-existing-postgresql -p MainPID --value").strip() == existing_pid
      machine.succeed("test ! -e /var/lib/harbor-db/postgresql/identity.json")
      machine.succeed(f"sed -i 's/--system-identifier 12345/--system-identifier {identifier}/g' /run/adoption-check")
      for action in ("boot", "dry-activate"):
          machine.succeed(f"/run/adoption-check {candidate} {action}")
          machine.succeed("test ! -e /var/lib/harbor-db/postgresql/identity.json; test ! -e /var/lib/harbor-db/postgresql/lock")
      machine.succeed("/run/adoption-switch dry-activate")
      machine.succeed("test ! -e /var/lib/harbor-db/postgresql/identity.json")
      machine.succeed("systemctl reset-failed postgresql")
      machine.succeed("/run/adoption-switch test")
      machine.fail("systemctl is-active fixture-existing-postgresql")
      machine.wait_for_unit("postgresql.service")
      # The postmaster itself is MAINPID and retains the shared authority lease.
      # This verifies the real package does not close it during initialization.
      machine.succeed('test "$(systemctl show postgresql -p MainPID --value)" = "$(head -1 /var/lib/postgres/18/postmaster.pid)"')
      machine.succeed('pid=$(head -1 /var/lib/postgres/18/postmaster.pid); ls -l /proc/$pid/fd | grep -F /var/lib/harbor-db/postgresql/lock')
      machine.fail("runuser -u postgres -- flock -n -x /var/lib/harbor-db/postgresql/lock true")
      machine.succeed("systemctl reload postgresql")
      machine.fail(f"runuser -u postgres -- harbor-db-postgres --config /etc/harbor-db/postgresql.json adopt --system-identifier {identifier}")
      # Idempotent switch-time verification succeeds with the real guarded
      # primary retaining its shared lease; it must not reacquire exclusivity.
      machine.succeed(f"runuser -u postgres -- harbor-db-postgres --config /etc/harbor-db/postgresql.json adopt-live --system-identifier {identifier}")
      machine.succeed("/run/adoption-switch test")
      machine.wait_for_unit("postgresql.service")
      machine.succeed("runuser -u postgres -- psql -v ON_ERROR_STOP=1 -f ${sql}")
      # Persistent ALTER SYSTEM values must not weaken the launcher's contract.
      machine.succeed("runuser -u postgres -- psql -c 'ALTER SYSTEM SET fsync = off'")
      machine.succeed("runuser -u postgres -- psql -c 'ALTER SYSTEM SET full_page_writes = off'")
      machine.succeed("runuser -u postgres -- psql -c 'ALTER SYSTEM SET synchronous_commit = off'")
      machine.succeed("systemctl restart postgresql")
      machine.wait_for_unit("postgresql.service")
      settings = machine.succeed("runuser -u postgres -- psql -Atqc \"SELECT name,setting,source FROM pg_settings WHERE name IN ('fsync','full_page_writes','synchronous_commit') ORDER BY name\"").strip()
      assert settings.splitlines() == ['fsync|on|command line', 'full_page_writes|on|command line', 'synchronous_commit|on|command line'], settings

      def verify_save():
          result = machine.succeed("runuser -u postgres -- psql -Atqc \"SELECT mutation,geometry->'circle',review,binding,receipt,revision FROM saves\"").strip()
          assert result == 'ack-1|[10, 20, 30]|reviewed|recording-1|receipt-1|42', result

      verify_save()
      # SIGKILL the server, then abruptly terminate the VM without shutdown.
      machine.succeed("systemctl kill --signal=SIGKILL --kill-whom=all postgresql.service")
      machine.wait_for_unit("postgresql.service")
      machine.wait_until_succeeds("runuser -u postgres -- psql -Atqc 'SELECT count(*) FROM saves'")
      verify_save()
      machine.crash()
      machine.start()
      machine.wait_for_unit("postgresql.service")
      verify_save()

      # Switch to a rollback-compatible generation after an acknowledged write.
      machine.succeed(f"{base_system}/specialisation/compat/bin/switch-to-configuration test")
      machine.wait_for_unit("postgresql.service")
      verify_save()
      machine.succeed("systemctl stop postgresql")
      machine.succeed("runuser -u postgres -- flock -n -x /var/lib/harbor-db/postgresql/lock true")
      ${pkgs.lib.optionalString interop ''
        # Both implementations resume the other's persisted authority records.
        # Invoke legacy executables explicitly: normal units remain native.
        machine.succeed("runuser -u postgres -- ${legacyTool}/bin/harbor-db-postgres --config /etc/harbor-db/postgresql.json check")
        machine.succeed("install -d -o postgres -g postgres -m 0700 /var/lib/harbor-db/python-parity")
        machine.succeed("python3 -c 'import json; c=json.load(open(\"/etc/harbor-db/postgresql.json\")); c[\"state_dir\"]=\"/var/lib/harbor-db/python-parity\"; json.dump(c,open(\"/run/python-parity.json\",\"w\"))'")
        machine.succeed(f"runuser -u postgres -- ${legacyTool}/bin/harbor-db-postgres --config /run/python-parity.json adopt --system-identifier {identifier}")
        machine.succeed("runuser -u postgres -- ${tool}/bin/harbor-db-postgres --config /run/python-parity.json check")
      ''}
      # Missing storage must not be turned into a fresh cluster by NixOS.
      machine.succeed("mv /var/lib/postgres/18 /var/lib/postgres/preserved; mkdir /var/lib/postgres/18; chown postgres:postgres /var/lib/postgres/18")
      machine.fail("systemctl start postgresql")
      machine.succeed("test ! -e /var/lib/postgres/18/PG_VERSION")
      # Stop Restart=always before restoring the directory: a restarting unit's
      # ReadWritePaths bind mount could otherwise capture the empty old inode.
      machine.succeed("systemctl stop postgresql")
      machine.succeed("rmdir /var/lib/postgres/18; mv /var/lib/postgres/preserved /var/lib/postgres/18")
      machine.succeed("systemctl reset-failed postgresql; systemctl start postgresql")
      verify_save()
    '';
  }
