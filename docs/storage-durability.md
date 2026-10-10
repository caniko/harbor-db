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

## Offline major upgrades

Set `upgrade.oldPackage`, `oldDataDir`, `initdbArgs`, optional `extraConfig`, and
`validateCommand`. The validation argv receives the staging directory as its
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
the authority lock. It returns JSON with `binding`, `directories`,
`required_files`, and optional `minimum_counters` (nonnegative integer witnesses).
The binding and directory identities must match adoption. All files established
at adoption remain mandatory; later files may be added. Every adopted counter
must still be present with an equal or higher value. Consumers define what those
monotonic witnesses mean, such as dataset revisions or permanent saved-review
identities. These floors protect adoption-time evidence, not freshness against
every subsequent save or arbitrary cross-file restores.

`serve -- /absolute/path/to/consumer ...` repeats validation and execs the consumer
with a shared authority lease inherited for its lifetime. Offline adoption cannot
race that writer. The validator, adoption operation, and runtime must use the
same bind mounts, permissions, and configuration environment.

## Backup publication

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
All completed backup directories, including expiration candidates, are classified
before deletion. Redirected/non-directory entries, malformed start/end LSNs,
unknown timeline values and missing manifests preserve both backups and WAL.

## Verification

Native Linux tests:

```sh
PYTHONPATH=python python3 -B -m unittest discover -s tests -p '*lifecycle.py'
```

Focused flake checks: `postgres-lifecycle-eval`, `postgres-lifecycle-test`,
`pg-backup-eval`, `postgres-interrupted-upgrade` and `postgres-crash-rollback`.
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
Clippy, documentation and module gates, limits parallel jobs to two, and retains
the exact source revision, installable, build log and JSON output map. VM jobs
require hosted KVM. A queued or skipped job is not acceptance evidence.

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
Push and pull-request runs share a branch concurrency group by Simit policy.
One run can be canceled when the other starts; qualification requires a complete
successful run for the exact PR head, rather than combining jobs across runs.

The workflow was generated with Simit commit
`beea3e284a613d46468779bd998e51be2d63566c` (Simit PR #26), which supports
`[ci.nix_build].only` for Rust flakes. Regenerate and verify with that capability:

```sh
simit init ci --platform github --ci-provider actions --runtime nix
simit init ci --platform github --ci-provider actions --runtime nix --check --diff
```
