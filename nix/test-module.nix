{
  lib,
  module,
  pkgs,
}:
pkgs.testers.nixosTest {
  name = "db-harbor-module-smoke";

  nodes.machine = {pkgs, ...}: let
    multiMigrator = pkgs.writeShellScriptBin "multi-migrator" ''
      set -eu
      case "$1" in
        apply-schema)
          mkdir -p /var/lib/db-harbor-demo
          touch /var/lib/db-harbor-demo/structured-schema
          echo structured-schema >> /var/lib/db-harbor-demo/structured-events
          ;;
        check-schema)
          test -f /var/lib/db-harbor-demo/structured-schema
          ;;
        apply-manual)
          mkdir -p /var/lib/db-harbor-demo
          touch /var/lib/db-harbor-demo/structured-manual
          echo structured-manual >> /var/lib/db-harbor-demo/structured-events
          ;;
        check-manual)
          true
          ;;
        *)
          echo "unknown command: $1" >&2
          exit 64
          ;;
      esac
    '';
    credentialMigrator = pkgs.writeShellScriptBin "credential-migrator" ''
      set -eu
      test -n "''${CREDENTIALS_DIRECTORY:-}"
      test -n "''${TOKEN_FILE:-}"
      test "$TOKEN_FILE" = "$CREDENTIALS_DIRECTORY/api-token"
      test "$(cat "$TOKEN_FILE")" = "lifecycle-secret"
      test -d "$STATE_DIRECTORY"
      test -d "$RUNTIME_DIRECTORY"
      case "$1" in
        ensure)
          if [ ! -e "$STATE_DIRECTORY/ensured" ]; then
            touch "$STATE_DIRECTORY/ensured"
            echo ensured >> "$STATE_DIRECTORY/events"
          fi
          ;;
        check)
          test -e "$STATE_DIRECTORY/ensured"
          ;;
        *)
          echo "unknown command: $1" >&2
          exit 64
          ;;
      esac
    '';
  in {
    imports = [module];

    environment.etc."db-harbor-lifecycle-secret".text = "lifecycle-secret\n";

    systemd.tmpfiles.rules = ["d /var/lib/db-harbor-demo 0775 postgres postgres -"];

    services.postgresql = {
      enable = true;
      ensureDatabases = ["db_harbor_project"];
      ensureUsers = [
        {name = "project_app";}
      ];
    };

    systemd.services.postgresql-setup.script = lib.mkAfter ''
      psql -d db_harbor_project -tAc "CREATE TABLE IF NOT EXISTS project_item (id bigserial primary key);"
    '';

    users.users.project_app = {
      isSystemUser = true;
      group = "project_app";
    };
    users.groups.project_app = {};

    systemd.services.project-app = {
      wantedBy = ["multi-user.target"];
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.writeShellScript "project-app" ''
          test -f /var/lib/db-harbor-demo/project-stamp
          echo project-app-started >> /var/lib/db-harbor-demo/project-events
        ''}";
      };
    };

    systemd.services.demo-app = {
      wantedBy = ["multi-user.target"];
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.writeShellScript "demo-app" ''
          test -f /var/lib/db-harbor-demo/stamp
          echo app-started >> /var/lib/db-harbor-demo/events
        ''}";
      };
    };

    systemd.services.multi-app = {
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.writeShellScript "multi-app" ''
          test -f /var/lib/db-harbor-demo/structured-schema
          test ! -e /var/lib/db-harbor-demo/structured-manual
          echo structured-app-started >> /var/lib/db-harbor-demo/structured-events
        ''}";
      };
    };

    systemd.services.lifecycle-app = {
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.writeShellScript "lifecycle-app" ''
          test -f /var/lib/db-harbor-lifecycle/ensured
        ''}";
      };
    };

    services.db-harbor.operations.demo = {
      enable = true;
      description = "Demo migration";
      command = "${pkgs.writeShellScript "demo-migration" ''
        mkdir -p /var/lib/db-harbor-demo
        touch /var/lib/db-harbor-demo/stamp
        echo migrated >> /var/lib/db-harbor-demo/events
      ''}";
      checkCommand = "${pkgs.writeShellScript "demo-migration-check" ''
        test -f /var/lib/db-harbor-demo/stamp
      ''}";
      beforeUnits = ["demo-app.service"];
      requiredByUnits = ["demo-app.service"];
      serviceConfig.ReadWritePaths = ["/var/lib/db-harbor-demo"];
    };

    services.db-harbor.projects.project = {
      enable = true;
      description = "Project migration";
      runner = {
        package = pkgs.writeShellScriptBin "project-migrator" ''
          set -eu
          case "$1" in
            apply)
              touch "$2/project-stamp"
              echo project-migrated >> "$2/project-events"
              ;;
            check)
              test -f "$2/project-stamp"
              ;;
            *)
              echo "unknown command: $1" >&2
              exit 64
              ;;
          esac
        '';
        executable = "bin/project-migrator";
        args = ["apply" "/var/lib/db-harbor-demo"];
        checkArgs = ["check" "/var/lib/db-harbor-demo"];
      };
      user = "postgres";
      group = "postgres";
      runtimeUnits = ["project-app.service"];
      postgres = {
        enable = true;
        databaseUrl = "postgres:///db_harbor_project?host=/run/postgresql";
        setupUnits = ["postgresql-setup.service"];
        grants = {
          enable = true;
          runtimeRole = "project_app";
        };
      };
      serviceConfig.ReadWritePaths = ["/var/lib/db-harbor-demo"];
    };

    services.db-harbor.projects.structured = {
      enable = true;
      description = "Structured migration plan";
      operations = {
        schema = {
          enable = true;
          backend = "postgres";
          phase = "schema";
          runner = {
            package = multiMigrator;
            executable = "bin/multi-migrator";
            args = ["apply-schema"];
            checkArgs = ["check-schema"];
          };
        };
        manual = {
          enable = true;
          backend = "clickhouse";
          phase = "operational";
          safety = "operator_confirmed";
          runner = {
            package = multiMigrator;
            executable = "bin/multi-migrator";
            args = ["apply-manual"];
            checkArgs = ["check-manual"];
          };
          dependsOn = ["schema"];
        };
      };
      runtimeUnits = ["multi-app.service"];
      serviceConfig.ReadWritePaths = ["/var/lib/db-harbor-demo"];
    };

    services.db-harbor.projects.lifecycle = {
      enable = true;
      description = "Generic credential lifecycle";
      operations.ensure = {
        enable = true;
        kind = "credential";
        lifecycle = "ensure";
        credentials.api-token = "/etc/db-harbor-lifecycle-secret";
        runner = {
          package = credentialMigrator;
          executable = "bin/credential-migrator";
          args = ["ensure"];
          checkArgs = ["check"];
          credentialEnvironment.TOKEN_FILE = "api-token";
        };
        stateDirectory = "db-harbor-lifecycle";
        runtimeDirectory = "db-harbor-lifecycle";
        runtimeUnits = ["lifecycle-app.service"];
      };
    };
  };

  testScript = ''
    machine.wait_until_succeeds("systemctl show db-harbor-demo.service -p Result --value | grep -Fx success")
    machine.wait_until_succeeds("systemctl show demo-app.service -p Result --value | grep -Fx success")
    machine.wait_for_unit("postgresql-setup.service")
    machine.succeed("systemctl start project-app.service")
    machine.wait_until_succeeds("systemctl show db-harbor-project.service -p Result --value | grep -Fx success")
    machine.wait_until_succeeds("systemctl show project-app.service -p Result --value | grep -Fx success")
    machine.succeed("systemctl start multi-app.service")
    machine.wait_until_succeeds("systemctl show db-harbor-structured.service -p Result --value | grep -Fx success")
    machine.wait_until_succeeds("systemctl show multi-app.service -p Result --value | grep -Fx success")
    machine.succeed("test -f /var/lib/db-harbor-demo/structured-schema")
    machine.succeed("test ! -e /var/lib/db-harbor-demo/structured-manual")
    machine.succeed("systemctl start db-harbor-structured-check.service")
    machine.succeed("test -f /var/lib/db-harbor-demo/stamp")
    machine.succeed("test -f /var/lib/db-harbor-demo/project-stamp")
    machine.succeed("grep -n migrated /var/lib/db-harbor-demo/events")
    machine.succeed("grep -n project-migrated /var/lib/db-harbor-demo/project-events")
    machine.succeed("grep -n app-started /var/lib/db-harbor-demo/events")
    machine.succeed("grep -n project-app-started /var/lib/db-harbor-demo/project-events")
    machine.succeed("systemctl start db-harbor-demo.service")
    machine.succeed("systemctl start db-harbor-demo-check.service")
    machine.succeed("systemctl start db-harbor-project.service")
    machine.succeed("systemctl start db-harbor-project-check.service")
    machine.succeed("test $(grep -c migrated /var/lib/db-harbor-demo/events) -ge 2")
    machine.succeed("sudo -u project_app psql -d db_harbor_project -tAc 'INSERT INTO project_item DEFAULT VALUES RETURNING id;' | grep -Fx 1")
    machine.wait_until_succeeds("systemctl show db-harbor-lifecycle.service -p Result --value | grep -Fx success")
    machine.wait_until_succeeds("systemctl show lifecycle-app.service -p Result --value | grep -Fx success")
    machine.succeed("systemctl start db-harbor-lifecycle.service")
    machine.succeed("systemctl start db-harbor-lifecycle-check.service")
    machine.succeed("test $(grep -c ensured /var/lib/db-harbor-lifecycle/events) -eq 1")
    machine.succeed("! grep -R -n lifecycle-secret /nix/store/*db-harbor-lifecycle-plan.json")
    machine.succeed("! systemctl show db-harbor-lifecycle.service | grep -F lifecycle-secret")
  '';
}
