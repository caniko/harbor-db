{pkgs}: let
  tool = import ./postgres-package.nix {inherit pkgs;};
  data = "/var/lib/postgres/18";
  authority = "/var/lib/fence-authority";
  startup = "/var/lib/fence-startup";
  manifest = pkgs.writeText "fence-fixture.json" (builtins.toJSON {
    resource = "postgresql";
    data_dir = data;
    state_dir = authority;
    package = toString pkgs.postgresql_18;
    major = "18";
    writer_fence = {
      control_role = "postgres";
      replication_roles = [];
      allowed_preload_libraries = [];
    };
    startup_inhibition = {
      state_dir = startup;
      unit = "postgresql.service";
      setup_units = ["postgresql-setup.service"];
      drop_in_root = "/etc/systemd/system.control";
      systemctl = "${pkgs.systemd}/bin/systemctl";
      busctl = "${pkgs.systemd}/bin/busctl";
      runuser = "${pkgs.util-linux}/bin/runuser";
      adapter = "${tool}/bin/harbor-db-postgres";
    };
  });
  interrupt = pkgs.writeText "interrupt-fence-transition.py" ''
    import json, sys
    from pathlib import Path
    from unittest.mock import patch
    from harbor_db import durable, writer_fence
    config = json.loads(Path('${manifest}').read_text())
    if sys.argv[1] == 'prepare':
        def publish(path, value):
            if Path(path) == Path(config['data_dir']) / 'postgresql.auto.conf':
                raise RuntimeError('interrupted before selector publication')
            durable.atomic_write(path, value)
        with patch.object(writer_fence, 'atomic_write', side_effect=publish):
            writer_fence.open_fence(config, sys.argv[2])
    else:
        def publish(path, value):
            if Path(path).name == 'closed.json':
                raise RuntimeError('interrupted after original selector restoration')
            durable.write_json(path, value)
        with patch.object(writer_fence, 'write_json', side_effect=publish):
            writer_fence.close_fence(config, sys.argv[2])
  '';
in
  pkgs.testers.runNixOSTest {
    name = "harbor-db-postgres-writer-fence";
    nodes.machine = {
      imports = [./postgres-lifecycle.nix];
      virtualisation.memorySize = 1024;
      services.postgresql = {
        enable = true;
        package = pkgs.postgresql_18;
        dataDir = data;
        authentication = pkgs.lib.mkForce "local all all trust";
      };
      services.harbor-db.postgresql = {
        stateDir = authority;
        writerFence.startupStateDir = startup;
      };
      systemd.tmpfiles.rules = [
        "d ${data} 0700 postgres postgres -"
        "d ${authority} 0700 postgres postgres -"
      ];
      environment.systemPackages = [tool pkgs.postgresql_18 pkgs.python3];
      systemd.services.fixture-client = {
        requires = ["postgresql.service"];
        after = ["postgresql.service"];
        unitConfig.ConditionPathExists = ["/etc/os-release"];
        serviceConfig = {
          Type = "oneshot";
          User = "postgres";
          Group = "postgres";
          ExecStart = "${pkgs.postgresql_18}/bin/psql -X -w -d postgres -U application -v ON_ERROR_STOP=1 -c 'INSERT INTO retained VALUES (2)'";
        };
      };
      specialisation.guarded.configuration.services.harbor-db.postgresql.enable = true;
      specialisation.guarded.configuration.services.harbor-db.postgresql.writerFence.blockedUnits = ["fixture-client.service"];
    };
    testScript = ''
      import json
      start_all()
      machine.wait_for_unit("postgresql.service")
      base = machine.succeed("readlink -f /run/current-system").strip()
      root = "harbor-db-postgres --config ${manifest} "
      pg = "runuser -u postgres -- " + root
      query = "runuser -u postgres -- psql -X -w -At -v ON_ERROR_STOP=1 -d postgres "
      machine.succeed(query + "-c 'CREATE ROLE application LOGIN SUPERUSER; CREATE TABLE retained(id int); INSERT INTO retained VALUES (1)'")
      identifier = machine.succeed(query + "-c 'SELECT system_identifier FROM pg_control_system()'").strip()
      held = json.loads(machine.succeed(root + f"inhibit-startup --system-identifier {identifier}"))
      machine.succeed("systemctl stop postgresql")
      # The legacy service has no Harbor startup wrapper. Interrupt before the
      # fence selector exists, then reboot: the persistent root gate must suffice.
      fault = "runuser -u postgres -- env PYTHONPATH=${tool}/lib python3 ${interrupt} "
      machine.fail(fault + f"prepare {identifier}")
      machine.crash()
      machine.start()
      machine.wait_for_unit("multi-user.target")
      machine.succeed("systemctl start postgresql")
      machine.fail("systemctl is-active postgresql")
      machine.succeed("test ! -e ${data}/postmaster.pid")
      machine.succeed("systemctl start postgresql-setup.service")
      machine.succeed("test \"$(systemctl show postgresql-setup.service -p ConditionResult --value)\" = no")
      opened = json.loads(machine.succeed(pg + f"fence-open --system-identifier {identifier}"))
      token = opened["token"]
      machine.succeed(root + f"release-startup --token {held['token']} --fence-token {token} --phase prepared")
      machine.succeed("systemctl start postgresql")
      machine.wait_for_unit("postgresql.service")
      machine.succeed(pg + f"inspect-fence --token {token}")
      machine.fail(query + "-U application -c 'INSERT INTO retained VALUES (2)'")
      # Reboot the unguarded generation with the durable selected HBA.
      machine.crash()
      machine.start()
      machine.wait_for_unit("postgresql.service")
      machine.succeed(pg + f"inspect-fence --token {token}")
      machine.fail(query + "-U application -c 'SELECT 1'")
      machine.succeed("systemctl stop postgresql")
      machine.succeed(pg + f"adopt --system-identifier {identifier}")
      machine.succeed(f"{base}/specialisation/guarded/bin/switch-to-configuration test")
      # Activation preserves an explicitly stopped service's state. Start the
      # adopted primary separately to exercise the guarded startup wrapper.
      machine.succeed("systemctl start postgresql")
      machine.wait_for_unit("postgresql.service")
      machine.succeed(pg + f"inspect-fence --token {token}")
      # A gated migration/runtime client is skipped, preserving existing unit
      # conditions and acknowledged records; PostgreSQL control SQL stays live.
      machine.succeed("systemctl start fixture-client")
      machine.succeed("test \"$(systemctl show fixture-client -p ConditionResult --value)\" = no")
      assert machine.succeed(query + "-c 'SELECT count(*) FROM retained'").strip() == "1"
      held = json.loads(machine.succeed(root + f"inhibit-startup --system-identifier {identifier}"))
      machine.succeed("systemctl stop postgresql")
      machine.fail(fault + f"thaw {token}")
      machine.fail(root + f"release-startup --token {held['token']} --fence-token {token} --phase closed")
      # Change back to the legacy unit while thaw is unfinished. NixOS unit
      # replacement and reboot must retain the system.control drop-in barrier.
      machine.succeed(f"{base}/bin/switch-to-configuration test")
      machine.succeed("test \"$(systemctl show postgresql-setup.service -p ConditionResult --value)\" = no")
      machine.crash()
      machine.start()
      machine.wait_for_unit("multi-user.target")
      machine.succeed("systemctl start postgresql")
      machine.fail("systemctl is-active postgresql")
      machine.succeed("test ! -e ${data}/postmaster.pid")
      machine.succeed(pg + f"fence-close --token {token}")
      machine.succeed(root + f"release-startup --token {held['token']} --fence-token {token} --phase closed")
      machine.succeed("systemctl start postgresql")
      machine.wait_for_unit("postgresql.service")
      machine.succeed("systemctl start fixture-client")
      assert machine.succeed(query + "-c 'SELECT count(*) FROM retained'").strip() == "2"
      machine.succeed("test -e ${startup}/lock; test ! -e ${startup}/inhibited.json")
    '';
  }
