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
      };
      transition = {
        enable = true;
        sourceManifest = toString (pkgs.writeText "source-authority.json" "{}");
        targetManifest = toString (pkgs.writeText "target-authority.json" "{}");
        backupManifest = toString config.environment.etc."harbor-db/fixture-backup.json".source;
        independentReceipt = "/var/lib/fixture-authority/independent.json";
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
      inherit (pkgs) system;
      modules = modules ++ [adapterFixture];
    };
  eval = evaluate [module];
  defaultEval = evaluate [defaultModule];
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
  backupPolicyAccepted = backup: let
    configured = evaluate [module {services.harbor-db.projects.fixture.backup = backup;}];
    rejected = builtins.filter (assertion: !assertion.assertion && pkgs.lib.hasPrefix "Harbor DB fixture backup " assertion.message) configured.config.assertions;
  in
    rejected == [];
  withoutPlaceholder = stage: placeholder:
    backupCommands // {${stage} = builtins.filter (arg: arg != placeholder) backupCommands.${stage};};
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
    assertions = [
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
