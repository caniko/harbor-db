{
  pkgs,
  nativePackage ? null,
}: let
  tool = import ./postgres-package.nix {inherit pkgs nativePackage;};
  old = pkgs.postgresql_17;
  new = pkgs.postgresql_18;
  state = "/var/lib/harbor-db/postgresql";
  # Fault injection is exclusively in the disposable test package. Real initdb
  # creates PG_VERSION, then the orchestrator is killed before pg_upgrade runs.
  interruptedPackage =
    pkgs.runCommand "postgresql-interrupted-initdb" {
      inherit (new) version;
      passthru =
        (new.withPackages (_: [])).passthru
        // {
          withoutJIT = interruptedPackage;
          withJIT = interruptedPackage;
          withPackages = _: interruptedPackage;
        };
    } ''
      mkdir -p "$out/bin"
      for directory in lib share include; do ln -s "${new}/$directory" "$out/$directory"; done
      for binary in ${new}/bin/*; do ln -s "$binary" "$out/bin/$(basename "$binary")"; done
      rm "$out/bin/initdb"
      cat > "$out/bin/initdb" <<'SH'
      #!${pkgs.runtimeShell}
      set -eu
      ${new}/bin/initdb "$@"
      if [ ! -e ${state}/fault-injected ]; then
        touch ${state}/fault-injected
        kill -KILL "$PPID"
      fi
      SH
      chmod +x "$out/bin/initdb"
    '';
  validator = pkgs.writeShellScript "verify-upgraded-records" ''
    set -euo pipefail
    data=$1
    ${new}/bin/pg_ctl -D "$data" -l "$data/validator.log" -o "-k ${state} -p 55439 -c data_directory='$data' -c listen_addresses=\"\" -c default_transaction_read_only=on" -w start
    trap '${new}/bin/pg_ctl -D "$data" -m fast -w stop' EXIT
    test "$(${new}/bin/psql -h ${state} -p 55439 -d postgres -Atqc 'SELECT receipt FROM acknowledged_saves')" = receipt-before-upgrade
  '';
  oldConfig = pkgs.writeText "old-adoption.json" (builtins.toJSON {
    resource = "postgresql";
    state_dir = state;
    data_dir = "/var/lib/postgres/17";
    major = "17";
    package = toString old;
    required_mounts = [];
  });
in
  pkgs.testers.runNixOSTest {
    extraDriverArgs = ["--junit-xml" "junit.xml"];
    name = "harbor-db-interrupted-pg-upgrade";
    nodes.machine = {
      imports = [./postgres-lifecycle.nix];
      _module.args.harborDbStoragePackage = nativePackage;
      virtualisation.memorySize = 1024;
      services.postgresql = {
        enable = true;
        package = interruptedPackage;
        dataDir = "/var/lib/postgres/18";
      };
      services.harbor-db.postgresql = {
        enable = true;
        upgrade = {
          oldPackage = old;
          oldDataDir = "/var/lib/postgres/17";
          initdbArgs = ["--locale=C" "--encoding=UTF8" "--no-data-checksums"];
          validateCommand = ["${validator}"];
        };
      };
      environment.systemPackages = [tool];
    };
    testScript = ''
      start_all()
      machine.wait_for_unit("multi-user.target")
      machine.succeed("systemctl stop postgresql")
      machine.succeed("install -d -o postgres -g postgres -m 0700 /var/lib/postgres")
      machine.succeed("install -d -o postgres -g postgres -m 0700 /var/lib/postgres/17")
      machine.succeed("runuser -u postgres -- ${old}/bin/initdb -D /var/lib/postgres/17 --locale=C --encoding=UTF8")
      # A copied configuration must not redirect pg_upgrade's temporary server
      # back onto the registered source, even when data_directory is explicit.
      machine.succeed("printf \"data_directory = '/var/lib/postgres/17'\\n\" >> /var/lib/postgres/17/postgresql.conf")
      machine.succeed("runuser -u postgres -- ${old}/bin/pg_ctl -D /var/lib/postgres/17 -l ${state}/old-postgres.log -o '-k ${state} -p 55438' -w start", timeout=60)
      machine.succeed("runuser -u postgres -- ${old}/bin/psql -h ${state} -p 55438 -d postgres -v ON_ERROR_STOP=1 -c \"CREATE TABLE acknowledged_saves(receipt text); INSERT INTO acknowledged_saves VALUES ('receipt-before-upgrade');\"")
      machine.succeed("runuser -u postgres -- ${old}/bin/pg_ctl -D /var/lib/postgres/17 -m fast -w stop")
      identifier = machine.succeed("runuser -u postgres -- ${old}/bin/pg_controldata /var/lib/postgres/17 | sed -n 's/^Database system identifier: *//p'").strip()
      machine.succeed(f"runuser -u postgres -- harbor-db-postgres --config ${oldConfig} adopt --system-identifier {identifier}")
      original_control = machine.succeed("sha256sum /var/lib/postgres/17/global/pg_control").split()[0]
      machine.succeed("test ! -e /var/lib/postgres/18/PG_VERSION")
      machine.fail("runuser -u postgres -- harbor-db-postgres --config /etc/harbor-db/postgresql.json upgrade", timeout=120)
      machine.succeed("test -e /var/lib/postgres/18.harbor-staging/PG_VERSION; test -e ${state}/upgrade.json")
      machine.fail("systemctl start postgresql")
      machine.succeed("systemctl stop postgresql")
      machine.succeed("test ! -e /var/lib/postgres/18/PG_VERSION")
      machine.succeed("runuser -u postgres -- harbor-db-postgres --config /etc/harbor-db/postgresql.json upgrade --retry-incomplete", timeout=120)
      machine.succeed("test -e /var/lib/postgres/18.harbor-staging.interrupted/PG_VERSION; test ! -e ${state}/upgrade.json")
      machine.succeed("systemctl reset-failed postgresql; systemctl start postgresql")
      result = machine.succeed("runuser -u postgres -- ${new}/bin/psql -d postgres -Atqc 'SELECT receipt FROM acknowledged_saves'").strip()
      assert result == "receipt-before-upgrade", result
      # Historical PG17 must remain intact, but cannot be accepted as authority.
      machine.succeed("test -e /var/lib/postgres/17/PG_VERSION")
      assert machine.succeed("sha256sum /var/lib/postgres/17/global/pg_control").split()[0] == original_control
      machine.fail("runuser -u postgres -- harbor-db-postgres --config ${oldConfig} check")
    '';
  }
