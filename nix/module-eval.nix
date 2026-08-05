{
  lib,
  module,
  pkgs,
}: let
  secretValue = "module-eval-secret";
  secretFile = pkgs.writeText "db-harbor-module-eval-secret" secretValue;
  rawCommand = pkgs.writeShellScript "db-harbor-module-eval-raw" "exit 0";
  runner = pkgs.writeShellScriptBin "module-eval-runner" "exit 0";
  eval = import "${pkgs.path}/nixos/lib/eval-config.nix" {
    system = pkgs.system;
    modules = [
      module
      {
        system.stateVersion = "24.11";
        services.db-harbor.package = pkgs.writeShellScriptBin "db-harbor" "exit 0";
        services.db-harbor.operations.raw = {
          enable = true;
          command = "${rawCommand}";
          checkCommand = "${rawCommand}";
          credentials.token = secretFile;
          stateDirectory = "db-harbor-raw";
          runtimeDirectory = "db-harbor-raw";
        };
        services.db-harbor.projects.demo = {
          enable = true;
          operations.ensure = {
            enable = true;
            kind = "credential";
            lifecycle = "ensure";
            credentials.token = secretFile;
            runner = {
              package = runner;
              executable = "bin/module-eval-runner";
              args = ["ensure"];
              checkArgs = ["check"];
              credentialEnvironment.TOKEN_FILE = "token";
            };
            stateDirectory = "db-harbor-module-eval";
            runtimeDirectory = "db-harbor-module-eval";
            after = ["network-online.target"];
            requires = ["network-online.target"];
            runtimeUnits = ["demo-app.service"];
          };
          operations.restore = {
            enable = true;
            lifecycle = "restore";
            safety = "operator_confirmed";
            runner = {
              command = "systemctl start demo-helper.service";
              checkCommand = "test -f /var/lib/db-harbor-restore-healed";
            };
          };
        };
        services.db-harbor.dataDirectories = [
          {
            path = "/var/lib/db-harbor-data";
            user = "postgres";
            group = "postgres";
            mode = "0700";
          }
        ];
      }
    ];
  };
  service = eval.config.systemd.services.db-harbor-demo;
  checkService = eval.config.systemd.services.db-harbor-demo-check;
  restoreService = eval.config.systemd.services.db-harbor-demo-restore;
  rawService = eval.config.systemd.services.db-harbor-raw;
  applyScript = builtins.replaceStrings ["\n"] [" "] (builtins.readFile service.serviceConfig.ExecStart);
  manifest = builtins.elemAt (builtins.match ".*--manifest ([^ ]+).*" applyScript) 0;
  plan = builtins.readFile manifest;
  restoreScript = builtins.replaceStrings ["\n"] [" "] (builtins.readFile restoreService.serviceConfig.ExecStart);
in
  (import ./eval-checks.nix {inherit pkgs;}).mkEvalCheck {
    name = "db-harbor-module-eval";
    resultMessage = "db-harbor generic lifecycle module keeps credentials out of plans";
    assertions = [
      {
        name = "credential-load-is-per-operation-source";
        assertion = lib.elem "token:${secretFile}" service.serviceConfig.LoadCredential;
        message = "the operation credential source must be rendered as a systemd LoadCredential entry";
      }
      {
        name = "credential-value-is-not-in-plan";
        assertion = !(lib.hasInfix secretValue plan);
        message = "credential contents must not be serialized into the generated plan";
      }
      {
        name = "credential-reference-is-a-file-path";
        assertion = lib.hasInfix "credential_environment" plan && lib.hasInfix "token" plan;
        message = "plans must contain only the credential name reference";
      }
      {
        name = "state-directory";
        assertion = service.serviceConfig.StateDirectory == ["db-harbor-module-eval"];
        message = "operation stateDirectory must reach systemd serviceConfig";
      }
      {
        name = "runtime-directory";
        assertion = service.serviceConfig.RuntimeDirectory == ["db-harbor-module-eval"];
        message = "operation runtimeDirectory must reach systemd serviceConfig";
      }
      {
        name = "dependency-gating";
        assertion = lib.elem "network-online.target" service.after && lib.elem "network-online.target" service.requires;
        message = "operation dependencies must gate the generated service";
      }
      {
        name = "runtime-gating";
        assertion = lib.elem "demo-app.service" service.requiredBy && lib.elem "demo-app.service" service.before;
        message = "runtime units must be ordered after and require successful lifecycle completion";
      }
      {
        name = "check-service-loads-credentials";
        assertion = lib.elem "token:${secretFile}" checkService.serviceConfig.LoadCredential;
        message = "read-only checks must receive the same systemd credentials";
      }
      {
        name = "check-command-is-present";
        assertion = checkService.serviceConfig.ExecStart != null;
        message = "ensure operations with checkArgs must expose a check unit";
      }
      {
        name = "raw-operation-credential-loading";
        assertion = lib.elem "token:${secretFile}" rawService.serviceConfig.LoadCredential;
        message = "the operations compatibility surface must load credentials per unit";
      }
      {
        name = "raw-operation-directories";
        assertion = rawService.serviceConfig.StateDirectory == "db-harbor-raw" && rawService.serviceConfig.RuntimeDirectory == "db-harbor-raw";
        message = "raw lifecycle operations must expose state and runtime directories";
      }
      {
        name = "data-directory-tmpfiles-rule";
        assertion = lib.elem "d /var/lib/db-harbor-data 0700 postgres postgres - -" eval.config.systemd.tmpfiles.rules;
        message = "dataDirectories must lower into a boot-time tmpfiles rule";
      }
      {
        name = "data-directory-activation-script";
        assertion = lib.hasInfix "install -d -o postgres -g postgres -m 0700 /var/lib/db-harbor-data" eval.config.system.activationScripts.db-harbor-establish-data-directories.text;
        message = "dataDirectories must lower into an activation script for live switches";
      }
      {
        name = "restore-unit-is-generated";
        assertion = restoreService.serviceConfig.ExecStart != null;
        message = "restore-lifecycle operations must generate an on-demand restore unit";
      }
      {
        name = "restore-unit-is-manual";
        assertion = (restoreService.wantedBy or []) == [];
        message = "the restore unit must never start during activation";
      }
      {
        name = "restore-targets-only-restore-operations";
        assertion =
          lib.hasInfix "--operation restore" restoreScript
          && lib.hasInfix "--confirm" restoreScript
          && !(lib.hasInfix "--operation ensure" restoreScript);
        message = "the restore command must select exactly the restore-lifecycle operations";
      }
    ];
  }
