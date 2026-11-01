{
  config,
  lib,
  options,
  pkgs,
  harborDbStoragePackage,
  ...
}: let
  inherit (lib) mkEnableOption mkIf mkMerge mkOption types;
  cfg = config.services.harbor-db.pgBackup;
  sourceRole = lib.elem cfg.role ["source" "both"];
  # localhost, 127.0.0.0/8 and the IPv6 loopback, in their common spellings.
  isLoopback = host: let
    h = lib.removeSuffix "." (lib.toLower host);
  in
    lib.elem h ["localhost" "::1" "0:0:0:0:0:0:0:1"]
    || builtins.match "127\\.[0-9.]*" h != null
    || builtins.match "::ffff:127\\.[0-9.]*" h != null;
  targetRole = lib.elem cfg.role ["target" "both"];
  loopbackAddresses = lib.optionals cfg.targetSettings.sourceLocalRecovery.enable (
    lib.optional (lib.elem cfg.source.hostName ["127.0.0.1" "localhost"]) "127.0.0.1"
    ++ lib.optional (lib.elem cfg.source.hostName ["::1" "localhost"]) "::1"
  );
  replicationHosts = lib.unique (cfg.sourceSettings.allowedReplicationHosts
    ++ map (address:
      address
      + (
        if address == "::1"
        then "/128"
        else "/32"
      ))
    loopbackAddresses);
  listenAddresses = lib.unique (cfg.sourceSettings.listenAddresses ++ loopbackAddresses);
  storagePackage = harborDbStoragePackage;
  socketDir = let
    dirs = config.services.postgresql.settings.unix_socket_directories or "/run/postgresql";
  in
    lib.head (lib.filter (d: d != "") (map lib.trim (lib.splitString "," dirs)));
  durable = "${storagePackage}/bin/harbor-db-durable";
  pruneTool = "${storagePackage}/bin/harbor-db-backup-prune";
  pgBackupTool = "${storagePackage}/bin/harbor-db-pg-backup";
  storagePaths = import ./storage-paths.nix {inherit lib;};
  custodyNamespaces = import ./custody-namespaces.nix {inherit config lib options;};
  sourceLocalTablespaceCheck = pkgs.writeShellScript "pg-backup-source-local-tablespaces" ''
    set -euo pipefail
    tablespaces=${lib.escapeShellArg "${config.services.postgresql.dataDir}/pg_tblspc"}
    if [ ! -d "$tablespaces" ] || [ -L "$tablespaces" ]; then
      echo "pg-backup: source-local recovery requires a primary without external tablespaces" >&2
      exit 1
    fi
    entries="$(${pkgs.findutils}/bin/find "$tablespaces" -mindepth 1 -maxdepth 1 -print -quit)"
    if [ -n "$entries" ]; then
      echo "pg-backup: source-local recovery requires a primary without external tablespaces" >&2
      exit 1
    fi
  '';

  # A hostname is also used as a directory component. Keep the legacy IPv4
  # paths stable while preventing IPv6 / URI punctuation from becoming path
  # separators when the target list is expanded.
  sourceDirName = hostName: lib.replaceStrings ["/" ":" "[" "]"] ["_" "_" "_" "_"] hostName;
  backupRootFor = source: "${cfg.targetSettings.backupDir}/${sourceDirName source.hostName}";

  targetOptions = {
    walSegmentBytes = mkOption {
      type = types.enum [1048576 2097152 4194304 8388608 16777216 33554432 67108864 134217728 268435456 536870912 1073741824];
      default = 16777216;
      description = "Source cluster WAL segment size in bytes, a power of two from 1 MiB through 1 GiB (initdb default: 16 MiB). Required for conservative WAL retention boundaries.";
    };
    package = mkOption {
      type = types.package;
      default = pkgs.postgresql;
      defaultText = lib.literalExpression "pkgs.postgresql";
      description = ''
        PostgreSQL package providing pg_receivewal, pg_basebackup, and
        pg_verifybackup. It should match the source server's major version.
      '';
    };

    replicatorPasswordFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = "Path to the replicator password file on the target host.";
    };

    receiveWal = {
      enable = mkEnableOption "continuous WAL streaming via pg_receivewal" // {default = true;};
      slotName = mkOption {
        type = types.str;
        default = "pgbackup_wal";
        description = "Replication slot name created on the source for pg_receivewal; 1-63 lowercase ASCII letters, digits or underscores.";
      };
      statusInterval = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        description = "pg_receivewal --status-interval seconds for the slot preflight and the receiver. null keeps the historical 5 s preflight and pg_receivewal's own default for the receiver.";
      };
      slotCreateTimeoutSeconds = mkOption {
        type = types.nullOr types.ints.positive;
        default = null;
        description = "When set, the slot-creation preflight is best effort (its failure is ignored) and killed after this many seconds, so an unreachable source cannot hold a system switch open. null keeps the strict, unbounded preflight.";
      };
    };

    baseBackup = {
      enable = mkEnableOption "periodic base backup via pg_basebackup" // {default = true;};
      schedule = mkOption {
        type = types.str;
        default = "daily";
        description = "systemd OnCalendar schedule for base backup pulls.";
      };
      maxRate = mkOption {
        type = types.nullOr types.str;
        default = "20M";
        description = "Bandwidth limit for pg_basebackup (null = unlimited).";
      };
      slotName = mkOption {
        type = types.str;
        default = "pgbackup_base";
        description = "Temporary replication slot name created during base backup; 1-63 lowercase ASCII letters, digits or underscores.";
      };
      recoveryMeta = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Publish recovery-ready bases: after the verified tree is published,
          write `base/<id>.meta.json` (backup_id, source_host, pg_major,
          timeline, epoch_id, system_identifier, checksum, post_backup_lsn,
          backup_stop_lsn, archive_flush_lsn, restore_point) and then
          `LAST_SUCCESS=<id>`, as read by the recovery gate's inspect-backup.
          It creates a restore point and switches WAL on the source through
          `sqlConninfo` and waits for the archive to hold the segment. Also
          enables gap tracking.
        '';
      };
      sqlConninfo = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "host=10.0.0.1 user=pgbackup_marker dbname=postgres passfile=/run/secrets/pgpass";
        description = ''
          libpq connection string for the source used by `recoveryMeta`. The
          role needs EXECUTE on `pg_create_restore_point(text)` and
          `pg_switch_wal()` and `pg_control_system()`, and must reach the primary (standby sources are
            refused at finalize). A source restored to an earlier point, or
            replaced, needs a fresh backup root. Defaults to the local postgres socket for
          role=source/both; required for role=target.
        '';
      };
      admission = {
        minRamBytes = mkOption {
          type = types.nullOr types.ints.unsigned;
          default = null;
          description = "Skip the base backup (with a log line, not a failure) when MemAvailable is below this many bytes. null or 0 disables the check.";
        };
        minSwapBytes = mkOption {
          type = types.nullOr types.ints.unsigned;
          default = null;
          description = "Skip the base backup when free swap is below this many bytes; checked only when the host has swap. null or 0 disables the check.";
        };
      };
    };

    health = {
      enable = mkEnableOption "the periodic backup-health check (slot, receiver, WAL freshness and contiguity, WAL gap marker, LAST_SUCCESS age) and WAL gap tracking";
      schedule = mkOption {
        type = types.str;
        default = "hourly";
        description = "systemd OnCalendar schedule for the health check.";
      };
      maxWalAgeSeconds = mkOption {
        type = types.ints.positive;
        default = 7200;
        description = "Fail when the newest received WAL file is older than this. An idle source may need a larger value or archive_timeout.";
      };
      maxBaseAgeSeconds = mkOption {
        type = types.nullOr types.ints.positive;
        default = 129600;
        description = "Fail when LAST_SUCCESS is older than this (null disables; ignored without baseBackup.enable).";
      };
    };

    retain = {
      baseBackupDays = mkOption {
        type = types.ints.positive;
        default = 30;
        description = "Days to keep base backups.";
      };
      walDays = mkOption {
        type = types.ints.positive;
        default = 31;
        description = "Days to keep WAL segments; must be at least baseBackupDays + 1.";
      };
    };
  };

  mkTarget = name: {
    source,
    ts,
    backupRoot,
  }: let
    sourceId = source.hostName;
    isLegacy = name == "legacy";
    suffix =
      if isLegacy
      then ""
      else "-${name}";
    # Source-local recovery is a property of the single legacy target.
    localRecovery = isLegacy && cfg.targetSettings.sourceLocalRecovery.enable;
    baseCfg = ts.baseBackup;
    healthCfg = ts.health;
    # Gap tracking is on only when something consumes it, so existing
    # deployments keep their units unchanged.
    gapTracking = baseCfg.recoveryMeta || healthCfg.enable;
    sqlConninfo =
      if baseCfg.sqlConninfo != null
      then baseCfg.sqlConninfo
      else if sourceRole && isLegacy && lib.elem source.hostName ["127.0.0.1" "localhost" "::1"]
      # libpq conninfo values: single-quoted, with backslash and quote escaped.
      then "host='${lib.replaceStrings ["\\" "'"] ["\\\\" "\\'"] socketDir}' port=${toString source.port} user=postgres dbname=postgres"
      else null;
    admissionArgs = lib.concatStringsSep " " (
      lib.optional (baseCfg.admission.minRamBytes != null) "--min-ram-bytes ${toString baseCfg.admission.minRamBytes}"
      ++ lib.optional (baseCfg.admission.minSwapBytes != null) "--min-swap-bytes ${toString baseCfg.admission.minSwapBytes}"
    );
    passwordFile =
      if ts.replicatorPasswordFile != null
      then ts.replicatorPasswordFile
      else if cfg.targetSettings.replicatorPasswordFile != null
      then cfg.targetSettings.replicatorPasswordFile
      else "/run/invalid/pg-replicator-password";
    wrapBin = bin:
      pkgs.writers.writeBash "pg-backup-${name}-${baseNameOf bin}" ''
        set -euo pipefail
        password_file="${passwordFile}"
        if [ ! -r "$password_file" ] || [ ! -s "$password_file" ]; then
          echo "pg-backup: password file $password_file not readable or empty" >&2
          exit 1
        fi
        export PGPASSWORD="$(cat "$password_file")"
        exec ${ts.package}/${bin} "$@"
      '';
    pgReceivewalCmd = "${wrapBin "bin/pg_receivewal"} -h ${sourceId} -p ${toString source.port} -U replicator";
    replicationConninfo = "host=${sourceId} port=${toString source.port} user=replicator replication=true connect_timeout=10";
    # The gap probe honours the same bound as best-effort slot creation.
    gapConninfo =
      if ts.receiveWal.slotCreateTimeoutSeconds == null
      then replicationConninfo
      else lib.replaceStrings ["connect_timeout=10"] ["connect_timeout=${toString ts.receiveWal.slotCreateTimeoutSeconds}"] replicationConninfo;
    createSlot = "${pgReceivewalCmd} -D ${walDir} --status-interval=${toString (
      if ts.receiveWal.statusInterval == null
      then 5
      else ts.receiveWal.statusInterval
    )} --no-loop --slot=${ts.receiveWal.slotName} --create-slot --if-not-exists";
    psqlWrapped = wrapBin "bin/psql";
    pgBasebackupCmd = "${wrapBin "bin/pg_basebackup"} -h ${sourceId} -p ${toString source.port} -U replicator";
    walDir = "${backupRoot}/wal";
    pruneBackupsScript = ''
      retain_days=${toString ts.retain.baseBackupDays}
      wal_retain=${toString ts.retain.walDays}

      prune_old_backups() {
        ${pruneTool} --root "$backup_root" --base-days "$retain_days" \
          --wal-days "$wal_retain" --segment-bytes ${toString ts.walSegmentBytes}
      }
    '';
  in {
    systemd.tmpfiles.rules =
      [
        "d ${backupRoot} 0750 postgres postgres -"
        "d ${backupRoot}/wal 0750 postgres postgres -"
        "d ${backupRoot}/base 0750 postgres postgres -"
      ]
      ++ lib.optionals localRecovery [
        "d ${backupRoot}/locks 0700 postgres postgres -"
        "f ${backupRoot}/locks/mutate 0600 postgres postgres -"
        "d ${backupRoot}/recovery 0700 postgres postgres -"
        "d ${backupRoot}/recovery/captures 0700 postgres postgres -"
        "d ${backupRoot}/recovery/snapshots 0700 postgres postgres -"
        "d ${backupRoot}/recovery/pins 0700 postgres postgres -"
        "f ${backupRoot}/recovery/PROTOCOL 0600 postgres postgres - source-local-v1"
      ];

    # `--create-slot` is a one-shot operation. It must be an ExecStartPre;
    # passing it to the long-running command makes pg_receivewal exit 0 after
    # creating the slot, which silently disables continuous WAL archiving.
    systemd.services."pg-receivewal${suffix}" = mkIf ts.receiveWal.enable {
      description = "Receive WAL segments from ${sourceId}";
      unitConfig.RequiresMountsFor = backupRoot;
      after = ["network-online.target"] ++ lib.optionals localRecovery ["postgresql.service" "postgresql-setup.service"];
      requires = lib.optionals localRecovery ["postgresql.service" "postgresql-setup.service"];
      wants = ["network-online.target"];
      wantedBy = ["multi-user.target"];
      serviceConfig = {
        User = "postgres";
        ExecStartPre =
          lib.optional localRecovery "${sourceLocalTablespaceCheck}"
          ++ lib.optional gapTracking "${pgBackupTool} mark-gap --root ${backupRoot} --segment-bytes ${toString ts.walSegmentBytes} --psql ${psqlWrapped} --conninfo \"${gapConninfo}\" --slot ${ts.receiveWal.slotName}"
          ++ [
            (
              if ts.receiveWal.slotCreateTimeoutSeconds == null
              then createSlot
              # Best effort and bounded: an offline source must not hold a switch open.
              else "-${pkgs.coreutils}/bin/timeout ${toString ts.receiveWal.slotCreateTimeoutSeconds}s ${createSlot}"
            )
          ];
        ExecStart = "${pgReceivewalCmd} -D ${walDir} --verbose --synchronous --slot=${ts.receiveWal.slotName}" + lib.optionalString (ts.receiveWal.statusInterval != null) " --status-interval=${toString ts.receiveWal.statusInterval}";
        Restart = "on-failure";
        RestartSec = "5s";
        RestartSteps = 6;
        RestartMaxDelaySec = "5min";
        PrivateTmp = true;
        AmbientCapabilities = "";
        CapabilityBoundingSet = "";
        NoNewPrivileges = true;
        UMask = "0077";
      };
    };

    systemd.services."pg-backup-prune${suffix}" = {
      description = "Prune retained PostgreSQL backups from ${sourceId}";
      unitConfig.RequiresMountsFor = backupRoot;
      serviceConfig = {
        Type = "oneshot";
        User = "postgres";
        ProtectSystem = "strict";
        ReadWritePaths = [backupRoot];
        UMask = "0077";
      };
      script = ''
        set -euo pipefail
        backup_root="${backupRoot}"
        wal_dir="${walDir}"
        ${pruneBackupsScript}
        prune_old_backups
      '';
    };

    # Pull a base backup into a partial directory, verify its manifest, then
    # publish it atomically. A failed transfer therefore cannot masquerade as
    # a complete dated backup.
    systemd.services."pg-basebackup${suffix}" = mkIf ts.baseBackup.enable {
      description = "Pull base backup from ${sourceId}";
      unitConfig.RequiresMountsFor = backupRoot;
      after = ["network-online.target" "pg-backup-prune${suffix}.service"] ++ lib.optionals localRecovery ["postgresql.service" "pg-receivewal${suffix}.service"];
      requires = ["pg-backup-prune${suffix}.service"] ++ lib.optionals localRecovery ["postgresql.service" "pg-receivewal${suffix}.service"];
      wants = ["network-online.target"];
      serviceConfig = {
        Type = "oneshot";
        User = "postgres";
        ExecStartPre = lib.optional localRecovery "${sourceLocalTablespaceCheck}";
        ExecCondition = lib.optional (admissionArgs != "") "${pgBackupTool} admit ${admissionArgs}";
        ProtectSystem = "strict";
        ReadWritePaths = [backupRoot];
        UMask = "0077";
      };
      script = ''
        set -euo pipefail

        backup_root="${backupRoot}"
        ${lib.optionalString localRecovery ''
          # Pin the recovery mutation inode before the legacy publication anchor.
          # Opening an existing inode never creates or replaces either anchor.
          test -f "$backup_root/locks/mutate"
          test ! -L "$backup_root/locks/mutate"
          exec 8<>"$backup_root/locks/mutate"
          ${pkgs.util-linux}/bin/flock -n 8
        ''}
        exec 9>>"$backup_root/BACKUP_LOCK"
        ${pkgs.util-linux}/bin/flock -n 9
        # Never replace the last good backup during another run on the same day.
        ${lib.optionalString gapTracking ''started_ms="$(date +%s%3N)"''}
        date_dir="$backup_root/base/$(date -u +%Y-%m-%dT%H%M%S)-$$"
        partial_dir="$date_dir.partial"
        wal_dir="${walDir}"
        # Preserve any interrupted transfer, even if its generated name collides.
        test ! -e "$partial_dir"
        test ! -e "$date_dir"
        mkdir -p "$partial_dir"

        max_rate=${
          if ts.baseBackup.maxRate != null
          then "'--max-rate=${ts.baseBackup.maxRate}'"
          else "''"
        }
        slot=${ts.baseBackup.slotName}

        # Ensure the temporary slot exists for the backup duration.
        ${pgReceivewalCmd} --status-interval=5 --no-loop --slot="$slot" --drop-slot 2>/dev/null || true
        ${pgReceivewalCmd} --status-interval=5 --no-loop --slot="$slot" --create-slot 2>/dev/null || true

        cleanup_slot() {
          ${pgReceivewalCmd} --slot="$slot" --drop-slot 2>/dev/null || true
        }
        trap cleanup_slot EXIT

        ${pgBasebackupCmd} -D "$partial_dir" --wal-method=stream \
          $max_rate --verbose --slot="$slot"

        ${ts.package}/bin/pg_verifybackup "$partial_dir"
        ${durable} publish-tree "$partial_dir" "$date_dir"
        ${
          if baseCfg.recoveryMeta
          then ''
            # Metadata first, then LAST_SUCCESS=<id>; a verified later base also clears a WAL gap.
            ${pgBackupTool} finalize --root "$backup_root" --id "$(basename "$date_dir")" \
              --started-ms "$started_ms" --segment-bytes ${toString ts.walSegmentBytes} \
              --package ${ts.package} --source-host ${lib.escapeShellArg sourceId} \
              --sql-psql ${ts.package}/bin/psql --sql-conninfo ${lib.escapeShellArg sqlConninfo}
          ''
          else ''
            # Sidecars left from an earlier recoveryMeta would keep health in metadata mode.
            rm -f "$backup_root"/base/*.meta.json
            printf '%s\n' "$(date --iso-8601=seconds)" | ${durable} write "$backup_root/LAST_SUCCESS"
            ${lib.optionalString gapTracking "${pgBackupTool} clear-gap --root \"$backup_root\" --started-ms \"$started_ms\""}
          ''
        }
        # The separate prune service uses the same lock; retention runs next time.
      '';
    };

    systemd.services."pg-backup-health${suffix}" = mkIf healthCfg.enable {
      description = "Check PostgreSQL backup chain health for ${sourceId}";
      unitConfig.RequiresMountsFor = backupRoot;
      serviceConfig = {
        Type = "oneshot";
        User = "postgres";
        ProtectSystem = "strict";
        ReadOnlyPaths = [backupRoot];
        UMask = "0077";
        ExecStart = lib.concatStringsSep " " ([
            "${pgBackupTool} backup-health --root ${backupRoot}"
            "--segment-bytes ${toString ts.walSegmentBytes}"
            "--slot ${ts.receiveWal.slotName}"
            "--psql ${psqlWrapped}"
            "--conninfo \"${replicationConninfo}\""
            "--systemctl ${config.systemd.package}/bin/systemctl --receiver-unit pg-receivewal${suffix}.service"
            "--max-wal-age-seconds ${toString healthCfg.maxWalAgeSeconds}"
          ]
          ++ lib.optional (healthCfg.maxBaseAgeSeconds != null) "--max-base-age-seconds ${toString healthCfg.maxBaseAgeSeconds}"
          ++ lib.optional baseCfg.recoveryMeta "--recovery-meta");
      };
    };

    systemd.timers."pg-backup-health${suffix}" = mkIf healthCfg.enable {
      description = "Schedule PostgreSQL backup health check for ${sourceId}";
      wantedBy = ["timers.target"];
      timerConfig = {
        OnCalendar = healthCfg.schedule;
        Persistent = true;
        RandomizedDelaySec = "5min";
      };
    };

    systemd.timers."pg-basebackup${suffix}" = mkIf ts.baseBackup.enable {
      description = "Schedule daily PostgreSQL base backup from ${sourceId}";
      wantedBy = ["timers.target"];
      timerConfig = {
        OnCalendar = ts.baseBackup.schedule;
        Persistent = true;
        RandomizedDelaySec = "30min";
      };
    };
  };
in {
  imports = [./storage-package-argument.nix];
  options.services.harbor-db.pgBackup = {
    enable = mkEnableOption "PostgreSQL backup replication (source or target)";

    role = mkOption {
      type = types.enum ["source" "target" "both"];
      description = ''
        Whether this host is the backup source (runs the PostgreSQL being
        backed up) or the backup target (receives WAL archives and pulls
        base backups). Both selects source and target services on this host,
        including source-local recovery production through a loopback connection.
      '';
    };

    source = {
      hostName = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "10.0.0.1";
        description = "Hostname or IP address of the source PostgreSQL server, reachable from the target. Required except for role=target with named `targets`.";
      };

      port = mkOption {
        type = types.port;
        default = 5432;
        description = "PostgreSQL port on the source server.";
      };
    };

    sourceSettings = {
      walLevel = mkOption {
        type = types.enum ["minimal" "replica" "logical"];
        default = "replica";
        description = "PostgreSQL wal_level. Must be replica or higher for replication.";
      };

      maxWalSenders = mkOption {
        type = types.ints.positive;
        default = 3;
        description = "Maximum concurrent WAL sender processes.";
      };

      maxReplicationSlots = mkOption {
        type = types.ints.positive;
        default = 2;
        description = "Maximum replication slots.";
      };

      listenAddresses = mkOption {
        type = types.listOf types.str;
        default = [];
        example = ["192.168.1.10"];
        description = "Exact IP addresses for PostgreSQL to listen on when replication is enabled. An empty list derives loopback listeners for source-local recovery.";
      };

      allowedReplicationHosts = mkOption {
        type = types.listOf types.str;
        default = [];
        example = ["192.168.1.20/32"];
        description = "CIDR-notation hosts allowed to connect for replication (added to pg_hba.conf).";
      };

      firewallInterface = mkOption {
        type = types.nullOr types.str;
        default = "wg-home";
        example = "eno1";
        description = "Interface on which to allow the PostgreSQL replication port, or null to manage the firewall elsewhere.";
      };

      replicatorPasswordFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        description = "Path to a file containing the 'replicator' role password. Required on the source, optional on the target.";
      };
    };

    targetSettings =
      targetOptions
      // {
        backupDir = mkOption {
          type = types.path;
          default = "/var/backups/pgbackup";
          description = "Root backup storage directory. Each source gets its own WAL/base subdirectory.";
        };

        sourceLocalRecovery.enable = mkEnableOption "the source-local-v1 capture namespace, authenticated loopback replication, and recovery-serialized backup publication";
      };

    targets = mkOption {
      default = {};
      description = ''
        Additional named backup targets. Each gets units suffixed `-<name>`
        (pg-receivewal-<name>, pg-basebackup-<name>, pg-backup-prune-<name>,
        pg-backup-health-<name>) and the root `targetSettings.backupDir/<name>`.
        The single-source `source`/`targetSettings` options keep describing the
        unsuffixed legacy target. Requires role target or both.
      '';
      type = types.attrsOf (types.submodule {
        options =
          targetOptions
          // {
            source = {
              hostName = mkOption {
                type = types.str;
                example = "10.0.0.1";
                description = "Hostname or IP address of this target's PostgreSQL server.";
              };
              port = mkOption {
                type = types.port;
                default = 5432;
                description = "PostgreSQL port of this target's server.";
              };
            };
          };
      });
    };
  };

  config = mkIf cfg.enable (let
    targetSources =
      # Backwards-compatible singleton target. Existing deployments retain
      # the historical pg-* unit names and backup directory layout.
      lib.optionalAttrs (cfg.source.hostName != null) {
        legacy = {
          inherit (cfg) source;
          ts = cfg.targetSettings;
          backupRoot = backupRootFor cfg.source;
        };
      }
      // lib.mapAttrs (name: t: {
        inherit (t) source;
        ts = t;
        backupRoot = "${cfg.targetSettings.backupDir}/${name}";
      })
      cfg.targets;
    targetAssertions = lib.concatLists (lib.mapAttrsToList (name: target: let
        inherit (target) ts;
        tag = lib.optionalString (name != "legacy") " [target ${name}]";
        base = ts.baseBackup;
        sqlOk = base.sqlConninfo != null || (sourceRole && name == "legacy" && lib.elem target.source.hostName ["127.0.0.1" "localhost" "::1"]);
      in [
        {
          assertion = ts.receiveWal.enable || base.enable;
          message = "services.harbor-db.pgBackup (role=target) requires at least one target operation to be enabled.${tag}";
        }
        {
          assertion = ts.retain.walDays >= ts.retain.baseBackupDays + 1;
          message = "services.harbor-db.pgBackup requires walDays >= baseBackupDays + 1 for safe PITR.${tag}";
        }
        {
          # The connection string lands in the world-readable unit script.
          assertion = base.sqlConninfo == null || builtins.match ".*password[[:space:]]*=.*" (lib.toLower base.sqlConninfo) == null && builtins.match "[a-z]+://[^/@]*:[^/@]*@.*" (lib.toLower base.sqlConninfo) == null && !lib.hasInfix "%" base.sqlConninfo;
          message = "services.harbor-db.pgBackup baseBackup.sqlConninfo must not embed a password or use percent-encoding; use passfile=/path (the store is world-readable).${tag}";
        }
        {
          assertion = !base.recoveryMeta || ts.receiveWal.enable;
          message = "services.harbor-db.pgBackup baseBackup.recoveryMeta requires receiveWal.enable (it waits for the archived WAL).${tag}";
        }
        {
          assertion = !(base.recoveryMeta || ts.health.enable) || lib.versionAtLeast ts.package.version "15";
          message = "services.harbor-db.pgBackup recoveryMeta and health probe the slot with READ_REPLICATION_SLOT and need a PostgreSQL 15 or newer package.${tag}";
        }
        {
          assertion = !base.recoveryMeta || (base.enable && sqlOk);
          message = "services.harbor-db.pgBackup baseBackup.recoveryMeta requires baseBackup.enable and a source SQL connection (sqlConninfo, or a loopback legacy target on role=source/both).${tag}";
        }
        {
          assertion = !ts.health.enable || (ts.receiveWal.enable && base.enable);
          message = "services.harbor-db.pgBackup health.enable requires receiveWal.enable and baseBackup.enable.${tag}";
        }
        {
          assertion = ts.replicatorPasswordFile != null || cfg.targetSettings.replicatorPasswordFile != null;
          message = "services.harbor-db.pgBackup (role=target) requires targetSettings.replicatorPasswordFile to be set.${tag}";
        }
        {
          assertion = lib.all (operation: !operation.enable || builtins.match "[a-z0-9_]{1,63}" operation.slotName != null) [ts.receiveWal base];
          message = "services.harbor-db.pgBackup enabled WAL/base-backup operations require valid PostgreSQL replication slot names: 1-63 lowercase ASCII letters, digits or underscores.${tag}";
        }
        {
          assertion = !ts.receiveWal.enable || !base.enable || ts.receiveWal.slotName != base.slotName;
          message = "services.harbor-db.pgBackup WAL reception and base backups require distinct replication slots when both operations are enabled.${tag}";
        }
      ])
      targetSources);
    slotClaims = lib.concatMap (t: let
      # DNS names are case-insensitive (RFC 4343) and a trailing root dot is the same name.
      name = lib.removeSuffix "." (lib.toLower t.source.hostName);
      host =
        if lib.elem name ["localhost" "::1"]
        then "127.0.0.1"
        else name;
      endpoint = "${host}:${toString t.source.port}";
    in
      lib.optional t.ts.receiveWal.enable "${endpoint}/${t.ts.receiveWal.slotName}"
      ++ lib.optional t.ts.baseBackup.enable "${endpoint}/${t.ts.baseBackup.slotName}") (lib.attrValues targetSources);
  in
    mkMerge [
      {
        assertions =
          lib.optionals targetRole targetAssertions
          ++ [
            {
              assertion = lib.all (name: name != "legacy" && builtins.match "[a-z0-9][a-z0-9_-]{0,40}" name != null) (lib.attrNames cfg.targets);
              message = "services.harbor-db.pgBackup target names are 1-41 lowercase ASCII letters, digits, `_` or `-`, and `legacy` is reserved.";
            }
            {
              assertion = cfg.targets == {} || targetRole;
              message = "services.harbor-db.pgBackup.targets requires role target or both.";
            }
            {
              # Named targets are for remote sources; the local-source plumbing (pg_hba, replicator role, capacity) belongs to the legacy target.
              assertion = lib.all (t: !isLoopback t.source.hostName) (lib.attrValues cfg.targets);
              message = "services.harbor-db.pgBackup.targets.<name> must point at a remote source; use the single-source options for a loopback source.";
            }
            {
              # Paths reach systemd command lines unquoted.
              assertion = !targetRole || builtins.match "/[A-Za-z0-9_./-]+" cfg.targetSettings.backupDir != null;
              message = "services.harbor-db.pgBackup targetSettings.backupDir may contain only letters, digits and _ . / - (it is used unquoted in systemd commands).";
            }
            {
              assertion = let
                roots = map (t: t.backupRoot) (lib.attrValues targetSources);
              in
                !targetRole || lib.length roots == lib.length (lib.unique roots);
              message = "services.harbor-db.pgBackup target backup roots collide (a legacy source host name sanitizes to a named target's name); rename the target or relocate backupDir.";
            }
            {
              # source-local-v1 keeps a timestamp in LAST_SUCCESS; recoveryMeta writes the backup id.
              # Only the legacy target runs source-local recovery; named targets have their own roots.
              assertion = !cfg.targetSettings.sourceLocalRecovery.enable || !(targetSources.legacy.ts.baseBackup.recoveryMeta or false);
              message = "services.harbor-db.pgBackup baseBackup.recoveryMeta cannot be combined with sourceLocalRecovery (it changes the LAST_SUCCESS contract).";
            }
            {
              assertion = !targetRole || lib.length slotClaims == lib.length (lib.unique slotClaims);
              message = "services.harbor-db.pgBackup targets sharing a source host and port need distinct replication slot names.";
            }
            {
              assertion = !targetRole || cfg.source.hostName != null || (cfg.role == "target" && cfg.targets != {});
              message = "services.harbor-db.pgBackup.source.hostName is required for target roles unless role=target with named targets.";
            }
            {
              assertion = sourceRole -> config.services.postgresql.enable or false;
              message = "services.harbor-db.pgBackup (role=source) requires services.postgresql.enable = true on this host.";
            }
            {
              assertion = !sourceRole || cfg.sourceSettings.replicatorPasswordFile != null;
              message = "services.harbor-db.pgBackup (role=source) requires sourceSettings.replicatorPasswordFile to be set.";
            }
            {
              assertion =
                !(targetRole && config.services.postgresql.enable)
                || (let
                  paths = map (t: t.backupRoot) (lib.attrValues targetSources) ++ [config.services.postgresql.dataDir];
                in
                  lib.all storagePaths.normalizedAbsolute paths && storagePaths.disjoint paths);
              message = "services.harbor-db.pgBackup target backup roots and the local primary data directory must be normalized absolute and pairwise disjoint.";
            }
            {
              assertion = !cfg.targetSettings.sourceLocalRecovery.enable || (sourceRole && targetRole && lib.elem cfg.source.hostName ["127.0.0.1" "localhost" "::1"]);
              message = "services.harbor-db.pgBackup sourceLocalRecovery requires role=both and a loopback source for fence-compatible WAL reception.";
            }
            {
              assertion =
                !targetRole
                || (let
                  paths = map (t: t.backupRoot) (lib.attrValues targetSources) ++ custodyNamespaces;
                in
                  lib.all storagePaths.normalizedAbsolute paths && storagePaths.disjoint paths);
              message = "services.harbor-db.pgBackup target backup roots must be normalized absolute and disjoint from all enabled custody namespaces, including PostgreSQL lifecycle and startup state.";
            }
            {
              assertion = !cfg.targetSettings.sourceLocalRecovery.enable || cfg.targetSettings.receiveWal.enable;
              message = "services.harbor-db.pgBackup sourceLocalRecovery requires receiveWal.enable for continuous post-target WAL coverage.";
            }
            {
              assertion = !sourceRole || cfg.source.port == config.services.postgresql.settings.port;
              message = "services.harbor-db.pgBackup source port must match the effective local PostgreSQL primary port.";
            }
            {
              assertion = !cfg.targetSettings.sourceLocalRecovery.enable || cfg.targetSettings.baseBackup.enable;
              message = "services.harbor-db.pgBackup sourceLocalRecovery requires baseBackup.enable for fresh verified recovery base backups.";
            }
            {
              assertion = !cfg.targetSettings.sourceLocalRecovery.enable || (cfg.sourceSettings.maxReplicationSlots >= 2 && cfg.sourceSettings.maxWalSenders >= 3 && config.services.postgresql.settings.max_replication_slots >= 2 && config.services.postgresql.settings.max_wal_senders >= 3);
              message = "services.harbor-db.pgBackup sourceLocalRecovery requires at least two replication slots and three WAL senders, including effective PostgreSQL settings.";
            }
            {
              # Local streams: each running receiver holds one sender and slot, and a
              # streaming base backup needs two sender connections and its own slot.
              assertion = let
                local = lib.filter (t: sourceRole && lib.elem t.source.hostName ["127.0.0.1" "localhost" "::1"] && t.source.port == config.services.postgresql.settings.port) (lib.attrValues targetSources);
                count = f: lib.length (lib.filter f local);
                receivers = count (t: t.ts.receiveWal.enable);
                bases = count (t: t.ts.baseBackup.enable);
              in
                local
                == []
                || (config.services.postgresql.settings.max_replication_slots
                  >= receivers + bases
                  && config.services.postgresql.settings.max_wal_senders >= receivers + 2 * bases);
              message = "services.harbor-db.pgBackup local receiver plus base backups need one slot per receiver and base backup and one WAL sender per receiver plus two per base backup in the effective PostgreSQL settings.";
            }
            {
              assertion = !sourceRole || (config.services.postgresql.settings.max_replication_slots >= 1 && config.services.postgresql.settings.max_wal_senders >= 1);
              message = "services.harbor-db.pgBackup source roles require nonzero effective replication capacity for WAL senders and slots.";
            }
            {
              assertion = !sourceRole || lib.elem config.services.postgresql.settings.wal_level ["replica" "logical"];
              message = "services.harbor-db.pgBackup source roles require effective wal_level=replica or logical for streaming replication and physical recovery.";
            }
            {
              assertion = !cfg.targetSettings.sourceLocalRecovery.enable || !(config.services.harbor-db.postgresql.enable or false) || lib.elem "replicator" config.services.harbor-db.postgresql.writerFence.replicationRoles;
              message = "services.harbor-db.pgBackup sourceLocalRecovery requires replicator in the writer-fence replication roles.";
            }
          ];
        # The generated receiver uses this physical-only SCRAM role while the
        # writer fence rejects every SQL client. Preserve configured peer roles.
        services.harbor-db = lib.optionalAttrs (options.services.harbor-db ? postgresql) {
          postgresql.writerFence.replicationRoles = lib.mkIf (sourceRole && cfg.targetSettings.sourceLocalRecovery.enable) (lib.mkAfter ["replicator"]);
        };
      }

      (mkIf sourceRole {
        services.postgresql = {
          settings = mkMerge [
            (lib.optionalAttrs (listenAddresses != []) {
              # NixOS' enableTCPIP setting otherwise forces '*'. Replication
              # should bind only to the explicitly selected interface addresses.
              listen_addresses = lib.mkForce (lib.concatStringsSep "," listenAddresses);
            })
            {
              wal_level = lib.mkDefault cfg.sourceSettings.walLevel;
              max_wal_senders = lib.mkDefault cfg.sourceSettings.maxWalSenders;
              max_replication_slots = lib.mkDefault cfg.sourceSettings.maxReplicationSlots;
            }
          ];
          ensureUsers = [
            {
              name = "replicator";
              ensureClauses.replication = true;
            }
          ];
          authentication = lib.mkAfter (
            lib.concatMapStringsSep "\n" (host: "host replication replicator ${host} scram-sha-256")
            replicationHosts
          );
        };

        systemd.services.postgresql-setup.script = lib.mkAfter (
          lib.optionalString cfg.targetSettings.sourceLocalRecovery.enable ''
            ${sourceLocalTablespaceCheck}
          ''
          + lib.optionalString (cfg.sourceSettings.replicatorPasswordFile != null) ''
            if [ ! -r ${cfg.sourceSettings.replicatorPasswordFile} ] || [ ! -s ${cfg.sourceSettings.replicatorPasswordFile} ]; then
              echo "pg-backup: replicator password file unreadable or empty" >&2
              exit 1
            fi
            printf '%s\n' \
              '\set replicator_password `cat ${cfg.sourceSettings.replicatorPasswordFile}`' \
              "ALTER ROLE replicator WITH PASSWORD :'replicator_password';" \
              | psql -d postgres
          ''
        );
      })

      (mkIf (sourceRole && cfg.sourceSettings.firewallInterface != null && cfg.sourceSettings.allowedReplicationHosts != []) {
        networking.firewall.interfaces."${cfg.sourceSettings.firewallInterface}".allowedTCPPorts = [cfg.source.port];
      })

      (mkIf targetRole
        (mkMerge [
          {
            # Preserve the legacy same-secret fallback for deployments that
            # put the agenix path under sourceSettings on both hosts.
            services.harbor-db.pgBackup.targetSettings.replicatorPasswordFile = lib.mkDefault cfg.sourceSettings.replicatorPasswordFile;
          }
          # Target names come from config, so they may only appear below the
          # option paths, never in the module's own merge structure.
          (let
            built = lib.attrValues (lib.mapAttrs mkTarget targetSources);
          in {
            systemd = {
              tmpfiles.rules = lib.concatMap (t: t.systemd.tmpfiles.rules) built;
              services = mkMerge (map (t: t.systemd.services) built);
              timers = mkMerge (map (t: t.systemd.timers) built);
            };
          })
        ]))
    ]);
}
