{
  config,
  lib,
  pkgs,
  harborDbStoragePackage,
  ...
}: let
  inherit (lib) mkEnableOption mkIf mkOption types;
  storagePaths = import ./storage-paths.nix {inherit lib;};
  immutableFiles = import ./immutable-files.nix {inherit lib;};
  normalizedPath = storagePaths.normalizedAbsolute;
  cfg = config.services.harbor-db.postgresql;
  pg = config.services.postgresql;
  rawManifest = pkgs.writeText "harbor-db-postgresql.json" (builtins.toJSON ({
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
        immutable_tools = true;
        executable_files = cfg.upgrade.executableFiles;
        copy_command = ["${pkgs.coreutils}/bin/cp" "-aL" "--reflink=auto"];
      };
    }));
  manifest =
    if cfg.upgrade == null
    then rawManifest
    else immutableFiles.checkedManifest "Harbor DB PostgreSQL upgrade" (cfg.upgrade.executableFiles ++ lib.optional (cfg.upgrade.validateCommand != []) (lib.head cfg.upgrade.validateCommand)) rawManifest;
  # With a rollout, every guard and service that inspects recovery, fence or
  # startup state renders from the maintenance manifest the coordinator fences,
  # prepares and adopts under, so the candidate's activation agrees with it.
  command = "${lib.getExe cfg.package} --config ${
    if cfg.rollout != null
    then cfg.rollout.maintenance
    else manifest
  }";
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
  inherit (cfg) rollout;
  rolloutManifest = pkgs.writeText "harbor-db-postgresql-rollout.json" (builtins.toJSON {
    version = 2;
    host = config.networking.hostName;
    system_identifier = cfg.recovery.systemIdentifier;
    harbor = lib.getExe cfg.package;
    systemctl = "${pkgs.systemd}/bin/systemctl";
    systemd_run = "${pkgs.systemd}/bin/systemd-run";
    maintenance = toString rollout.maintenance;
    preparation = toString preparationManifest;
    execution_lock = rollout.executionLock;
    socket_dir = preparation.socketDir;
    inherit (preparation) port;
    inherit (rollout) clients;
    retired_clients = rollout.retiredClients;
    setup_units = cfg.writerFence.setupUnits;
    receiver_units = rollout.receiverUnits;
    paused_publishers = rollout.pausedPublishers;
    setup_settle_seconds = rollout.setupSettleSeconds;
    prepare = {
      export_root = rollout.exportRoot;
      read_write_paths = preparation.readWritePaths;
      required_mounts = cfg.requiredMounts ++ preparation.requiredMounts;
      supplementary_groups = preparation.supplementaryGroups;
      provision_directories =
        map (path: {
          inherit path;
          group = "postgres";
          mode = 448;
        })
        preparation.provisionDirectories
        ++ [
          {
            path = rollout.exportRoot;
            group = rollout.exportGroup;
            mode = 488;
          }
        ];
    };
    import_slot = preparation.offHostReceiptImportFile;
    # Written only when set, so a coordinator predating the hook can still load
    # (and abort under) a manifest without one.
    executor = {inherit (rollout.executor) host user command;} // lib.optionalAttrs (rollout.executor.transport != null) {inherit (rollout.executor) transport;};
    inherit (rollout) cutover;
  });
  # Refuses activating a generation that would strand an open rollout.
  activationGuard =
    if rollout != null
    then "${lib.getExe cfg.package} --config ${rolloutManifest} rollout activation-check"
    else "${lib.getExe cfg.package} --config ${manifest} rollout-guard";
in {
  imports = [./postgres-writer-clients.nix ./storage-package-argument.nix];
  options.services.harbor-db.postgresql = {
    enable = mkEnableOption "adopted PostgreSQL identity guards and staged upgrades";
    package = mkOption {
      type = types.package;
      default = harborDbStoragePackage;
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
        apply = lib.unique;
        description = "Physical-only localhost SCRAM replication roles admitted during an explicit writer fence. They cannot open SQL connections. Source-local recovery automatically includes its replicator receiver role.";
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
            description = "Root-controlled transported independent receipt. Root pins and copies only sources with trusted ownership and non-writable ancestry into private storage before systemd credential loading, validation and atomic evidence import.";
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
    rollout = mkOption {
      default = null;
      description = ''
        Opt-in first-rollout coordinator manifest for `harbor-db-postgres
        --config /etc/harbor-db/postgresql-rollout.json rollout ...`. It sequences
        the writer fence, preparation, independent certification hand-off,
        adoption and thaw (or abort) from one resumable journal.
      '';
      type = types.nullOr (types.submodule {
        options = {
          maintenance = mkOption {
            type = types.path;
            default = manifest;
            defaultText = "the module's PostgreSQL manifest";
            description = "Fenced maintenance manifest; its recovery must require the writer fence.";
          };
          # The journal lives at the fixed /var/lib/harbor-db/postgresql-rollout,
          # so no manifest can relocate (and lose) an open rollout.
          executionLock = mkOption {
            type = types.nullOr (types.strMatching "/[^/.][^/]*/[^/.][^/]*/[^/.][^/]*(/[^/.][^/]*)*");
            default = null;
            example = "/run/lock/canix/switch.lock";
            description = ''
              Existing host lock file every mutating rollout step holds (a
              non-blocking exclusive flock), shared with the consumer's
              activations and other database coordinators.
            '';
          };
          clients = mkOption {
            type = types.listOf types.nonEmptyStr;
            default = cfg.writerFence.blockedUnits;
            defaultText = "services.harbor-db.postgresql.writerFence.blockedUnits";
            description = "Writer services stopped by the fence and restarted by thaw/abort (template `name@.service` allowed).";
          };
          retiredClients = mkOption {
            type = types.listOf types.nonEmptyStr;
            default = [];
            description = "Legacy writers that are stopped but never restarted.";
          };
          receiverUnits = mkOption {
            type = types.listOf types.nonEmptyStr;
            default = [];
            description = "Installed WAL receivers stopped around preparation and restored afterwards.";
          };
          pausedPublishers = mkOption {
            type = types.listOf types.nonEmptyStr;
            default = [];
            description = "Backup/evidence publishers skipped (via a drop-in condition) for the whole rollout.";
          };
          setupSettleSeconds = mkOption {
            type = types.ints.positive;
            default = 120;
            description = "Bound for setup units to finish after a primary start before fence inspection.";
          };
          exportRoot = mkOption {type = types.strMatching "/[^/.][^/]*/[^/.][^/]*/[^/.][^/]*(/[^/.][^/]*)*";};
          exportGroup = mkOption {
            type = types.nonEmptyStr;
            default = "users";
          };
          executor = {
            host = mkOption {type = types.nonEmptyStr;};
            user = mkOption {type = types.nonEmptyStr;};
            command = mkOption {
              type = types.listOf types.nonEmptyStr;
              description = "Independent restore/certify argv run as the executor user on the executor host; `--export/--export-sha256/--workspace` are appended, and it must write its receipt to `<workspace>/off-host-recovery.json`.";
            };
            transport = mkOption {
              type = types.nullOr types.str;
              default = null;
              description = ''
                Optional executor hand-off hook: one immutable store executable (wrap
                arguments in a store script). From `prepared`, `rollout next` runs it as
                the transient root unit `harbor-db-rollout-transport.service`, in `/` with
                systemd's clean environment plus `HOME`, `LANG` and the caller's
                `SSH_AUTH_SOCK`, appending `--config/--export/--export-sha256/--receipt-dir`.
                It copies the manifest closure and export to the executor, runs
                `rollout certify` there, writes `<receipt-dir>/off-host-recovery.json`
                (mode 0600) and prints only certify's `receipt_sha256`; Harbor imports it.
                A manifest with a hook needs a hook-aware coordinator to load or abort it.
              '';
            };
          };
          cutover = mkOption {
            type = types.nullOr (types.submodule {
              options = {
                # Thaw runs the checker as root: immutable store inputs only.
                checker = mkOption {type = types.strMatching "/nix/store/[^/]+/.+";};
                contract = mkOption {type = types.strMatching "/nix/store/[^/]+/.+";};
              };
            });
            default = null;
            description = "Optional cutover checker run in `certify` phase before thaw.";
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
            description = "Validation argv with an immutable store executable. Receives the offline staging directory as its final argument; must stop any server it starts before returning.";
          };
          executableFiles = mkOption {
            type = types.listOf types.str;
            default = [];
            description = "Additional immutable upgrade adapter/interpreter files bound into the journal contract.";
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
        assertion = lib.length cfg.writerFence.setupUnits == lib.length (lib.unique cfg.writerFence.setupUnits) && !(lib.elem "postgresql.service" cfg.writerFence.setupUnits);
        message = "Writer-fence setup units must be unique and distinct from the primary postgresql.service unit.";
      }
      {
        assertion = cfg.stateDir != pg.dataDir && !(lib.hasPrefix "${pg.dataDir}/" cfg.stateDir);
        message = "Harbor DB identity state must be outside the PostgreSQL data directory.";
      }
      {
        assertion = cfg.stateDir != "/" && normalizedPath cfg.stateDir && builtins.match "/[A-Za-z0-9_./-]+" cfg.stateDir != null;
        message = "Harbor DB identity state requires a dedicated normalized absolute directory.";
      }
      {
        assertion = let
          state = cfg.writerFence.startupStateDir;
        in
          state
          != "/"
          && normalizedPath state
          && builtins.match "[^[:space:]]*" state != null
          && !(lib.hasInfix "\\" state || lib.hasInfix "%" state)
          && lib.all normalizedPath [pg.dataDir cfg.stateDir]
          && lib.all (protected: storagePaths.disjoint [state protected]) [pg.dataDir cfg.stateDir];
        message = "PostgreSQL startup inhibition requires a normalized dedicated path disjoint from identity state and PostgreSQL data storage.";
      }
      {
        assertion = cfg.upgrade == null || (cfg.upgrade.validateCommand != [] && lib.all immutableFiles.storePath (cfg.upgrade.executableFiles ++ [(lib.head cfg.upgrade.validateCommand)]));
        message = "Staged PostgreSQL upgrades require consumer validation with immutable store executables and adapter files.";
      }
      {
        assertion = cfg.recovery == null || cfg.recovery.recordChecks != {};
        message = "PostgreSQL recovery admission requires consumer record-level checks.";
      }
      {
        assertion =
          cfg.recovery
          == null
          || lib.length (lib.unique ([cfg.recovery.snapshotFile cfg.recovery.receiptFile] ++ lib.optional (cfg.recovery.offHostReceiptFile != null) cfg.recovery.offHostReceiptFile))
          == (
            if cfg.recovery.offHostReceiptFile == null
            then 2
            else 3
          );
        message = "PostgreSQL recovery snapshots, local receipts and off-host receipts must use distinct paths.";
      }
      {
        assertion =
          cfg.recovery
          == null
          || (let
            evidence = [cfg.recovery.backupRoot cfg.recovery.snapshotFile cfg.recovery.receiptFile] ++ lib.optional (cfg.recovery.offHostReceiptFile != null) cfg.recovery.offHostReceiptFile;
            protected = [pg.dataDir cfg.stateDir cfg.writerFence.startupStateDir];
          in
            lib.all normalizedPath protected && lib.all (path: lib.all (root: storagePaths.disjoint [path root]) protected) evidence);
        message = "PostgreSQL recovery evidence must remain outside PostgreSQL storage, lifecycle authority and startup-inhibition trees.";
      }
      {
        assertion = cfg.recovery == null || cfg.switchAdoption == null || cfg.recovery.systemIdentifier == cfg.switchAdoption.systemIdentifier;
        message = "Recovery and switch adoption must name the same authoritative cluster.";
      }
      {
        assertion =
          cfg.recovery
          == null
          || (let
            evidence = [cfg.recovery.snapshotFile cfg.recovery.receiptFile] ++ lib.optional (cfg.recovery.offHostReceiptFile != null) cfg.recovery.offHostReceiptFile;
            controls = map (name: "${cfg.recovery.backupRoot}/${name}") ["recovery" "base" "wal" "locks" "BACKUP_LOCK" "LAST_SUCCESS"] ++ map (name: "${builtins.dirOf cfg.recovery.snapshotFile}/${name}") ["recovery.lock" "preparation.lock" "preparation.json"];
          in
            cfg.recovery.backupRoot != "/" && lib.all normalizedPath (evidence ++ [cfg.recovery.backupRoot]) && lib.all (path: lib.all (control: storagePaths.disjoint [path control]) controls) evidence);
        message = "PostgreSQL recovery evidence must use normalized paths outside producer repository control namespaces and evidence lock files.";
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
              path != "/" && path != protected && !(lib.hasPrefix "${path}/" protected) && !(lib.hasPrefix "${protected}/" path)) [pg.dataDir cfg.stateDir cfg.writerFence.startupStateDir])
          preparation.readWritePaths;
        message = "Managed recovery preparation must not grant writable access to the primary, authority or startup inhibition tree.";
      }
      {
        assertion = preparation == null || lib.all normalizedPath (preparation.readWritePaths ++ preparation.provisionDirectories ++ [pg.dataDir cfg.stateDir cfg.writerFence.startupStateDir]);
        message = "Managed recovery preparation requires normalized absolute writable, provisioned and protected paths before containment checks.";
      }
      {
        assertion = rollout == null || (cfg.recovery != null && preparation != null && preparation.offHostReceiptImportFile != null && preparation.exportCommand != [] && rollout.clients != []);
        message = "The rollout coordinator needs recovery, recoveryPreparation with an off-host receipt import file and an export command (the executor hand-off), and an explicit client inventory.";
      }
      {
        assertion = rollout == null || rollout.executor.transport == null || immutableFiles.storePath rollout.executor.transport;
        message = "The rollout's root executor transport hook must be one immutable store executable.";
      }
      {
        # Mirrors the coordinator: each unit has one role (templates cover
        # their instances), e.g. a retired client must not be a paused
        # publisher, which the lifted pause would restart.
        assertion = let
          template = pattern: unit:
            lib.hasSuffix "@.service" pattern
            && unit != pattern
            && lib.hasSuffix ".service" unit
            && lib.hasPrefix (lib.removeSuffix ".service" pattern) unit;
          overlap = a: b: a == b || template a b || template b a;
          roles = [
            (rollout.clients ++ rollout.retiredClients)
            rollout.pausedPublishers
            rollout.receiverUnits
            cfg.writerFence.setupUnits
            ["postgresql.service"]
          ];
          disjoint = xs: ys: lib.all (x: lib.all (y: !(overlap x y)) ys) xs;
          pairs = lib.concatLists (lib.imap0 (i: xs: map (ys: disjoint xs ys) (lib.drop (i + 1) roles)) roles);
        in
          rollout == null || lib.all lib.id pairs;
        message = "Rollout units must each have one role: clients and retired clients, paused publishers, receivers, setup units and postgresql.service may not overlap (templates included).";
      }
      {
        # Mirrors the coordinator: only clients are enumerated from active
        # units, and the export root gets its own owner and mode contract.
        assertion = let
          named = rollout.pausedPublishers ++ rollout.receiverUnits ++ cfg.writerFence.setupUnits;
          valid = lib.all (unit: !(lib.hasSuffix "@.service" unit)) named && !(lib.elem rollout.exportRoot preparation.provisionDirectories);
        in
          rollout == null || preparation == null || valid;
        message = "Rollout paused publishers, receivers and setup units must name units, not templates, and the export root must not also be a recoveryPreparation provision directory.";
      }
      {
        # Mirrors the coordinator's manifest load, so a generation it cannot
        # run fails at evaluation instead of at activation.
        assertion = let
          units = rollout.clients ++ rollout.retiredClients ++ rollout.receiverUnits ++ rollout.pausedPublishers ++ cfg.writerFence.setupUnits;
          validUnit = unit: builtins.match "[A-Za-z0-9_@.:-]+[.]service" unit != null;
          valid =
            cfg.recovery.requireWriterFence
            && cfg.recovery.repositoryProtocol == "legacy"
            && lib.all validUnit units
            && rollout.executor.command != []
            # The executor command runs on another host: checked lexically.
            && lib.hasPrefix "/" (lib.head rollout.executor.command)
            # Certification is independent only on another host, never as root.
            && rollout.executor.host != config.networking.hostName
            && rollout.executor.user != "root";
        in
          rollout == null || cfg.recovery == null || valid;
        message = "Rollout coordinator: recovery must require the writer fence over the legacy repository protocol, units must be `.service` names, and the executor must be another host, a non-root user and an absolute program with arguments.";
      }
      {
        # Mirrors the coordinator's load: the roots post-fence steps write are
        # pairwise disjoint (the writer fence lives in the identity state
        # directory), except a provisioned directory inside a writable path or
        # another provisioned one (evidence in its backup root) and the export
        # root inside a writable path; the worker's evidence directories lie
        # inside its writable paths.
        assertion = let
          contains = root: path: root == path || lib.hasPrefix "${root}/" path;
          slot = preparation.offHostReceiptImportFile;
          root = kind: path: {inherit kind path;};
          roots =
            map (root "control") ([pg.dataDir cfg.stateDir cfg.writerFence.startupStateDir "/var/lib/harbor-db/postgresql-rollout"] ++ lib.optional (slot != null) (builtins.dirOf slot))
            ++ [(root "export" rollout.exportRoot)]
            ++ map (root "writable") preparation.readWritePaths
            ++ map (root "provisioned") preparation.provisionDirectories;
          intended = inner: outer:
            contains outer.path inner.path
            && (inner.kind == "provisioned" && lib.elem outer.kind ["writable" "provisioned"] || inner.kind == "export" && outer.kind == "writable");
          apart = a: b: !(contains a.path b.path || contains b.path a.path) || intended a b || intended b a;
          pairs = lib.concatLists (lib.imap0 (i: a: map (apart a) (lib.drop (i + 1) roots)) roots);
          # The worker writes the evidence directories and the backup root.
          evidence = map builtins.dirOf ([cfg.recovery.snapshotFile cfg.recovery.receiptFile] ++ lib.optional (cfg.recovery.offHostReceiptFile != null) cfg.recovery.offHostReceiptFile) ++ [cfg.recovery.backupRoot];
          writable = path: lib.any (root: contains root path) preparation.readWritePaths;
        in
          rollout == null || preparation == null || cfg.recovery == null || (lib.all lib.id pairs && lib.all writable evidence);
        message = "Rollout roots: the PostgreSQL data directory, identity (writer-fence) state, startup inhibition state, journal root (/var/lib/harbor-db/postgresql-rollout), import slot directory, export root, writable paths and provisioned directories must be pairwise disjoint (a provisioned directory may lie in a writable path or another provisioned one, the export root in a writable path), and the recovery evidence directories and backup root inside the writable paths.";
      }
      {
        # Mirrors the coordinator: systemd splits these property values on
        # whitespace (LoadCredential on `:`).
        assertion = let
          plain = extra: value: builtins.match ".*[[:space:][:cntrl:]\"'%\\\\${extra}].*" value == null;
          values = [rollout.exportRoot] ++ preparation.readWritePaths ++ cfg.requiredMounts ++ preparation.requiredMounts ++ preparation.provisionDirectories ++ preparation.supplementaryGroups;
          slot = preparation.offHostReceiptImportFile;
          valid = lib.all (plain "") values && (slot == null || plain ":" slot);
        in
          rollout == null || preparation == null || valid;
        message = "Rollout systemd property values (writable paths, mounts, export root, groups, import file) must not contain whitespace, quotes, `\\`, `%` or (in the import file) `:`.";
      }
      {
        assertion = preparation == null || lib.all (path: lib.any (writable: path == writable || lib.hasPrefix "${writable}/" path) preparation.readWritePaths) preparation.provisionDirectories;
        message = "Managed recovery provisioning must stay within the explicit writable preparation paths.";
      }
    ];
    environment.systemPackages = [cfg.package];
    environment.etc."harbor-db/postgresql.json".source = manifest;
    environment.etc."harbor-db/postgresql-rollout.json" = mkIf (rollout != null) {source = rolloutManifest;};
    systemd.tmpfiles.rules = ["d ${cfg.stateDir} 0700 postgres postgres -"];
    system.preSwitchChecks = lib.mkMerge [
      # Every generation refuses activation while a rollout it would strand is
      # open, sorted before any preparation. With a rollout, the coordinator
      # prepares, imports and adopts under the fence (re-running preparation and
      # the exporter at activation would bypass its locks and receiver
      # quiescence and could mutate the certified hand-off); without one, only
      # the fixed journal is read, so removing `rollout` cannot strand one.
      {
        "00-0-0-harbor-db-postgresql-rollout-guard" = ''
          case "''${2-}" in
            switch|test)
              ${activationGuard} >/dev/null || exit $?
              ;;
          esac
        '';
      }
      (lib.mkIf (preparation != null && rollout == null) {
        "00-0-harbor-db-postgresql-prepare" = ''
          case "''${2-}" in
            switch|test)
              ${lib.optionalString (preparation.provisionDirectories != []) ''
            ${lib.concatMapStringsSep "\n" (mount: "${pkgs.util-linux}/bin/mountpoint -q ${lib.escapeShellArg mount} || exit 1") (cfg.requiredMounts ++ preparation.requiredMounts)}
            ${pkgs.coreutils}/bin/install -d -m 0700 -o postgres -g postgres ${lib.escapeShellArgs preparation.provisionDirectories} || exit $?
          ''}
              credential_args=()
              credential_dir=""
              worker_started=false
              unit="harbor-db-recovery-prepare-$$"
              # Invoked only from the EXIT trap below, which ShellCheck cannot see.
              # shellcheck disable=SC2329
              cleanup_preparation() {
                if "$worker_started"; then
                  if ! ${pkgs.systemd}/bin/systemctl stop "$unit.service"; then
                    # --collect may already have unloaded a failed unit, or
                    # dispatch may have failed before the unit was created.
                    # Remove staged credentials only after confirming no live
                    # worker can still need them when stopping itself failed.
                    local state
                    state=$(${pkgs.systemd}/bin/systemctl show --property=ActiveState --value "$unit.service") || return $?
                    case "$state" in
                      inactive|failed) ;;
                      *) return 1 ;;
                    esac
                  fi
                fi
                if [ -n "$credential_dir" ]; then ${pkgs.coreutils}/bin/rm -rf -- "$credential_dir"; fi
              }
              trap 'cleanup_preparation || :' EXIT
              trap 'exit 130' INT
              trap 'exit 143' TERM
              ${lib.optionalString (preparation.offHostReceiptImportFile != null) ''
            if [ -e ${lib.escapeShellArg preparation.offHostReceiptImportFile} ]; then
              credential_dir=$(${pkgs.coreutils}/bin/mktemp -d /run/harbor-db-recovery-import.XXXXXXXX) || exit $?
              ${command} stage-off-host-receipt --source ${lib.escapeShellArg preparation.offHostReceiptImportFile} --destination "$credential_dir/receipt.json" || exit $?
              credential_args+=("--property=LoadCredential=recovery-off-host:$credential_dir/receipt.json")
            fi
          ''}
              # Do not release the caller's activation lease with a surviving
              # transient backup/restore process after interruption.
              worker_started=true
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
              if [ -n "$credential_dir" ]; then ${pkgs.coreutils}/bin/rm -rf -- "$credential_dir" || exit $?; fi
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
        ExecStart = "${lib.getExe cfg.package} --config ${manifest} upgrade";
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
