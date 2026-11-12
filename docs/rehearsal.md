# Rehearsal on a restored production copy

`harbor-db-rehearse` restores a copy of a database into a private, disposable,
Unix-socket-only PostgreSQL cluster and runs migrations and maintenance tasks
there, as the same non-superuser owner role production uses. It answers "will
these steps work, and how long do they take, on a copy of the real data?" before a
revision is admitted. It never writes to the source.

It builds on the logical-restore drill (`harbor-db-postgres-drill`): the same
cluster setup, fixed private socket port, PID-carrier cleanup and execution
deadlines, in `src/storage/postgres_drill.rs`. The rehearsal engine is
`src/storage/rehearsal.rs`; the CLI is

```text
harbor-db-rehearse --plan PLAN.json --receipt-dir DIR
```

It prints `{"receipt": PATH, "receipt_sha256": DIGEST, "status": "passed|failed"}`
and exits 0 only for `passed`. A plan or source rejected before anything was
created exits 1 with a diagnostic and publishes no receipt. `receipt_sha256` is the
SHA-256 of the receipt file's bytes (`sha256sum PATH`), the same convention as the
rollout `receipt`/`receipt_sha256` pair.

This is a Rust-only command. The PR #14 Python baseline pins only the original
modules, and `tests/pr14_cli_parity.rs` compares only their flag surfaces; new
commands and gates may extend that baseline
([Rust migration baseline](rust-migration-baseline.md)), so no Python
counterpart exists or is required.

## Plan: `harbor-db.rehearsal-plan.v1`

A JSON object. Unknown fields are rejected at every level, and `schema` is
required and must equal the schema id.

| Field                  | Meaning                                                                                                                                                                                                                                                  |
| ---------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `schema`               | `"harbor-db.rehearsal-plan.v1"`.                                                                                                                                                                                                                         |
| `package`              | Absolute PostgreSQL package path with `bin/{initdb,pg_ctl,pg_restore,pg_dump,psql}` and every extension the source and the steps need.                                                                                                                   |
| `source`               | `{"live": {"host": "/run/postgresql", "database": "x", "user"?: "y"}}` or `{"dump": "/path"}`.                                                                                                                                                           |
| `owner`                | Role created `LOGIN NOSUPERUSER`; steps and probes run as it. Use the source's own owner role name: `ALTER DEFAULT PRIVILEGES FOR ROLE <owner>` entries only restore for that role.                                                                       |
| `superuser`            | Bootstrap superuser role `initdb` creates in the disposable cluster. Defaults to the OS account running the engine; it must differ from `owner`, which is why the module sets `harbor_rehearsal_admin`. Use a name no production role has.               |
| `database`             | Database created in the disposable cluster, owned by `owner`.                                                                                                                                                                                            |
| `locale`               | Collation/ctype for `CREATE DATABASE`. Defaults to the live source's `datcollate`, and to `C` for a dump source, so set it explicitly for dumps whose collation matters.                                                                                 |
| `superuser_extensions` | Extensions created as superuser before the restore, in addition to every extension in the dump's table of contents. Use it for extensions the rehearsed migration introduces (the source does not have them yet).                                         |
| `grantee_roles`        | Extra roles to stand in as `NOLOGIN` roles, in addition to those found in the dump's ACL entries.                                                                                                                                                        |
| `steps`                | Ordered `[{"name", "kind": "migrate\|check\|maintenance", "argv": [...], "env": {...}, "expect_exit": 0}]`. `name` is unique; `kind` is recorded, not interpreted. `env` may not set `DATABASE_URL` or any `PG*` variable, so a step cannot retarget the copy. |
| `probes`               | `[{"name", "sql"}]`, run after all steps pass.                                                                                                                                                                                                           |
| `work_directory`       | Existing directory owned by the caller without group/other write. Each run uses a private `r-<epoch>-<pid>` subdirectory.                                                                                                                                |
| `timeout_seconds`      | 1..86400, default 14400. Bounds the whole run; cleanup has its own 120 seconds.                                                                                                                                                                          |
| `jobs`                 | 1..64, default 4, for `pg_dump -j` and `pg_restore -j`.                                                                                                                                                                                                  |
| `keep`                 | Keep the (stopped) run directory for inspection. Default `false`.                                                                                                                                                                                        |

A live source is dumped with `pg_dump -Fd -j N -Z zstd:3` into the run
directory (about 31 GB in 482 s with four jobs for a 77 GB database). The unit
user must reach it without a password, normally by peer authentication on the
socket directory in `host`. A dump source is a directory or custom-format dump and
is only read.

## What the restore does

1. The cluster's bootstrap superuser (`superuser`, by default the OS account
   running the engine) creates `owner`, `NOLOGIN` stand-ins for every grantee
   and `FOR ROLE` target found in the dump's ACL and default-ACL entries (plus
   `grantee_roles`), and the database.
2. The superuser creates every extension named in the dump plus
   `superuser_extensions` (`CREATE EXTENSION IF NOT EXISTS … CASCADE`, default
   schema). The owner could not create untrusted ones such as postgis, and ACLs may
   reference their objects.
3. `pg_restore --exit-on-error --no-owner --role=<owner>` restores everything except
   the extension and `COMMENT ON EXTENSION` entries and the data of tables that
   belong to an extension (for example postgis `spatial_ref_sys`); the extension
   already populated them, and the owner may not write them. Production edits to
   such tables are not carried over.
4. A second `pg_restore` pass as the superuser, without `--role`, restores only the
   default ACLs.

ACLs are restored, not dropped, so privileges match production. Any `pg_restore`
error fails the rehearsal and its stderr tail is in the receipt's `failure`. If
the line-wise ACL scan misses a role, the restore fails loudly with
`role "x" does not exist`; add it to `grantee_roles`. A source that keeps an
extension outside the default schema also fails loudly.

Steps then run, in order, with `cwd` an empty private directory and an
environment of only `PATH` (the package's `bin` first), `HOME` (private),
`LANG`, `LOCALE_ARCHIVE`, `TZ`, the injected `DATABASE_URL`
(`postgres:///DB?host=SOCKET&port=55439&user=OWNER`), `PGHOST`, `PGPORT`, `PGUSER`
and `PGDATABASE`, plus the step's `env`. A step whose exit code is not
`expect_exit`, that is killed by a signal, or that exceeds the deadline stops the
run. Each step's process group is killed when it ends. Probes run as the owner
with `default_transaction_read_only=on`; a failing probe fails the rehearsal.

## Receipt: `harbor-db.rehearsal-receipt.v1`

Written atomically (mode 0600) as `<receipt-dir>/<UTC YYYYmmddTHHMMSSZ>.json`
(with the PID appended if that name exists), for passed and failed rehearsals.

```json
{
  "schema": "harbor-db.rehearsal-receipt.v1",
  "status": "passed",
  "plan_sha256": "…",
  "started_at": "2026-11-04T09:00:00Z",
  "package": "/nix/store/…-postgresql-and-plugins-18.6",
  "owner": "app_owner",
  "database": "app",
  "locale": "en_US.UTF-8",
  "source": {
    "kind": "live",
    "database": "app",
    "server_version": "18.6",
    "dump_bytes": 31000000000,
    "toc_sha256": "…"
  },
  "extensions": ["pg_trgm", "postgis"],
  "standin_roles": ["etl", "operator"],
  "dump_seconds": 482.1,
  "restore_seconds": 1210.4,
  "steps": [
    {
      "name": "migrate",
      "kind": "migrate",
      "argv": ["/nix/store/…/bin/app", "db", "migrate"],
      "expect_exit": 0,
      "exit_code": 0,
      "seconds": 311.9,
      "error": null,
      "stdout_tail": "…",
      "stderr_tail": "…"
    }
  ],
  "probes": [
    {
      "name": "head",
      "sql": "select max(version) from _sqlx_migrations",
      "result": "105",
      "error": null
    }
  ],
  "failure": null,
  "cleanup": "removed",
  "total_seconds": 2100.7
}
```

`toc_sha256` is the digest of the dump's `pg_restore -l` listing, which identifies
the archive. `dump_seconds` is null for a dump source. Step tails keep the last
4096 bytes of each stream; probe results are capped at 16 KiB. `exit_code` is null
when the step could not start, was killed by a signal or timed out, and `error`
says why. `cleanup` is `removed`, `cluster stopped, work directory kept at …`, or a
failure message; a run directory whose cluster could not be stopped is never
removed.

## Safety rules

- The source is only read: `pg_dump` for a live source, `pg_restore -l`/restore
  from a dump. A dump is never modified or deleted.
- A work directory that contains, or lies inside, the dump, the live server's data
  directory (when visible to the connecting role) or its socket directory is
  refused. The work directory must be owned, unredirected and not group/other
  writable; the run directory is `0700`.
- The cluster listens only on a Unix socket in the run directory and uses the
  drill's PID-carrier cleanup, which checks process identity before signalling.
  The cluster is stopped on every exit; the run directory is removed unless `keep`.
- Everything is bounded by `timeout_seconds`; steps and probes inherit the
  remaining time. The socket path must fit the Unix limit, so keep
  `work_directory` short.
- The cluster runs with `fsync`, `synchronous_commit` and `full_page_writes` off and
  needs disk for the dump (live source) and the restored database. Run it under a
  memory limit.

## NixOS module

`services.harbor-db.projects.<name>.postgres.superuserExtensions` lists
extensions only a superuser may create. The generated apply unit runs
`CREATE EXTENSION IF NOT EXISTS` for each as the PostgreSQL superuser, in
`postgres.database` (or `postgres.provision.database`), before the owner's
migration command, through a privileged `ExecStartPre`. That step names the
superuser with `-U` and runs with an empty environment, so a project `PGUSER` or
`PGHOST` cannot retarget it; it connects through the cluster
`services.postgresql` configures (`settings.unix_socket_directories`, default
`/run/postgresql`, and `settings.port`). Because the extension is created there,
evaluation refuses `superuserExtensions` when the project's own targets name
another cluster: `postgres.provision.socketDirectory`/`port` (when provisioning
is enabled), `environment.PGHOST`/`PGPORT`, or the host/port named by
`postgres.databaseUrl`. A URL that names host or port more than once (repeated
query keyword, or both the authority and the query), names them as a
comma-separated list, uses a query key that could select another cluster
(`hostaddr`, `service`, `target_session_attrs`, ...), or uses a percent-encoded
query key is refused as ambiguous; only a small set of non-targeting query keys
is accepted. `PGSERVICE`, `PGSERVICEFILE`, `PGHOSTADDR` and `PGSYSCONFDIR` are
refused in the project environment. Point those at the local cluster, or set
`superuserExtensions = []` and create the extension yourself. The read-only
check and restore units never run it.

`services.harbor-db.projects.<name>.rehearsal` generates an on-demand
`harbor-db-<name>-rehearse.service`: a oneshot with no timer and no `wantedBy`.
Its plan is built at evaluation from:

- `runner`: a `migrate` step and, when a check is defined, a `check` step;
- operations with `phase = "maintenance"`: for each, in `dependsOn` order, a
  `maintenance-<operation>` step and its `maintenance-<operation>-check` step;
- `postgres.superuserExtensions` and the `rehearsal` options.

Other operations are not rehearsed. The project's `environment` reaches the steps
except `DATABASE_URL` and `PG*`. Rehearsed runners cannot use credentials; the
copy must not need production secrets. Receipts go to the unit's own state
directory, `/var/lib/harbor-db-<name>-rehearse/`, never to the project's.

| Option                                                                | Default                                                      |
| --------------------------------------------------------------------- | ------------------------------------------------------------ |
| `enable`                                                              | `false`                                                      |
| `source.database` / `source.host` / `source.user`                     | project database / `/run/postgresql` / none                  |
| `owner`                                                               | `postgres.provision.ownerRole`, else the project `user`      |
| `user`                                                                | the project `user` (initdb refuses root)                     |
| `superuser`                                                           | `harbor_rehearsal_admin`, which must not be the `owner`      |
| `workDirectory`                                                       | required, an existing absolute directory                     |
| `package`                                                             | `services.postgresql.finalPackage`, else `pkgs.postgresql`   |
| `jobs` / `timeoutSeconds` / `keep`                                    | `4` / `14400` / `false`                                      |
| `probes`                                                              | `[]`; each is `{ name, sql }`                                |

The unit user must be able to `pg_dump` the live database over `source.host` and
write `workDirectory` (which is its only extra writable path under
`ProtectSystem=strict`). Grant read access the way a read-only operator role is
granted, for example for a role `rehearsal` on database `app` owned by `app_owner`:

```sql
GRANT pg_read_all_data TO rehearsal;  -- PostgreSQL 14+, reads data and sequences
-- or, table by table:
GRANT SELECT ON ALL TABLES IN SCHEMA public TO rehearsal;
GRANT SELECT ON ALL SEQUENCES IN SCHEMA public TO rehearsal;  -- pg_dump needs sequence state
ALTER DEFAULT PRIVILEGES FOR ROLE app_owner IN SCHEMA public GRANT SELECT ON TABLES TO rehearsal;
ALTER DEFAULT PRIVILEGES FOR ROLE app_owner IN SCHEMA public GRANT SELECT ON SEQUENCES TO rehearsal;
```

and map the unit's OS user to that role (`rehearsal.source.user` when the names
differ, with a matching `pg_ident` map).

### Example: postgis and a maintenance operation

```nix
services.postgresql = {
  extensions = ps: [ps.postgis ps.pgvector];
  ensureDatabases = ["app"];
  ensureUsers = [{ name = "app"; ensureDBOwnership = true; }];
};

services.harbor-db.projects.app = {
  enable = true;
  user = "app";
  stateDirectory = "app";
  postgres = {
    enable = true;
    database = "app";
    superuserExtensions = ["postgis"];
  };
  runner = {
    package = pkgs.app;
    executable = "bin/app";
    args = ["db" "migrate"];
    checkArgs = ["db" "migrate" "--check"];
  };
  operations.search-reindex = {
    enable = true;
    phase = "maintenance";
    safety = "operator_confirmed";
    runner = {
      package = pkgs.app;
      executable = "bin/app";
      args = ["search" "rebuild"];
      checkArgs = ["db" "migrate" "--check"];
    };
  };
  rehearsal = {
    enable = true;
    workDirectory = "/data/scratch/rehearsal";
    jobs = 4;
    probes = [
      { name = "head"; sql = "select max(version) from _sqlx_migrations"; }
      { name = "entries"; sql = "select count(*) from search_entry"; }
    ];
  };
};
```

Run it with `systemctl start harbor-db-app-rehearse.service` and read the newest file
in `/var/lib/harbor-db-app-rehearse/`. A passing receipt is rehearsal evidence for that
copy and revision; it does not qualify a deployment by itself.

## Qualification

`tests/rehearsal.rs` runs the actual CLI against real source and rehearsal
clusters of `HARBOR_DB_TEST_POSTGRES` (including a postgis case that skips with a
printed message when the package lacks postgis), and the documented defaults where
the project owner is also the unit account; unit tests in
`src/storage/rehearsal.rs` cover plan parsing, TOC filtering, ACL role extraction
and the work-directory guard; `nix/module-eval.nix` checks the generated unit, plan
and `ExecStartPre`; the `module-smoke` NixOS test rehearses a live database with the
documented defaults and with a distinct `rehearsal.user`. All are registered in
`tests/suite.toml`.
