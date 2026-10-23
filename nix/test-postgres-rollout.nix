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
        # The host activation lock every mutating rollout step shares.
        "f /run/lock/fixture-switch.lock 0644 root root -"
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
            executionLock = "/run/lock/fixture-switch.lock";
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
          return json.loads(primary.succeed(rollout + arguments))

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
          # The publisher active at pause time is restarted with its condition lifted.
          primary.succeed("systemctl is-active fixture-publisher.service")
          primary.succeed("test \"$(systemctl show fixture-publisher.service -p ConditionResult --value)\" = yes")
          # Every scenario starts with the retired client running again.
          primary.succeed("systemctl start fixture-retired.service")

      def drop_in(unit, text):
          primary.succeed(f"mkdir -p /run/systemd/system/{unit}.d; printf {shlex.quote(text)} > /run/systemd/system/{unit}.d/fault.conf; systemctl daemon-reload")

      def clear_drop_in(unit):
          primary.succeed(f"rm -r /run/systemd/system/{unit}.d; systemctl daemon-reload; systemctl reset-failed {unit}")

      def primary_pid():
          return primary.succeed("systemctl show postgresql.service -p MainPID --value").strip()

      def activation_refused():
          running = primary_pid()
          code, output = primary.execute(f"{candidate}/bin/switch-to-configuration test 2>&1")
          assert code != 0 and "between fence and adoption" in output, output
          assert primary.succeed("readlink -f /run/current-system").strip() == base
          assert primary_pid() == running

      with subtest("an acquisition interrupted before fence-open is aborted"):
          drop_in("fixture-daemon.service", "[Unit]\nRefuseManualStop=true\n")
          refused("fence", "refuse")
          journal = step("status")
          assert journal["phase"] == "idle" and journal["startup_token"] and journal["fence_token"] is None, journal
          clear_drop_in("fixture-daemon.service")
          assert step("abort")["phase"] == "aborted"
          reopened()

      with subtest("a fenced rollout holds the host lock and is aborted"):
          # A finished abort is archived; the same candidate starts afresh.
          assert step("fence")["phase"] == "fenced"
          assert len(step("status")["archived"]) == 1
          fenced()
          assert step("fence")["phase"] == "fenced"
          # Mutating steps share the host activation lock; status takes none.
          primary.succeed("systemd-run --unit=fixture-lock-holder --collect flock /run/lock/fixture-switch.lock ${pkgs.coreutils}/bin/sleep infinity")
          primary.wait_until_fails("flock -n /run/lock/fixture-switch.lock true")
          refused("abort", "execution lock")
          assert step("status")["phase"] == "fenced"
          primary.succeed("systemctl stop fixture-lock-holder.service")
          assert step("abort")["phase"] == "aborted"
          reopened()

      with subtest("a lost journal is unwound only by adopting Harbor's fence"):
          assert step("fence")["phase"] == "fenced"
          token = step("status")["fence_token"]
          primary.succeed(f"rm {state}/rollout.json")
          # Harbor's live fence is not this (fresh) rollout's: refused before any
          # side effect, and no journal is written.
          running = primary_pid()
          refused("fence", "did not open")
          assert primary_pid() == running
          primary.succeed(f"test ! -e {state}/rollout.json; test ! -e /var/lib/harbor-db/postgresql-startup/inhibited.json")
          fenced()
          refused("abort", f"Harbor holds writer fence {token}")
          refused("abort --adopt-fence-token " + "0" * 32, "differs")
          assert step(f"abort --adopt-fence-token {token}")["phase"] == "aborted"
          # The lost journal's stopped clients are the operator's to restart.
          primary.succeed("systemctl start fixture-daemon.service")
          reopened()

      with subtest("an opening interrupted before its token is adopted only explicitly"):
          lock = "/var/lib/harbor-db/postgresql/writer-fence.lock"
          maintenance = json.loads(primary.succeed(f"cat {manifest}"))["maintenance"]
          # Harbor's fence-open fails after the coordinator recorded its intent.
          primary.succeed(f"systemd-run --unit=fixture-fence-holder --collect flock {lock} ${pkgs.coreutils}/bin/sleep infinity")
          primary.wait_until_fails(f"flock -n {lock} true")
          refused("fence", "temporarily unavailable")
          journal = step("status")
          assert journal["fence_opening"] and journal["fence_token"] is None, journal
          primary.succeed("systemctl stop fixture-fence-holder.service")
          # As if the interrupted coordinator's own fence-open had completed.
          token = json.loads(primary.succeed(f"runuser -u postgres -- harbor-db-postgres --config {maintenance} fence-open --system-identifier {identifier}"))["token"]
          refused("fence", "cannot prove")
          refused("fence --adopt-fence-token " + "0" * 32, "cannot prove")
          adopted = step(f"fence --adopt-fence-token {token}")
          assert adopted["phase"] == "fenced" and adopted["fence_token"] == token, adopted
          fenced()
          refused(f"abort --adopt-fence-token {token}", "records its fence")
          assert step("abort")["phase"] == "aborted"
          reopened()

      with subtest("the full rollout hands off to the executor and thaws"):
          assert step("next")["phase"] == "fenced"
          assert len(step("status")["archived"]) == 4
          # The candidate's pre-switch check refuses activation mid-rollout.
          activation_refused()
          # A surviving preparation worker blocks the step, before any receiver stops.
          primary.succeed("systemd-run --unit=harbor-db-rollout-prepare --collect ${pkgs.coreutils}/bin/sleep infinity")
          refused("next", "still running")
          assert step("status")["preparation_worker"] == "harbor-db-rollout-prepare.service"
          primary.succeed("systemctl is-active fixture-receiver.service")
          primary.succeed("systemctl stop harbor-db-rollout-prepare.service")
          prepared = step("next")
          assert prepared["phase"] == "prepared", prepared
          fenced()
          activation_refused()
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
          # The legacy generation does not carry this manifest.
          refused("activated", "is not this manifest")
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
          # A client that cannot start fails thaw after the fence closed and the
          # primary restarted; the retry resumes without restarting it again.
          drop_in("fixture-daemon.service", "[Service]\nExecStartPre=${pkgs.coreutils}/bin/false\n")
          refused("thaw --acceptance /root/acceptance/receipt.json", "rollout command failed")
          reopened_pid = primary_pid()
          clear_drop_in("fixture-daemon.service")
          assert step("thaw --acceptance /root/acceptance/receipt.json")["phase"] == "thawed"
          assert primary_pid() == reopened_pid
          reopened()
          assert sql("SELECT count(*) FROM retained WHERE id = 'daemon'") == "6"
    '';
  }
