{pkgs}: let
  inherit (pkgs) lib;
  inherit (import ./eval-checks.nix {inherit pkgs;}) mkEvalCheck;
  eval = import (pkgs.path + "/nixos/lib/eval-config.nix") {
    system = "x86_64-linux";
    modules = [
      ./pg-backup.nix
      {
        system.stateVersion = "24.11";
        services.harbor-db.pgBackup = {
          enable = true;
          role = "target";
          source.hostName = "10.0.0.2";
          targetSettings.replicatorPasswordFile = "/run/secrets/pg-replicator-password";
        };
      }
    ];
  };
  sourceEval = import (pkgs.path + "/nixos/lib/eval-config.nix") {
    system = "x86_64-linux";
    modules = [
      ./pg-backup.nix
      {
        system.stateVersion = "24.11";
        services.postgresql.enable = true;
        services.harbor-db.pgBackup = {
          enable = true;
          role = "source";
          source.hostName = "10.0.0.1";
          sourceSettings = {
            listenAddresses = ["10.0.0.1"];
            allowedReplicationHosts = ["10.0.0.2/32"];
            replicatorPasswordFile = "/run/secrets/pg-replicator-password";
          };
        };
      }
    ];
  };
  receive = eval.config.systemd.services.pg-receivewal.serviceConfig;
  base = eval.config.systemd.services.pg-basebackup.script;
  sourcePostgresql = sourceEval.config.services.postgresql;
  localEval = import "${pkgs.path}/nixos/lib/eval-config.nix" {
    system = "x86_64-linux";
    modules = [
      ./pg-backup.nix
      {
        system.stateVersion = "24.11";
        services.postgresql.enable = true;
        services.harbor-db.pgBackup = {
          enable = true;
          role = "both";
          source.hostName = "127.0.0.1";
          sourceSettings = {
            replicatorPasswordFile = "/run/secrets/pg-replicator-password";
          };
          targetSettings.sourceLocalRecovery.enable = true;
        };
      }
    ];
  };
  localBackup = localEval.config.systemd.services.pg-basebackup.script;
  ipv6Local =
    (localEval.extendModules {
      modules = [{services.harbor-db.pgBackup.source.hostName = lib.mkForce "::1";}];
    }).config;
  localhost =
    (localEval.extendModules {
      modules = [{services.harbor-db.pgBackup.source.hostName = lib.mkForce "localhost";}];
    }).config;
  explicitLocal = host:
    (localEval.extendModules {
      modules = [
        {
          services.harbor-db.pgBackup.source.hostName = lib.mkForce host;
          services.harbor-db.pgBackup.sourceSettings.listenAddresses = ["10.0.0.1"];
        }
      ];
    }).config;
  fencedLocal =
    (localEval.extendModules {
      modules = [./postgres-lifecycle.nix {services.harbor-db.postgresql.enable = true;}];
    }).config;
  blockedFence =
    (localEval.extendModules {
      modules = [
        ./postgres-lifecycle.nix
        {
          services.harbor-db.postgresql = {
            enable = true;
            writerFence.replicationRoles = lib.mkForce [];
          };
        }
      ];
    }).config;
  explicitFence =
    (localEval.extendModules {
      modules = [
        ./postgres-lifecycle.nix
        {
          services.harbor-db.postgresql = {
            enable = true;
            writerFence.replicationRoles = ["peer" "replicator"];
          };
        }
      ];
    }).config;
  recoveryReady =
    (localEval.extendModules {
      modules = [
        {
          services.harbor-db.pgBackup.targetSettings = {
            health.enable = true;
            baseBackup = {
              recoveryMeta = true;
              admission = {
                minRamBytes = 1073741824;
                minSwapBytes = 536870912;
              };
            };
          };
        }
      ];
    }).config;
  named = modules:
    (import (pkgs.path + "/nixos/lib/eval-config.nix") {
      system = "x86_64-linux";
      modules =
        [
          ./pg-backup.nix
          {
            system.stateVersion = "24.11";
            services.harbor-db.pgBackup = {
              enable = true;
              role = "target";
              targetSettings = {
                backupDir = "/data/scratch/pgbackup";
                replicatorPasswordFile = "/run/secrets/pg-replicator-password";
              };
              targets.thething = {
                source.hostName = "10.0.0.2";
                retain = {
                  baseBackupDays = 7;
                  walDays = 8;
                };
                baseBackup.maxRate = "20M";
                receiveWal = {
                  statusInterval = 1;
                  slotCreateTimeoutSeconds = 10;
                };
              };
            };
          }
        ]
        ++ modules;
    }).config;
  thething = named [];
  walSizeAccepted = size: let
    selected = eval.extendModules {
      modules = [{services.harbor-db.pgBackup.targetSettings.walSegmentBytes = lib.mkForce size;}];
    };
  in
    (builtins.tryEval (builtins.deepSeq selected.config.systemd.services.pg-backup-prune.script true)).success;
  tablespaceEval =
    (localEval.extendModules {
      modules = [{services.postgresql.dataDir = lib.mkForce "/build/guard-source/cluster";}];
    }).config;
  tablespaceGuard = lib.head tablespaceEval.systemd.services.pg-basebackup.serviceConfig.ExecStartPre;
in
  mkEvalCheck {
    name = "harbor-db-pg-backup-eval";
    resultMessage = "harbor-db PostgreSQL backup creates slots before streaming and verifies base backups";
    nativeBuildInputs = [pkgs.postgresql_18 pkgs.coreutils pkgs.findutils];
    runtimeScript = ''
      set -euo pipefail
      unset PGHOST PGPORT PGUSER PGDATABASE PGSERVICE PGOPTIONS PGPASSFILE
      root=/build/guard-source
      mkdir -p "$root/socket" "$root/tablespace"
      chmod 0700 "$root" "$root/socket" "$root/tablespace"
      if ${tablespaceGuard}; then echo "missing primary was admitted" >&2; exit 1; fi
      initdb -D "$root/cluster" --locale=C --auth=trust >/dev/null
      ${tablespaceGuard}
      chmod 000 "$root/cluster/pg_tblspc"
      if ${tablespaceGuard}; then echo "unreadable tablespace inventory was admitted" >&2; exit 1; fi
      chmod 0700 "$root/cluster/pg_tblspc"
      mv "$root/cluster/pg_tblspc" "$root/retained-pg_tblspc"
      ln -s "$root/retained-pg_tblspc" "$root/cluster/pg_tblspc"
      if ${tablespaceGuard}; then echo "redirected tablespace inventory was admitted" >&2; exit 1; fi
      rm "$root/cluster/pg_tblspc"
      mv "$root/retained-pg_tblspc" "$root/cluster/pg_tblspc"
      pg_ctl -D "$root/cluster" -l "$root/server.log" -o "-k $root/socket -p 55450 -c listen_addresses=" -w start >/dev/null
      trap 'pg_ctl -D "$root/cluster" -m immediate -w stop >/dev/null' EXIT
      mkdir "$root/slot-receiver"
      for slot in "" "Upper" "with-hyphen" "with space" "slash/name"; do
        if pg_receivewal -h "$root/socket" -p 55450 -D "$root/slot-receiver" --create-slot "--slot=$slot" >"$out/invalid-slot.log" 2>&1; then
          echo "invalid replication slot name was admitted" >&2; exit 1
        fi
      done
      pg_receivewal -h "$root/socket" -p 55450 -D "$root/slot-receiver" --create-slot --slot=${lib.escapeShellArg (lib.concatStrings (lib.replicate 64 "a"))}
      test "$(psql -X -At -v ON_ERROR_STOP=1 -h "$root/socket" -p 55450 -d postgres -c 'SELECT slot_name FROM pg_replication_slots')" = ${lib.escapeShellArg (lib.concatStrings (lib.replicate 63 "a"))}
      pg_receivewal -h "$root/socket" -p 55450 -D "$root/slot-receiver" --drop-slot --slot=${lib.escapeShellArg (lib.concatStrings (lib.replicate 63 "a"))}
      for slot in slot_0 0 ${lib.escapeShellArg (lib.concatStrings (lib.replicate 63 "a"))}; do
        pg_receivewal -h "$root/socket" -p 55450 -D "$root/slot-receiver" --create-slot "--slot=$slot"
        pg_receivewal -h "$root/socket" -p 55450 -D "$root/slot-receiver" --drop-slot "--slot=$slot"
      done
      test "$(psql -X -At -v ON_ERROR_STOP=1 -h "$root/socket" -p 55450 -d postgres -c 'SELECT count(*) FROM pg_replication_slots')" = 0
      printf '%s\n' 'invalid-slot-names-rejected; oversized-client-name-truncation-reproduced; 63-byte-name-accepted; slots-retired' > "$out/slot-admission"
      psql -X -v ON_ERROR_STOP=1 -h "$root/socket" -p 55450 -d postgres -c "CREATE TABLESPACE test_external LOCATION '$root/tablespace'" >/dev/null
      psql -X -v ON_ERROR_STOP=1 -h "$root/socket" -p 55450 -d postgres -c "CREATE TABLE retained(id int) TABLESPACE test_external;INSERT INTO retained VALUES(86)" >/dev/null
      if pg_basebackup -h "$root/socket" -p 55450 -D "$root/unmapped" --checkpoint=fast --wal-method=stream >"$out/unmapped-basebackup.log" 2>&1; then
        echo "unmapped same-host backup unexpectedly succeeded" >&2; exit 1
      fi
      grep -q 'exists but is not empty' "$out/unmapped-basebackup.log"
      if ${tablespaceGuard}; then echo "external tablespace was admitted" >&2; exit 1; fi
      test "$(psql -X -At -v ON_ERROR_STOP=1 -h "$root/socket" -p 55450 -d postgres -c 'SELECT id FROM retained')" = 86
      test "$(find "$root/cluster/pg_tblspc" -mindepth 1 -maxdepth 1 -type l -print | wc -l)" = 1
      printf '%s\n' 'real-unmapped-basebackup-rejected; generated-guard-rejected; primary-records-preserved' > "$out/tablespace-admission"
    '';
    assertions = [
      {
        name = "source-roles-require-effective-replication-capacity";
        message = "Every local source needs nonzero effective WAL sender and slot capacity; source-local minima and target-only independence are retained.";
        assertion = let
          accepted = role: local: slots: senders: let
            selected =
              (localEval.extendModules {
                modules = [
                  {
                    services.harbor-db.pgBackup = {
                      role = lib.mkForce role;
                      targetSettings.sourceLocalRecovery.enable = lib.mkForce local;
                    };
                    services.postgresql.settings = {
                      max_replication_slots = lib.mkForce slots;
                      max_wal_senders = lib.mkForce senders;
                    };
                  }
                ];
              }).config;
          in
            lib.all (a: a.assertion || !lib.hasInfix "source roles require nonzero effective replication capacity" a.message) selected.assertions;
        in
          lib.all (role:
            lib.all (local:
              accepted role local 2 3
              && !accepted role local 0 3
              && !accepted role local 2 0
              && !accepted role local 0 0
              && !accepted role local (-1) 3
              && !accepted role local 2 (-1))
            (
              if role == "both"
              then [false true]
              else [false]
            )
            && accepted role false 1 1) ["source" "both"]
          && accepted "target" false 0 0;
      }
      {
        name = "source-roles-require-effective-replication-wal-level";
        message = "Every local replication source requires effective replica or logical WAL; target-only hosts retain independent remote source settings.";
        assertion = let
          accepted = role: enabled: declared: effective: let
            selected =
              (localEval.extendModules {
                modules = [
                  {
                    services.harbor-db.pgBackup = {
                      role = lib.mkForce role;
                      sourceSettings.walLevel = lib.mkForce declared;
                      targetSettings.sourceLocalRecovery.enable = lib.mkForce enabled;
                    };
                    services.postgresql.settings = effective;
                  }
                ];
              }).config;
          in
            lib.all (a: a.assertion || !lib.hasInfix "source roles require effective wal_level" a.message) selected.assertions;
        in
          lib.all (role:
            lib.all (local:
              accepted role local "replica" {}
              && accepted role local "logical" {}
              && !accepted role local "minimal" {}
              && !accepted role local "replica" {wal_level = lib.mkForce "minimal";}
              && !accepted role local "logical" {wal_level = lib.mkForce "minimal";}
              && accepted role local "minimal" {wal_level = lib.mkForce "replica";}
              && accepted role local "minimal" {wal_level = lib.mkForce "logical";})
            (
              if role == "both"
              then [false true]
              else [false]
            )) ["source" "both"]
          && accepted "target" false "minimal" {wal_level = lib.mkForce "minimal";};
      }
      {
        name = "source-role-port-matches-effective-primary";
        message = "Every locally configured replication source matches the primary listener and firewall port; remote targets retain distinct ports.";
        assertion = let
          accepted = role: local: primary: source: let
            selected =
              (localEval.extendModules {
                modules = [
                  {
                    services.postgresql.settings.port = lib.mkForce primary;
                    services.harbor-db.pgBackup = {
                      role = lib.mkForce role;
                      source.port = lib.mkForce source;
                      targetSettings.sourceLocalRecovery.enable = lib.mkForce local;
                    };
                  }
                ];
              }).config;
          in
            lib.all (a: a.assertion || !lib.hasInfix "source port" a.message) selected.assertions;
        in
          lib.all (role:
            lib.all (local:
              accepted role local 5432 5432
              && accepted role local 55486 55486
              && !accepted role local 55486 5432
              && !accepted role local 5432 55486)
            (
              if role == "both"
              then [false true]
              else [false]
            )) ["source" "both"]
          && accepted "target" false 55486 5432;
      }
      {
        name = "replication-slot-names-reject-before-producer-services";
        message = "Enabled receiver/base-backup slots require 1-63 lowercase ASCII letters, digits or underscores; disabled operations retain inert settings.";
        assertion = let
          accepted = configuration: operation: name: enabled: let
            selected =
              (configuration.extendModules {
                modules = [
                  {
                    services.harbor-db.pgBackup.targetSettings.${operation} = {
                      slotName = lib.mkForce name;
                      enable = lib.mkForce enabled;
                    };
                  }
                ];
              }).config;
          in
            lib.all (a: a.assertion || !lib.hasInfix "valid PostgreSQL replication slot names" a.message) selected.assertions;
        in
          lib.all (configuration:
            lib.all (operation:
              lib.all (name: accepted configuration operation name true) ["slot_0" "0" (lib.concatStrings (lib.replicate 63 "a"))]
              && lib.all (name: !accepted configuration operation name true) ["" "Upper" "with-hyphen" "with space" "with\nnewline" "quote'" "slash/name" "é" (lib.concatStrings (lib.replicate 64 "a"))]
              && accepted configuration operation "unused slot" false) ["receiveWal" "baseBackup"]) [eval localEval];
      }
      {
        name = "physical-backup-roots-exclude-global-custody-namespaces";
        message = "Target-only and local producers cannot mutate an enabled filesystem corpus, authority, or enrolled PostgreSQL primary/lifecycle/startup namespace.";
        assertion = let
          accepted = configuration: enabled: kind: path: override: let
            selected =
              (configuration.extendModules {
                modules = [
                  ./module.nix
                  ./cutover.nix
                  ./postgres-lifecycle.nix
                  {
                    services.postgresql = {
                      enable = lib.mkForce (kind == "postgres" || configuration.config.services.postgresql.enable);
                      dataDir = "/srv/pg-primary";
                    };
                    services.harbor-db.cutover = {
                      enable = enabled;
                      resources.history = {
                        user = "archive";
                        custody_file = "/srv/archive-state/custody.json";
                        authority = {
                          state_dir = "/srv/archive-state";
                          directories = ["/srv/archive"];
                          binding.backend = "files";
                        };
                      };
                    };
                    services.harbor-db.postgresql = {
                      enable = kind == "postgres";
                      stateDir = "/srv/pg-state";
                      writerFence.startupStateDir = "/srv/pg-startup";
                    };
                    services.harbor-db.pgBackup.targetSettings.backupDir = lib.mkForce path;
                  }
                  override
                ];
              }).config;
          in
            lib.all (a: a.assertion || !(lib.hasInfix "custody namespaces" a.message || lib.hasPrefix "services.harbor-db.pgBackup" a.message)) selected.assertions;
        in
          lib.all (configuration:
            accepted configuration true "files" "/srv/archive-sibling" {}
            && accepted configuration false "files" "/srv/archive/backups" {}
            && accepted configuration true "files" "/srv" {}
            && lib.all (path: !accepted configuration true "files" path {}) ["/srv/archive" "/srv/archive/backups" "/srv/archive-state" "/srv/archive-state/backups"]
            && lib.all (path: !accepted configuration true "postgres" path {}) ["/srv/pg-state" "/srv/pg-state/backups" "/srv/pg-startup" "/srv/pg-startup/backups"]
            && lib.all (suffix:
              !accepted configuration true "files" "/srv/backups" {
                services.harbor-db.cutover.resources.history.authority.directories = lib.mkForce ["/srv/backups/${configuration.config.services.harbor-db.pgBackup.source.hostName}${suffix}"];
              }) ["" "/nested"])
          [eval localEval];
      }
      {
        name = "local-backup-waits-for-primary-and-wal-slot";
        message = "Local receiver requires the ready primary/setup, and the base backup requires/orders after primary plus receiver; remote targets retain independence.";
        assertion = let
          local = localEval.config.systemd.services;
          remote = eval.config.systemd.services;
        in
          lib.all (unit: lib.elem unit local.pg-receivewal.requires && lib.elem unit local.pg-receivewal.after) ["postgresql.service" "postgresql-setup.service"]
          && lib.all (unit: lib.elem unit local.pg-basebackup.requires && lib.elem unit local.pg-basebackup.after) ["postgresql.service" "pg-receivewal.service"]
          && lib.all (unit: !(lib.elem unit remote.pg-basebackup.requires) && !(lib.elem unit remote.pg-basebackup.after)) ["postgresql.service" "pg-receivewal.service"]
          && !(lib.elem "postgresql.service" remote.pg-receivewal.requires)
          && !(lib.elem "pg-receivewal.service" local.postgresql.after);
      }
      {
        name = "source-local-tablespaces-reject-before-replication";
        message = "Source setup, receiver and basebackup invoke the same tablespace admission before any replication connection; independent targets have no local-primary guard.";
        assertion =
          lib.head tablespaceEval.systemd.services.pg-receivewal.serviceConfig.ExecStartPre
          == tablespaceGuard
          && lib.hasInfix (builtins.unsafeDiscardStringContext tablespaceGuard) tablespaceEval.systemd.services.postgresql-setup.script
          && eval.config.systemd.services.pg-basebackup.serviceConfig.ExecStartPre == [];
      }
      {
        name = "source-local-recovery-requires-replication-capacity";
        message = "Source-local recovery needs two slots and three WAL senders, including effective PostgreSQL overrides; standalone targets stay valid.";
        assertion = let
          accepted = configuration: slots: senders: effective:
            lib.all (a: a.assertion || !lib.hasPrefix "services.harbor-db.pgBackup" a.message)
            (configuration.extendModules {
              modules = [
                {
                  services.harbor-db.pgBackup.sourceSettings = {
                    maxReplicationSlots = slots;
                    maxWalSenders = senders;
                  };
                  services.postgresql.settings = effective;
                }
              ];
            }).config.assertions;
        in
          accepted localEval 2 3 {}
          && accepted localEval 3 4 {}
          && !accepted localEval 1 3 {}
          && !accepted localEval 2 1 {}
          && !accepted localEval 2 2 {}
          && !accepted localEval 2 3 {max_wal_senders = lib.mkForce 2;}
          && !accepted localEval 2 3 {max_replication_slots = lib.mkForce 1;}
          && accepted eval 1 1 {};
      }
      {
        name = "local-backup-roots-are-independent-of-the-primary";
        message = "Every derived target backup root on a primary host must be normalized and disjoint from the live data directory before generating receive/base-backup services.";
        assertion = let
          accepted = primary: backup: let
            selected =
              (localEval.extendModules {
                modules = [
                  {
                    services.postgresql.dataDir = lib.mkForce primary;
                    services.harbor-db.pgBackup.targetSettings.backupDir = lib.mkForce backup;
                  }
                ];
              }).config;
          in
            lib.all (a: a.assertion || !lib.hasPrefix "services.harbor-db.pgBackup" a.message) selected.assertions;
          targetOnly =
            (eval.extendModules {
              modules = [
                {
                  services.postgresql.dataDir = "/srv/pg/10.0.0.2";
                  services.harbor-db.pgBackup.targetSettings.backupDir = "/srv/pg";
                }
              ];
            }).config;
        in
          lib.all (pair: !accepted (lib.head pair) (lib.last pair)) [
            ["/srv/pg/main" "/srv/pg/main"]
            ["/srv/pg/main" "/srv/pg/main/backups"]
            ["/srv/pg/127.0.0.1" "/srv/pg"]
            ["/srv/pg/127.0.0.1/primary" "/srv/pg"]
            ["/srv/pg/main" "/srv/pg/./main/backups"]
            ["/srv/pg/main" "/srv/pg//main"]
            ["/srv/pg/./main" "/srv/backups"]
          ]
          && accepted "/srv/pg/main" "/srv/pg/main-sibling"
          && accepted "/srv/pg/main" "/srv/pg"
          && lib.all (a: a.assertion || !lib.hasPrefix "services.harbor-db.pgBackup" a.message) targetOnly.assertions;
      }
      {
        name = "wal-and-base-backup-require-distinct-slots";
        message = "Continuous WAL reception and base backups cannot acquire the same replication slot; one-operation targets remain supported.";
        assertion = let
          accepted = selected: lib.all (a: a.assertion || !lib.hasInfix "distinct replication slots" a.message) selected.assertions;
          shared = configuration: overrides:
            (configuration.extendModules {
              modules = [
                {
                  services.harbor-db.pgBackup.targetSettings =
                    {
                      receiveWal.slotName = "shared_slot";
                      baseBackup.slotName = "shared_slot";
                    }
                    // overrides;
                }
              ];
            }).config;
        in
          accepted localEval.config
          && accepted eval.config
          && !accepted (shared localEval {})
          && !accepted (shared eval {})
          && accepted (shared eval {baseBackup.enable = false;})
          && accepted (shared eval {receiveWal.enable = false;});
      }
      {
        name = "source-local-recovery-requires-base-backup-production";
        message = "Source-local recovery requires both fresh verified base backups and continuous WAL; independent WAL-only targets remain supported.";
        assertion = let
          selected = (localEval.extendModules {modules = [{services.harbor-db.pgBackup.targetSettings.baseBackup.enable = false;}];}).config;
          walOnlyTarget = (eval.extendModules {modules = [{services.harbor-db.pgBackup.targetSettings.baseBackup.enable = false;}];}).config;
        in
          lib.any (a: !a.assertion && lib.hasInfix "sourceLocalRecovery requires baseBackup" a.message) selected.assertions
          && lib.all (a: a.assertion || !lib.hasPrefix "services.harbor-db.pgBackup" a.message) localEval.config.assertions
          && lib.all (a: a.assertion || !lib.hasPrefix "services.harbor-db.pgBackup" a.message) walOnlyTarget.assertions
          && localEval.config.systemd.services ? pg-basebackup
          && !(walOnlyTarget.systemd.services ? pg-basebackup);
      }
      {
        name = "invalid-wal-segment-size-rejects-before-backup-services";
        assertion = lib.all (size: !walSizeAccepted size) [0 3 524288 1048577 3145728 1073741825 2147483648];
        message = "WAL segment sizes must reject before generating a prune service that blocks base backups.";
      }
      {
        name = "all-runtime-supported-wal-segment-sizes-evaluate";
        assertion = lib.all walSizeAccepted [1048576 2097152 4194304 8388608 16777216 33554432 67108864 134217728 268435456 536870912 1073741824];
        message = "Every supported power-of-two WAL segment size must remain valid.";
      }
      {
        name = "source-local-recovery-requires-wal-receiver";
        assertion = let
          blocked = (localEval.extendModules {modules = [{services.harbor-db.pgBackup.targetSettings.receiveWal.enable = false;}];}).config;
        in
          lib.any (a: !a.assertion && lib.hasInfix "sourceLocalRecovery requires receiveWal" a.message) blocked.assertions;
        message = "Base backups alone cannot establish source-local post-target WAL coverage.";
      }
      {
        name = "source-local-fence-admits-generated-receiver";
        assertion = lib.elem "replicator" fencedLocal.services.harbor-db.postgresql.writerFence.replicationRoles;
        message = "The generated source-local WAL receiver must reconnect under the default writer fence.";
      }
      {
        name = "source-local-fence-preserves-peer-roles-without-duplicates";
        assertion = explicitFence.services.harbor-db.postgresql.writerFence.replicationRoles == ["peer" "replicator"];
        message = "Automatic receiver enrollment must preserve explicitly configured replication peers and avoid duplicate fence roles.";
      }
      {
        name = "source-local-forced-empty-fence-rejected";
        assertion = lib.any (a: !a.assertion && lib.hasInfix "replicator" a.message) blockedFence.assertions;
        message = "An explicit forced fence override must not silently disconnect source-local recovery.";
      }
      {
        name = "flush-wal-on-receipt";
        assertion = lib.hasInfix "--synchronous" receive.ExecStart;
        message = "WAL must be flushed on receipt, including partial segments.";
      }
      {
        name = "slot-create-is-pre-start";
        assertion = lib.hasInfix "--create-slot" (lib.concatStringsSep "\n" receive.ExecStartPre) && !(lib.hasInfix "--create-slot" receive.ExecStart);
        message = "pg_receivewal slot creation must be a one-shot ExecStartPre";
      }
      {
        name = "restart-policy";
        assertion = receive.Restart == "on-failure" && receive.RestartSteps == 6 && receive.RestartMaxDelaySec == "5min";
        message = "continuous WAL streaming must restart after failure";
      }
      {
        name = "source-replication-hba";
        assertion = lib.hasInfix "host replication replicator 10.0.0.2/32 scram-sha-256" sourcePostgresql.authentication;
        message = "source role must render the allowed replication host in pg_hba.conf";
      }
      {
        name = "source-replication-firewall";
        assertion = lib.elem 5432 sourceEval.config.networking.firewall.interfaces.wg-home.allowedTCPPorts;
        message = "source role must open PostgreSQL on the configured replication interface";
      }
      {
        name = "verify-backup";
        assertion = lib.hasInfix "pg_verifybackup" base;
        message = "base backups must pass pg_verifybackup before publication";
      }
      {
        name = "partial-publish";
        assertion = lib.hasInfix ".partial" base && lib.hasInfix "publish-tree \"$partial_dir\" \"$date_dir\"" base;
        message = "base backups must publish through a partial directory and atomic rename";
      }
      {
        name = "source-local-both-roles";
        assertion = !(builtins.any (a: lib.hasPrefix "services.harbor-db.pgBackup" a.message) (builtins.filter (a: !a.assertion) localEval.config.assertions)) && lib.hasInfix "host replication replicator 127.0.0.1/32 scram-sha-256" localEval.config.services.postgresql.authentication && lib.hasInfix "-h 127.0.0.1" localEval.config.systemd.services.pg-receivewal.serviceConfig.ExecStart && !(localEval.config.networking.firewall.interfaces ? wg-home);
        message = "Source-local reception must configure both source credentials and generated receiver services without opening a replication firewall.";
      }
      {
        name = "source-local-ipv6-replication-hba";
        assertion = lib.hasInfix "host replication replicator ::1/128 scram-sha-256" ipv6Local.services.postgresql.authentication;
        message = "IPv6 loopback reception must have a generated authenticated replication rule.";
      }
      {
        name = "source-local-localhost-replication-hba";
        assertion = lib.all (host: lib.hasInfix "host replication replicator ${host} scram-sha-256" localhost.services.postgresql.authentication) ["127.0.0.1/32" "::1/128"];
        message = "localhost reception must admit both loopback address families.";
      }
      {
        name = "source-local-default-loopback-listeners";
        assertion = localEval.config.services.postgresql.settings.listen_addresses == "127.0.0.1" && ipv6Local.services.postgresql.settings.listen_addresses == "::1" && localhost.services.postgresql.settings.listen_addresses == "127.0.0.1,::1";
        message = "Default source-local receivers must have reachable TCP listeners for their selected loopback address families.";
      }
      {
        name = "explicit-source-listener-preserved";
        assertion = sourcePostgresql.settings.listen_addresses == "10.0.0.1";
        message = "Explicit replication listeners must remain unchanged.";
      }
      {
        name = "explicit-source-local-listeners-include-selected-loopback";
        assertion =
          (explicitLocal "127.0.0.1").services.postgresql.settings.listen_addresses
          == "10.0.0.1,127.0.0.1"
          && (explicitLocal "::1").services.postgresql.settings.listen_addresses == "10.0.0.1,::1"
          && (explicitLocal "localhost").services.postgresql.settings.listen_addresses == "10.0.0.1,127.0.0.1,::1";
        message = "An explicit replication listener must also bind the selected source-local loopback families.";
      }
      {
        name = "source-local-persistent-mutation-before-publication";
        assertion = lib.hasInfix ''exec 8<>"$backup_root/locks/mutate"'' localBackup && lib.hasInfix ''flock -n 8'' localBackup && !(lib.hasInfix ''locks/mutate'' base);
        message = "Opt-in backup publication must own the existing recovery mutation inode before its legacy anchor; legacy publication remains unchanged.";
      }
      {
        name = "source-local-private-recovery-namespace";
        assertion = lib.elem "f /var/backups/pgbackup/127.0.0.1/recovery/PROTOCOL 0600 postgres postgres - source-local-v1" localEval.config.systemd.tmpfiles.rules && lib.elem "f /var/backups/pgbackup/127.0.0.1/locks/mutate 0600 postgres postgres -" localEval.config.systemd.tmpfiles.rules;
        message = "The capture namespace and mutation inode must be provisioned outside completed backup directories with private ownership.";
      }
      {
        name = "recovery-ready-backup-opt-in";
        assertion = let
          script = recoveryReady.systemd.services.pg-basebackup.script;
          units = recoveryReady.systemd.services;
        in
          lib.hasInfix "harbor-db-pg-backup finalize" script
          && !(lib.hasInfix "harbor-db-pg-backup" localBackup)
          && !(lib.hasInfix "harbor-db-pg-backup" base)
          && lib.any (c: lib.hasInfix "admit --min-ram-bytes 1073741824 --min-swap-bytes 536870912" c) (lib.toList units.pg-basebackup.serviceConfig.ExecCondition)
          && lib.any (c: lib.hasInfix "mark-gap" c) (lib.toList units.pg-receivewal.serviceConfig.ExecStartPre)
          && units ? pg-backup-health
          && recoveryReady.systemd.timers ? pg-backup-health
          && !(eval.config.systemd.services ? pg-backup-health)
          && eval.config.systemd.services.pg-basebackup.serviceConfig.ExecCondition == [];
        message = "recoveryMeta, gap tracking, health and admission are opt-in and leave default units unchanged.";
      }
      {
        name = "recovery-meta-requires-source-sql";
        assertion = let
          accepted = let
            selected =
              (eval.extendModules {
                modules = [{services.harbor-db.pgBackup.targetSettings.baseBackup.recoveryMeta = true;}];
              }).config;
          in
            lib.all (a: a.assertion || !lib.hasInfix "recoveryMeta requires" a.message) selected.assertions;
        in
          !accepted
          && (let
            ok =
              (eval.extendModules {
                modules = [
                  {
                    services.harbor-db.pgBackup.targetSettings.baseBackup = {
                      recoveryMeta = true;
                      sqlConninfo = "host=10.0.0.2 user=marker";
                    };
                  }
                ];
              }).config;
          in
            lib.all (a: a.assertion || !lib.hasInfix "recoveryMeta requires" a.message) ok.assertions);
        message = "A target-only host must name a source SQL connection before enabling recoveryMeta.";
      }
      {
        name = "named-targets-use-suffixed-units-and-private-roots";
        assertion = let
          services = thething.systemd.services;
          receive = services.pg-receivewal-thething.serviceConfig;
          both = named [{services.harbor-db.pgBackup.source.hostName = "10.0.0.9";}];
        in
          lib.hasInfix "/data/scratch/pgbackup/thething" services.pg-basebackup-thething.script
          && services ? pg-backup-prune-thething
          && thething.systemd.timers ? pg-basebackup-thething
          && !(services ? pg-receivewal)
          && !(services ? pg-basebackup)
          && lib.any (c: lib.hasPrefix "-" c && lib.hasInfix "/timeout 10s " c && lib.hasInfix "--create-slot --if-not-exists" c) (lib.toList receive.ExecStartPre)
          && lib.hasInfix "--status-interval=1" receive.ExecStart
          && lib.hasInfix "--status-interval=1 " (lib.head (lib.toList receive.ExecStartPre))
          && lib.elem "d /data/scratch/pgbackup/thething 0750 postgres postgres -" thething.systemd.tmpfiles.rules
          && both.systemd.services ? pg-receivewal
          && both.systemd.services ? pg-receivewal-thething
          && lib.hasInfix "retain_days=7" services.pg-backup-prune-thething.script
          && lib.hasInfix "wal_retain=8" services.pg-backup-prune-thething.script;
        message = "Named targets must get -<name> units, a backupDir/<name> root, bounded best-effort slot creation and a configurable status interval, alongside the legacy target.";
      }
      {
        name = "named-targets-carry-recovery-options";
        assertion = let
          selected = named [
            {
              services.harbor-db.pgBackup.targets.thething = {
                health.enable = true;
                baseBackup = {
                  recoveryMeta = true;
                  sqlConninfo = "host=10.0.0.2 user=marker";
                  admission.minRamBytes = 1;
                };
              };
            }
          ];
          unsupported = named [{services.harbor-db.pgBackup.targets.thething.baseBackup.recoveryMeta = true;}];
          failing = c: lib.any (a: !a.assertion && lib.hasInfix "services.harbor-db.pgBackup" a.message) c.assertions;
        in
          lib.hasInfix "harbor-db-pg-backup finalize" selected.systemd.services.pg-basebackup-thething.script
          && lib.hasInfix "/data/scratch/pgbackup/thething" (builtins.elemAt (lib.toList selected.systemd.services.pg-backup-health-thething.serviceConfig.ExecStart) 0)
          && lib.any (c: lib.hasInfix "mark-gap" c) (lib.toList selected.systemd.services.pg-receivewal-thething.serviceConfig.ExecStartPre)
          && !failing selected
          && failing unsupported
          && !(failing thething)
          && failing (named [{services.harbor-db.pgBackup.targets.legacy.source.hostName = "10.0.0.3";}]);
        message = "recoveryMeta, gap tracking, health and admission must work per named target, require an explicit SQL connection remotely, and reserve the legacy name.";
      }
      {
        name = "local-streams-need-sender-and-slot-headroom";
        assertion = let
          # A named loopback target adds its own receiver and base backup.
          namedLocalRejected =
            lib.any (a: !a.assertion && lib.hasInfix "local receiver plus base backups" a.message)
            (localEval.extendModules {
              modules = [{services.harbor-db.pgBackup.targets.extra.source.hostName = "127.0.0.1";}];
            }).config.assertions;
          rejected = slots: senders:
            lib.any (a: !a.assertion && lib.hasInfix "local receiver plus base backups" a.message)
            (localEval.extendModules {
              modules = [
                {
                  services.postgresql.settings = {
                    max_replication_slots = lib.mkForce slots;
                    max_wal_senders = lib.mkForce senders;
                  };
                }
              ];
            }).config.assertions;
        in
          rejected 1 1 && rejected 2 2 && !rejected 2 3 && namedLocalRejected;
        message = "A loopback receiver plus streaming base backups need two slots and three WAL senders.";
      }
      {
        name = "local-marker-connection-carries-the-source-port";
        assertion = let
          script =
            (localEval.extendModules {
              modules = [
                {
                  services.postgresql.settings.port = lib.mkForce 55432;
                  services.harbor-db.pgBackup = {
                    source.port = lib.mkForce 55432;
                    targetSettings.baseBackup.recoveryMeta = true;
                  };
                }
              ];
            }).config.systemd.services.pg-basebackup.script;
        in
          # The conninfo is shell-escaped in the script, so match its parts.
          lib.hasInfix "/run/postgresql" script && lib.hasInfix "port=55432 user=postgres" script;
        message = "The local restore-point connection must use the configured PostgreSQL port.";
      }
      {
        name = "named-targets-on-one-source-need-distinct-base-slots";
        assertion = let
          failing = c: lib.any (a: !a.assertion && lib.hasInfix "distinct replication slot names" a.message) c.assertions;
          second = {
            source.hostName = "10.0.0.2";
            receiveWal.slotName = "other_wal";
          };
        in
          failing (named [{services.harbor-db.pgBackup.targets.second = second;}])
          && !failing (named [{services.harbor-db.pgBackup.targets.second = second // {baseBackup.slotName = "other_base";};}]);
        message = "Targets sharing a source must not share the temporary base-backup slot either.";
      }
      {
        name = "recovery-meta-needs-receiver-and-modern-postgresql";
        assertion = let
          failing = text: modules:
            lib.any (a: !a.assertion && lib.hasInfix text a.message) (named modules).assertions;
          meta = {
            services.harbor-db.pgBackup.targets.thething.baseBackup = {
              recoveryMeta = true;
              sqlConninfo = "host=10.0.0.2 user=marker";
            };
          };
        in
          !failing "requires receiveWal.enable" [meta]
          && failing "requires receiveWal.enable" [meta {services.harbor-db.pgBackup.targets.thething.receiveWal.enable = false;}]
          && failing "PostgreSQL 15 or newer" [meta {services.harbor-db.pgBackup.targets.thething.package = pkgs.postgresql_14;}]
          && !failing "PostgreSQL 15 or newer" [meta];
        message = "recoveryMeta needs a receiver and a PostgreSQL 15+ package.";
      }
      {
        name = "colliding-target-roots-are-rejected";
        assertion = let
          failing = c: lib.any (a: !a.assertion && lib.hasInfix "backup roots collide" a.message) c.assertions;
        in
          failing (named [{services.harbor-db.pgBackup.source.hostName = "thething";}])
          && !failing (named [{services.harbor-db.pgBackup.source.hostName = "10.0.0.9";}]);
        message = "A legacy host-named root must not equal a named target's root.";
      }
      {
        name = "marker-connection-rejects-embedded-passwords";
        assertion = let
          failing = conninfo:
            lib.any (a: !a.assertion && lib.hasInfix "must not embed a password" a.message)
            (named [{services.harbor-db.pgBackup.targets.thething.baseBackup.sqlConninfo = conninfo;}]).assertions;
        in
          failing "host=h user=u password=secret" && failing "postgresql://u@h/db?%70assword=x" && failing "postgresql://u:secret@h/postgres" && !failing "postgresql://u@h/postgres" && !failing "host=h user=u passfile=/run/secrets/pgpass";
        message = "Passwords must not be rendered into the unit script.";
      }
      {
        name = "recovery-meta-excludes-source-local-recovery";
        assertion = let
          failing = c: lib.any (a: !a.assertion && lib.hasInfix "cannot be combined with sourceLocalRecovery" a.message) c.assertions;
          meta = {
            baseBackup = {
              recoveryMeta = true;
              sqlConninfo = "host=h user=u";
            };
          };
        in
          failing (localEval.extendModules {modules = [{services.harbor-db.pgBackup.targetSettings = meta;}];}).config
          # A named target has its own root and LAST_SUCCESS; only the legacy target is source-local.
          && !failing (localEval.extendModules {modules = [{services.harbor-db.pgBackup.targets.extra = {source.hostName = "10.0.0.2";} // meta;}];}).config
          && !failing localEval.config;
        message = "recoveryMeta must not combine with sourceLocalRecovery on the legacy target; named targets stay independent.";
      }
      {
        name = "loopback-aliases-share-slot-endpoints";
        assertion = let
          failing = c: lib.any (a: !a.assertion && lib.hasInfix "distinct replication slot names" a.message) c.assertions;
        in
          failing (named [{services.harbor-db.pgBackup.targets.a = {source.hostName = "localhost";};} {services.harbor-db.pgBackup.targets.b = {source.hostName = "127.0.0.1";};}]);
        message = "localhost and 127.0.0.1 are the same endpoint for slot uniqueness.";
      }
      {
        name = "backup-dir-is-systemd-safe-and-socket-dir-is-configured";
        assertion = let
          failing = c: lib.any (a: !a.assertion && lib.hasInfix "backupDir may contain only" a.message) c.assertions;
          sock =
            (localEval.extendModules {
              modules = [
                {
                  services.postgresql.settings.unix_socket_directories = "/run/custom, /tmp";
                  services.harbor-db.pgBackup.targetSettings.baseBackup = {
                    recoveryMeta = true;
                    sqlConninfo = null;
                  };
                }
              ];
            }).config.systemd.services.pg-basebackup.script;
        in
          failing (named [{services.harbor-db.pgBackup.targetSettings.backupDir = lib.mkForce "/srv/my backups";}])
          && !failing thething
          && lib.hasInfix "/run/custom" sock
          && !lib.hasInfix "/tmp" sock;
        message = "backupDir must be systemd-safe and the marker connection must use the configured socket directory.";
      }
    ];
  }
