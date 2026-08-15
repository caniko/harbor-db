# db-harbor

<!-- simit:badges:start -->

[![CI](https://img.shields.io/badge/CI-managed-2088ff)](.github/workflows/ci.yaml) [![Nix](https://img.shields.io/badge/Nix-managed-5277c3)](flake.nix) [![docs](https://img.shields.io/badge/docs-enabled-6f42c1)](https://docs.rs/db-harbor)

<!-- simit:badges:end -->

`db-harbor` provides secure generic lifecycle-operation plans and NixOS
systemd wiring for project-owned work.

The flake does not know about a migration framework, database, or application.
Projects keep their own idempotent ensure/check commands; `db-harbor` owns
dependency ordering, confirmation policy, readiness checks, credentials, state
directories, and the deployment envelope around those commands. Operations can
cover schema changes, backfills, backups, maintenance, credential provisioning,
replication, and cutovers.

## Project surface

For Rust/Postgres services such as Pink Raven and SynDB, prefer the project
surface. It lowers into the raw migration units described below:

```nix
{
  imports = [inputs.db-harbor.nixosModules.default];

  services.db-harbor.projects.my-app = {
    enable = true;
    description = "My App lifecycle operations";

    runner = {
      package = pkgs.my-app;
      executable = "bin/my-app";
      args = ["db" "migrate" "--database-url" "postgres:///my_app?host=/run/postgresql"];
      checkArgs = ["db" "migrate" "--check" "--database-url" "postgres:///my_app?host=/run/postgresql"];
    };

    user = "my_app_migrator";
    group = "my_app";
    runtimeUnits = ["my-app.service" "my-app-worker.service"];

    postgres = {
      enable = true;
      databaseUrl = "postgres:///my_app?host=/run/postgresql";
      setupUnits = ["postgresql-setup.service"];
      grants = {
        enable = true;
        runtimeRole = "my_app";
      };
    };

    serviceConfig.ReadWritePaths = ["/var/lib/my-app"];
  };
}
```

The flake module injects its own `db-harbor` package. When importing
`nix/module.nix` directly, set `services.db-harbor.package` to the package
output explicitly.

This generates `db-harbor-my-app.service` and, when `checkArgs` or
`checkCommand` is set, `db-harbor-my-app-check.service`. Runtime units are
ordered after the migration unit and require it, so each start can re-run the
idempotent migration command.

### Pink Raven shape

Pink Raven should keep SQLx migrations behind its `raven db migrate` CLI and
let `db-harbor` own ordering, migration/runtime user separation, and grants:

```nix
services.db-harbor.projects.pink-raven = {
  enable = true;
  runner = {
    package = config.services.pink-raven.package;
    executable = "bin/raven";
    args = commonArgs ++ ["db" "migrate"];
    checkArgs = commonArgs ++ ["db" "migrate" "--check"];
  };
  user = "can";
  group = "pink_raven";
  runtimeUnits = ["pink-raven.service" "pink-raven-worker.service"];
  postgres = {
    enable = true;
    databaseUrl = "postgres:///pink_raven?host=/run/postgresql";
    setupUnits = ["postgresql-setup.service"];
    grants = {
      enable = true;
      runtimeRole = "pink_raven";
    };
  };
  serviceConfig.ReadWritePaths = ["/data/nvme0/can/state/pink-raven"];
};
```

### SynDB shape

SynDB exposes its SeaORM metadata migrator and ClickHouse lifecycle commands
through `syndb migrate`. Register all four operations with `db-harbor`; only
the Postgres and ClickHouse schema operations are automatic:

```nix
services.db-harbor.projects.syndb = {
  enable = true;
  operations = {
    postgres = {
      enable = true;
      backend = "postgres";
      runner = {
        package = pkgs.syndb-cli;
        executable = "bin/syndb";
        args = ["migrate" "postgres" "--database-url" "postgres:///syndb?host=/run/postgresql"];
        checkArgs = ["migrate" "postgres" "--check" "--database-url" "postgres:///syndb?host=/run/postgresql"];
      };
    };
    clickhouse-schema = {
      enable = true;
      backend = "clickhouse";
      runner = {
        package = pkgs.syndb-cli;
        executable = "bin/syndb";
        args = ["migrate" "ensure-schema" "--database" "syndb"];
        checkArgs = ["migrate" "ensure-schema" "--database" "syndb" "--check"];
      };
    };
    mv-backfill = {
      enable = true;
      backend = "clickhouse";
      phase = "backfill";
      safety = "operator_confirmed";
      dependsOn = ["clickhouse-schema"];
      runner = {
        package = pkgs.syndb-cli;
        executable = "bin/syndb";
        args = ["migrate" "mv-backfill" "--database" "syndb"];
        checkArgs = ["migrate" "ensure-schema" "--database" "syndb" "--check"];
      };
    };
    provenance-events-to-distributed = {
      enable = true;
      backend = "clickhouse";
      phase = "operational";
      safety = "operator_confirmed";
      dependsOn = ["clickhouse-schema"];
      runner = {
        package = pkgs.syndb-cli;
        executable = "bin/syndb";
        args = ["migrate" "provenance-events-to-distributed" "--database" "syndb" "--reason" "operator supplied reason"];
        checkArgs = ["migrate" "ensure-schema" "--database" "syndb" "--check"];
      };
    };
  };
  runtimeUnits = ["syndb-api.service"];
  postgres = {
    enable = true;
    databaseUrl = "postgres:///syndb?host=/run/postgresql";
    setupUnits = ["postgresql-setup.service"];
  };
};
```

The generated activation unit runs only automatic operations. Operators run a
manual operation with the manifest and explicit `--operation` plus `--confirm`;
SynDB’s provenance command still requires its existing reason and journal
preconditions. The generic invocation is:

```sh
db-harbor apply --manifest /path/to/syndb-plan.json \
  --operation mv-backfill --confirm
```

## Raw migration surface

Use the raw lifecycle surface when a project needs complete control over the
command or when the operation is not tied to the project-level Postgres
conventions:

```nix
{
  imports = [inputs.db-harbor.nixosModules.default];

  services.db-harbor.operations.my-app = {
    enable = true;
    command = "${pkgs.my-app}/bin/my-app migrate";
    checkCommand = "${pkgs.my-app}/bin/my-app migrate --check";
    after = ["postgresql-setup.service"];
    requires = ["postgresql-setup.service"];
    beforeUnits = ["my-app.service"];
    requiredByUnits = ["my-app.service"];
    serviceConfig.ReadWritePaths = ["/var/lib/my-app"];
  };
}
```

This generates `db-harbor-my-app.service`, a `Type=oneshot` unit without
`RemainAfterExit`, so starting a dependent application unit can re-run the
idempotent lifecycle command when needed. `services.db-harbor.migrations` is
kept as the compatibility spelling.

## Credential-backed operations

Credential sources are declared by name and file path. The source contents are
loaded by systemd and are never written to the generated plan or passed as a
command argument or environment value. Credential references resolve to paths
under systemd's `CREDENTIALS_DIRECTORY`:

```nix
services.db-harbor.projects.provision = {
  enable = true;
  operations.ensure = {
    enable = true;
    kind = "credential";
    lifecycle = "ensure";
    credentials.api-token = config.age.secrets.api-token.path;
    stateDirectory = "my-app-provision";
    runtimeDirectory = "my-app-provision";
    runner = {
      package = pkgs.my-app;
      executable = "bin/my-app";
      args = ["provision"];
      checkArgs = ["provision" "--check"];
      credentialEnvironment.API_TOKEN_FILE = "api-token";
    };
  };
};
```

`credentialArgs` appends credential file paths to the runner arguments;
`credentialEnvironment` maps environment names to credential names. Do not put
secret values in `args`, `environment`, or generated plans.
