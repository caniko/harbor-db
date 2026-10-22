{
  pkgs,
  module,
  lifecycleModule,
}: let
  inherit (pkgs) lib;
  inherit (import ./eval-checks.nix {inherit pkgs;}) mkEvalCheck;
  # Preserve the input path during cold read-only flake qualification.
  eval = import (pkgs.path + "/nixos/lib/eval-config.nix") {
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      lifecycleModule
      # Multiple consumers import the default module; its options must appear
      # once, including when the standalone lifecycle module is also imported.
      {imports = [module];}
      {imports = [module];}
      {
        system.stateVersion = "26.05";
        services.postgresql = {
          enable = true;
          package = pkgs.postgresql_18;
          dataDir = "/srv/postgres/18";
        };
        services.harbor-db.postgresql = {
          enable = true;
          stateDir = "/srv/postgres/authority";
          requiredMounts = ["/srv"];
          writerFence.blockedUnits = ["fixture-client.service"];
          switchAdoption.systemIdentifier = "12345";
          recovery = {
            requireWriterFence = true;
            systemIdentifier = "12345";
            backupRoot = "/srv/backups";
            snapshotFile = "/srv/backups/records.json";
            receiptFile = "/srv/backups/recovery.json";
            offHostReceiptFile = "/srv/backups/off-host.json";
            recordChecks.records = {
              database = "app";
              sql = "SELECT count(*) FROM records";
            };
          };
          upgrade = {
            oldPackage = pkgs.postgresql_17;
            oldDataDir = "/srv/postgres/17";
            validateCommand = ["${pkgs.writeShellScript "validate-upgrade" "exit 0"}"];
          };
        };
        systemd.services.fixture-client = {
          unitConfig.ConditionPathExists = ["/fixture/ready"];
          serviceConfig.ExecStart = "/fixture/client";
        };
      }
    ];
  };
  preStart = eval.config.systemd.services.postgresql.preStart;
  upgrade = eval.config.systemd.services.harbor-db-postgresql-upgrade;
  withoutAdoption = eval.extendModules {
    modules = [{services.harbor-db.postgresql.switchAdoption = lib.mkForce null;}];
  };
  withPreparation = eval.extendModules {
    modules = [
      {
        services.harbor-db.postgresql.recoveryPreparation = {
          readinessCommand = ["/fixture/readiness"];
          backupCommand = ["/fixture/backup"];
          restoreCommand = ["/fixture/restore"];
          readWritePaths = ["/srv/backups" "/srv/disposable"];
          requiredMounts = ["/srv"];
          offHostReceiptImportFile = "/srv/import/off-host.json";
        };
      }
    ];
  };
  preparationHook = pkgs.writeText "recovery-preparation-hook" withPreparation.config.system.preSwitchChecks."00-0-harbor-db-postgresql-prepare";
  sourceLocalPolicy = {
    prepare,
    fenced,
  }:
    eval.extendModules {
      modules = [
        {
          services.harbor-db.postgresql = {
            recovery.repositoryProtocol = "source-local-v1";
            recovery.requireWriterFence = lib.mkForce fenced;
            recoveryPreparation =
              if prepare
              then withPreparation.config.services.harbor-db.postgresql.recoveryPreparation
              else null;
          };
        }
      ];
    };
  sourceLocalFenceRejected = policy:
    lib.any (entry:
      !entry.assertion && entry.message == "Managed source-local recovery preparation requires writer fencing; read-only certification and retired enrollment may remain unfenced.")
    policy.config.assertions;
  preparationPathsAccepted = overrides: let
    selected = (withPreparation.extendModules {modules = [overrides];}).config;
  in
    lib.all (entry: entry.assertion || !(lib.hasPrefix "Managed recovery preparation" entry.message || lib.hasPrefix "Managed recovery provisioning" entry.message)) selected.assertions;
  receiptPathsAccepted = overrides: let
    selected = (eval.extendModules {modules = [{services.harbor-db.postgresql.recovery = overrides;}];}).config;
  in
    lib.all (entry: entry.assertion || entry.message != "PostgreSQL recovery snapshots, local receipts and off-host receipts must use distinct paths.") selected.assertions;
in
  mkEvalCheck {
    name = "harbor-db-postgres-lifecycle-eval";
    resultMessage = "PostgreSQL identity guard precedes initialization and upgrade is explicit";
    nativeBuildInputs = [pkgs.python3 pkgs.bash];
    runtimeScript = ''
      python3 ${../tests/check_recovery_preparation_hook.py} ${preparationHook} \
        ${pkgs.systemd}/bin/systemd-run ${pkgs.systemd}/bin/systemctl
    '';
    assertions = [
      {
        name = "lifecycle-configuration-matches-check-platform";
        message = "Lifecycle configuration and preparation dependencies must use the check's native platform.";
        assertion = eval.pkgs.stdenv.hostPlatform.system == pkgs.stdenv.hostPlatform.system;
      }
      {
        name = "upgrade-requires-immutable-validator";
        message = "Worker-writable validator entrypoints cannot authorize staged upgrade publication.";
        assertion = let
          accepted = command:
            lib.all (a: a.assertion || !lib.hasPrefix "Staged PostgreSQL upgrades require consumer validation" a.message)
            (eval.extendModules {modules = [{services.harbor-db.postgresql.upgrade.validateCommand = lib.mkForce command;}];}).config.assertions;
        in
          accepted ["${pkgs.writeShellScript "immutable-upgrade-validator" "exit 0"}"]
          && !accepted ["/run/worker-validator"]
          && !accepted ["/srv/postgres/validator"]
          && !accepted ["/nix/store/invalid-validator"]
          && !accepted [];
      }
      {
        name = "recovery-evidence-excludes-postgres-primary-and-policy-trees";
        message = "Snapshot/local/off-host outputs cannot replace PostgreSQL cluster, lifecycle authority, or startup-inhibition files under legacy or source-local policy.";
        assertion = let
          accepted = protocol: key: path: let
            selected =
              (eval.extendModules {
                modules = [
                  {
                    services.harbor-db.postgresql.recovery = {
                      repositoryProtocol = protocol;
                      ${key} = lib.mkForce path;
                    };
                  }
                ];
              }).config;
          in
            lib.all (a: a.assertion || !lib.hasInfix "PostgreSQL storage" a.message) selected.assertions;
        in
          lib.all (protocol:
            lib.all (key:
              accepted protocol key "/srv/postgres/18-sibling/receipt.json"
              && lib.all (path: !accepted protocol key path) ["/srv/postgres/18" "/srv/postgres/18/PG_VERSION" "/srv/postgres/18/global/pg_control" "/srv/postgres/authority" "/srv/postgres/authority/identity.json" "/var/lib/harbor-db/postgresql-startup" "/var/lib/harbor-db/postgresql-startup/policy.json" "/srv/postgres"])
            ["backupRoot" "snapshotFile" "receiptFile" "offHostReceiptFile"]) ["legacy" "source-local-v1"];
      }
      {
        name = "writer-fence-setup-units-are-unique-and-not-the-primary";
        message = "Evaluation must reject setup unit lists that runtime inhibition rejects before installing a persistent gate.";
        assertion = let
          accepted = setupUnits: let
            selected = (eval.extendModules {modules = [{services.harbor-db.postgresql.writerFence.setupUnits = lib.mkForce setupUnits;}];}).config;
          in
            lib.all (entry: entry.assertion || !(lib.hasPrefix "Writer-fence setup units" entry.message)) selected.assertions;
        in
          accepted []
          && accepted ["postgresql-setup.service" "fixture-setup.service"]
          && !accepted ["postgresql.service"]
          && !accepted ["postgresql-setup.service" "postgresql-setup.service"];
      }
      {
        name = "source-local-evidence-reserves-producer-control-namespaces";
        message = "Snapshots and receipts must not replace repository selectors, locks, base backups or WAL; the dedicated evidence subtree remains supported.";
        assertion = let
          accepted = overrides: let
            selected = (eval.extendModules {modules = [{services.harbor-db.postgresql.recovery = {repositoryProtocol = "source-local-v1";} // overrides;}];}).config;
          in
            lib.all (a: a.assertion || !(lib.hasPrefix "PostgreSQL recovery evidence" a.message)) selected.assertions;
        in
          accepted {receiptFile = lib.mkForce "/srv/backups/evidence/local.json";}
          && lib.all (key: lib.all (suffix: !accepted {${key} = lib.mkForce "/srv/backups/${suffix}";}) ["recovery/SELECTED" "base/point/backup_manifest" "wal/segment" "locks/mutate" "BACKUP_LOCK" "LAST_SUCCESS" "recovery.lock" "preparation.lock" "preparation.json"]) ["snapshotFile" "receiptFile" "offHostReceiptFile"];
      }
      {
        name = "legacy-evidence-also-reserves-producer-control-namespaces";
        message = "Default legacy recovery must reserve selectors, locks, base and WAL storage for every evidence output, while accepting dedicated evidence files.";
        assertion = let
          accepted = overrides: let
            selected = (eval.extendModules {modules = [{services.harbor-db.postgresql.recovery = overrides;}];}).config;
          in
            lib.all (entry: entry.assertion || !(lib.hasPrefix "PostgreSQL recovery evidence" entry.message)) selected.assertions;
        in
          accepted {receiptFile = lib.mkForce "/srv/backups/evidence/local.json";}
          && lib.all (key: lib.all (suffix: !accepted {${key} = lib.mkForce "/srv/backups/${suffix}";}) ["recovery/SELECTED" "base/point/backup_manifest" "wal/segment" "locks/mutate" "BACKUP_LOCK" "LAST_SUCCESS" "recovery.lock" "preparation.lock" "preparation.json"]) ["snapshotFile" "receiptFile" "offHostReceiptFile"];
      }
      {
        name = "upgrade-validator-requires-an-absolute-executable";
        message = "Evaluated upgrades must supply a validator executable usable by the bound runtime contract.";
        assertion = let
          accepted = command: let
            selected = (eval.extendModules {modules = [{services.harbor-db.postgresql.upgrade.validateCommand = lib.mkForce command;}];}).config;
          in
            lib.all (entry: entry.assertion || !(lib.hasPrefix "Staged PostgreSQL upgrades require" entry.message)) selected.assertions;
        in
          accepted ["${pkgs.writeShellScript "validator" "exit 0"}" "literal argument"]
          && lib.all (command: !accepted command) [[] [""] ["validator"] ["relative/validator"] ["./validator"]];
      }
      {
        name = "lifecycle-state-is-dedicated-and-normalized";
        message = "Identity state requires a dedicated normalized absolute directory before tmpfiles ownership is generated, independent of managed preparation.";
        assertion = let
          accepted = stateDir: let
            selected = (eval.extendModules {modules = [{services.harbor-db.postgresql.stateDir = lib.mkForce stateDir;}];}).config;
          in
            lib.all (entry: entry.assertion || entry.message != "Harbor DB identity state requires a dedicated normalized absolute directory.") selected.assertions;
        in
          accepted "/srv/harbor-state" && lib.all (path: !accepted path) ["/" "" "relative" "/srv/state/" "/srv//state" "/srv/./state" "/srv/../state" "/srv/state other" "/srv/state\nother"];
      }
      {
        name = "startup-state-is-normalized-and-disjoint";
        message = "Startup inhibition requires normalized storage disjoint in both directions from identity state and PostgreSQL data, including default-path overlaps.";
        assertion = let
          accepted = overrides: let
            selected = (eval.extendModules {modules = [overrides];}).config;
          in
            lib.all (entry: entry.assertion || !(lib.hasPrefix "PostgreSQL startup inhibition" entry.message)) selected.assertions;
          startup = path: {services.harbor-db.postgresql.writerFence.startupStateDir = lib.mkForce path;};
        in
          accepted (startup "/srv/startup-state")
          && accepted (startup "/srv/postgres/authority-other")
          && lib.all (path: !accepted (startup path)) ["/" "/srv/startup/" "/srv//startup" "/srv/./startup" "/srv/../startup" "/srv/startup other" "/srv/startup%other" "/srv/startup\\other" "/srv/postgres/authority" "/srv/postgres/authority/child" "/srv/postgres/18" "/srv/postgres/18/child" "/srv/postgres"]
          && !accepted {services.harbor-db.postgresql.stateDir = lib.mkForce "/var/lib/harbor-db";}
          && !accepted {services.postgresql.dataDir = lib.mkForce "/var/lib/harbor-db";};
      }
      {
        name = "recovery-receipt-paths-are-distinct";
        message = "Local and off-host recovery receipts cannot alias one another or overwrite the snapshot.";
        assertion =
          receiptPathsAccepted {}
          && receiptPathsAccepted {offHostReceiptFile = lib.mkForce null;}
          && !(receiptPathsAccepted {offHostReceiptFile = lib.mkForce "/srv/backups/recovery.json";})
          && !(receiptPathsAccepted {receiptFile = lib.mkForce "/srv/backups/records.json";})
          && !(receiptPathsAccepted {offHostReceiptFile = lib.mkForce "/srv/backups/records.json";});
      }
      {
        name = "preparation-cannot-provision-startup-inhibition-storage";
        message = "Preparation writable/provisioned storage must not change root-owned startup inhibition storage in either containment direction.";
        assertion = let
          startup = withPreparation.config.services.harbor-db.postgresql.writerFence.startupStateDir;
          accepted = path:
            preparationPathsAccepted {
              services.harbor-db.postgresql.recoveryPreparation = {
                readWritePaths = lib.mkForce [path];
                provisionDirectories = lib.mkForce [path];
              };
            };
        in
          accepted "${startup}-sibling"
          && lib.all (path: !accepted path) [startup "${startup}/child" (builtins.dirOf startup)];
      }
      {
        name = "preparation-paths-reject-traversal-before-root-provisioning";
        message = "Provisioning, writable paths and protected roots must have normalized spelling before containment and isolation checks.";
        assertion =
          preparationPathsAccepted {services.harbor-db.postgresql.recoveryPreparation.provisionDirectories = ["/srv/disposable/new"];}
          && lib.all (path:
            !(preparationPathsAccepted {
              services.harbor-db.postgresql.recoveryPreparation.provisionDirectories = [path];
            })) ["/srv/disposable/../postgres/18/new" "/srv/disposable/../postgres/authority/new" "/srv/disposable/./new" "/srv/disposable//new" "/srv/disposable/new/"]
          && lib.all (path:
            !(preparationPathsAccepted {
              services.harbor-db.postgresql.recoveryPreparation.readWritePaths = lib.mkForce [path];
            })) ["/srv/disposable/../postgres/18" "/srv/disposable/../postgres/authority" "/srv/./disposable" "/srv//disposable" "/srv/disposable/"]
          && !(preparationPathsAccepted {services.postgresql.dataDir = lib.mkForce "/srv/postgres/./18";})
          && !(preparationPathsAccepted {services.harbor-db.postgresql.stateDir = lib.mkForce "/srv/postgres/./authority";});
      }
      {
        name = "source-local-producer-fence-policy";
        assertion =
          sourceLocalFenceRejected (sourceLocalPolicy {
            prepare = true;
            fenced = false;
          })
          && !(sourceLocalFenceRejected (sourceLocalPolicy {
            prepare = true;
            fenced = true;
          }))
          && !(sourceLocalFenceRejected (sourceLocalPolicy {
            prepare = false;
            fenced = false;
          }));
        message = "Managed source-local capture rejects an unfenced policy without banning read-only certification or retired enrollment.";
      }
      {
        name = "writer-fence-client-startup";
        assertion =
          eval.config.systemd.services.fixture-client.unitConfig.ConditionPathExists
          == ["/fixture/ready" "!/srv/postgres/authority/writer-fence.json"]
          && !(eval.config.systemd.services.postgresql.unitConfig ? ConditionPathExists)
          && !(eval.config.systemd.services.postgresql-setup.unitConfig ? ConditionPathExists)
          && eval.config.services.harbor-db.postgresql.recovery.requireWriterFence;
        message = "Explicit client gates must preserve existing conditions and leave PostgreSQL control/startup available; bootstrap recovery retains enforced fencing.";
      }
      {
        name = "recovery-writer-fence-policy";
        assertion = let
          manifest = builtins.fromJSON (builtins.unsafeDiscardStringContext eval.config.environment.etc."harbor-db/postgresql.json".source.text);
        in
          manifest.recovery.require_writer_fence && manifest.writer_fence.control_role == "postgres";
        message = "Explicit recovery enrollment must render its writer-fence requirement in the candidate manifest.";
      }
      {
        name = "startup-inhibits-setup-with-primary";
        assertion = let
          manifest = builtins.fromJSON (builtins.unsafeDiscardStringContext eval.config.environment.etc."harbor-db/postgresql.json".source.text);
        in
          manifest.startup_inhibition.setup_units == ["postgresql-setup.service"];
        message = "Persistent startup inhibition must cover setup so a legacy switch cannot run setup SQL against the inhibited primary.";
      }
      {
        name = "managed-preactivation-preparation";
        assertion = let
          checks = withPreparation.config.system.preSwitchChecks;
          hook = checks."00-0-harbor-db-postgresql-prepare";
        in
          !(eval.config.system.preSwitchChecks ? "00-0-harbor-db-postgresql-prepare")
          && builtins.head (builtins.attrNames checks) == "00-0-harbor-db-postgresql-prepare"
          && lib.hasInfix "prepare-recovery --preparation-config" hook
          && lib.hasInfix "--property=User=postgres" hook
          && lib.hasInfix "--property=LoadCredential=recovery-off-host:" hook
          && !lib.hasInfix "prepare-recovery" preStart;
        message = "Preparation must be explicit, run candidate commands before admission, import private independent evidence and never bootstrap at boot.";
      }
      {
        name = "recovery-before-adoption";
        assertion = let
          checks = eval.config.system.preSwitchChecks;
          service = eval.config.systemd.services.harbor-db-postgresql-recovery-check;
        in
          lib.hasInfix "inspect-recovery" checks."00-harbor-db-postgresql-recovery"
          && service.wantedBy == []
          && service.serviceConfig.ReadWritePaths == []
          && service.serviceConfig.User == "postgres"
          && !lib.hasInfix "recovery" preStart;
        message = "Recovery admission must be read-only, precede adoption and never start a boot-time drill.";
      }
      {
        name = "no-implicit-switch-adoption";
        assertion = !(withoutAdoption.config.system.preSwitchChecks ? harbor-db-postgresql-adoption);
        message = "A guarded cluster must not gain an implicit switch-time adoption request.";
      }
      {
        name = "switch-adoption-before-unit-stop-only";
        assertion = let
          check = eval.config.system.preSwitchChecks.harbor-db-postgresql-adoption;
        in
          lib.hasInfix "runuser -u postgres --" check
          && lib.hasInfix "inspect-live --system-identifier 12345" check
          && lib.hasInfix "switch|test)" check
          && lib.hasInfix "adopt-live --system-identifier 12345" check
          && !lib.hasInfix "adopt" preStart;
        message = "Explicit switch adoption must verify the live primary before unit stop; startup cannot adopt.";
      }
      {
        name = "guard-before-initdb";
        assertion =
          lib.hasPrefix "${lib.getExe eval.config.services.harbor-db.postgresql.package} --config" preStart
          && lib.hasInfix "check\n" (builtins.head (lib.splitString "initdb" preStart));
        message = "Identity guard must run before NixOS initdb on every startup.";
      }
      {
        name = "writer-lifetime-lease";
        assertion =
          lib.hasSuffix " serve" eval.config.systemd.services.postgresql.serviceConfig.ExecStart
          && eval.config.systemd.services.postgresql.serviceConfig.NotifyAccess == "main";
        message = "The PostgreSQL process must retain the shared authority lease for its lifetime.";
      }
      {
        name = "offline-upgrade-only";
        assertion = upgrade.wantedBy == [] && lib.elem "postgresql.service" upgrade.conflicts;
        message = "Major upgrades must be explicit and stop the writer.";
      }
      {
        name = "persistent-state-mount";
        assertion = lib.elem "/srv/postgres/authority" eval.config.systemd.services.postgresql.unitConfig.RequiresMountsFor;
        message = "Identity state must be mounted before startup.";
      }
      {
        name = "durable-commit-settings";
        assertion =
          eval.config.services.postgresql.settings.fsync
          && eval.config.services.postgresql.settings.full_page_writes
          && eval.config.services.postgresql.settings.synchronous_commit == "on";
        message = "Guarded clusters must retain PostgreSQL durable-commit settings.";
      }
    ];
  }
