{
  lib,
  module,
  pkgs,
}:
pkgs.testers.runNixOSTest {
  extraDriverArgs = ["--junit-xml" "junit.xml"];
  name = "harbor-db-module-smoke";

  nodes.machine = {pkgs, ...}: let
    multiMigrator = pkgs.writeShellScriptBin "multi-migrator" ''
      set -eu
      case "$1" in
        apply-schema)
          mkdir -p /var/lib/harbor-db-demo
          touch /var/lib/harbor-db-demo/structured-schema
          echo structured-schema >> /var/lib/harbor-db-demo/structured-events
          ;;
        check-schema)
          test -f /var/lib/harbor-db-demo/structured-schema
          ;;
        apply-manual)
          mkdir -p /var/lib/harbor-db-demo
          touch /var/lib/harbor-db-demo/structured-manual
          echo structured-manual >> /var/lib/harbor-db-demo/structured-events
          ;;
        check-manual)
          true
          ;;
        apply-restore)
          mkdir -p /var/lib/harbor-db-demo
          touch /var/lib/harbor-db-demo/structured-restored
          echo structured-restored >> /var/lib/harbor-db-demo/structured-events
          ;;
        check-restore)
          test -f /var/lib/harbor-db-demo/structured-restored || exit 2
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
    rehearsalMigrator = pkgs.writeShellScriptBin "rehearsal-migrator" ''
      set -eu
      case "$1" in
        migrate)
          psql -v ON_ERROR_STOP=1 -c 'CREATE TABLE IF NOT EXISTS rehearsed_item (id integer)'
          ;;
        check)
          psql -v ON_ERROR_STOP=1 -tAc 'select 1 from rehearsed_item limit 0' > /dev/null
          ;;
        *)
          echo "unknown command: $1" >&2
          exit 64
          ;;
      esac
    '';
  in {
    imports = [module];

    environment.etc."harbor-db-lifecycle-secret".text = "lifecycle-secret\n";

    systemd.tmpfiles.rules = [
      "d /var/lib/harbor-db-demo 0775 postgres postgres -"
      "d /var/lib/harbor-db-rehearsal-work 0700 rehearsal_app rehearsal_app -"
      "d /var/lib/harbor-db-rehearsal-reader-work 0700 rehearsal_reader rehearsal_reader -"
      "d /var/lib/harbor-db-rehearsal-reader-project 0700 rehearsal_app rehearsal_app -"
    ];

    services.postgresql = {
      enable = true;
      ensureDatabases = ["db_harbor_project" "db_rehearsal"];
      ensureUsers = [
        {name = "project_app";}
        {name = "rehearsal_app";}
        {name = "rehearsal_reader";}
      ];
    };

    systemd.services.postgresql-setup.script = lib.mkAfter ''
      psql -d db_harbor_project -tAc "CREATE TABLE IF NOT EXISTS project_item (id bigserial primary key);"
      psql -d db_rehearsal -tAc "CREATE TABLE IF NOT EXISTS rehearsal_item (id integer);"
      psql -d db_rehearsal -tAc "INSERT INTO rehearsal_item VALUES (1), (2);"
      psql -d db_rehearsal -tAc "GRANT pg_read_all_data TO rehearsal_app, rehearsal_reader;"
    '';

    users.users.rehearsal_app = {
      isSystemUser = true;
      group = "rehearsal_app";
    };
    users.groups.rehearsal_app = {};
    users.users.rehearsal_reader = {
      isSystemUser = true;
      group = "rehearsal_reader";
    };
    users.groups.rehearsal_reader = {};

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
          test -f /var/lib/harbor-db-demo/project-stamp
          echo project-app-started >> /var/lib/harbor-db-demo/project-events
        ''}";
      };
    };

    systemd.services.demo-app = {
      wantedBy = ["multi-user.target"];
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.writeShellScript "demo-app" ''
          test -f /var/lib/harbor-db-demo/stamp
          echo app-started >> /var/lib/harbor-db-demo/events
        ''}";
      };
    };

    systemd.services.multi-app = {
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.writeShellScript "multi-app" ''
          test -f /var/lib/harbor-db-demo/structured-schema
          test ! -e /var/lib/harbor-db-demo/structured-manual
          echo structured-app-started >> /var/lib/harbor-db-demo/structured-events
        ''}";
      };
    };

    systemd.services.lifecycle-app = {
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.writeShellScript "lifecycle-app" ''
          test -f /var/lib/harbor-db-lifecycle/ensured
        ''}";
      };
    };

    services.harbor-db.operations.demo = {
      enable = true;
      description = "Demo migration";
      command = "${pkgs.writeShellScript "demo-migration" ''
        mkdir -p /var/lib/harbor-db-demo
        touch /var/lib/harbor-db-demo/stamp
        echo migrated >> /var/lib/harbor-db-demo/events
      ''}";
      checkCommand = "${pkgs.writeShellScript "demo-migration-check" ''
        test -f /var/lib/harbor-db-demo/stamp
      ''}";
      beforeUnits = ["demo-app.service"];
      requiredByUnits = ["demo-app.service"];
      serviceConfig.ReadWritePaths = ["/var/lib/harbor-db-demo"];
    };

    services.harbor-db.projects.project = {
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
        args = ["apply" "/var/lib/harbor-db-demo"];
        checkArgs = ["check" "/var/lib/harbor-db-demo"];
      };
      user = "postgres";
      group = "postgres";
      runtimeUnits = ["project-app.service"];
      postgres = {
        enable = true;
        database = "db_harbor_project";
        databaseUrl = "postgres:///db_harbor_project?host=/run/postgresql";
        superuserExtensions = ["pg_trgm"];
        setupUnits = ["postgresql-setup.service"];
        grants = {
          enable = true;
          runtimeRole = "project_app";
        };
      };
      serviceConfig.ReadWritePaths = ["/var/lib/harbor-db-demo"];
    };

    # The privileged pre-start inherits the unit environment, where a project
    # PGUSER names a non-superuser role. It must still connect as the
    # PostgreSQL superuser.
    services.harbor-db.projects.superuser-gate = {
      enable = true;
      description = "Privileged pre-start with the project PGUSER";
      user = "postgres";
      environment.PGUSER = "project_app";
      runner.command = "true";
      postgres = {
        enable = true;
        database = "db_harbor_project";
        superuserExtensions = ["fuzzystrmatch"];
      };
    };

    services.harbor-db.projects.rehearsal = {
      enable = true;
      description = "Documented-defaults rehearsal";
      # The documented default: the project user is the unit account, and the
      # restored owner falls back to the same name.
      user = "rehearsal_app";
      stateDirectory = "harbor-db-rehearsal-project";
      runner = {
        package = rehearsalMigrator;
        executable = "bin/rehearsal-migrator";
        args = ["migrate"];
        checkArgs = ["check"];
      };
      postgres = {
        enable = true;
        database = "db_rehearsal";
      };
      rehearsal = {
        enable = true;
        workDirectory = "/var/lib/harbor-db-rehearsal-work";
        probes = [
          {
            name = "items";
            sql = "select count(*) from rehearsal_item";
          }
        ];
      };
    };

    services.harbor-db.projects.rehearsal-reader = {
      enable = true;
      description = "Rehearsal as a distinct user";
      user = "rehearsal_app";
      stateDirectory = "harbor-db-rehearsal-reader-project";
      runner = {
        package = rehearsalMigrator;
        executable = "bin/rehearsal-migrator";
        args = ["migrate"];
        checkArgs = ["check"];
      };
      postgres = {
        enable = true;
        database = "db_rehearsal";
      };
      rehearsal = {
        enable = true;
        user = "rehearsal_reader";
        owner = "rehearsal_app";
        workDirectory = "/var/lib/harbor-db-rehearsal-reader-work";
        probes = [
          {
            name = "items";
            sql = "select count(*) from rehearsal_item";
          }
        ];
      };
    };

    services.harbor-db.projects.structured = {
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
        restore = {
          enable = true;
          lifecycle = "restore";
          safety = "operator_confirmed";
          phase = "operational";
          runner = {
            package = multiMigrator;
            executable = "bin/multi-migrator";
            args = ["apply-restore"];
            checkArgs = ["check-restore"];
          };
        };
      };
      runtimeUnits = ["multi-app.service"];
      serviceConfig.ReadWritePaths = ["/var/lib/harbor-db-demo"];
    };

    services.harbor-db.projects.lifecycle = {
      enable = true;
      description = "Generic credential lifecycle";
      operations.ensure = {
        enable = true;
        kind = "credential";
        lifecycle = "ensure";
        credentials.api-token = "/etc/harbor-db-lifecycle-secret";
        runner = {
          package = credentialMigrator;
          executable = "bin/credential-migrator";
          args = ["ensure"];
          checkArgs = ["check"];
          credentialEnvironment.TOKEN_FILE = "api-token";
        };
        stateDirectory = "harbor-db-lifecycle";
        runtimeDirectory = "harbor-db-lifecycle";
        runtimeUnits = ["lifecycle-app.service"];
      };
    };
  };

  testScript = ''
    machine.wait_until_succeeds("systemctl show harbor-db-demo.service -p Result --value | grep -Fx success")
    machine.wait_until_succeeds("systemctl show demo-app.service -p Result --value | grep -Fx success")
    machine.wait_for_unit("postgresql-setup.service")
    machine.succeed("systemctl start project-app.service")
    machine.wait_until_succeeds("systemctl show harbor-db-project.service -p Result --value | grep -Fx success")
    machine.wait_until_succeeds("systemctl show project-app.service -p Result --value | grep -Fx success")
    machine.succeed("systemctl start multi-app.service")
    machine.wait_until_succeeds("systemctl show harbor-db-structured.service -p Result --value | grep -Fx success")
    machine.wait_until_succeeds("systemctl show multi-app.service -p Result --value | grep -Fx success")
    machine.succeed("test -f /var/lib/harbor-db-demo/structured-schema")
    machine.succeed("test ! -e /var/lib/harbor-db-demo/structured-manual")
    machine.succeed("test ! -e /var/lib/harbor-db-demo/structured-restored")
    machine.succeed("systemctl start harbor-db-structured-restore.service")
    machine.wait_until_succeeds("systemctl show harbor-db-structured-restore.service -p Result --value | grep -Fx success")
    machine.succeed("test -f /var/lib/harbor-db-demo/structured-restored")
    machine.succeed("grep -n structured-restored /var/lib/harbor-db-demo/structured-events")
    machine.succeed("systemctl start harbor-db-structured-check.service")
    machine.succeed("test -f /var/lib/harbor-db-demo/stamp")
    machine.succeed("test -f /var/lib/harbor-db-demo/project-stamp")
    machine.succeed("grep -n migrated /var/lib/harbor-db-demo/events")
    machine.succeed("grep -n project-migrated /var/lib/harbor-db-demo/project-events")
    machine.succeed("grep -n app-started /var/lib/harbor-db-demo/events")
    machine.succeed("grep -n project-app-started /var/lib/harbor-db-demo/project-events")
    machine.succeed("systemctl start harbor-db-demo.service")
    machine.succeed("systemctl start harbor-db-demo-check.service")
    machine.succeed("systemctl start harbor-db-project.service")
    machine.succeed("systemctl start harbor-db-project-check.service")
    machine.succeed("test $(grep -c migrated /var/lib/harbor-db-demo/events) -ge 2")
    machine.succeed("sudo -u project_app psql -d db_harbor_project -tAc 'INSERT INTO project_item DEFAULT VALUES RETURNING id;' | grep -Fx 1")
    machine.succeed("sudo -u postgres psql -d db_harbor_project -tAc \"select count(*) from pg_extension where extname = 'pg_trgm'\" | grep -Fx 1")
    machine.succeed("systemctl start harbor-db-superuser-gate.service")
    machine.succeed("sudo -u postgres psql -d db_harbor_project -tAc \"select count(*) from pg_extension where extname = 'fuzzystrmatch'\" | grep -Fx 1")
    # Documented rehearsal defaults: the project user is both the unit account
    # and the restored owner. Only a distinct bootstrap superuser in the
    # disposable cluster makes that restorable.
    machine.succeed("systemctl start harbor-db-rehearsal-rehearse.service")
    machine.succeed("grep -F '\"status\": \"passed\"' /var/lib/harbor-db-rehearsal-rehearse/*.json")
    machine.succeed("test \"$(stat -c %U /var/lib/harbor-db-rehearsal-rehearse)\" = rehearsal_app")
    # A distinct rehearsal.user must not take over the project state directory.
    machine.succeed("test \"$(stat -c %U /var/lib/harbor-db-rehearsal-reader-project)\" = rehearsal_app")
    machine.succeed("systemctl start harbor-db-rehearsal-reader-rehearse.service")
    machine.succeed("grep -F '\"status\": \"passed\"' /var/lib/harbor-db-rehearsal-reader-rehearse/*.json")
    machine.succeed("test \"$(stat -c %U /var/lib/harbor-db-rehearsal-reader-project)\" = rehearsal_app")
    machine.succeed("test \"$(stat -c %U /var/lib/harbor-db-rehearsal-reader-rehearse)\" = rehearsal_reader")
    machine.wait_until_succeeds("systemctl show harbor-db-lifecycle.service -p Result --value | grep -Fx success")
    machine.wait_until_succeeds("systemctl show lifecycle-app.service -p Result --value | grep -Fx success")
    machine.succeed("systemctl start harbor-db-lifecycle.service")
    machine.succeed("systemctl start harbor-db-lifecycle-check.service")
    machine.succeed("test $(grep -c ensured /var/lib/harbor-db-lifecycle/events) -eq 1")
    machine.succeed("! grep -R -n lifecycle-secret /nix/store/*harbor-db-lifecycle-plan.json")
    machine.succeed("! systemctl show harbor-db-lifecycle.service | grep -F lifecycle-secret")
  '';
}
