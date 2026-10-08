# harbor-db

<!-- simit:badges:start -->

![CI](https://img.shields.io/badge/CI-managed-2088ff) [![Nix](https://img.shields.io/badge/Nix-drift-5277c3)](flake.nix) [![docs](https://img.shields.io/badge/docs-enabled-6f42c1)](https://docs.rs/harbor-db)

<!-- simit:badges:end -->

[Nix qualification workflow](.github/workflows/nix-builds.yaml)

[Mandatory cutover admission and historical corpus custody](docs/cutover-admission.md)

`harbor-db` provides secure generic lifecycle-operation plans and NixOS
systemd wiring for project-owned work.

The flake does not know about a migration framework, database, or application.
Projects keep their own idempotent ensure/check commands; `harbor-db` owns
dependency ordering, confirmation policy, readiness checks, credentials, state
directories, and the deployment envelope around those commands. Operations can
cover schema changes, backfills, backups, maintenance, credential provisioning,
replication, and cutovers.

## Project surface

For Rust/Postgres services such as Pink Raven and SynDB, prefer the project
surface. It lowers into the raw migration units described below:

```nix
{
  imports = [inputs.harbor-db.nixosModules.default];

  services.harbor-db.projects.my-app = {
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

The flake module injects its own `harbor-db` package. When importing
`nix/module.nix` directly, set `services.harbor-db.package` to the package
output explicitly.

This generates `harbor-db-my-app.service` and, when `checkArgs` or
`checkCommand` is set, `harbor-db-my-app-check.service`. Runtime units are
ordered after the migration unit and require it, so each start can re-run the
idempotent migration command.

### Pink Raven shape

Pink Raven should keep SQLx migrations behind its `raven db migrate` CLI and
let `harbor-db` own ordering, migration/runtime user separation, and grants:

```nix
services.harbor-db.projects.pink-raven = {
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
through `syndb migrate`. Register all four operations with `harbor-db`; only
the Postgres and ClickHouse schema operations are automatic:

```nix
services.harbor-db.projects.syndb = {
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
harbor-db apply --manifest /path/to/syndb-plan.json \
  --operation mv-backfill --confirm
```

## Raw migration surface

Use the raw lifecycle surface when a project needs complete control over the
command or when the operation is not tied to the project-level Postgres
conventions:

```nix
{
  imports = [inputs.harbor-db.nixosModules.default];

  services.harbor-db.operations.my-app = {
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

This generates `harbor-db-my-app.service`, a `Type=oneshot` unit without
`RemainAfterExit`, so starting a dependent application unit can re-run the
idempotent lifecycle command when needed. `services.harbor-db.migrations` is
kept as the compatibility spelling.

## Credential-backed operations

Credential sources are declared by name and file path. The source contents are
loaded by systemd and are never written to the generated plan or passed as a
command argument or environment value. Credential references resolve to paths
under systemd's `CREDENTIALS_DIRECTORY`:

```nix
services.harbor-db.projects.provision = {
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

## Existing PostgreSQL adoption through NixOS

The PostgreSQL lifecycle module rejects unadopted storage before initialization.
For an operator-authorized first rollout, after independently verifying backups,
application records and the cluster identifier, NixOS can perform adoption while
switching the existing primary into its guarded configuration:

```nix
services.harbor-db.postgresql = {
  enable = true;
  stateDir = "/srv/postgres/authority";
  requiredMounts = ["/srv"];
  switchAdoption = {
    systemIdentifier = "INDEPENDENTLY_VERIFIED_NUMERIC_IDENTIFIER";
    socketDir = "/run/postgresql";
    port = 5432;
  };
};
```

Supply the actual numeric identifier; the placeholder is deliberately not valid
configuration. The pre-switch check uses the candidate's Harbor DB and PostgreSQL
packages under the PostgreSQL service identity, before NixOS stops the existing
primary. `inspect-live` compares its reported data directory, major, identifier,
recovery state and durability settings with the declared physical cluster and
independently supplied identifier. It ignores ambient PostgreSQL routing and
client startup files, requires local peer-authenticated access and emits JSON.

Only `switch` and `test` execute `adopt-live`; boot/check/dry actions inspect
without adoption. Existing authority is verified with a shared lease, so a later
guarded primary remains the lease owner. Missing lock anchors, changed identities
and incomplete upgrade journals fail. Normal service startup never adopts.
Remove the `switchAdoption` request after the rollout; keep the persistent
authority record and its backups. This option verifies identity, not recovery
coverage or application-record freshness, which remain consumer rollout gates.

### Explicit PostgreSQL writer fence

`lib.postgresWriterFence = 3` exposes stopped `fence-open` / `fence-close`,
live `inspect-fence`, persistent systemd startup inhibition, client startup
gates and enforced snapshot-bound recovery admission. The storage-owner engine
retains original configuration and fence history; no failure or cancellation
automatically thaws the primary or starts an application.

Configure `writerFence.replicationRoles` for physical localhost SCRAM replication
users and `writerFence.allowedPreloadLibraries` for audited non-writing preload
libraries. Only local OS-peer PostgreSQL control SQL and physical replication
remain available. Ordinary SQL is rejected even for a reconnecting superuser or
replication role. `writerFence.blockedUnits` adds a startup condition to explicitly
named migration/runtime clients while retaining their existing conditions.

Before stopping the primary, root runs `inhibit-startup --system-identifier ID`
using the immutable manifest. The persistent root-owned gate must be installed
and read back from systemd before stopping the service. Then the PostgreSQL OS
identity runs `fence-open --system-identifier ID`. It returns `prepared-offline`,
never live readiness. Root releases only the startup gate with
`release-startup --token STARTUP_TOKEN --fence-token FENCE_TOKEN --phase prepared`
after stopped readback. Starting the primary is a separate service-manager
operation; `inspect-fence --token FENCE_TOKEN` must pass afterwards.

`recovery.requireWriterFence = true` rejects missing/unready exclusion before
managed preparation, source capture, live adoption and preflight/activation/
certification. Source snapshots retain `writer_fence_token`; existing snapshots
must match that token before preparation reuse or deployment admission. A shared
fence anchor is retained across those operations, blocking an offline thaw until
their verification/publication finishes. Off-host certification remains bound
to the source snapshot digest and never contacts the production primary.

The consumer keeps its activation/campaign lease and stopped application writers
through recovery, independent restore acceptance, adoption and deployment
post-verification. It explicitly decides when a qualified deployment is accepted.
After acceptance it inhibits startup again, stops the primary, runs
`fence-close --token FENCE_TOKEN` under the PostgreSQL OS identity, verifies the
stopped closed boundary, releases startup with `--phase closed`, and separately
starts PostgreSQL and its clients. Unfinished thaw retains startup inhibition;
the same token must complete the transition. No anchor is deleted.

After accepted bootstrap retirement, disabling `requireWriterFence` permits
ordinary historical recovery admission. An existing fence remains inspected and
enforced at startup; retirement never creates identity, removes evidence or
thaws a pending fence. The native real-PostgreSQL regression and
`checks.x86_64-linux.postgres-writer-fence` exercise reconnecting superusers,
rollback/reboot, interrupted acquisition/thaw and client startup conditions.
Those fixtures do not certify a consumer's production recovery or activation.

### Executed recovery admission

`lib.postgresRecoveryReadiness = 1` advertises the optional recovery protocol.
Consumers configure `services.harbor-db.postgresql.recovery` to require it before
explicit adoption and activating rollouts:

```nix
services.harbor-db.postgresql.recovery = {
  systemIdentifier = "INDEPENDENTLY_VERIFIED_NUMERIC_IDENTIFIER";
  backupRoot = "/srv/backups/primary";
  snapshotFile = "/srv/backups/primary/evidence/records.json";
  receiptFile = "/srv/backups/primary/evidence/recovery.json";
  offHostReceiptFile = "/srv/backups/primary/evidence/off-host.json";
  sourceHostname = "primary";
  maxAgeSeconds = 172800;
  recordChecks.saves = {
    database = "app";
    sql = "SELECT mutation, geometry, review, revision FROM saves ORDER BY mutation";
  };
};
```

The backup layout has `LAST_SUCCESS` containing a completed backup identifier,
`base/<identifier>/backup_manifest`, `base/<identifier>.meta.json` and the
persistent `locks/mutate` anchor. Metadata supplies `backup_id`, `pg_major`,
`system_identifier`, `epoch_id`, `backup_stop_lsn` and `post_backup_lsn`.
The last LSN must be a post-backup recovery point. The tool uses the configured
PostgreSQL package's `pg_controldata` and `pg_verifybackup` to validate actual
retained bytes. WAL replay is established by the disposable restored server,
not by a successful service exit or a copied report.

Use the candidate's `/etc/harbor-db/postgresql.json` and package before first
activation. Under the consumer's existing consistency window, keep application
writers paused from backup capture through the record snapshot. Queries must
be deterministic and cover the application records whose recovery matters.
They run in read-only transactions; only SHA-256 digests enter evidence.

For a first guarded NixOS rollout, set an explicit
`services.harbor-db.postgresql.recoveryPreparation` request. Its absolute argv
`readinessCommand`, `backupCommand` and `restoreCommand` are consumer-owned:
check the live receiver/flush lag, publish the conservative backup, then restore
and certify a disposable endpoint while it is alive. Declare only the writable
backup/evidence and disposable paths; the module rejects primary/authority trees.
The candidate pre-switch hook runs these commands as `postgres` before any unit
replacement or adoption. Boot/dry/check actions cannot execute preparation.
The consumer must hold its writer consistency window for backup through snapshot.

An optional consumer `exportCommand` runs only after local acceptance, before
the missing off-host receipt abort. It may publish the selected immutable
backup/WAL/metadata/snapshot copy for independent transport. Its writable paths
and `supplementaryGroups` are explicitly declared; Harbor does not perform
transport or restore orchestration. Receipts bind the exact metadata bytes as
well as the base manifest, replay target and record contract.

Missing independent evidence aborts that activation after retaining local
evidence. On retry, the preparation journal and source snapshot retain the same
backup; stale, changed or incomplete existing evidence fails rather than starting
a replacement backup. A consumer may transport the independently executed
receipt to `offHostReceiptImportFile`. The next managed activation privately loads
it as a systemd credential, validates its backup/query/record/hostname binding,
then publishes it atomically under the backup/evidence leases. There is no
executor-host override. Ordinary inspection/startup remains read-only. Retire the
one-time preparation request after acceptance; renew stale evidence only through
the consumer's next explicitly coordinated consistency window.

1. Create and verify the completed backup and post-backup recovery point.
2. Run `harbor-db-postgres --config <candidate-manifest> snapshot-records
   --socket-dir /run/postgresql --port 5432` as the PostgreSQL service user.
3. Restore into a disposable directory, replay through `post_backup_lsn`,
   promote the disposable copy and start it with
   `default_transaction_read_only = on`. The primary is never the drill target.
4. Run `harbor-db-postgres --config <candidate-manifest> certify-recovery
   --data-dir <disposable-directory> --socket-dir <local-restore-socket>
   --port <restore-port>` while that recovered endpoint is running.
5. For required off-host recovery, copy the same backup/metadata/snapshot and
   query contract to the independent host. Perform a fresh restore and execute
   the same certifier there, with that host's configured `receiptFile` selecting
   the off-host receipt. Copy the resulting receipt back with private ownership
   and atomic publication. The CLI records the real executor hostname; it has
   no hostname override.
6. Run `inspect-recovery`. It is read-only and emits JSON, as does the optional
   `harbor-db-postgresql-recovery-check.service`. Missing/stale evidence, changed
   query contracts or backup bytes, wrong cluster identities, incomplete replay
   and mismatched records fail before authority creation.

Evidence publication uses a separate persistent `recovery.lock` next to the
snapshot, while retaining the backup's shared mutation lease. Read-only checks
never create missing anchors. Adoption retains both accepted-evidence leases
until the authority record is durably published. No service starts or repairs a
recovery drill at boot. `offHostReceiptFile = null` explicitly selects local-only qualification;
consumers that require independent coverage must configure the off-host path.
Receipts are trusted service-user-owned local evidence, not remote attestation
or a substitute for consumer-owned writer coordination and backup transport.
