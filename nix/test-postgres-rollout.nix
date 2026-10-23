{
  pkgs,
  nativePackage ? import ./native-package.nix {inherit pkgs;},
}: let
  inherit (pkgs) lib;
  pg = pkgs.postgresql_18;
  harbor = "${nativePackage}/bin/harbor-db-postgres";
  data = "/var/lib/postgres/18";
  # Every rollout input binds the authoritative identifier at build time, so the
  # test gives the fresh cluster this identifier before any rollout step.
  identifier = "7000000000000000042";
  setIdentifier = pkgs.writeText "set-system-identifier.py" ''
    import sys
    path, identifier = sys.argv[1], int(sys.argv[2])
    data = bytearray(open(path, "rb").read())
    def crc32c(buf):
        crc = 0xFFFFFFFF
        for byte in buf:
            crc ^= byte
            for _ in range(8):
                crc = (crc >> 1) ^ (0x82F63B78 if crc & 1 else 0)
        return crc ^ 0xFFFFFFFF
    # pg_control starts with the identifier; ControlFileData ends with its CRC-32C.
    end = next(n for n in range(8, 512, 4) if crc32c(data[:n]) == int.from_bytes(data[n:n + 4], "little"))
    data[:8] = identifier.to_bytes(8, "little")
    data[end:end + 4] = crc32c(data[:end]).to_bytes(4, "little")
    open(path, "wb").write(data)
  '';
  path = lib.makeBinPath [pg pkgs.coreutils pkgs.gnutar pkgs.jq];
  # Consumer-owned preparation commands, run by the coordinator's transient unit.
  backup = pkgs.writeShellScript "fixture-rollout-backup" ''
    set -euxo pipefail
    export PATH=${path} PGHOST=/run/postgresql
    # Harbor suppresses worker diagnostics; keep the fixture's in the journal.
    exec 2> >(${pkgs.util-linux}/bin/logger -t fixture-rollout)
    rm -rf /srv/backup/base/base-1
    pg_basebackup -D /srv/backup/base/base-1 -X stream --checkpoint=fast
    stop=$(jq -r '."WAL-Ranges"[0]."End-LSN"' /srv/backup/base/base-1/backup_manifest)
    target=$(psql -X -d postgres -Atqc "SELECT pg_create_restore_point('rollout')")
    psql -X -d postgres -Atqc 'SELECT pg_switch_wal()' > /dev/null
    cp ${data}/pg_wal/0000000* /srv/backup/wal/
    jq -n --arg stop "$stop" --arg target "$target" \
      '{backup_id: "base-1", system_identifier: "${identifier}", pg_major: 18, epoch_id: "fixture-1", backup_stop_lsn: $stop, post_backup_lsn: $target}' \
      > /srv/backup/base/base-1.meta.json
    printf 'base-1\n' > /srv/backup/LAST_SUCCESS
  '';
  # Restore the selected backup at a disposable read-only endpoint and certify it.
  certifyAt = pkgs.writeShellScript "fixture-rollout-certify-at" ''
    set -euxo pipefail
    export PATH=${path}
    # Harbor suppresses worker diagnostics; keep the fixture's in the journal.
    exec 2> >(${pkgs.util-linux}/bin/logger -t fixture-rollout) >&2
    config=$1 repository=$2 root=$3
    rm -rf "$root"
    mkdir -p "$root/wal" "$root/socket"
    cp -a "$repository/base/base-1" "$root/18"
    cp "$repository"/wal/* "$root/wal/"
    target=$(jq -r .post_backup_lsn "$repository/base/base-1.meta.json")
    rm -f "$root/18/postgresql.conf"
    printf '%s\n' "listen_addresses = '''" "unix_socket_directories = '$root/socket'" "port = 55432" \
      "restore_command = '${pkgs.coreutils}/bin/cp $root/wal/%f %p'" "recovery_target_lsn = '$target'" \
      "recovery_target_action = 'promote'" "default_transaction_read_only = on" > "$root/18/postgresql.conf"
    printf 'local all postgres peer map=recovery\n' > "$root/pg_hba.conf"
    printf 'recovery %s postgres\n' "$(id -un)" > "$root/pg_ident.conf"
    touch "$root/18/recovery.signal"
    # The backup carries the fenced HBA selector; the command line overrides it.
    pg_ctl -D "$root/18" -o "-c hba_file=$root/pg_hba.conf -c ident_file=$root/pg_ident.conf" -l "$root/server.log" -w start
    trap 'pg_ctl -D "$root/18" -m fast -w stop' EXIT
    deadline=$((SECONDS + 120))
    until [ "$(psql -X -h "$root/socket" -p 55432 -U postgres -d postgres -Atqc 'SELECT NOT pg_is_in_recovery()')" = t ]; do
      [ "$SECONDS" -lt "$deadline" ]
      sleep 0.5
    done
    ${harbor} --config "$config" certify-recovery --data-dir "$root/18" --socket-dir "$root/socket" --port 55432
  '';
  export = pkgs.writeShellScript "fixture-rollout-export" ''
    set -euxo pipefail
    export PATH=${path}
    # Harbor suppresses worker diagnostics; keep the fixture's in the journal.
    exec 2> >(${pkgs.util-linux}/bin/logger -t fixture-rollout)
    out=/srv/rollout/export/base-1
    rm -rf "$out.tmp"
    mkdir "$out.tmp"
    tar -C /srv -cf "$out.tmp/export.tar" backup/base backup/wal backup/LAST_SUCCESS evidence/records.json
    jq -n --arg sha "$(sha256sum "$out.tmp/export.tar" | cut -d' ' -f1)" \
      '{version: 1, backup_id: "base-1", archive: "export.tar", archive_sha256: $sha}' > "$out.tmp/recovery-export.json"
    rm -rf "$out"
    mv "$out.tmp" "$out"
  '';
  # The executor's independent restore/certify argv; the coordinator appends
  # --export, --export-sha256 and --workspace.
  certifier = maintenance:
    pkgs.writeShellScript "fixture-rollout-certifier" ''
      set -euxo pipefail
      export PATH=${path}
      while [ $# -gt 0 ]; do
        case $1 in
          --export) source=$2 ;;
          --export-sha256) sha=$2 ;;
          --workspace) workspace=$2 ;;
          *) echo "unexpected argument $1" >&2; exit 2 ;;
        esac
        shift 2
      done
      echo "$sha  $source/recovery-export.json" | sha256sum -c --quiet
      echo "$(jq -r .archive_sha256 "$source/recovery-export.json")  $source/export.tar" | sha256sum -c --quiet
      mkdir -m 0700 "$workspace"
      tar -C "$workspace" -xf "$source/export.tar"
      mkdir "$workspace/backup/locks"
      touch "$workspace/backup/locks/mutate"
      jq --arg w "$workspace" '.recovery.backup_root = "\($w)/backup" | .recovery.snapshot_file = "\($w)/evidence/records.json"
        | .recovery.receipt_file = "\($w)/off-host-recovery.json" | .recovery.off_host_receipt_file = .recovery.receipt_file' \
        ${maintenance} > "$workspace/config.json"
      exec ${certifyAt} "$workspace/config.json" "$workspace/backup" "$workspace/restore"
    '';
  client = name: {
    requires = ["postgresql.service"];
    after = ["postgresql.service" "postgresql-setup.service"];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      User = "postgres";
      ExecStart = "${pg}/bin/psql -X -w -d postgres -U application -v ON_ERROR_STOP=1 -c \"INSERT INTO retained VALUES ('${name}')\"";
    };
  };
in
  pkgs.testers.runNixOSTest {
    extraDriverArgs = ["--junit-xml" "junit.xml"];
    name = "harbor-db-postgres-rollout";
    defaults = {
      # The driver aligns clocks at each evidence hand-off (see the recovery test).
      services.timesyncd.enable = lib.mkForce false;
      environment.systemPackages = [nativePackage pg pkgs.jq];
    };
    nodes.primary = {
      imports = [./postgres-lifecycle.nix];
      _module.args.harborDbStoragePackage = nativePackage;
      virtualisation = {
        memorySize = 2048;
        diskSize = 3072;
      };
      environment.systemPackages = [pkgs.python3];
      # The legacy (unguarded) generation the rollout starts from.
      services.postgresql = {
        enable = true;
        package = pg;
        dataDir = data;
        authentication = lib.mkForce "local all all trust\nlocal replication postgres peer\n";
        initialScript = pkgs.writeText "rollout-fixture.sql" ''
          CREATE ROLE application LOGIN;
          CREATE TABLE retained (id text NOT NULL);
          GRANT SELECT, INSERT ON retained TO application;
        '';
      };
      # A slow control-role setup session races any fence inspection that does
      # not wait for setup units after a primary start.
      systemd.services.postgresql-setup.script = lib.mkAfter ''
        psql -X -d postgres -Atqc 'SELECT pg_sleep(6)' > /dev/null
      '';
      systemd.services = {
        fixture-daemon = client "daemon";
        fixture-retired = client "retired";
        fixture-receiver.serviceConfig.ExecStart = "${pkgs.coreutils}/bin/sleep infinity";
        fixture-publisher.serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${pkgs.coreutils}/bin/true";
        };
      };
      systemd.tmpfiles.rules = [
        "d ${data} 0700 postgres postgres -"
        "d /srv/backup 0700 postgres postgres -"
        "d /srv/backup/base 0700 postgres postgres -"
        "d /srv/backup/wal 0700 postgres postgres -"
        "d /srv/backup/locks 0700 postgres postgres -"
        "f /srv/backup/locks/mutate 0600 postgres postgres -"
        "d /srv/evidence 0700 postgres postgres -"
        "d /srv/restore 0700 postgres postgres -"
        "d /srv/rollout 0755 root root -"
        "d /var/lib/harbor-db 0755 root root -"
        "d /var/lib/harbor-db/import 0700 root root -"
      ];
      # The candidate generation carries the guarded primary and the rollout manifest.
      specialisation.guarded.configuration = {config, ...}: let
        maintenance = config.environment.etc."harbor-db/postgresql.json".source;
      in {
        services.harbor-db.postgresql = {
          enable = true;
          writerFence.blockedUnits = ["fixture-daemon.service"];
          recovery = {
            systemIdentifier = identifier;
            requireWriterFence = true;
            backupRoot = "/srv/backup";
            snapshotFile = "/srv/evidence/records.json";
            receiptFile = "/srv/evidence/recovery.json";
            offHostReceiptFile = "/srv/evidence/off-host.json";
            maxAgeSeconds = 3600;
            verifyTimeoutSeconds = 120;
            recordChecks.retained = {
              database = "postgres";
              sql = "SELECT id FROM retained ORDER BY id";
            };
          };
          recoveryPreparation = {
            readinessCommand = ["${pkgs.coreutils}/bin/true"];
            backupCommand = ["${backup}"];
            restoreCommand = ["${certifyAt}" "${maintenance}" "/srv/backup" "/srv/restore/attempt"];
            exportCommand = ["${export}"];
            readWritePaths = ["/srv/backup" "/srv/evidence" "/srv/restore" "/srv/rollout/export"];
            offHostReceiptImportFile = "/var/lib/harbor-db/import/off-host.json";
          };
          rollout = {
            clients = ["fixture-daemon.service"];
            retiredClients = ["fixture-retired.service"];
            receiverUnits = ["fixture-receiver.service"];
            pausedPublishers = ["fixture-publisher.service"];
            exportRoot = "/srv/rollout/export";
            executor = {
              host = "executor";
              user = "can";
              command = ["${certifier maintenance}"];
            };
          };
        };
      };
    };
    nodes.executor = {
      users.users.can = {
        isNormalUser = true;
        uid = 1000;
      };
    };
    testScript = ''
      import json
      import shlex

      identifier = "${identifier}"
      start_all()
      primary.wait_for_unit("postgresql.service")
      executor.wait_for_unit("multi-user.target")

      def align_receiver(receiver, sender):
          seconds = max(int(host.succeed("date +%s").strip()) for host in (receiver, sender))
          receiver.succeed(f"date --set=@{seconds}")

      def sql(statement, user="postgres"):
          return primary.succeed(f"runuser -u postgres -- psql -X -w -d postgres -U {user} -v ON_ERROR_STOP=1 -Atqc {shlex.quote(statement)}").strip()

      # Give the fresh cluster the identifier the manifests were built with.
      primary.succeed("systemctl start postgresql-setup.service")
      primary.succeed("systemctl stop postgresql.service")
      primary.succeed("runuser -u postgres -- python3 ${setIdentifier} ${data}/global/pg_control ${identifier}")
      primary.succeed("runuser -u postgres -- pg_resetwal -D ${data}")
      primary.succeed("systemctl start postgresql.service postgresql-setup.service")
      assert sql("SELECT system_identifier FROM pg_control_system()") == identifier
      primary.succeed("systemctl start fixture-daemon fixture-retired fixture-receiver fixture-publisher")

      base = primary.succeed("readlink -f /run/current-system").strip()
      candidate = f"{base}/specialisation/guarded"
      manifest = primary.succeed(f"readlink -f {candidate}/etc/harbor-db/postgresql-rollout.json").strip()
      rollout = f"harbor-db-postgres --config {manifest} rollout "
      state = "/var/lib/harbor-db/postgresql-rollout"
      pause = "/etc/systemd/system.control/fixture-publisher.service.d/zz-harbor-db-rollout-pause.conf"

      def step(arguments):
          # TEMP: strict json.loads of the whole stdout once the coordinator
          # keeps child stdout (prepare-recovery, executor) out of its own.
          return json.loads(primary.succeed(rollout + arguments).strip().splitlines()[-1])

      def refused(arguments, reason):
          code, output = primary.execute(rollout + arguments + " 2>&1")
          assert code != 0 and reason in output, (arguments, output)

      def fenced():
          primary.fail("runuser -u postgres -- psql -X -w -d postgres -U application -c 'SELECT 1'")
          primary.fail("systemctl is-active fixture-daemon.service")
          primary.succeed(f"test -e {pause}")
          primary.succeed("systemctl start fixture-publisher.service")
          primary.succeed("test \"$(systemctl show fixture-publisher.service -p ConditionResult --value)\" = no")

      def reopened():
          primary.succeed("test ! -e /var/lib/harbor-db/postgresql/writer-fence.json")
          primary.succeed("test ! -e /var/lib/harbor-db/postgresql-startup/inhibited.json")
          sql("INSERT INTO retained VALUES ('probe')", user="application")
          primary.succeed("systemctl is-active fixture-daemon.service fixture-receiver.service")
          primary.fail("systemctl is-active fixture-retired.service")
          primary.succeed(f"test ! -e {pause}; test ! -e {state}/paused")
          primary.succeed("systemctl start fixture-publisher.service")
          primary.succeed("test \"$(systemctl show fixture-publisher.service -p ConditionResult --value)\" = yes")

      def drop_in(unit, text):
          primary.succeed(f"mkdir -p /run/systemd/system/{unit}.d; printf {shlex.quote(text)} > /run/systemd/system/{unit}.d/fault.conf; systemctl daemon-reload")

      def clear_drop_in(unit):
          primary.succeed(f"rm -r /run/systemd/system/{unit}.d; systemctl daemon-reload; systemctl reset-failed {unit}")

      def restart_rollout(retired):
          primary.succeed(f"mv {state}/rollout.json {state}/{retired}.json")
          primary.succeed("systemctl start fixture-retired.service")
          primary.succeed("systemctl reset-failed postgresql.service")  # TEMP until start_primary resets the start limit

      with subtest("an acquisition interrupted before fence-open is aborted"):
          drop_in("fixture-daemon.service", "[Unit]\nRefuseManualStop=true\n")
          refused("fence", "refuse")
          journal = step("status")
          assert journal["phase"] == "idle" and journal["startup_token"] and journal["fence_token"] is None, journal
          clear_drop_in("fixture-daemon.service")
          assert step("abort")["phase"] == "aborted"
          reopened()

      with subtest("a fenced rollout is aborted and every client returns"):
          restart_rollout("aborted-before-fence")
          assert step("fence")["phase"] == "fenced"
          fenced()
          assert step("fence")["phase"] == "fenced"
          assert step("abort")["phase"] == "aborted"
          reopened()

      with subtest("the full rollout hands off to the executor and thaws"):
          restart_rollout("aborted-fenced")
          assert step("next")["phase"] == "fenced"
          prepared = step("next")
          assert prepared["phase"] == "prepared", prepared
          fenced()
          primary.succeed("systemctl is-active fixture-receiver.service")
          export = prepared["export"]
          primary.succeed(f"tar -C {export}/.. -cf /tmp/export.tar base-1")
          primary.copy_from_machine("/tmp/export.tar", "rollout-transfer")
          executor.copy_from_host(str(primary.out_dir / "rollout-transfer/export.tar"), "/tmp/export.tar")
          executor.succeed("install -d -o can -g users -m 0700 /home/can/export; tar -C /home/can/export -xf /tmp/export.tar; chown -R can:users /home/can/export")
          align_receiver(executor, primary)
          certified = json.loads(executor.succeed(f"runuser -u can -- harbor-db-postgres --config {manifest} rollout certify --export /home/can/export/base-1 --export-sha256 {prepared['export_sha256']} --workspace /home/can/workspace"))
          receipt = certified["receipt"]
          receipt_sha256 = executor.succeed(f"sha256sum {receipt}").split()[0]
          executor.copy_from_machine(receipt, "rollout-transfer")
          primary.copy_from_host(str(executor.out_dir / "rollout-transfer/off-host-recovery.json"), "/root/incoming/off-host-recovery.json")
          primary.succeed("chmod 0600 /root/incoming/off-host-recovery.json")
          align_receiver(primary, executor)
          refused(f"import --receipt /root/incoming/off-host-recovery.json --sha256 {'0' * 64}", "differs")
          assert step(f"import --receipt /root/incoming/off-host-recovery.json --sha256 {receipt_sha256}")["phase"] == "imported"
          assert step("next")["phase"] == "adopted"
          primary.succeed("test -s /var/lib/harbor-db/postgresql/identity.json")
          fenced()
          primary.succeed(f"{candidate}/bin/switch-to-configuration test")
          assert step("activated")["phase"] == "activated"
          refused("abort", "thaw with an acceptance")
          journal = step("status")
          acceptance = {
              "version": 1, "status": "accepted", "host": "primary", "system_identifier": identifier,
              "manifest_sha256": primary.succeed(f"sha256sum {manifest}").split()[0], "token": journal["fence_token"],
              "snapshot_sha256": primary.succeed("sha256sum /srv/evidence/records.json").split()[0],
          }
          primary.succeed("install -d -m 0700 /root/acceptance; printf '%s' " + shlex.quote(json.dumps(acceptance)) + " > /root/acceptance/receipt.json; chmod 0600 /root/acceptance/receipt.json")
          # A client that cannot start fails thaw after the fence closed; the
          # retry resumes from there.
          drop_in("fixture-daemon.service", "[Service]\nExecStartPre=${pkgs.coreutils}/bin/false\n")
          refused("thaw --acceptance /root/acceptance/receipt.json", "rollout command failed")
          clear_drop_in("fixture-daemon.service")
          assert step("thaw --acceptance /root/acceptance/receipt.json")["phase"] == "thawed"
          reopened()
          assert sql("SELECT count(*) FROM retained WHERE id = 'daemon'") == "4"
    '';
  }
