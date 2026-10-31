# Storage identity and durable lifecycle operations

Harbor DB supplies filesystem publication, persistent resource authority, and a
PostgreSQL adapter. Applications supply their schema and record-level validators;
deployments supply persistent mounts, destinations and supported generations.
The Rust adapters own locking, durable publication, process execution and
recovery. Storage modules select the native lifecycle package by default.
Frozen Python sources remain test-only migration evidence.

## PostgreSQL startup

Import `nixosModules.postgres-lifecycle` (also included in the default module):

```nix
services.harbor-db.postgresql = {
  enable = true;
  resource = "application-cluster";
  stateDir = "/srv/postgresql/authority";
  requiredMounts = ["/srv"];
};
```

The guard runs before nixpkgs' `initdb` on **every** PostgreSQL start. A fresh
installation therefore fails until the operator explicitly provisions and adopts
the intended cluster. Verify its identity against the authoritative endpoint
using `SELECT system_identifier FROM pg_control_system()` and then, as postgres:

```sh
harbor-db-postgres --config /etc/harbor-db/postgresql.json adopt \
  --system-identifier VERIFIED_SYSTEM_IDENTIFIER
harbor-db-postgres --config /etc/harbor-db/postgresql.json check
```

Adoption is idempotent for the same registered cluster and cannot replace an
existing authority record. A wrong major, wrong directory, missing mount,
missing identity, changed system identifier or unfinished upgrade fails startup.
The launcher checks authority under a shared lock, then execs PostgreSQL with the
lock descriptor open. The postmaster itself is systemd's MAINPID and retains the
lease throughout startup and its writer lifetime; forked children also inherit it.
There is no supervisor whose death can release a surviving writer's lease.
The selected data directory is explicit, overriding PGDATA and configuration
redirects. An upgrade between pre-start and exec either completes before the
launcher's fresh check or prevents launch by holding the exclusive lock/journal.
Back up the authority directory with the cluster. Provisioning and disaster
recovery are explicit operator operations, never normal-boot initialization.

Only first adoption may create the lock anchor. Repeated adoption and upgrades
require the existing inode; a missing anchor fails without replacing it. This
also applies when a surviving writer still holds an unlinked lock descriptor.

The launcher supplies `fsync=on`, `full_page_writes=on` and
`synchronous_commit=on` as command-line settings, outranking persistent
`postgresql.auto.conf` values. PostgreSQL permits role/session overrides of
`synchronous_commit`; consumers must set it locally in every acknowledged write
transaction. A generated configuration default alone is insufficient.

## Startup inhibition

`writerFence.startupStateDir` must be a normalized, dedicated absolute path,
separate in both containment directions from the cluster data and identity state.
Nix evaluation rejects overlapping defaults after a parent-directory override,
as well as traversal, duplicate separators and unsafe startup-path spellings.
Recovery preparation writable and provisioned directories must also be disjoint
from startup inhibition storage in both containment directions. Preparation cannot
change the owner or mode of the root-owned inhibition namespace.

## Offline major upgrades

Set `upgrade.oldPackage`, `oldDataDir`, `initdbArgs`, optional `extraConfig`, and
`validateCommand`, whose first argument must name an immutable store executable;
empty, relative or runtime-mutable validators fail Nix evaluation. Declare
additional interpreter/adapter files through `upgrade.executableFiles`; they must
also be immutable store files. Both operator CLIs validate the executable, alias
hops, PostgreSQL tools, copy command and adapter files before authority mutation.
This prevents a worker from swapping in a successful validator and restoring the
original bytes around journal contract checks.
The validation argv receives the staging directory as its
last argument. It must verify the application's actual recovered records, stop
any server it started and return nonzero on any mismatch.
Validation is read-only: it must never accept application saves or call a
lock-acquiring Harbor DB operation while the upgrade holds its exclusive lock.
The systemd upgrade unit uses a private network namespace; validators use a
private Unix socket rather than an application-accessible listener.

Run `harbor-db-postgresql-upgrade.service` explicitly after stopping writers.
It has no boot target. The adapter copies the offline source with reflinks where
available, initializes a separate staging destination, runs `pg_upgrade` by copy,
validates it, flushes the tree, writes a durable ready journal, renames the target,
flushes its parent, publishes the identity and finally removes and flushes the journal.
Commands inherit the kernel lock so killing the parent cannot unlock a still
running modifying child. The registered source is never given to `pg_upgrade`.
Both temporary servers receive explicit data-directory overrides, so copied
configuration cannot redirect them back onto the registered source.

Source, target, staging and disposable source-copy paths must be pairwise
disjoint in both containment directions before the authority lock or copy runs.
This includes nested destinations that would make a recursive copy include itself.
Version-2 upgrade journals bind the full selected configuration and the bytes of
the PostgreSQL tools, copy/validation executables and file arguments used by
interpreter adapters. Retries reject any policy or executable change before
reusing staged output, including a `ready` publication. The same contract is
rechecked after validation. Unbound version-1 journals require explicit operator
inspection and fresh validation under the selected policy. Rewriting their
version or reconstructing a contract from a newer configuration does not establish
matching validation evidence.

A `building` journal blocks startup, even when `PG_VERSION` exists. Inspect the
failure, confirm all upgrade servers are stopped, then retry with:

```sh
harbor-db-postgres --config /etc/harbor-db/postgresql.json upgrade --retry-incomplete
```

Interrupted output is preserved under `.interrupted`; it is never deleted
automatically. A second failure with an already-preserved tree requires operator
inspection. A `ready` journal resumes publication idempotently, including after
the new identity was written. External WAL and tablespaces are intentionally
unsupported in this adapter. Staging must be on the destination filesystem.
Resumption flushes the destination parent even when a previous invocation already
renamed staging, before publishing authority or removing the journal.
The source copy, staging and preserved output require additional disk capacity.

## Supported rollback generations

A supported generation uses the same authority location and registered cluster,
the current PostgreSQL major, and an application that accepts the current schema.
The previous major's files and `previous-identity.json` are historical evidence,
**not a writable rollback target**. Selecting them after new saves would expose
stale data, so the guard rejects that selection. There is deliberately no command
to silently replace the current identity with the previous one.

This contract cannot retrofit immutable generations created before the guard.
Certify rollback targets after installing guards and application compatibility
checks. Hardware must honor PostgreSQL/filesystem flush requests; software tests
do not certify a drive's power-loss behavior.

## Restore storage isolation

Restore certification inventories the primary, selected backup and disposable
restore separately. Any primary/backup inode intersection rejects certification,
including hard links and nested bind mounts of empty directories; their union must
also remain disjoint from the restored tree. An absent primary is permitted only
on an independent executor. Immutable primary configuration links remain supported.

## Retiring source-local captures

Source-local captures pin their base backup and required WAL until explicitly
retired. After publishing and selecting a successor capture, an operator may run:

```sh
harbor-db-postgres --config /etc/harbor-db/postgresql.json retire-capture --capture-id OBSOLETE_ID
```

Both engines require `source-local-v1` and the existing exclusive repository
mutation lease; live capture, inspection, preparation or certification blocks
retirement. The selected generation cannot be retired. Completed and abandoned
unselected captures may be retired without a live primary, including expired pins.
The operation binds the repository identity and every present carrier digest in
an immutable `.retired` journal before removing capture metadata, WAL inventory,
snapshot and finally its pin. Interrupted removal resumes from this journal;
changed retry carriers reject before further deletion. Retired identifiers cannot
be selected or republished. Subsequent ordinary pruning can reclaim the unpinned
backup and WAL under its existing retention policy. Scheduled application pruning
does not retire these recovery pins.

## Recovery SQL checks

Managed preparation may import a transported independent receipt through
`recoveryPreparation.offHostReceiptImportFile`. Its runtime source, every ancestor
and every alias must be root-controlled with no group/other write permission. The
root hook uses `stage-off-host-receipt` to read a pinned, single-link regular inode
under a 16 MiB limit into a fresh private root-owned `0400` file. Only that private
copy is passed to systemd's `LoadCredential`; a rejected source never starts the
preparation worker. The private staging directory is cleaned after success,
failure or interruption after the worker has stopped. If stopping fails because
the transient unit was never created or has already been collected, cleanup also
accepts a successful systemd readback of `ActiveState=inactive` or `failed`. Active,
transitioning, empty or unavailable readback preserves the staged copy for a
potentially surviving worker. Cleanup preserves the original dispatch, worker or
interruption failure status, including with shell `errexit` enabled. Missing
evidence retains the existing
independent-restore readiness boundary.

Configured recovery record, compatibility and transition custody inventory checks
share one command builder and execute as one prepared SQL
statement inside an explicit read-only transaction. `psql`'s zero-parameter
`\bind` uses PostgreSQL's extended protocol, which rejects multiple statements
before executing any part of the input. The SQL `PREPARE` grammar also excludes
transaction control, `CALL`, `DO` and client meta-commands. Modifying queries are
rejected by the read-only transaction. Quoted semicolons, comments and escaped
strings are parsed by PostgreSQL itself. A fixed, explicitly escaped connection
string pins timezone, date/bytea formatting and standard string quoting before
parsing, so typed SQL literals have consistent semantics. It preserves the
endpoint's `default_transaction_read_only` setting for certification admission;
only the explicit validation transaction is forced read-only. A disposable
endpoint whose unmodified session default is writable rejects certification
before receipt publication. Inherited
libpq routing/options stay scrubbed. Checks require `psql` 16 or newer; older
clients fail admission rather than falling back to simple-query execution.
Custody queries retain the existing `postgres` worker identity, scrubbed worker
environment, inherited leases and configured worker deadline, with a 30-second
statement timeout. Their JSON path-array admission still runs after SQL execution.

## Consumer storage authority

`harbor-db-resource` adopts an application binding (backend, schema, paths or
endpoint identity) and markers in its explicitly selected storage directories.
The independent authority directory must be outside those guarded directories.
Consumers can run `check` through Harbor DB project runners and on every runtime
unit start. Initialization is never inferred from a missing directory or marker.

```sh
harbor-db-resource --config /etc/application/storage-authority.json adopt \
  --identity VERIFIED_STORAGE_ID
harbor-db-resource --config /etc/application/storage-authority.json check
```

Verify the data before choosing the adoption identifier. Backend or root changes
are explicit cutovers requiring an independently verified authority transition;
ordinary `adopt` cannot overwrite an existing authority. Identity guards do not
establish content freshness for a manually restored backup with the same identity;
that requires the consumer's record/revision checks.

An optional `consumer_command` argv runs a read-only application validator under
the authority lock. Its executable and every alias hop must be immutable Nix
store files. The authority's `user` selects the unprivileged resource account;
root-run inspection drops UID, GID and supplementary groups before executing the
validator. Missing or root identities fail closed. Already-unprivileged callers
must match the selected account; older manifests without `user` may use only the
caller's own unprivileged account. Nix cutover authorities default `user` to the
resource's service user. Hand-written transition authority manifests with a
validator must add that account explicitly before root-run planning.

Validator execution uses the bounded storage worker: a 30-second default deadline
(`consumer_timeout_seconds`, from 1 to 300 seconds), a separate 1 MiB limit on
stdout and stderr, and process-group cleanup on completion or rejection. Timeout
and oversized output release the inspection lease without publishing authority.

Validators receive a clean environment containing only `USER`, `LOGNAME`,
`LANG=C.UTF-8`, `HOME=/`, and a `PATH` limited to the immutable executable's
directory. Their working directory is `/`; pass configuration and data paths
explicitly in the immutable argv rather than relying on operator environment or
relative paths. Executable aliases retain their original argv spelling for
multi-call binaries. The validator returns JSON with `binding`, `directories`,
`required_files`, and optional `minimum_counters` (nonnegative integer witnesses).
The binding and directory identities must match adoption. All files established
at adoption remain mandatory; later files may be added. Every adopted counter
must still be present with an equal or higher value. Consumers define what those
monotonic witnesses mean, such as dataset revisions or permanent saved-review
identities. These floors protect adoption-time evidence, not freshness against
every subsequent save or arbitrary cross-file restores.

Custody certification and runtime namespace admission include the validator's
additional directories and binding. Restore roots must cover the complete sorted
directory union for consumer-backed authorities; every root participates in byte
verification, inode isolation, metadata checks and custody publication. Consumer
contract changes during certification reject publication. Existing receipts for
static authorities retain their declared directory ordering.

Transition target custody uses the expanded `target_authority` already retained
in the preparation journal, excluding only its adoption identity field. It does
not invoke the validator again to select a different corpus. Preparation,
commit and writer release therefore retain custody for the same authority that
will be published; live authority verification still checks the consumer output
against that adopted contract.

`serve -- /absolute/path/to/consumer ...` repeats validation and execs the consumer
with a shared authority lease inherited for its lifetime. Offline adoption cannot
race that writer. The validator, adoption operation, and runtime must use the
same bind mounts, resource permissions, and explicit configuration paths.

## Backup publication

When continuous WAL reception and base backup are both enabled, their configured
replication slot names must differ. A slot can have only one active receiver;
sharing it with the long-running WAL receiver prevents `pg_basebackup` from
acquiring its WAL stream. Nix evaluation rejects this configuration for local and
remote targets; single-operation targets may retain the unused slot setting.

`harbor-db-durable write` fsyncs a temporary file, atomically replaces metadata and
fsyncs its parent. `publish-file` and `publish-tree` flush immutable offline backup
content before rename and then flush affected parent directories. Writers own
concurrency exclusion. Trees containing symlinks or special files are rejected.

The PostgreSQL backup module uses `pg_receivewal --synchronous`; this flushes
received partial segments immediately. It does not make application commits
synchronously replicated to the backup receiver. Base backups have unique names,
pass `pg_verifybackup` and are never replaced by another run on the same day.
Retention shares the backup writer lock, preserves the two latest complete base
backups and WAL from every retained manifest's earliest start. Unknown manifests,
timelines and segment-size mismatches retain WAL conservatively.
`targetSettings.walSegmentBytes` accepts only powers of two from 1 MiB through
1 GiB, matching the runtime pruning contract. Invalid sizes reject during Nix
evaluation before the prune service can block dependent base backups.
All completed backup directories, including expiration candidates, are classified
before deletion. Redirected/non-directory entries, malformed start/end LSNs,
unknown timeline values and missing manifests preserve both backups and WAL.

## Collation-version drift

After a glibc upgrade a libc-provider database records the old
`datcollversion` while `pg_database_collation_actual_version(oid)` reports the
new one. Indexes ordered by that collation may be silently corrupt until
rebuilt, and PostgreSQL only warns. `harbor-db-postgres collation-refresh`
finds drifted connectable databases (template0 is skipped; C/POSIX databases
have no version and never drift), lists the affected indexes and rebuilds them:

```sh
harbor-db-postgres --config /etc/harbor-db/postgresql.json collation-refresh --dry-run   # JSON-line plan
systemctl start harbor-db-postgresql-collation-refresh                                   # repair
```

Flags: `--dry-run`, `--fail-on-drift` (with `--dry-run`, exit nonzero on drift),
`--verify`, `--database NAME` (repeatable), `--socket-dir`, `--port`.
`services.harbor-db.postgresql.collation.check.enable` (default false) with
`collation.check.schedule` (default `daily`) runs the read-only check from a
timer; the unit fails while any database drifts and never reindexes.

Affected indexes (every access method, hash included) are those with a key
column (INCLUDE columns excluded) whose collation is the drifted database
default or any explicit libc/ICU collation whose `pg_collation.collversion`
drifted, plus every partial or expression index (collations used inside
predicates and expressions are not recorded, so this errs toward rebuilding).
After the rebuild the database version and each drifted named collation are
refreshed (`ALTER COLLATION ... REFRESH VERSION`). Check constraints, partition
bounds and generated columns that depend on ordering are not analysed.

Btree and most indexes use `REINDEX INDEX CONCURRENTLY`: no write blocking, but
two full scans and extra disk and IO per index, so run large databases in a
quiet window. Catalog, exclusion-constraint and invalid indexes cannot be
rebuilt concurrently and use plain `REINDEX INDEX`, which blocks writes to the
table for the rebuild. Invalid `_ccnew`/`_ccold` leftovers of an interrupted run
are dropped first. `ALTER DATABASE ... REFRESH COLLATION VERSION` runs only after
every index of that database succeeded, so an interrupted run stays visible as
drift; progress is kept in `<stateDir>/collation-refresh-*.json` and a rerun
skips indexes already rebuilt. `--verify` runs `bt_index_check` only where
`amcheck` is already installed and reports `verify-skipped` otherwise; the
extension is never created. The command exits nonzero if any database failed.

## Verification

Native Linux tests:

```sh
PYTHONPATH=python python3 -B -m unittest discover -s tests -p '*lifecycle.py'
```

Focused flake checks: `postgres-lifecycle-eval`, `postgres-lifecycle-test`,
`pg-backup-eval`, `postgres-interrupted-upgrade` and `postgres-crash-rollback`
(which also simulates collation drift and checks the check and refresh units).
The VM checks use disposable databases, acknowledged structured saves, process
SIGKILL, abrupt VM termination, compatible generation switching, missing storage
and interruption after real initdb creates PG_VERSION. Application end-to-end
crash and restore drills remain the consumer's responsibility.
The adoption VM exercises the generated pre-switch hook with an independently
inspected fixture identifier through the real NixOS switch executable. Wrong
identity aborts before stopping the primary, inspection-only actions do not
adopt, and later guarded switches coexist with the writer's shared lease.

Hosted qualification selects the lifecycle package and each check explicitly in
`simit.toml`. The generated matrix preserves the existing Rust format, test,
Clippy, documentation and module gates. Four shared-store groups (`aarch64`,
`x86-rust`, `x86-native-vm` and `x86-lifecycle-vm`) run concurrently, so each
builds the shared Rust closure once; every member still retains its exact
source revision, installable, build log and JSON output map. x86 jobs require
hosted KVM. A queued or skipped job is not acceptance evidence.

The default `harbor-db` package and its aliases are portable uncached builds.
`harbor-db-cached` explicitly opts into the managed compiler-cache transport and
is qualified as an additional installable alongside the original thirteen gates.
Hosted jobs provision a root-owned sticky `/var/cache/sccache` and explicitly
expose it to Nix sandboxes before restarting the daemon. The cached package
selects that disk cache root explicitly: mounting it alone does not supply a
transport to the pinned Harbor RS wrapper. The wrapper creates and admits only its
versioned, mode-0770 `nixbld` namespace, and still prefers the host Redis socket
when present. The managed-transport requirement and per-sandbox compiler daemon
remain enforced; the hosted disk cache needs no credentials.
Branch pushes run only on `trunk`; pull requests qualify their exact head once.
A newer push supersedes an older run of the same branch and event; qualification
requires a complete successful run for the exact PR head, rather than combining
jobs across runs.

The workflow was generated with Simit commit
`de3325be17104fbdb0d70f1da38149d4016f1790`, which supports grouped matrices.
Grouped jobs build the engine named by the repository variable
`SIMIT_CI_ENGINE_REVISION`, enrolled at the same revision. Regenerate and verify:

```sh
simit init ci --platform github --ci-provider actions --runtime nix
simit init ci --platform github --ci-provider actions --runtime nix --check --diff
```
