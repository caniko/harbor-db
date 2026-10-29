{
  pkgs,
  module,
  defaultModule ? module,
  nativePackage,
}: let
  backupCommands =
    pkgs.lib.genAttrs ["capture" "restore" "verify" "cleanup"] (stage:
      ["${pkgs.coreutils}/bin/true" "{backup}"] ++ pkgs.lib.optional (stage != "capture") "{workspace}");
  adapterFixture = {config, ...}: {
    imports = [./pg-backup.nix];
    system.stateVersion = "26.05";
    services.postgresql = {
      enable = true;
      package = pkgs.postgresql_18;
      dataDir = "/var/lib/postgresql/18";
    };
    services.harbor-db.postgresql.enable = true;
    services.harbor-db.cutover.enable = true;
    services.harbor-db.pgBackup = {
      enable = true;
      role = "target";
      source.hostName = "127.0.0.1";
      targetSettings.replicatorPasswordFile = "/run/fixture-password";
    };
    services.harbor-db.projects.fixture = {
      postgres.provision = {
        enable = true;
        database = "fixture";
        ownerRole = "fixture_owner";
        runtimeRole = "fixture_runtime";
        runtimeOsUser = "fixture";
      };
      backup = {
        enable = true;
        user = "fixture";
        group = "fixture";
        directory = "/var/lib/fixture-backup";
        commands = backupCommands;
        credentials.token = "/run/fixture-secret";
      };
      transition = {
        enable = true;
        sourceManifest = toString (pkgs.writeText "source-authority.json" "{}");
        targetManifest = toString (pkgs.writeText "target-authority.json" "{}");
        backupManifest = toString config.environment.etc."harbor-db/fixture-backup.json".source;
        independentReceipt = "/var/lib/fixture-evidence/independent.json";
        runtimeUnits = ["fixture.service"];
        commands = pkgs.lib.genAttrs ["import" "verify-target" "verify-source" "health"] (_: {
          user = "fixture";
          argv = ["${pkgs.coreutils}/bin/true"];
        });
      };
    };
  };
  evaluate = modules:
    import "${pkgs.path}/nixos/lib/eval-config.nix" {
      system = pkgs.stdenv.hostPlatform.system;
      modules = modules ++ [adapterFixture];
    };
  eval = evaluate [module];
  defaultEval = evaluate [defaultModule];
  mountedEval = evaluate [
    module
    {
      fileSystems = pkgs.lib.genAttrs ["/var/lib" "/var/lib/fixture-backup" "/var/lib/fixture-backup-sibling"] (_: {
        device = "/dev/fixture";
        fsType = "ext4";
        options = ["nofail"];
      });
    }
  ];
  backupNameAccepted = name: let
    configured = evaluate [
      module
      {
        services.harbor-db.projects.${name}.backup = {
          enable = true;
          user = "fixture";
          group = "fixture";
          directory = "/var/lib/fixture-backup";
          commands = backupCommands;
        };
      }
    ];
    rejected = builtins.filter (assertion: !assertion.assertion && pkgs.lib.hasInfix "backup project name" assertion.message) configured.config.assertions;
  in
    rejected == [];
  boundaryName = count: pkgs.lib.concatStrings (builtins.genList (_: "a") count);
  transitionNameAccepted = name: let
    configured = evaluate [
      module
      ({config, ...}: {
        services.harbor-db.projects.${name}.transition =
          (builtins.removeAttrs config.services.harbor-db.projects.fixture.transition ["manifest"])
          // {
            barrierDirectory = "/var/lib/named-transition";
            runtimeUnits = ["named.service"];
          };
      })
    ];
  in
    builtins.filter (assertion: !assertion.assertion && pkgs.lib.hasInfix "transition project name" assertion.message) configured.config.assertions == [];
  backupPolicyAccepted = backup: let
    configured = evaluate [module {services.harbor-db.projects.fixture.backup = backup;}];
    rejected = builtins.filter (assertion: !assertion.assertion && pkgs.lib.hasPrefix "Harbor DB fixture backup " assertion.message) configured.config.assertions;
  in
    rejected == [];
  withoutPlaceholder = stage: placeholder:
    backupCommands // {${stage} = builtins.filter (arg: arg != placeholder) backupCommands.${stage};};
  backupFileManifest = executableFiles:
    (evaluate [module {services.harbor-db.projects.fixture.backup = {inherit executableFiles;};}]).config.environment.etc."harbor-db/fixture-backup.json".source;
  backupCommandManifest = files:
    (evaluate [
      module
      {
        services.harbor-db.projects.fixture.backup.commands =
          pkgs.lib.mkForce
          (pkgs.lib.genAttrs ["capture" "restore" "verify" "cleanup"] (_: [(builtins.head files) "{backup}" "{workspace}"]));
      }
    ]).config.environment.etc."harbor-db/fixture-backup.json".source;
  transitionFileManifest = executableFiles:
    (evaluate [module {services.harbor-db.projects.fixture.transition = {inherit executableFiles;};}]).config.environment.etc."harbor-db/fixture-transition.json".source;
  transitionCommandManifest = files:
    (evaluate [
      module
      {
        services.harbor-db.projects.fixture.transition.commands =
          pkgs.lib.mkForce
          (pkgs.lib.genAttrs ["import" "verify-target" "verify-source" "health"] (_: {
            user = "fixture";
            argv = [(builtins.head files)];
          }));
      }
    ]).config.environment.etc."harbor-db/fixture-transition.json".source;
  redirectedExecutable = pkgs.runCommand "harbor-db-redirected-adapter" {} ''
    mkdir "$out"
    ln -s /proc/version "$out/adapter"
  '';
  transitionCommands = pkgs.lib.genAttrs ["import" "verify-target" "verify-source" "health"] (_: {
    user = "fixture";
    argv = ["${pkgs.coreutils}/bin/true"];
  });
  transitionCommandsAccepted = commands: let
    configured = evaluate [module {services.harbor-db.projects.fixture.transition.commands = pkgs.lib.mkForce commands;}];
  in
    builtins.filter (assertion: !assertion.assertion && pkgs.lib.hasPrefix "Harbor DB fixture transition" assertion.message) configured.config.assertions == [];
  backupRootsAccepted = {
    directory,
    user ? "fixture",
    enable ? true,
  }: let
    configured = evaluate [
      module
      {
        services.harbor-db.projects.peer.backup = {
          inherit directory user enable;
          group = "fixture";
          commands = backupCommands;
        };
      }
    ];
  in
    builtins.filter (assertion: !assertion.assertion && pkgs.lib.hasPrefix "Harbor DB backup roots" assertion.message) configured.config.assertions == [];
  transitionPoliciesAccepted = {
    peer ? {},
    fixture ? {},
  }: let
    configured = evaluate [
      module
      ({config, ...}: {
        services.harbor-db.projects.fixture.transition = fixture;
        services.harbor-db.projects.peer.transition =
          (builtins.removeAttrs config.services.harbor-db.projects.fixture.transition ["manifest"])
          // {
            barrierDirectory = "/var/lib/harbor-db-transitions/peer";
            runtimeUnits = ["peer.service"];
            retiredUnits = [];
          }
          // peer;
      })
    ];
  in
    builtins.filter (assertion: !assertion.assertion && pkgs.lib.hasPrefix "Harbor DB transition namespaces" assertion.message) configured.config.assertions == [];
  transitionNamespacesAccepted = peer: transitionPoliciesAccepted {inherit peer;};
  custodyStorageAccepted = kind: path: enabled: let
    selected = evaluate [
      module
      {
        services.harbor-db.cutover = {
          enable = pkgs.lib.mkForce enabled;
          resources.peer = {
            user = "fixture";
            runtime_units = [];
            custody_file = "/var/lib/peer-authority/custody.json";
            authority = {
              state_dir = "/var/lib/peer-authority";
              directories = ["/srv/peer-history"];
              binding.backend = "files";
            };
          };
        };
        services.harbor-db.projects.fixture =
          if kind == "backup"
          then {backup.directory = pkgs.lib.mkForce path;}
          else {transition.barrierDirectory = pkgs.lib.mkForce path;};
      }
    ];
  in
    builtins.filter (a: !a.assertion && pkgs.lib.hasInfix "custody namespaces" a.message) selected.config.assertions == [];
  directEval = evaluate [
    ./module.nix
    ./postgres-lifecycle.nix
    ./cutover.nix
    ./pg-backup.nix
  ];
  usesPackage = evaluated: package:
    (builtins.fromJSON (builtins.unsafeDiscardStringContext evaluated.config.services.harbor-db.projects.fixture.transition.manifest.text)).storage_package
    == "${package}/bin"
    && pkgs.lib.all
    (command:
      pkgs.lib.hasInfix
      (builtins.unsafeDiscardStringContext "${package}/bin/")
      (builtins.unsafeDiscardStringContext command))
    (with evaluated.config; [
      systemd.services.harbor-db-fixture-provision.serviceConfig.ExecStart
      systemd.services.harbor-db-fixture-permissions.serviceConfig.ExecStartPost
      systemd.services.harbor-db-fixture-backup.serviceConfig.ExecStart
      systemd.services.pg-backup-prune.script
      system.preSwitchChecks."00---harbor-db-cutover"
    ]);
in
  (import ./eval-checks.nix {inherit pkgs;}).mkEvalCheck {
    name = "harbor-db-native-storage-eval";
    resultMessage = "Native storage package is selected through the module argument";
    runtimeScript = pkgs.lib.concatMapStringsSep "\n" (manifestFor: let
      regular = manifestFor ["${pkgs.coreutils}/bin/true"];
      invalid = map manifestFor [["${pkgs.coreutils}/bin"] ["${pkgs.coreutils}/bin/absent-harbor-adapter"] ["${redirectedExecutable}/adapter"]];
    in ''
      (${regular.executableValidation})
      test -f ${regular}
      ${pkgs.lib.concatMapStringsSep "\n" (manifest: ''
          if (${manifest.executableValidation}); then
            echo 'Invalid additional backup file admitted by manifest construction.' >&2
            exit 1
          fi
        '')
        invalid}
    '') [backupFileManifest transitionFileManifest backupCommandManifest transitionCommandManifest];
    assertions = [
      {
        name = "transition-worker-entrypoints-require-immutable-store-files";
        message = "Every transition worker command rejects mutable entrypoints and runtime store aliases.";
        assertion = pkgs.lib.all (stage:
          transitionCommandsAccepted transitionCommands
          && pkgs.lib.all (path:
            !transitionCommandsAccepted (transitionCommands
              // {
                ${stage} = {
                  user = "fixture";
                  argv = [path];
                };
              }))
          ["/var/lib/fixture/adapter" "/run/store-alias" "${pkgs.coreutils}/bin/../bin/true"])
        ["import" "verify-target" "verify-source" "health"];
      }
      {
        name = "protected-backup-commands-require-immutable-store-files";
        message = "Every protected command entrypoint must reject mutable paths and ambiguous store spelling before worker execution.";
        assertion =
          backupPolicyAccepted {commands = pkgs.lib.mkForce backupCommands;}
          && pkgs.lib.all (stage:
            pkgs.lib.all (path:
              !backupPolicyAccepted {
                commands = pkgs.lib.mkForce (backupCommands // {${stage} = [path "{backup}" "{workspace}"];});
              })
            ["relative-adapter" "/var/lib/app/backup-adapter" "/run/store-alias" "${pkgs.coreutils}/bin/../bin/true" "${pkgs.coreutils}/bin//true"])
          ["capture" "restore" "verify" "cleanup"];
      }
      {
        name = "backup-coordinator-retains-unbounded-publication-and-retention";
        message = "The service must not kill valid coordinator copy/prune work after bounded adapter stages; each stage still carries its configured deadline.";
        assertion = pkgs.lib.all (seconds: let
          selected = evaluate [module {services.harbor-db.projects.fixture.backup.timeoutSeconds = pkgs.lib.mkForce seconds;}];
          service = selected.config.systemd.services.harbor-db-fixture-backup.serviceConfig;
          manifest = builtins.fromJSON (builtins.unsafeDiscardStringContext selected.config.environment.etc."harbor-db/fixture-backup.json".source.text);
        in
          service.TimeoutStartSec
          == "infinity"
          && manifest.timeout_seconds == seconds
          && service.ExecStartPost == service.ExecStartPre
          && pkgs.lib.hasInfix "prune --keep" service.ExecStartPre)
        [1 3600 86400];
      }
      {
        name = "backup-roots-exclude-all-custody-namespaces";
        message = "Scheduled backup mutations must stay outside every enabled corpus, authority and PostgreSQL primary/state root in both containment directions.";
        assertion =
          custodyStorageAccepted "backup" "/srv/peer-history-sibling" true
          && custodyStorageAccepted "backup" "/srv/peer-history/backups" false
          && pkgs.lib.all (path: !custodyStorageAccepted "backup" path true) ["/srv/peer-history" "/srv/peer-history/backups" "/srv" "/var/lib/peer-authority" "/var/lib/peer-authority/backups" "/var/lib" "/var/lib/postgresql/18/backups" "/var/lib/harbor-db/postgresql/backups" "/var/lib/harbor-db/postgresql-startup" "/var/lib/harbor-db/postgresql-startup/backups"];
      }
      {
        name = "transition-barriers-exclude-all-custody-namespaces";
        message = "Transition setup and barrier writes must stay outside every enabled corpus, authority and PostgreSQL primary/state root in both containment directions.";
        assertion =
          custodyStorageAccepted "transition" "/srv/peer-history-sibling" true
          && custodyStorageAccepted "transition" "/srv/peer-history/barriers" false
          && pkgs.lib.all (path: !custodyStorageAccepted "transition" path true) ["/srv/peer-history" "/srv/peer-history/barriers" "/srv" "/var/lib/peer-authority" "/var/lib/peer-authority/barriers" "/var/lib" "/var/lib/postgresql/18/barriers" "/var/lib/harbor-db/postgresql/barriers" "/var/lib/harbor-db/postgresql-startup" "/var/lib/harbor-db/postgresql-startup/barriers"];
      }
      {
        name = "scheduled-backup-retention-precedes-capture";
        message = "Every recurring backup installs serialized pruning before capture, with positive bounded retention beyond capture/retry deadlines.";
        assertion = let
          service = eval.config.systemd.services.harbor-db-fixture-backup.serviceConfig;
        in
          pkgs.lib.hasInfix " prune --keep 96 --incomplete-age-seconds 604800" service.ExecStartPre
          && service.ExecStartPost == service.ExecStartPre
          && pkgs.lib.hasSuffix " capture --scheduled" service.ExecStart
          && backupPolicyAccepted {}
          && !backupPolicyAccepted {retention.incompleteAgeSeconds = 30;};
      }
      {
        name = "transition-evidence-cannot-select-barrier-control-storage";
        message = "Nix admission reserves every enabled barrier plus the systemd control namespace for transition state before writer inhibition.";
        assertion = let
          accepted = independentReceipt: let
            selected = evaluate [
              module
              {
                services.harbor-db.projects.fixture.transition.independentReceipt = pkgs.lib.mkForce independentReceipt;
                services.harbor-db.cutover.resources.fixture = {
                  user = "fixture";
                  runtime_units = [];
                  custody_file = "/var/lib/fixture-authority/custody.json";
                  authority = {
                    state_dir = "/var/lib/fixture-authority";
                    directories = ["/srv/fixture-history"];
                    binding.backend = "files";
                  };
                };
              }
            ];
          in
            pkgs.lib.all (a: a.assertion || !(pkgs.lib.hasInfix "transition independent receipts" a.message)) selected.config.assertions;
        in
          accepted "/var/lib/fixture-evidence/independent.json"
          && !accepted "/var/lib/fixture-authority/independent.json"
          && !accepted "/var/lib/fixture-authority/receipts/independent.json"
          && !accepted "/var/lib/harbor-db-transitions/fixture/inhibited.json"
          && !accepted "/etc/systemd/system.control/fixture.service.d/receipt.json"
          && !accepted "/var/lib/fixture-backup/lock"
          && !accepted "/var/lib/fixture-backup/LAST_SUCCESS"
          && !accepted "/var/lib/fixture-backup/point/acceptance.json";
      }
      {
        name = "backup-storage-mount-required";
        message = "Backup captures must require their storage mount even when it is nofail.";
        assertion = (eval.config.systemd.services.harbor-db-fixture-backup.unitConfig.RequiresMountsFor or []) == ["/var/lib/fixture-backup"];
      }
      {
        name = "backup-declared-mount-dependencies-survive-masking";
        assertion = let
          service = mountedEval.config.systemd.services.harbor-db-fixture-backup;
          mounts = ["var-lib.mount" "var-lib-fixture\\x2dbackup.mount"];
        in
          pkgs.lib.all (mount: pkgs.lib.elem mount service.requires && pkgs.lib.elem mount service.after) mounts
          && !(pkgs.lib.elem "var-lib-fixture\\x2dbackup\\x2dsibling.mount" service.requires);
        message = "Declared backup mount ancestors must retain explicit Requires/After edges even when masked; sibling mounts must not be required.";
      }
      {
        name = "backup-credential-sources-bound-for-forced-capture";
        message = "The immutable selected backup policy must retain runtime credential references for captures outside its scheduled service.";
        assertion = ((builtins.fromJSON (builtins.unsafeDiscardStringContext eval.config.environment.etc."harbor-db/fixture-backup.json".source.text)).credentials or {}) == {token = "/run/fixture-secret";};
      }
      {
        name = "transition-only-resource-name-boundaries";
        message = "Transition-only projects must use the runtime's bounded resource-name grammar.";
        assertion =
          pkgs.lib.all transitionNameAccepted ["valid_Name-0" (boundaryName 128)]
          && pkgs.lib.all (name: !transitionNameAccepted name) ["" "with.dot" "with@instance" "non-ascii-é" (boundaryName 129)];
      }
      {
        name = "backup-roots-are-exclusive-normalized-namespaces";
        message = "Enabled projects must have distinct normalized backup roots, regardless of owner; disabled projects do not reserve a namespace.";
        assertion =
          backupRootsAccepted {directory = "/var/lib/peer-backup";}
          && backupRootsAccepted {
            directory = "/var/lib/fixture-backup";
            enable = false;
          }
          && !(backupRootsAccepted {directory = "/var/lib/fixture-backup";})
          && !(backupRootsAccepted {directory = "/var/lib/fixture-backup/peer";})
          && !(backupRootsAccepted {directory = "/var/lib";})
          && backupRootsAccepted {directory = "/var/lib/fixture-backup-peer";}
          && !(backupRootsAccepted {
            directory = "/var/lib/fixture-backup";
            user = "other";
          })
          && pkgs.lib.all (directory: !(backupRootsAccepted {inherit directory;}))
          ["/var/lib/./fixture-backup" "/var/lib/peer/../fixture-backup" "/var/lib//fixture-backup" "/var/lib/fixture-backup/"];
      }
      {
        name = "transition-writers-have-one-owner";
        message = "Active and retired writers must belong to only one enabled transition.";
        assertion =
          transitionNamespacesAccepted {}
          && !(transitionNamespacesAccepted {runtimeUnits = ["fixture.service"];})
          && !(transitionNamespacesAccepted {retiredUnits = ["fixture.service"];})
          && !(transitionNamespacesAccepted {runtimeUnits = ["peer.service" "peer.service"];})
          && !(transitionPoliciesAccepted {
            fixture.retiredUnits = ["legacy.service"];
            peer.runtimeUnits = ["legacy.service"];
          })
          && !(transitionPoliciesAccepted {
            fixture.retiredUnits = ["legacy.service"];
            peer.retiredUnits = ["legacy.service"];
          })
          && transitionPoliciesAccepted {fixture.retiredUnits = ["legacy.service"];}
          && transitionNamespacesAccepted {
            enable = false;
            runtimeUnits = ["fixture.service"];
          };
      }
      {
        name = "transition-barriers-have-dedicated-disjoint-namespaces";
        message = "Barrier namespaces must be normalized and non-overlapping with enabled transition or backup namespaces.";
        assertion =
          transitionNamespacesAccepted {}
          && transitionNamespacesAccepted {
            enable = false;
            barrierDirectory = "/var/lib/harbor-db-transitions/fixture";
          }
          && pkgs.lib.all (barrierDirectory: !(transitionNamespacesAccepted {inherit barrierDirectory;}))
          ["/var/lib/harbor-db-transitions/fixture" "/var/lib/harbor-db-transitions/fixture/peer" "/var/lib/harbor-db-transitions" "/var/lib/./harbor-db-transitions/peer" "/var/lib/harbor-db-transitions/other/../peer" "/var/lib//harbor-db-transitions/peer" "/var/lib/harbor-db-transitions/peer/" "/var/lib/fixture-backup" "/var/lib/fixture-backup/barrier"];
      }
      {
        name = "transition-worker-argv-boundaries";
        message = "Every declared transition worker must have nonempty argv with an absolute executable.";
        assertion =
          transitionCommandsAccepted transitionCommands
          && pkgs.lib.all (stage:
            pkgs.lib.all (argv:
              !transitionCommandsAccepted (transitionCommands // {${stage} = transitionCommands.${stage} // {inherit argv;};}))
            [[] [""] ["relative-tool"]]) (builtins.attrNames transitionCommands);
      }
      {
        name = "backup-command-placeholder-boundaries";
        message = "All stages require a literal backup argument, and restore/verify/cleanup also require a literal workspace argument.";
        assertion =
          backupPolicyAccepted {commands = pkgs.lib.mkForce backupCommands;}
          && pkgs.lib.all (stage: !backupPolicyAccepted {commands = pkgs.lib.mkForce (withoutPlaceholder stage "{backup}");}) ["capture" "restore" "verify" "cleanup"]
          && pkgs.lib.all (stage: !backupPolicyAccepted {commands = pkgs.lib.mkForce (withoutPlaceholder stage "{workspace}");}) ["restore" "verify" "cleanup"]
          && !backupPolicyAccepted {commands = pkgs.lib.mkForce (backupCommands // {capture = ["${pkgs.coreutils}/bin/true" "--backup={backup}"];});};
      }
      {
        name = "additional-backup-executables-are-immutable-files";
        assertion =
          backupPolicyAccepted {executableFiles = ["${pkgs.coreutils}/bin/true"];}
          && pkgs.lib.all (path: !backupPolicyAccepted {executableFiles = [path];}) ["relative-adapter" "/absent/adapter" "/run/runtime-adapter" "${pkgs.coreutils}/bin/../bin/true" "${pkgs.coreutils}/bin//true"]
          && (let manifest = backupFileManifest ["${pkgs.coreutils}/bin/true"]; in pkgs.lib.hasPrefix manifest.executableValidation manifest.buildCommand);
        message = "Additional adapters require normalized immutable paths during evaluation and regular files before the backup manifest is built; the exact production validation rejects directories and nonexistent files at realization without IFD.";
      }
      {
        name = "additional-transition-executables-are-immutable-paths";
        assertion = let
          accepted = executableFiles: let
            selected = (evaluate [module {services.harbor-db.projects.fixture.transition.executableFiles = executableFiles;}]).config;
          in
            pkgs.lib.all (entry: entry.assertion || !(pkgs.lib.hasInfix "transition executableFiles" entry.message)) selected.assertions;
        in
          accepted ["${pkgs.coreutils}/bin/true"]
          && pkgs.lib.all (path: !accepted [path]) ["relative-adapter" "/absent/adapter" "/run/runtime-adapter" "${pkgs.coreutils}/bin/../bin/true" "${pkgs.coreutils}/bin//true"]
          && (let manifest = transitionFileManifest ["${pkgs.coreutils}/bin/true"]; in pkgs.lib.hasPrefix manifest.executableValidation manifest.buildCommand);
        message = "Transition adapters reject relative and mutable or ambiguous paths, preserving normalized store-local files.";
      }
      {
        name = "backup-credential-name-boundaries";
        message = "Credential IDs must be bounded filename-safe ASCII names before generating LoadCredential.";
        assertion =
          pkgs.lib.all (name: backupPolicyAccepted {credentials.${name} = "/run/fixture-secret";}) ["A_0-z.token" (boundaryName 255)]
          && pkgs.lib.all (name: !backupPolicyAccepted {credentials.${name} = "/run/fixture-secret";}) ["" "." ".." "a:b" "a/b" "a b" "a\n" (boundaryName 256)];
      }
      {
        name = "backup-resource-name-boundaries";
        message = "Enabled backup project names must match the runtime resource alphabet and 1..128-byte boundary.";
        assertion = backupNameAccepted "A_0-z" && backupNameAccepted (boundaryName 128) && pkgs.lib.all (name: !backupNameAccepted name) ["a.b" "a@b" "" (boundaryName 129)];
      }
      {
        name = "default-native-package";
        message = "The default flake module selects Rust without native-storage enrollment";
        assertion =
          defaultEval.config.services.harbor-db.postgresql.package
          == nativePackage
          && defaultEval.config.services.harbor-db.cutover.package == nativePackage
          && usesPackage defaultEval nativePackage;
      }
      {
        name = "direct-module-native-package";
        message = "Direct module imports also select Rust for every lifecycle adapter";
        assertion =
          (directEval.config.services.harbor-db.postgresql.package.harborDbRuntime or null)
          == "rust"
          && directEval.config.services.harbor-db.postgresql.package == directEval.config.services.harbor-db.cutover.package
          && usesPackage directEval directEval.config.services.harbor-db.postgresql.package;
      }
      {
        name = "postgres-native-package";
        message = "PostgreSQL lifecycle selects the native package";
        assertion = eval.config.services.harbor-db.postgresql.package == nativePackage;
      }
      {
        name = "cutover-native-package";
        message = "Cutover dispatch selects the native package";
        assertion = eval.config.services.harbor-db.cutover.package == nativePackage;
      }
      {
        name = "postgres-native-prestart";
        message = "The managed PostgreSQL pre-start hook executes the native adapter";
        assertion =
          pkgs.lib.hasInfix
          (builtins.unsafeDiscardStringContext "${nativePackage}/bin/harbor-db-postgres")
          (builtins.unsafeDiscardStringContext eval.config.systemd.services.postgresql.preStart);
      }
    ];
  }
