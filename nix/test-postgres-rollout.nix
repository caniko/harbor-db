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
    # Byte-reproducible: the import's preparation reruns the exporter, and the
    # coordinator admits only an unchanged certified hand-off. Source mtimes
    # are kept: the executor checks the backup manifest's age.
    tar --sort=name --owner=0 --group=0 --numeric-owner \
      -C /srv -cf "$out.tmp/export.tar" backup/base backup/wal backup/LAST_SUCCESS evidence/records.json
    # The test's trigger makes a rerun rewrite the hand-off.
    rewritten=$([ -e /run/fixture-rewrite-export ] && echo true || echo false)
    jq -n --arg sha "$(sha256sum "$out.tmp/export.tar" | cut -d' ' -f1)" --argjson rewritten "$rewritten" \
      '{version: 1, backup_id: "base-1", archive: "export.tar", archive_sha256: $sha} + if $rewritten then {rewritten: true} else {} end' \
      > "$out.tmp/recovery-export.json"
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
  dependent = {
    requires = ["postgresql.service"];
    after = ["postgresql.service"];
    serviceConfig.ExecStart = "${pkgs.coreutils}/bin/sleep infinity";
  };
  publisher.serviceConfig = {
    Type = "oneshot";
    RemainAfterExit = true;
    ExecStart = "${pkgs.coreutils}/bin/true";
  };
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
  guarded = {config, ...}: let
    maintenance = config.environment.etc."harbor-db/postgresql.json".source;
  in {
    # A client only the candidate brings: its start during activation is
    # skipped by the fence's condition and resumed when the fence reopens.
    systemd.services.fixture-candidate = client "candidate" // {wantedBy = ["multi-user.target"];};
    services.harbor-db.postgresql = {
      enable = true;
      writerFence.blockedUnits = ["fixture-daemon.service" "fixture-candidate.service"];
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
        clients = ["fixture-daemon.service" "fixture-candidate.service"];
        retiredClients = ["fixture-retired.service"];
        receiverUnits = ["fixture-receiver.service"];
        pausedPublishers = ["fixture-publisher.service" "fixture-skipped.service" "fixture-conditional.service"];
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
in
  pkgs.testers.runNixOSTest {
    extraDriverArgs = ["--junit-xml" "junit.xml"];
    name = "harbor-db-postgres-rollout";
    defaults = {
      # The driver aligns clocks at each evidence hand-off (see the recovery test).
      services.timesyncd.enable = lib.mkForce false;
      environment.systemPackages = [nativePackage pg pkgs.jq];
    };
    nodes.primary = {config, ...}: {
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
        fixture-publisher = publisher;
        # Inactive at the pause; a start the pause skips is resumed on reopen.
        fixture-skipped = publisher;
        # Its own condition failed before the pause: never started by reopen.
        fixture-conditional = publisher // {unitConfig.ConditionPathExists = "/run/fixture-conditional";};
        # Undeclared hard dependents the primary's stop takes down; only the
        # legacy generation loads the legacy one.
        fixture-dependent = dependent;
        fixture-legacy = lib.mkIf (!config.services.harbor-db.postgresql.enable) dependent;
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
        # The host activation lock every mutating rollout step shares.
        "f /run/lock/fixture-switch.lock 0644 root root -"
      ];
      # The candidate generation carries the guarded primary and the rollout
      # manifest; a second candidate differs only in its manifest, and a third
      # enables Harbor without the coordinator.
      specialisation = {
        guarded.configuration = guarded;
        other.configuration = {
          imports = [guarded];
          services.harbor-db.postgresql.rollout.setupSettleSeconds = 119;
        };
        plain.configuration.services.harbor-db.postgresql.enable = true;
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
      import posixpath
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
      primary.succeed("systemctl start fixture-daemon fixture-retired fixture-receiver fixture-publisher fixture-dependent fixture-legacy")
      # Skipped by its own condition long before any pause.
      primary.succeed("systemctl start fixture-conditional.service")
      primary.succeed("test \"$(systemctl show fixture-conditional.service -p ConditionResult --value)\" = no")

      base = primary.succeed("readlink -f /run/current-system").strip()
      candidate = f"{base}/specialisation/guarded"
      manifest = primary.succeed(f"readlink -f {candidate}/etc/harbor-db/postgresql-rollout.json").strip()
      other = primary.succeed(f"readlink -f {base}/specialisation/other/etc/harbor-db/postgresql-rollout.json").strip()
      assert other != manifest
      rollout = f"harbor-db-postgres --config {manifest} rollout "
      state = "/var/lib/harbor-db/postgresql-rollout"
      gc_root = "/nix/var/nix/gcroots/harbor-db-postgresql-rollout"
      slot = "/var/lib/harbor-db/import/off-host.json"
      pause = "/etc/systemd/system.control/fixture-publisher.service.d/zz-harbor-db-rollout-pause.conf"

      def step(arguments, config=manifest):
          return json.loads(primary.succeed(f"harbor-db-postgres --config {config} rollout {arguments}"))

      def refused(arguments, reason, config=manifest):
          code, output = primary.execute(f"harbor-db-postgres --config {config} rollout {arguments} 2>&1")
          assert code != 0 and reason in output, (arguments, output)

      def active(*units):
          primary.wait_until_succeeds("systemctl is-active " + " ".join(units))

      def skip_publisher():
          # Inactive at the pause; the pause skips its start.
          primary.succeed("systemctl start fixture-skipped.service")
          primary.succeed("test \"$(systemctl show fixture-skipped.service -p ConditionResult --value)\" = no")

      def fenced():
          primary.fail("runuser -u postgres -- psql -X -w -d postgres -U application -c 'SELECT 1'")
          primary.fail("systemctl is-active fixture-daemon.service")
          primary.fail("systemctl is-active fixture-dependent.service")
          primary.succeed(f"test -e {pause}")
          primary.succeed("systemctl start fixture-publisher.service")
          primary.succeed("test \"$(systemctl show fixture-publisher.service -p ConditionResult --value)\" = no")
          skip_publisher()

      def reopened(legacy=False):
          primary.succeed("test ! -e /var/lib/harbor-db/postgresql/writer-fence.json")
          primary.succeed("test ! -e /var/lib/harbor-db/postgresql-startup/inhibited.json")
          sql("INSERT INTO retained VALUES ('probe')", user="application")
          # Recorded clients, the undeclared hard dependent and quiesced receivers
          # return; retired clients do not.
          active("fixture-daemon.service", "fixture-dependent.service", "fixture-receiver.service")
          if legacy:
              active("fixture-legacy.service")
          primary.fail("systemctl is-active fixture-retired.service")
          primary.succeed(f"test ! -e {pause}; test ! -e {state}/paused")
          # The publisher stopped by the pause and the one whose start it skipped
          # resume; one whose own condition failed before the pause does not.
          active("fixture-publisher.service", "fixture-skipped.service")
          primary.succeed("test \"$(systemctl show fixture-publisher.service -p ConditionResult --value)\" = yes")
          primary.fail("systemctl is-active fixture-conditional.service")
          # The next scenario starts with the retired client running and the
          # skipped publisher inactive again.
          primary.succeed("systemctl start fixture-retired.service; systemctl stop fixture-skipped.service")

      def drop_in(unit, text):
          primary.succeed(f"mkdir -p /run/systemd/system/{unit}.d; printf {shlex.quote(text)} > /run/systemd/system/{unit}.d/fault.conf; systemctl daemon-reload")

      def clear_drop_in(unit):
          # An inactive unit nothing references is unloaded; only a failed one needs resetting.
          primary.succeed(f"rm -r /run/systemd/system/{unit}.d; systemctl daemon-reload; if systemctl is-failed --quiet {unit}; then systemctl reset-failed {unit}; fi")

      def primary_pid():
          return primary.succeed("systemctl show postgresql.service -p MainPID --value").strip()

      def switch_refused(generation, reason):
          running = primary_pid()
          current = primary.succeed("readlink -f /run/current-system").strip()
          code, output = primary.execute(f"{generation}/bin/switch-to-configuration test 2>&1")
          assert code != 0 and reason in output, output
          assert primary.succeed("readlink -f /run/current-system").strip() == current
          assert primary_pid() == running

      def archived():
          return step("status").get("archived", [])

      def exports():
          # Every preparation worker that reaches the exporter logs this line.
          return int(primary.succeed("journalctl -t fixture-rollout -o cat | grep -c '^+ out=' || true").strip())

      def rewind(phase, *removed):
          # Simulate a coordinator killed after its worker finished but before
          # it recorded the outcome.
          edit = " | ".join([f'.phase = "{phase}"'] + [f"del(.{key})" for key in removed])
          primary.succeed(f"jq '{edit}' {state}/rollout.json > {state}/rewound.json; chmod 0600 {state}/rewound.json; mv {state}/rewound.json {state}/rollout.json")

      with subtest("an acquisition interrupted before fence-open is aborted"):
          drop_in("fixture-daemon.service", "[Unit]\nRefuseManualStop=true\n")
          refused("fence", "refuse")
          journal = step("status")
          assert journal["phase"] == "idle" and journal.get("startup_token") and journal.get("fence_token") is None, journal
          skip_publisher()
          clear_drop_in("fixture-daemon.service")
          assert step("abort")["phase"] == "aborted"
          reopened(legacy=True)

      with subtest("a lost journal holding only startup inhibition is adopted by its token"):
          drop_in("fixture-daemon.service", "[Unit]\nRefuseManualStop=true\n")
          refused("fence", "refuse")
          assert archived() == ["rollout.1.aborted.json"]
          startup = step("status")["startup_token"]
          primary.succeed(f"rm {state}/rollout.json")
          clear_drop_in("fixture-daemon.service")
          refused("abort", f"Harbor holds startup inhibition {startup}")
          refused("abort --adopt-startup-token " + "0" * 32, "differs")
          assert step(f"abort --adopt-startup-token {startup}")["phase"] == "aborted"
          reopened(legacy=True)

      with subtest("a fenced rollout holds the host lock and is aborted"):
          # A finished abort is archived; the same candidate starts afresh.
          assert step("fence")["phase"] == "fenced"
          assert archived() == ["rollout.1.aborted.json", "rollout.2.aborted.json"]
          # The primary's stop takes down its undeclared hard dependent: recorded.
          assert "fixture-dependent.service" in step("status")["clients"]
          fenced()
          assert step("fence")["phase"] == "fenced"
          # Mutating steps share the host activation lock; status takes none.
          primary.succeed("systemd-run --unit=fixture-lock-holder --collect flock /run/lock/fixture-switch.lock ${pkgs.coreutils}/bin/sleep infinity")
          primary.wait_until_fails("flock -n /run/lock/fixture-switch.lock true")
          refused("abort", "execution lock")
          assert step("status")["phase"] == "fenced"
          primary.succeed("systemctl stop fixture-lock-holder.service")
          assert step("abort")["phase"] == "aborted"
          reopened(legacy=True)

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
          primary.succeed("systemctl start fixture-daemon.service fixture-dependent.service fixture-legacy.service")
          reopened(legacy=True)

      with subtest("an opening interrupted before its token is adopted only explicitly"):
          lock = "/var/lib/harbor-db/postgresql/writer-fence.lock"
          maintenance = json.loads(primary.succeed(f"cat {manifest}"))["maintenance"]
          # Harbor's fence-open fails after the coordinator recorded its intent.
          primary.succeed(f"systemd-run --unit=fixture-fence-holder --collect flock {lock} ${pkgs.coreutils}/bin/sleep infinity")
          primary.wait_until_fails(f"flock -n {lock} true")
          refused("fence", "temporarily unavailable")
          journal = step("status")
          assert journal.get("fence_opening") and journal.get("fence_token") is None, journal
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
          reopened(legacy=True)

      with subtest("the full rollout hands off to the executor and thaws"):
          # `next` never re-fences a finished rollout; `fence` archives it first.
          assert step("next")["phase"] == "aborted"
          assert step("fence")["phase"] == "fenced"
          assert len(archived()) == 5
          # Only the opening manifest steps or unwinds the rollout, and it is
          # rooted against GC while the journal is open.
          for arguments in ("next", "abort"):
              refused(arguments, f"--config {manifest}", config=other)
          assert primary.succeed(f"readlink -f {gc_root}").strip() == manifest
          # Pre-switch checks refuse activation mid-rollout, with or without the
          # coordinator in the target generation.
          switch_refused(candidate, "between fence and adoption")
          switch_refused(f"{base}/specialisation/plain", "between fence and adoption")
          # A surviving preparation worker blocks the step, before any receiver stops.
          primary.succeed("systemd-run --unit=harbor-db-rollout-prepare --collect ${pkgs.coreutils}/bin/sleep infinity")
          refused("next", "still running")
          assert step("status")["preparation_worker"] == "harbor-db-rollout-prepare.service"
          primary.succeed("systemctl is-active fixture-receiver.service")
          primary.succeed("systemctl stop harbor-db-rollout-prepare.service")
          prepared = step("next")
          assert prepared["phase"] == "prepared", prepared
          fenced()
          switch_refused(candidate, "between fence and adoption")
          active("fixture-receiver.service")
          # A finished worker's outcome is recovered, never rerun.
          runs = exports()
          rewind("fenced", "export", "export_sha256")
          recovered = step("next")
          assert (recovered["phase"], recovered["export"], recovered["export_sha256"]) == ("prepared", prepared["export"], prepared["export_sha256"]), recovered
          assert exports() == runs
          export = prepared["export"]
          export_root = posixpath.dirname(export)
          primary.succeed(f"tar -C {export_root} -cf /tmp/export.tar base-1")
          primary.copy_from_machine("/tmp/export.tar", "rollout-transfer")
          executor.copy_from_host(str(primary.out_dir / "rollout-transfer/export.tar"), "/tmp/export.tar")
          executor.succeed("install -d -o can -g users -m 0700 /home/can/export; tar -C /home/can/export -xf /tmp/export.tar; chown -R can:users /home/can/export")
          align_receiver(executor, primary)
          certified = json.loads(executor.succeed(f"runuser -u can -- harbor-db-postgres --config {manifest} rollout certify --export /home/can/export/base-1 --export-sha256 {prepared['export_sha256']} --workspace /home/can/workspace"))
          receipt = certified["receipt"]
          receipt_sha256 = certified["receipt_sha256"]
          assert executor.succeed(f"sha256sum {receipt}").split()[0] == receipt_sha256
          executor.copy_from_machine(receipt, "rollout-transfer")
          primary.copy_from_host(str(executor.out_dir / "rollout-transfer/off-host-recovery.json"), "/root/incoming/off-host-recovery.json")
          primary.succeed("chmod 0600 /root/incoming/off-host-recovery.json")
          align_receiver(primary, executor)
          good = f"import --receipt /root/incoming/off-host-recovery.json --sha256 {receipt_sha256}"
          refused(f"import --receipt /root/incoming/off-host-recovery.json --sha256 {'0' * 64}", "differs")
          # A receipt with the right digest that Harbor refuses is not admitted
          # and is set aside by the next import.
          primary.succeed("jq -c '.records = []' /root/incoming/off-host-recovery.json > /root/incoming/rejected.json; chmod 0600 /root/incoming/rejected.json")
          rejected_sha256 = primary.succeed("sha256sum /root/incoming/rejected.json").split()[0]
          refused(f"import --receipt /root/incoming/rejected.json --sha256 {rejected_sha256}", "not admitted")
          assert step("status")["phase"] == "prepared"
          # An exporter that rewrites the certified hand-off during the import's
          # preparation is refused, with the writer window retained.
          primary.succeed("touch /run/fixture-rewrite-export")
          refused(good, "rewrote the certified hand-off")
          primary.succeed(f"test -e {state}/receipt.rejected.1.json")
          assert step("status")["phase"] == "prepared"
          fenced()
          # Restore the certified export: the recorded, admitted receipt settles.
          primary.succeed("rm /run/fixture-rewrite-export")
          primary.succeed(f"rm -r {export}; tar --same-owner -p -C {export_root} -xf /tmp/export.tar")
          runs = exports()
          assert step(good)["phase"] == "imported"
          assert exports() == runs
          rewind("prepared")
          assert step(good)["phase"] == "imported"
          assert exports() == runs
          assert step("next")["phase"] == "adopted"
          primary.succeed("test -s /var/lib/harbor-db/postgresql/identity.json")
          fenced()
          # The legacy generation does not carry this manifest.
          refused("activated", "is not this manifest")
          primary.succeed(f"{candidate}/bin/switch-to-configuration test")
          # The fence's condition skips a start of the client only the candidate brings.
          primary.succeed("systemctl start fixture-candidate.service")
          primary.succeed("test \"$(systemctl show fixture-candidate.service -p ConditionResult --value)\" = no")
          assert step("activated")["phase"] == "activated"
          refused("abort", "thaw with an acceptance")
          step("accept")
          acceptance = f"{state}/acceptance.json"
          assert f"--acceptance {acceptance}" in step("status")["next"]
          thaw = f"thaw --acceptance {acceptance}"
          refused(thaw, f"--config {manifest}", config=other)
          # Its own condition now holds, yet it must stay down: it failed before the pause.
          primary.succeed("touch /run/fixture-conditional")
          daemon_rows = sql("SELECT count(*) FROM retained WHERE id = 'daemon'")
          # A client whose start is refused fails thaw after the fence closed and
          # the primary restarted; the retry resumes without restarting it again.
          drop_in("fixture-daemon.service", "[Unit]\nRefuseManualStart=true\n")
          refused(thaw, "starting fixture-daemon.service")
          reopened_pid = primary_pid()
          clear_drop_in("fixture-daemon.service")
          assert step(thaw)["phase"] == "thawed"
          assert primary_pid() == reopened_pid
          reopened()
          primary.succeed("rm /run/fixture-conditional")
          assert int(sql("SELECT count(*) FROM retained WHERE id = 'daemon'")) == int(daemon_rows) + 1
          primary.succeed(f"test ! -e {slot}")
          # The client the candidate introduced runs; the legacy-only dependent
          # the candidate does not load is skipped and listed.
          active("fixture-candidate.service")
          journal = step("status")
          assert "fixture-legacy.service" in journal.get("unloaded", []), journal
          assert "rollout archive" in journal["next"], journal

      def certify(export, workspace):
          export_root = posixpath.dirname(export)
          primary.succeed(f"tar -C {export_root} -cf /tmp/export.tar base-1")
          primary.copy_from_machine("/tmp/export.tar", workspace)
          executor.copy_from_host(str(primary.out_dir / workspace / "export.tar"), f"/tmp/{workspace}.tar")
          executor.succeed(f"install -d -o can -g users -m 0700 /home/can/{workspace}-export; tar -C /home/can/{workspace}-export -xf /tmp/{workspace}.tar; chown -R can:users /home/can/{workspace}-export")
          align_receiver(executor, primary)
          certified = json.loads(executor.succeed(f"runuser -u can -- harbor-db-postgres --config {manifest} rollout certify --export /home/can/{workspace}-export/base-1 --export-sha256 {step('status')['export_sha256']} --workspace /home/can/{workspace}"))
          executor.copy_from_machine(certified["receipt"], workspace)
          primary.copy_from_host(str(executor.out_dir / workspace / "off-host-recovery.json"), f"/root/{workspace}/off-host-recovery.json")
          primary.succeed(f"chmod 0600 /root/{workspace}/off-host-recovery.json")
          align_receiver(primary, executor)
          return f"import --receipt /root/{workspace}/off-host-recovery.json --sha256 {certified['receipt_sha256']}"

      with subtest("a thawed rollout is archived explicitly; a rolled-back one aborts"):
          # `next` neither archives nor re-fences a finished rollout.
          assert step("next")["phase"] == "thawed"
          primary.succeed(f"{base}/specialisation/plain/bin/switch-to-configuration test")
          assert step("archive")["phase"] == "idle"
          assert {"rollout.6.thawed.json", "rollout.6.receipt.json"} <= set(archived()), archived()
          primary.succeed(f"test ! -e {state}/rollout.json; test ! -e {gc_root}")
          # The previous window's evidence is refused before any side effect,
          # receipts without their snapshot included.
          running = primary_pid()
          refused("fence", "another writer window")
          assert primary_pid() == running
          active("fixture-daemon.service", "fixture-dependent.service")
          primary.succeed("install -d -m 0700 /root/stale; mv /srv/evidence/records.json /srv/evidence/preparation.json /root/stale/")
          refused("fence", "/srv/evidence/off-host.json")
          primary.succeed("mv /srv/evidence/recovery.json /srv/evidence/off-host.json /root/stale/")
          assert primary_pid() == running
          assert step("fence")["phase"] == "fenced"
          fenced()
          assert step("next")["phase"] == "prepared"
          assert step(certify(step("status")["export"], "second"))["phase"] == "imported"
          assert step("next")["phase"] == "adopted"
          # Activated as `nixos-rebuild switch` does: the candidate becomes the
          # boot default too.
          profile = "/nix/var/nix/profiles/system"
          # The specialisation's own toplevel: nix-env would truncate a path
          # inside the base toplevel to the base itself.
          toplevel = primary.succeed(f"readlink -f {candidate}").strip()
          primary.succeed(f"nix-env -p {profile} --set {toplevel}; {toplevel}/bin/switch-to-configuration switch")
          assert primary.succeed(f"readlink -f {profile}/etc/harbor-db/postgresql-rollout.json").strip() == manifest
          assert step("activated")["phase"] == "activated"
          # A generation without the coordinator may roll it back; thaw is then
          # refused, and abort too while the candidate would still boot next.
          primary.succeed(f"{base}/specialisation/plain/bin/switch-to-configuration test")
          refused(f"thaw --acceptance {state}/acceptance.json", "is not this manifest")
          refused("abort", "still the boot default")
          # Rolled back to the legacy generation as the boot default (what
          # `nixos-rebuild switch --rollback` does); the host booted it too.
          primary.succeed(f"nix-env -p {profile} --set {base}; {base}/bin/switch-to-configuration switch")
          assert primary.succeed(f"readlink -f {profile}").strip() == base
          assert primary.succeed("readlink -f /run/booted-system").strip() == base
          assert step("abort")["phase"] == "aborted"
          reopened()
          assert step("archive")["phase"] == "idle"
          assert {"rollout.7.aborted.json", "rollout.7.receipt.json"} <= set(archived()), archived()
          primary.succeed(f"test ! -e {gc_root}")
    '';
  }
