{
  config,
  lib,
  pkgs,
  harborDbStoragePackage ? null,
  ...
}: let
  inherit (lib) mkEnableOption mkIf mkOption types;
  cfg = config.services.harbor-db.postgresql;
  pg = config.services.postgresql;
  manifest = pkgs.writeText "harbor-db-postgresql.json" (builtins.toJSON ({
      inherit (cfg) resource;
      data_dir = pg.dataDir;
      major = lib.versions.major pg.finalPackage.version;
      package = toString pg.finalPackage;
      state_dir = cfg.stateDir;
      required_mounts = cfg.requiredMounts;
      writer_fence = {
        control_role = "postgres";
        replication_roles = cfg.writerFence.replicationRoles;
        allowed_preload_libraries = cfg.writerFence.allowedPreloadLibraries;
      };
      startup_inhibition = {
        state_dir = cfg.writerFence.startupStateDir;
        unit = "postgresql.service";
        setup_units = cfg.writerFence.setupUnits;
        drop_in_root = "/etc/systemd/system.control";
        systemctl = "${pkgs.systemd}/bin/systemctl";
        busctl = "${pkgs.systemd}/bin/busctl";
        runuser = "${pkgs.util-linux}/bin/runuser";
        adapter = lib.getExe cfg.package;
      };
    }
    // lib.optionalAttrs (cfg.recovery != null) {
      recovery =
        {
          system_identifier = cfg.recovery.systemIdentifier;
          require_writer_fence = cfg.recovery.requireWriterFence;
          backup_root = cfg.recovery.backupRoot;
          snapshot_file = cfg.recovery.snapshotFile;
          receipt_file = cfg.recovery.receiptFile;
          off_host_receipt_file = cfg.recovery.offHostReceiptFile;
          source_hostname = cfg.recovery.sourceHostname;
          max_age_seconds = cfg.recovery.maxAgeSeconds;
          verify_timeout_seconds = cfg.recovery.verifyTimeoutSeconds;
          record_checks =
            lib.mapAttrsToList (name: check: {
              inherit name;
              inherit (check) database sql;
            })
            cfg.recovery.recordChecks;
        }
        // lib.optionalAttrs (cfg.recovery.repositoryProtocol != "legacy") {
          repository_protocol = cfg.recovery.repositoryProtocol;
        };
    }
    // lib.optionalAttrs (cfg.upgrade != null) {
      upgrade = {
        data_dir = cfg.upgrade.oldDataDir;
        major = lib.versions.major cfg.upgrade.oldPackage.version;
        package = toString cfg.upgrade.oldPackage;
        initdb_args = cfg.upgrade.initdbArgs;
        extra_config = cfg.upgrade.extraConfig;
        validate_command = cfg.upgrade.validateCommand;
        copy_command = ["${pkgs.coreutils}/bin/cp" "-aL" "--reflink=auto"];
      };
    }));
  command = "${lib.getExe cfg.package} --config ${manifest}";
  liveArgs = lib.optionalString (cfg.switchAdoption != null) (lib.escapeShellArgs [
    "--system-identifier"
    cfg.switchAdoption.systemIdentifier
    "--socket-dir"
    cfg.switchAdoption.socketDir
    "--port"
    (toString cfg.switchAdoption.port)
  ]);
  serviceUserCommand = "${pkgs.util-linux}/bin/runuser -u postgres -- ${command}";
  preparation = cfg.recoveryPreparation;
  preparationManifest = pkgs.writeText "harbor-db-recovery-preparation.json" (builtins.toJSON {
    readiness_command = preparation.readinessCommand;
    backup_command = preparation.backupCommand;
    restore_command = preparation.restoreCommand;
    export_command = preparation.exportCommand;
  });
in {
  imports = [./postgres-writer-clients.nix ./storage-package-argument.nix];
  options.services.harbor-db.postgresql = {
    enable = mkEnableOption "adopted PostgreSQL identity guards and staged upgrades";
    package = mkOption {
      type = types.package;
      default = import ./postgres-package.nix {
        inherit pkgs;
        nativePackage = harborDbStoragePackage;
      };
      description = "Harbor DB PostgreSQL lifecycle adapter.";
    };
    resource = mkOption {
      type = types.str;
      default = "postgresql";
      description = "Stable cluster authority name, shared by supported generations.";
    };
    stateDir = mkOption {
      type = types.str;
      default = "/var/lib/harbor-db/postgresql";
      description = "Persistent identity/journal storage outside the cluster and Nix generations. Must be backed up with the cluster.";
    };
    requiredMounts = mkOption {
      type = types.listOf types.str;
      default = [];
      description = "Exact mountpoints that must exist before adoption, checking or upgrade.";
    };
    recovery = mkOption {
      default = null;
      description = ''
        Executed backup and record-level recovery acceptance required before
        adoption and each activating rollout. Startup never runs a recovery drill.
        Certification compares a disposable read-only restore with a source snapshot
        captured in the consumer's consistency window. Off-host acceptance, when
        configured, must certify the same backup and records on another host.
      '';
      type = types.nullOr (types.submodule {
        options = {
          systemIdentifier = mkOption {type = types.strMatching "[1-9][0-9]*";};
          requireWriterFence = mkOption {
            type = types.bool;
            default = false;
            description = "Require confirmed writer exclusion and the same fence token in the source snapshot before live adoption and cutover. Disable only when retiring accepted bootstrap enrollment; active fencing remains enforced at startup.";
          };
          backupRoot = mkOption {type = types.strMatching "/.*";};
          repositoryProtocol = mkOption {
            type = types.enum ["legacy" "source-local-v1"];
            default = "legacy";
            description = "Recovery repository selection. source-local-v1 consumes an immutable fenced capture under recovery/ and preserves the backup service's legacy LAST_SUCCESS timestamp.";
          };
          snapshotFile = mkOption {type = types.strMatching "/.*";};
          receiptFile = mkOption {type = types.strMatching "/.*";};
          offHostReceiptFile = mkOption {
            type = types.nullOr (types.strMatching "/.*");
            default = null;
            description = "Independent off-host restore receipt; null explicitly selects local-only qualification.";
          };
          sourceHostname = mkOption {
            type = types.nonEmptyStr;
            default = config.networking.hostName;
          };
          maxAgeSeconds = mkOption {
            type = types.ints.positive;
            default = 172800;
          };
          verifyTimeoutSeconds = mkOption {
            type = types.ints.positive;
            default = 900;
          };
          recordChecks = mkOption {
            type = types.attrsOf (types.submodule {
              options = {
                database = mkOption {type = types.nonEmptyStr;};
                sql = mkOption {
                  type = types.nonEmptyStr;
                  description = "Deterministic record-level SQL; source and restored results must match exactly.";
                };
              };
            });
            description = "Consumer-owned application checks, not merely database/table presence.";
          };
        };
      });
    };
    writerFence = {
      setupUnits = mkOption {
        type = types.listOf (types.strMatching "[a-zA-Z0-9_-]+\\.service");
        default = ["postgresql-setup.service"];
        description = "Setup services inhibited with primary startup during stopped fence transitions, including across legacy generations. The persistent root-owned gate is read back for every unit before stopping the primary.";
      };
      startupStateDir = mkOption {
        type = types.strMatching "/.*";
        default = "/var/lib/harbor-db/postgresql-startup";
        description = "Separate root-owned persistent systemd startup-inhibition state. Explicit inhibit-startup provisions it; PostgreSQL never writes it.";
      };
      replicationRoles = mkOption {
        type = types.listOf (types.strMatching "[a-z_][a-z0-9_]*");
        default = [];
        description = "Physical-only localhost SCRAM replication roles admitted during an explicit writer fence. They cannot open SQL connections.";
      };
      allowedPreloadLibraries = mkOption {
        type = types.listOf (types.strMatching "[A-Za-z0-9_.-]+");
        default = [];
        description = "Audited non-application-writer preload libraries allowed by live fence inspection; unknown libraries block readiness.";
      };
    };
    recoveryPreparation = mkOption {
      default = null;
      description = ''
        Explicit preactivation bootstrap request. Candidate commands run as
        postgres before unit replacement, create the backup, record snapshot and
        local restore receipt, and abort until matching independent evidence exists.
        The consumer owns its writer consistency window, transport and restore
        orchestration. Retire the request after rollout; ordinary startup and
        recovery admission never execute these commands.
      '';
      type = types.nullOr (types.submodule {
        options = {
          readinessCommand = mkOption {
            type = types.listOf types.nonEmptyStr;
            description = "Absolute executable argv checking the live WAL receiver and measured flush lag before preparation.";
          };
          backupCommand = mkOption {
            type = types.listOf types.nonEmptyStr;
            description = "Absolute executable argv publishing a verified conservative base backup and post-backup replay point.";
          };
          restoreCommand = mkOption {
            type = types.listOf types.nonEmptyStr;
            description = "Absolute executable argv restoring and certifying the selected backup at a disposable read-only endpoint.";
          };
          exportCommand = mkOption {
            type = types.listOf types.nonEmptyStr;
            default = [];
            description = "Optional absolute executable argv publishing the bound backup/WAL/snapshot copy for consumer-owned independent transport. Runs after local acceptance and before off-host admission.";
          };
          supplementaryGroups = mkOption {
            type = types.listOf types.nonEmptyStr;
            default = [];
            description = "Explicit groups needed by consumer preparation/export commands.";
          };
          socketDir = mkOption {
            type = types.strMatching "/[^,]*";
            default = "/run/postgresql";
          };
          port = mkOption {
            type = types.port;
            default = 5432;
          };
          readWritePaths = mkOption {
            type = types.listOf (types.strMatching "/.*");
            description = "Consumer backup/evidence and disposable-restore paths; never the primary or authority directory.";
          };
          requiredMounts = mkOption {
            type = types.listOf (types.strMatching "/.*");
            default = [];
          };
          provisionDirectories = mkOption {
            type = types.listOf (types.strMatching "/.*");
            default = [];
            description = "Explicit postgres-owned directories to provision before the transient unit. Must be inside readWritePaths; declared mounts are checked before creation.";
          };
          offHostReceiptImportFile = mkOption {
            type = types.nullOr (types.strMatching "/.*");
            default = null;
            description = "Transported independent receipt. When present, systemd passes a private credential copy for validation and atomic evidence import.";
          };
        };
      });
    };
    switchAdoption = mkOption {
      default = null;
      description = ''
        Explicit first-rollout adoption through NixOS switch/test pre-switch checks.
        The live local primary, physical control file and independently recorded
        identifier must agree, with all durability settings enabled. Boot and
        dry/check actions inspect without adoption; normal service startup never
        adopts. Enable only after consumer backup/record acceptance, and retire
        the request from configuration after the rollout.
      '';
      type = types.nullOr (types.submodule {
        options = {
          systemIdentifier = mkOption {
            type = types.strMatching "[1-9][0-9]*";
            description = "Independently recorded authoritative PostgreSQL system identifier.";
          };
          socketDir = mkOption {
            type = types.strMatching "/[^,]*";
            default = "/run/postgresql";
            description = "Local Unix socket directory of the existing authoritative primary.";
          };
          port = mkOption {
            type = types.port;
            default = 5432;
            description = "Port suffix of the local PostgreSQL socket.";
          };
        };
      });
    };
    upgrade = mkOption {
      default = null;
      description = "Explicit offline copy upgrade contract. Never runs automatically at boot.";
      type = types.nullOr (types.submodule {
        options = {
          oldPackage = mkOption {
            type = types.package;
            description = "Old PostgreSQL package including its extensions.";
          };
          oldDataDir = mkOption {
            type = types.str;
            description = "Adopted old cluster directory.";
          };
          initdbArgs = mkOption {
            type = types.listOf types.str;
            default = [];
            description = "Explicit locale, encoding and checksum arguments matching the old cluster.";
          };
          extraConfig = mkOption {
            type = types.lines;
            default = "";
            description = "Configuration required while pg_upgrade starts the new server.";
          };
          validateCommand = mkOption {
            type = types.listOf types.str;
            description = "Validation argv. Receives the offline staging directory as its final argument; must stop any server it starts before returning.";
          };
        };
      });
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = pg.enable;
        message = "services.harbor-db.postgresql requires services.postgresql.enable.";
      }
      {
        assertion = cfg.stateDir != pg.dataDir && !(lib.hasPrefix "${pg.dataDir}/" cfg.stateDir);
        message = "Harbor DB identity state must be outside the PostgreSQL data directory.";
      }
      {
        assertion = cfg.upgrade == null || cfg.upgrade.validateCommand != [];
        message = "Staged PostgreSQL upgrades require consumer validation.";
      }
      {
        assertion = cfg.recovery == null || cfg.recovery.recordChecks != {};
        message = "PostgreSQL recovery admission requires consumer record-level checks.";
      }
      {
        assertion = cfg.recovery == null || cfg.switchAdoption == null || cfg.recovery.systemIdentifier == cfg.switchAdoption.systemIdentifier;
        message = "Recovery and switch adoption must name the same authoritative cluster.";
      }
      {
        assertion = preparation == null || (cfg.recovery != null && lib.all (argv: argv != [] && lib.hasPrefix "/" (builtins.head argv)) ([preparation.readinessCommand preparation.backupCommand preparation.restoreCommand] ++ lib.optional (preparation.exportCommand != []) preparation.exportCommand));
        message = "Managed recovery preparation requires a recovery policy and absolute executable argv.";
      }
      {
        assertion = preparation == null || cfg.recovery == null || cfg.recovery.repositoryProtocol != "source-local-v1" || cfg.recovery.requireWriterFence;
        message = "Managed source-local recovery preparation requires writer fencing; read-only certification and retired enrollment may remain unfenced.";
      }
      {
        assertion =
          preparation
          == null
          || lib.all (path:
            lib.all (protected:
              path != "/" && path != protected && !(lib.hasPrefix "${path}/" protected) && !(lib.hasPrefix "${protected}/" path)) [pg.dataDir cfg.stateDir])
          preparation.readWritePaths;
        message = "Managed recovery preparation must not grant writable access to the primary or authority tree.";
      }
      {
        assertion = preparation == null || lib.all (path: lib.any (writable: path == writable || lib.hasPrefix "${writable}/" path) preparation.readWritePaths) preparation.provisionDirectories;
        message = "Managed recovery provisioning must stay within the explicit writable preparation paths.";
      }
    ];
    environment.systemPackages = [cfg.package];
    environment.etc."harbor-db/postgresql.json".source = manifest;
    systemd.tmpfiles.rules = ["d ${cfg.stateDir} 0700 postgres postgres -"];
    system.preSwitchChecks = lib.mkMerge [
      (lib.mkIf (preparation != null) {
        "00-0-harbor-db-postgresql-prepare" = ''
          case "''${2-}" in
            switch|test)
              ${lib.optionalString (preparation.provisionDirectories != []) ''
            ${lib.concatMapStringsSep "\n" (mount: "${pkgs.util-linux}/bin/mountpoint -q ${lib.escapeShellArg mount} || exit 1") (cfg.requiredMounts ++ preparation.requiredMounts)}
            ${pkgs.coreutils}/bin/install -d -m 0700 -o postgres -g postgres ${lib.escapeShellArgs preparation.provisionDirectories} || exit $?
          ''}
              credential_args=()
              ${lib.optionalString (preparation.offHostReceiptImportFile != null) ''
            if [ -e ${lib.escapeShellArg preparation.offHostReceiptImportFile} ]; then
              credential_args+=(${lib.escapeShellArg "--property=LoadCredential=recovery-off-host:${preparation.offHostReceiptImportFile}"})
            fi
          ''}
              unit="harbor-db-recovery-prepare-$$"
              # Do not release the caller's activation lease with a surviving
              # transient backup/restore process after interruption.
              trap '${pkgs.systemd}/bin/systemctl stop "$unit.service"' EXIT
              trap 'exit 130' INT
              trap 'exit 143' TERM
              ${pkgs.systemd}/bin/systemd-run --quiet --wait --pipe --collect \
                --unit="$unit" --property=Type=oneshot \
                --property=User=postgres --property=Group=postgres \
                ${lib.escapeShellArg "--property=SupplementaryGroups=${lib.concatStringsSep " " preparation.supplementaryGroups}"} \
                --property=ProtectSystem=strict --property=PrivateTmp=true \
                --property=NoNewPrivileges=true \
                --property=UMask=0077 --property=TimeoutStartSec=infinity \
                ${lib.escapeShellArg "--property=ReadWritePaths=${lib.concatStringsSep " " preparation.readWritePaths}"} \
                ${lib.escapeShellArg "--property=RequiresMountsFor=${lib.concatStringsSep " " (cfg.requiredMounts ++ preparation.requiredMounts)}"} \
                "''${credential_args[@]}" \
                -- ${command} prepare-recovery --preparation-config ${preparationManifest} \
                ${lib.escapeShellArgs ["--socket-dir" preparation.socketDir "--port" (toString preparation.port)]} || exit $?
              trap - EXIT INT TERM
              ;;
          esac
        '';
      })
      (lib.mkIf (cfg.recovery != null) {
        # Lexical ordering rejects missing recovery evidence before any adoption.
        "00-harbor-db-postgresql-recovery" = ''
          ${serviceUserCommand} inspect-recovery
        '';
      })
      (lib.mkIf (cfg.switchAdoption != null) {
        harbor-db-postgresql-adoption = ''
          # Verify while the old primary is still running, before NixOS stops units.
          ${serviceUserCommand} inspect-live ${liveArgs}
          case "''${2-}" in
            switch|test)
              ${pkgs.coreutils}/bin/install -d -m 0700 -o postgres -g postgres ${lib.escapeShellArg cfg.stateDir}
              ${serviceUserCommand} adopt-live ${liveArgs}
              ;;
          esac
        '';
      })
    ];

    systemd.services.harbor-db-postgresql-recovery-check = mkIf (cfg.recovery != null) {
      description = "Read-only PostgreSQL backup and recovery admission";
      unitConfig.RequiresMountsFor = [cfg.recovery.backupRoot];
      serviceConfig = {
        Type = "oneshot";
        User = "postgres";
        Group = "postgres";
        ExecStart = "${command} inspect-recovery";
        TimeoutStartSec = cfg.recovery.verifyTimeoutSeconds + 60;
        ProtectSystem = "strict";
        ReadWritePaths = [];
        PrivateTmp = true;
        PrivateNetwork = true;
        NoNewPrivileges = true;
      };
    };

    # Runs on EVERY start, in the same unit as nixpkgs' initialization code.
    # A failed guard exits before initdb can turn missing storage into an empty DB.
    systemd.services.postgresql = {
      preStart = lib.mkBefore ''
        ${command} check
      '';
      unitConfig.RequiresMountsFor = [cfg.stateDir] ++ cfg.requiredMounts;
      serviceConfig = {
        ExecStart = lib.mkForce "${command} serve";
        # serve execs postgres: MAINPID owns the lease, readiness and signals.
        NotifyAccess = "main";
      };
    };
    services.postgresql.settings = {
      fsync = lib.mkForce true;
      full_page_writes = lib.mkForce true;
      synchronous_commit = lib.mkForce "on";
    };

    systemd.services.harbor-db-postgresql-upgrade = mkIf (cfg.upgrade != null) {
      description = "Explicit staged PostgreSQL major upgrade";
      conflicts = ["postgresql.service"];
      after = ["postgresql.service"];
      unitConfig.RequiresMountsFor = [cfg.stateDir pg.dataDir cfg.upgrade.oldDataDir] ++ cfg.requiredMounts;
      serviceConfig = {
        Type = "oneshot";
        User = "postgres";
        Group = "postgres";
        ExecStart = "${command} upgrade";
        TimeoutStartSec = "infinity";
        UMask = "0077";
        PrivateTmp = true;
        PrivateNetwork = true;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        # Staging/publication need the destination parent, including on first upgrade.
        ReadWritePaths = [cfg.stateDir (builtins.dirOf pg.dataDir) cfg.upgrade.oldDataDir];
      };
      environment.LOCALE_ARCHIVE = "${pkgs.glibcLocales}/lib/locale/locale-archive";
    };
  };
}
