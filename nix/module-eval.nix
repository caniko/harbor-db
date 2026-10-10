{
  lib,
  module,
  pkgs,
}: let
  secretValue = "module-eval-secret";
  secretFile = pkgs.writeText "harbor-db-module-eval-secret" secretValue;
  rawCommand = pkgs.writeShellScript "harbor-db-module-eval-raw" "exit 0";
  runner = pkgs.writeShellScriptBin "module-eval-runner" "exit 0";
  eval = import "${pkgs.path}/nixos/lib/eval-config.nix" {
    inherit (pkgs) system;
    modules = [
      module
      {
        system.stateVersion = "24.11";
        services.harbor-db.package = pkgs.writeShellScriptBin "harbor-db" "exit 0";
        services.harbor-db.operations.raw = {
          enable = true;
          command = "${rawCommand}";
          checkCommand = "${rawCommand}";
          credentials.token = secretFile;
          stateDirectory = "harbor-db-raw";
          runtimeDirectory = "harbor-db-raw";
        };
        services.harbor-db.projects.demo = {
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
            stateDirectory = "harbor-db-module-eval";
            runtimeDirectory = "harbor-db-module-eval";
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
              checkCommand = "test -f /var/lib/harbor-db-restore-healed";
            };
          };
        };
        services.harbor-db.dataDirectories = [
          {
            path = "/var/lib/harbor-db-data";
            user = "postgres";
            group = "postgres";
            mode = "0700";
          }
        ];
      }
    ];
  };
  service = eval.config.systemd.services.harbor-db-demo;
  checkService = eval.config.systemd.services.harbor-db-demo-check;
  restoreService = eval.config.systemd.services.harbor-db-demo-restore;
  rawService = eval.config.systemd.services.harbor-db-raw;
in
  (import ./eval-checks.nix {inherit pkgs;}).mkEvalCheck {
    name = "harbor-db-module-eval";
    resultMessage = "harbor-db generic lifecycle module keeps credentials out of plans";
    nativeBuildInputs = [pkgs.python3];
    runtimeScript = ''
      python3 - ${lib.escapeShellArgs [service.serviceConfig.ExecStart restoreService.serviceConfig.ExecStart secretValue (toString secretFile)]} "$out/assertions.json" <<'PY'
      import json
      from pathlib import Path
      import shlex
      import sys

      apply_path, restore_path, secret_value, secret_source, evidence_path = sys.argv[1:]

      def command(path):
          commands = []
          for line in Path(path).read_text().splitlines():
              tokens = shlex.split(line, comments=True)
              if tokens and tokens[0] == "exec":
                  commands.append(tokens[1:])
          if len(commands) != 1:
              raise RuntimeError(f"{path}: expected exactly one exec command")
          return commands[0]

      def values(tokens, option):
          result = []
          for index, token in enumerate(tokens):
              if token == option:
                  if index + 1 == len(tokens) or tokens[index + 1].startswith("--"):
                      raise RuntimeError(f"missing value for {option}")
                  result.append(tokens[index + 1])
          return result

      apply_command = command(apply_path)
      restore_command = command(restore_path)
      manifests = values(apply_command, "--manifest")
      if len(manifests) != 1 or apply_command[1:2] != ["apply"]:
          raise RuntimeError("apply command must select exactly one manifest")
      plan_text = Path(manifests[0]).read_text()
      plan = json.loads(plan_text)
      if type(plan.get("version")) is not int or plan["version"] != 1:
          raise RuntimeError("generated plan must have explicit version 1")

      passed = []

      def check(name, condition, message):
          if not condition:
              raise RuntimeError(f"{name}: {message}")
          passed.append({"name": name, "message": message})

      check(
          "credential-value-is-not-in-plan",
          secret_value not in plan_text and secret_source not in plan_text,
          "credential contents must not be serialized into the generated plan",
      )
      operations = {operation["id"]: operation for operation in plan["operations"]}
      credential_commands = [operations["ensure"][kind] for kind in ("apply", "check")]
      check(
          "credential-reference-is-a-file-path",
          "credential_environment" in plan_text and "token" in plan_text
          and all(spec["credential_environment"] == {"TOKEN_FILE": "token"}
                  and spec["credential_args"] == [] and spec["environment"] == {}
                  for spec in credential_commands),
          "plans must contain only the credential name reference",
      )
      selected = values(restore_command, "--operation")
      expected = [operation["id"] for operation in plan["operations"]
                  if operation["lifecycle"] == "restore"]
      check(
          "restore-targets-only-restore-operations",
          restore_command[1:2] == ["restore"]
          and values(restore_command, "--manifest") == manifests
          and selected == expected == ["restore"]
          and restore_command.count("--confirm") == 1
          and "ensure" not in selected,
          "the restore command must select exactly the restore-lifecycle operations",
      )
      evidence = Path(evidence_path)
      evidence.write_text(json.dumps(json.loads(evidence.read_text()) + passed) + "\n")
      PY
    '';
    assertions = [
      {
        name = "credential-load-is-per-operation-source";
        assertion = lib.elem "token:${secretFile}" service.serviceConfig.LoadCredential;
        message = "the operation credential source must be rendered as a systemd LoadCredential entry";
      }
      {
        name = "state-directory";
        assertion = service.serviceConfig.StateDirectory == ["harbor-db-module-eval"];
        message = "operation stateDirectory must reach systemd serviceConfig";
      }
      {
        name = "runtime-directory";
        assertion = service.serviceConfig.RuntimeDirectory == ["harbor-db-module-eval"];
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
        assertion = rawService.serviceConfig.StateDirectory == "harbor-db-raw" && rawService.serviceConfig.RuntimeDirectory == "harbor-db-raw";
        message = "raw lifecycle operations must expose state and runtime directories";
      }
      {
        name = "data-directory-tmpfiles-rule";
        assertion = lib.elem "d /var/lib/harbor-db-data 0700 postgres postgres - -" eval.config.systemd.tmpfiles.rules;
        message = "dataDirectories must lower into a boot-time tmpfiles rule";
      }
      {
        name = "data-directory-activation-script";
        assertion = lib.hasInfix "install -d -o postgres -g postgres -m 0700 /var/lib/harbor-db-data" eval.config.system.activationScripts.harbor-db-establish-data-directories.text;
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
    ];
  }
