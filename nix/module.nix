{
  config,
  lib,
  pkgs,
  ...
}: let
  inherit (lib) mkEnableOption mkIf mkMerge mkOption optionalAttrs types;

  cfg = config.services.db-harbor;

  sqlIdentifier = value: "\"" + builtins.replaceStrings ["\""] ["\"\""] value + "\"";

  credentialEntries = credentials:
    lib.mapAttrsToList (name: path: "${name}:${path}") credentials;

  migrationType = types.submodule ({name, ...}: {
    options = {
      enable = mkEnableOption "db-harbor database operation ${name}";

      kind = mkOption {
        type = types.enum ["generic" "database" "credential"];
        default = "generic";
        description = "Broad kind of lifecycle operation.";
      };

      lifecycle = mkOption {
        type = types.enum ["ensure" "reconcile"];
        default = "ensure";
        description = "Idempotent lifecycle contract for this operation.";
      };

      description = mkOption {
        type = types.str;
        default = "${name} database operation";
        description = "Human-readable description for the database-operation unit.";
      };

      command = mkOption {
        type = types.str;
        description = "Full command that applies this database operation idempotently.";
      };

      checkCommand = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional read-only command that reports pending or incompatible database state.";
      };

      user = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "User to run the database operation as.";
      };

      group = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Group to run the database operation as.";
      };

      environment = mkOption {
        type = types.attrsOf types.str;
        default = {};
        description = "Environment variables for database-operation units.";
      };

      path = mkOption {
        type = types.listOf types.package;
        default = [];
        description = "Packages added to PATH for database-operation units.";
      };

      loadCredentials = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "systemd LoadCredential entries for database-operation units.";
      };

      credentials = mkOption {
        type = types.attrsOf types.path;
        default = {};
        description = "Credential name to source-file mapping; contents stay outside the plan and process environment.";
      };

      stateDirectory = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional systemd StateDirectory for this operation.";
      };

      runtimeDirectory = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional systemd RuntimeDirectory for this operation.";
      };

      after = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Units this database-operation unit should start after.";
      };

      requires = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Units required by this database-operation unit.";
      };

      wants = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Units wanted by this database-operation unit.";
      };

      beforeUnits = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Application units ordered after this database-operation unit.";
      };

      requiredByUnits = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Application units that require this database-operation unit.";
      };

      serviceConfig = mkOption {
        type = types.attrs;
        default = {};
        description = "Additional or overriding systemd serviceConfig for database-operation units.";
      };
    };
  });

  runnerType = types.submodule {
    options = {
      package = mkOption {
        type = types.nullOr types.package;
        default = null;
        description = "Optional package containing the migration executable.";
      };

      executable = mkOption {
        type = types.nullOr types.str;
        default = null;
        example = "bin/my-app";
        description = "Executable path, relative to package when package is set or absolute otherwise.";
      };

      args = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Arguments passed to the migration executable.";
      };

      checkArgs = mkOption {
        type = types.nullOr (types.listOf types.str);
        default = null;
        description = "Arguments passed to the migration executable for read-only readiness checks.";
      };

      credentialArgs = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Credential names appended as file paths to the executable arguments.";
      };

      credentialEnvironment = mkOption {
        type = types.attrsOf types.str;
        default = {};
        description = "Environment names mapped to credential names; values are runtime file paths, never secret contents.";
      };

      command = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Full migration command. Overrides package/executable/args when set.";
      };

      checkCommand = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Full read-only readiness check command. Overrides package/executable/checkArgs when set.";
      };
    };
  };

  operationType = types.submodule ({name, ...}: {
    options = {
      enable = mkEnableOption "db-harbor lifecycle operation ${name}";

      kind = mkOption {
        type = types.enum ["generic" "database" "credential"];
        default = "generic";
        description = "Broad kind of lifecycle operation.";
      };

      lifecycle = mkOption {
        type = types.enum ["ensure" "reconcile"];
        default = "ensure";
        description = "Idempotent lifecycle contract for this operation.";
      };

      backend = mkOption {
        type = types.enum ["generic" "postgres" "clickhouse"];
        default = "generic";
        description = "Database family owned by this operation.";
      };

      phase = mkOption {
        type = types.enum ["schema" "backfill" "operational"];
        default = "schema";
        description = "Lifecycle phase used for reporting and deployment policy.";
      };

      safety = mkOption {
        type = types.enum ["automatic" "operator_confirmed"];
        default = "automatic";
        description = "Whether apply runs this operation during normal activation.";
      };

      runner = mkOption {
        type = runnerType;
        default = {};
        description = "Structured apply/check command for this operation.";
      };

      dependsOn = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Operation identifiers that must run first.";
      };

      credentials = mkOption {
        type = types.attrsOf types.path;
        default = {};
        description = "Credential name to source-file mapping; contents stay outside the plan and process environment.";
      };

      loadCredentials = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Legacy systemd LoadCredential entries for this operation.";
      };

      stateDirectory = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional systemd StateDirectory for this operation.";
      };

      runtimeDirectory = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional systemd RuntimeDirectory for this operation.";
      };

      after = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Units this lifecycle operation should start after.";
      };

      requires = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Units required by this lifecycle operation.";
      };

      wants = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Units wanted by this lifecycle operation.";
      };

      runtimeUnits = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Runtime units gated on this lifecycle operation.";
      };

      beforeUnits = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Units ordered after this lifecycle operation.";
      };

      requiredByUnits = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Units that require this lifecycle operation.";
      };

      serviceConfig = mkOption {
        type = types.attrs;
        default = {};
        description = "Additional systemd serviceConfig for this operation.";
      };
    };
  });

  grantType = types.submodule {
    options = {
      enable = mkEnableOption "post-migration PostgreSQL grants";

      runtimeRole = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "PostgreSQL role used by runtime services after migrations have run.";
      };

      schema = mkOption {
        type = types.str;
        default = "public";
        description = "PostgreSQL schema to grant runtime privileges on.";
      };

      tablePrivileges = mkOption {
        type = types.listOf types.str;
        default = ["SELECT" "INSERT" "UPDATE" "DELETE"];
        description = "Table privileges granted to runtimeRole on all current tables in schema.";
      };

      sequencePrivileges = mkOption {
        type = types.listOf types.str;
        default = ["USAGE" "SELECT" "UPDATE"];
        description = "Sequence privileges granted to runtimeRole on all current sequences in schema.";
      };
    };
  };

  postgresType = types.submodule {
    options = {
      enable = mkEnableOption "PostgreSQL migration helpers";

      databaseUrl = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "PostgreSQL connection URL used by generated grant commands.";
      };

      setupUnits = mkOption {
        type = types.listOf types.str;
        default = [];
        example = ["postgresql-setup.service"];
        description = "PostgreSQL setup units the migration must run after and require.";
      };

      package = mkOption {
        type = types.package;
        default =
          if config.services.postgresql.enable or false
          then config.services.postgresql.package
          else pkgs.postgresql;
        defaultText = "config.services.postgresql.package or pkgs.postgresql";
        description = "PostgreSQL package providing psql for generated helper commands.";
      };

      grants = mkOption {
        type = grantType;
        default = {};
        description = "Optional grants applied after the migration command succeeds.";
      };
    };
  };

  projectType = types.submodule ({name, ...}: {
    options = {
      enable = mkEnableOption "db-harbor project ${name}";

      description = mkOption {
        type = types.str;
        default = "${name} lifecycle operations";
        description = "Human-readable description for the generated lifecycle unit.";
      };

      runner = mkOption {
        type = runnerType;
        default = {};
        description = "Command runner used to apply and optionally check database state.";
      };

      operations = mkOption {
        type = types.attrsOf operationType;
        default = {};
        description = "Structured lifecycle operations. The runner shorthand becomes the default operation.";
      };

      user = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "User to run the migration command as.";
      };

      group = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Group to run the migration command as.";
      };

      environment = mkOption {
        type = types.attrsOf types.str;
        default = {};
        description = "Environment variables for generated migration units.";
      };

      path = mkOption {
        type = types.listOf types.package;
        default = [];
        description = "Packages added to PATH for generated migration units.";
      };

      loadCredentials = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Legacy systemd LoadCredential entries for generated lifecycle units.";
      };

      credentials = mkOption {
        type = types.attrsOf types.path;
        default = {};
        description = "Credential name to source-file mapping for generated lifecycle units.";
      };

      stateDirectory = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional systemd StateDirectory for generated lifecycle units.";
      };

      runtimeDirectory = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Optional systemd RuntimeDirectory for generated lifecycle units.";
      };

      after = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Additional units the generated migration unit should start after.";
      };

      requires = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Additional units required by the generated migration unit.";
      };

      wants = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Units wanted by the generated migration unit.";
      };

      runtimeUnits = mkOption {
        type = types.listOf types.str;
        default = [];
        description = "Runtime units ordered after and requiring the generated migration unit.";
      };

      serviceConfig = mkOption {
        type = types.attrs;
        default = {};
        description = "Additional or overriding systemd serviceConfig for generated lifecycle units.";
      };

      postgres = mkOption {
        type = postgresType;
        default = {};
        description = "PostgreSQL-specific ordering and post-operation helpers.";
      };
    };
  });

  enabledMigrations = lib.filterAttrs (_: migration: migration.enable) cfg.migrations;
  enabledProjects = lib.filterAttrs (_: project: project.enable) cfg.projects;

  runnerConfigured = runner: runner.command != null || runner.executable != null;

  projectOperations = project:
    (lib.optionalAttrs (runnerConfigured project.runner) {
      default = {
        enable = true;
        kind = "database";
        lifecycle = "ensure";
        backend =
          if project.postgres.enable
          then "postgres"
          else "generic";
        phase = "schema";
        safety = "automatic";
        runner = project.runner;
        dependsOn = [];
        credentials = {};
        loadCredentials = [];
        stateDirectory = null;
        runtimeDirectory = null;
        after = [];
        requires = [];
        wants = [];
        runtimeUnits = [];
        beforeUnits = [];
        requiredByUnits = [];
        serviceConfig = {};
      };
    })
    // project.operations;

  enabledOperations = project:
    lib.filterAttrs (_: operation: operation.enable) (projectOperations project);

  projectHasChecks = project:
    lib.any
    (operation: operation.runner.checkCommand != null || operation.runner.checkArgs != null)
    (lib.attrValues (enabledOperations project));

  operationValues = project: lib.attrValues (enabledOperations project);

  projectCredentialSources = project:
    lib.foldl'
    (sources: operation: sources // operation.credentials)
    project.credentials
    (operationValues project);

  projectUnitList = field: project:
    lib.unique ((project.${field} or []) ++ lib.concatMap (operation: operation.${field}) (operationValues project));

  projectDirectoryConfig = project: let
    stateDirectories = lib.unique (
      lib.optional (project.stateDirectory != null) project.stateDirectory
      ++ lib.concatMap (operation: lib.optional (operation.stateDirectory != null) operation.stateDirectory) (operationValues project)
    );
    runtimeDirectories = lib.unique (
      lib.optional (project.runtimeDirectory != null) project.runtimeDirectory
      ++ lib.concatMap (operation: lib.optional (operation.runtimeDirectory != null) operation.runtimeDirectory) (operationValues project)
    );
  in
    (lib.optionalAttrs (stateDirectories != []) {StateDirectory = stateDirectories;})
    // (lib.optionalAttrs (runtimeDirectories != []) {RuntimeDirectory = runtimeDirectories;});

  sqlLiteral = value: "'" + builtins.replaceStrings ["'"] ["''"] value + "'";

  fullCommandSpec = name: suffix: command: let
    script = pkgs.writeShellScript "db-harbor-${name}-${suffix}" ''
      set -eu
      ${command}
    '';
  in {
    program = "${script}";
    args = [];
    environment = {};
  };

  withCredentialReferences = runner: {
    credential_args = runner.credentialArgs;
    credential_environment = runner.credentialEnvironment;
  };

  runnerCommandSpec = name: suffix: runner: args:
    if runner.command != null
    then (fullCommandSpec name suffix runner.command) // (withCredentialReferences runner)
    else
      {
        program =
          if runner.package != null
          then "${runner.package}/${runner.executable}"
          else runner.executable;
        inherit args;
        environment = {};
      }
      // (withCredentialReferences runner);

  runnerCheckSpec = name: runner:
    if runner.checkCommand != null
    then (fullCommandSpec name "check" runner.checkCommand) // (withCredentialReferences runner)
    else if runner.checkArgs != null
    then runnerCommandSpec name "check" (runner // {command = null;}) runner.checkArgs
    else null;

  grantApplySpec = name: project: {
    program = "${project.postgres.package}/bin/psql";
    args = [
      project.postgres.databaseUrl
      "-v"
      "ON_ERROR_STOP=1"
      "-c"
      "GRANT USAGE ON SCHEMA ${sqlIdentifier project.postgres.grants.schema} TO ${sqlIdentifier project.postgres.grants.runtimeRole};"
      "-c"
      "GRANT ${lib.concatStringsSep ", " project.postgres.grants.tablePrivileges} ON ALL TABLES IN SCHEMA ${sqlIdentifier project.postgres.grants.schema} TO ${sqlIdentifier project.postgres.grants.runtimeRole};"
      "-c"
      "GRANT ${lib.concatStringsSep ", " project.postgres.grants.sequencePrivileges} ON ALL SEQUENCES IN SCHEMA ${sqlIdentifier project.postgres.grants.schema} TO ${sqlIdentifier project.postgres.grants.runtimeRole};"
    ];
    environment = {};
  };

  grantCheckSpec = project: let
    grants = project.postgres.grants;
    tableChecks = map (privilege: "COALESCE((SELECT bool_and(has_table_privilege(${sqlLiteral grants.runtimeRole}, format('%I.%I', schemaname, tablename), ${sqlLiteral privilege})) FROM pg_tables WHERE schemaname = ${sqlLiteral grants.schema}), true)") grants.tablePrivileges;
    sequenceChecks = map (privilege: "COALESCE((SELECT bool_and(has_sequence_privilege(${sqlLiteral grants.runtimeRole}, format('%I.%I', sequence_schema, sequence_name), ${sqlLiteral privilege})) FROM information_schema.sequences WHERE sequence_schema = ${sqlLiteral grants.schema}), true)") grants.sequencePrivileges;
    checks =
      [
        "has_schema_privilege(${sqlLiteral grants.runtimeRole}, ${sqlLiteral grants.schema}, 'USAGE')"
      ]
      ++ tableChecks
      ++ sequenceChecks;
  in {
    program = "${project.postgres.package}/bin/psql";
    args = [
      project.postgres.databaseUrl
      "-v"
      "ON_ERROR_STOP=1"
      "-tAc"
      "SELECT ${lib.concatStringsSep " AND " checks};"
    ];
    environment = {};
  };

  operationToPlan = name: operation: {
    id = name;
    kind = operation.kind;
    lifecycle = operation.lifecycle;
    backend = operation.backend;
    phase = operation.phase;
    safety = operation.safety;
    apply = runnerCommandSpec name "apply" operation.runner operation.runner.args;
    check = runnerCheckSpec name operation.runner;
    depends_on = operation.dependsOn;
  };

  projectPlan = name: project: let
    operations = enabledOperations project;
    baseOperations = lib.mapAttrsToList operationToPlan operations;
    grantOperation = lib.optional (project.postgres.enable && project.postgres.grants.enable) {
      id = "postgres-grants";
      kind = "database";
      lifecycle = "ensure";
      backend = "postgres";
      phase = "schema";
      safety = "automatic";
      apply = grantApplySpec name project;
      check = grantCheckSpec project;
      depends_on = ["default"];
    };
  in
    pkgs.writeText "db-harbor-${name}-plan.json" (builtins.toJSON {
      version = 1;
      inherit name;
      operations = baseOperations ++ grantOperation;
    });

  projectApplyCommand = name: project:
    assert cfg.package != null; let
      command = lib.escapeShellArgs (map toString [
        "${toString cfg.package}/bin/db-harbor"
        "apply"
        "--manifest"
        (projectPlan name project)
      ]);
    in "${pkgs.writeShellScript "db-harbor-${name}-apply" ''
      set -eu
      exec ${command}
    ''}";

  projectCheckCommand = name: project:
    assert cfg.package != null; let
      command = lib.escapeShellArgs (map toString [
        "${toString cfg.package}/bin/db-harbor"
        "check"
        "--manifest"
        (projectPlan name project)
      ]);
    in "${pkgs.writeShellScript "db-harbor-${name}-check" ''
      set -eu
      exec ${command}
    ''}";

  projectToMigration = name: project: {
    enable = true;
    inherit (project) description user group environment path;
    loadCredentials = project.loadCredentials ++ lib.concatMap (operation: operation.loadCredentials) (operationValues project);
    credentials = projectCredentialSources project;
    command = projectApplyCommand name project;
    checkCommand =
      if projectHasChecks project
      then projectCheckCommand name project
      else null;
    after = project.postgres.setupUnits ++ projectUnitList "after" project;
    requires = project.postgres.setupUnits ++ projectUnitList "requires" project;
    wants = projectUnitList "wants" project;
    beforeUnits = projectUnitList "beforeUnits" project ++ projectUnitList "runtimeUnits" project;
    requiredByUnits = projectUnitList "requiredByUnits" project ++ projectUnitList "runtimeUnits" project;
    serviceConfig =
      projectDirectoryConfig project
      // lib.foldl' (serviceConfig: operation: serviceConfig // operation.serviceConfig) {} (operationValues project)
      // project.serviceConfig;
  };

  serviceConfigFor = migration:
    {
      Type = "oneshot";
      NoNewPrivileges = true;
      PrivateTmp = true;
      ProtectHome = true;
      ProtectSystem = "strict";
    }
    // optionalAttrs (migration.user != null) {
      User = migration.user;
    }
    // optionalAttrs (migration.group != null) {
      Group = migration.group;
    }
    // optionalAttrs (migration.loadCredentials != [] || migration.credentials != {}) {
      LoadCredential = migration.loadCredentials ++ credentialEntries migration.credentials;
    }
    // optionalAttrs (migration.stateDirectory != null) {
      StateDirectory = migration.stateDirectory;
    }
    // optionalAttrs (migration.runtimeDirectory != null) {
      RuntimeDirectory = migration.runtimeDirectory;
    }
    // migration.serviceConfig;

  migrationService = name: migration: {
    description = migration.description;
    inherit (migration) environment path;
    after = migration.after;
    requires = migration.requires;
    wants = migration.wants;
    before = migration.beforeUnits;
    requiredBy = migration.requiredByUnits;
    # A migration is a deployment gate, not a one-shot dependency that only
    # runs when an application happens to be started.  Want it from the
    # normal boot target so a corrected candidate gets another chance after a
    # previous migration failure, and restart it when its generated command
    # or manifest changes during NixOS activation.
    wantedBy = lib.optionals (migration.requiredByUnits != []) ["multi-user.target"];
    restartIfChanged = true;
    stopIfChanged = true;
    serviceConfig =
      serviceConfigFor migration
      // {
        ExecStart = migration.command;
      };
  };

  runtimeActivationService = name: migration:
    mkIf (migration.requiredByUnits != []) {
      description = "Start ${migration.description} runtime units after a successful migration";
      after = ["db-harbor-${name}.service"];
      requires = ["db-harbor-${name}.service"];
      wantedBy = ["multi-user.target"];
      restartIfChanged = true;
      stopIfChanged = true;
      serviceConfig = {
        Type = "oneshot";
        ExecStart = pkgs.writeShellScript "db-harbor-${name}-start-runtime" ''
          set -eu
          for unit in ${lib.escapeShellArgs migration.requiredByUnits}; do
            ${pkgs.systemd}/bin/systemctl reset-failed "$unit" || true
            ${pkgs.systemd}/bin/systemctl start --no-block "$unit"
          done
        '';
      };
    };

  checkService = name: migration:
    mkIf (migration.checkCommand != null) {
      description = "${migration.description} readiness check";
      inherit (migration) environment path;
      after = migration.after;
      requires = migration.requires;
      wants = migration.wants;
      serviceConfig =
        serviceConfigFor migration
        // {
          ExecStart = migration.checkCommand;
        };
    };
in {
  imports = [
    (lib.mkAliasOptionModule ["services" "db-harbor" "operations"] ["services" "db-harbor" "migrations"])
  ];

  options.services.db-harbor = {
    package = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = "db-harbor package used for generated project plan units.";
    };

    migrations = mkOption {
      type = types.attrsOf migrationType;
      default = {};
      description = "Compatibility name for named lifecycle operations managed as systemd units.";
    };

    projects = mkOption {
      type = types.attrsOf projectType;
      default = {};
      description = "Higher-level generic project lifecycle definitions lowered into db-harbor operations.";
    };

    dataDirectories = mkOption {
      type = types.listOf (types.submodule {
        options = {
          path = mkOption {
            type = types.str;
            description = "Absolute path of the data directory.";
          };

          user = mkOption {
            type = types.str;
            default = "root";
            description = "Owner of the data directory.";
          };

          group = mkOption {
            type = types.str;
            default = "root";
            description = "Group owner of the data directory.";
          };

          mode = mkOption {
            type = types.str;
            default = "0750";
            description = "Mode of the data directory.";
          };
        };
      });
      default = [];
      description = "Data directories that must exist before any unit that bind-mounts them starts, on every boot and switch.";
    };
  };

  config = mkMerge [
    (mkIf (cfg.dataDirectories != []) {
      assertions = [
        {
          assertion = lib.all (dir: lib.hasPrefix "/" dir.path) cfg.dataDirectories;
          message = "services.db-harbor.dataDirectories: each path must be absolute";
        }
      ];

      # systemd-tmpfiles runs only at boot, ordered after local-fs.target.
      # The activation script covers live switches, where the dir would
      # otherwise be missing when a unit sets up its mount namespace.
      systemd.tmpfiles.rules = map (dir: "d ${dir.path} ${dir.mode} ${dir.user} ${dir.group} - -") cfg.dataDirectories;

      system.activationScripts.db-harbor-establish-data-directories = lib.stringAfter ["groups" "users"] (
        lib.concatMapStringsSep "\n" (dir: "install -d -o ${dir.user} -g ${dir.group} -m ${dir.mode} ${dir.path}") cfg.dataDirectories
      );
    })

    (mkIf (enabledProjects != {}) {
      assertions = lib.flatten (lib.mapAttrsToList (name: project: let
        operations = enabledOperations project;
        credentialSources = projectCredentialSources project;
        operationAssertions = lib.flatten (lib.mapAttrsToList (operationName: operation: [
            {
              assertion = operation.runner.command != null || operation.runner.executable != null;
              message = "services.db-harbor.projects.${name}.operations.${operationName}: set runner.command or runner.executable";
            }
            {
              assertion = operation.runner.command != null || operation.runner.package == null || operation.runner.executable != null;
              message = "services.db-harbor.projects.${name}.operations.${operationName}: runner.package requires runner.executable when runner.command is unset";
            }
            {
              assertion = lib.all (dependency: builtins.hasAttr dependency operations) operation.dependsOn;
              message = "services.db-harbor.projects.${name}.operations.${operationName}: dependsOn references an unknown operation";
            }
            {
              assertion = lib.all (credential: builtins.hasAttr credential credentialSources) (operation.runner.credentialArgs ++ lib.attrValues operation.runner.credentialEnvironment);
              message = "services.db-harbor.projects.${name}.operations.${operationName}: credential references need a matching credentials entry";
            }
          ])
          operations);
      in
        [
          {
            assertion = operations != {};
            message = "services.db-harbor.projects.${name}: configure runner or at least one enabled operation";
          }
          {
            assertion = cfg.package != null;
            message = "services.db-harbor.package must be set when a project uses generated plan units";
          }
          {
            assertion = lib.all (credential: builtins.match "[A-Za-z0-9_.-]+" credential != null) (builtins.attrNames credentialSources);
            message = "services.db-harbor.projects.${name}: credential names must be safe systemd credential names";
          }
          {
            assertion = !project.postgres.grants.enable || project.postgres.enable;
            message = "services.db-harbor.projects.${name}: postgres.grants.enable requires postgres.enable";
          }
          {
            assertion = !project.postgres.grants.enable || project.postgres.databaseUrl != null;
            message = "services.db-harbor.projects.${name}: postgres.databaseUrl is required when postgres.grants.enable is set";
          }
          {
            assertion = !project.postgres.grants.enable || project.postgres.grants.runtimeRole != null;
            message = "services.db-harbor.projects.${name}: postgres.grants.runtimeRole is required when postgres.grants.enable is set";
          }
          {
            assertion = !project.postgres.grants.enable || builtins.hasAttr "default" operations;
            message = "services.db-harbor.projects.${name}: postgres grants require the default migration operation";
          }
          {
            assertion = !project.postgres.grants.enable || project.postgres.grants.tablePrivileges != [];
            message = "services.db-harbor.projects.${name}: postgres.grants.tablePrivileges must not be empty";
          }
          {
            assertion = !project.postgres.grants.enable || project.postgres.grants.sequencePrivileges != [];
            message = "services.db-harbor.projects.${name}: postgres.grants.sequencePrivileges must not be empty";
          }
          {
            assertion =
              !(projectHasChecks project)
              || lib.all
              (operation: operation.runner.checkCommand != null || operation.runner.checkArgs != null)
              (lib.attrValues operations);
            message = "services.db-harbor.projects.${name}: every enabled operation needs a read-only check when project checks are configured";
          }
        ]
        ++ operationAssertions)
      enabledProjects);

      services.db-harbor.migrations = lib.mapAttrs projectToMigration enabledProjects;
    })

    (mkIf (enabledMigrations != {}) {
      assertions = lib.flatten (lib.mapAttrsToList (name: migration: [
          {
            assertion = builtins.match "[A-Za-z0-9_.@-]+" name != null;
            message = "services.db-harbor.migrations.${name}: migration names must be valid systemd unit-name fragments";
          }
          {
            assertion = lib.all (credential: builtins.match "[A-Za-z0-9_.-]+" credential != null) (builtins.attrNames migration.credentials);
            message = "services.db-harbor.migrations.${name}: credential names must be safe systemd credential names";
          }
        ])
        enabledMigrations);

      systemd.services =
        mkMerge
        [
          (lib.mapAttrs' (name: migration:
            lib.nameValuePair "db-harbor-${name}" (migrationService name migration))
          enabledMigrations)
          (lib.mapAttrs' (name: migration:
            lib.nameValuePair "db-harbor-${name}-check" (checkService name migration))
          enabledMigrations)
          (lib.mapAttrs' (name: migration:
            lib.nameValuePair "db-harbor-${name}-activate" (runtimeActivationService name migration))
          enabledMigrations)
        ];
    })
  ];
}
