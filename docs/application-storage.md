# Application storage contracts

Harbor DB owns database infrastructure, durable backup publication and authority
transitions. Applications retain schema migration, coherent capture, import and
record-level validators. Consumers declare paths, identities and deployment policy.

## Dedicated PostgreSQL provisioning (version 1)

`services.harbor-db.projects.<name>.postgres.provision` declares `database`,
`ownerRole`, `runtimeRole`, `runtimeOsUser`, optional `ownerOsUser` (root), and
schema/current/default privileges. Enable it explicitly. It may be used without
enabling a lifecycle migration. Database and roles must be unique across projects.
`lib.applicationProvisioning = 1` advertises this interface.

`schemaUnits` require the generated provision service. `runtimeUnits` require
the generated permissions service, which runs after the explicit schema units.
Harbor DB does not invoke schema migrations itself. `tables` overrides exact
privileges for existing named tables, including append-only tables. New tables
initially receive the declared default privileges; all owner migrations must run
with application writers stopped and reconcile named exceptions before startup.

The control command is `harbor-db-provision --config MANIFEST apply|check`.
`check` is read-only and returns 2 for privilege drift. The manifest records
version 1, a policy and an explicit local OS-peer control endpoint. Neither
passwords nor credential contents belong in this manifest. A persistent
cluster-scoped inode serializes apply across connection changes.

Existing databases owned by another role, privileged/inherited roles and objects
owned by other roles are rejected. Provisioning does not perform implicit
adoption or role takeover. Explicitly validate and adopt existing storage through
the owning recovery workflow before using this contract.

Checks cover schema ownership, role membership, exact table/sequence privileges,
column grants, grant options and defaults for future owner-created objects.
PostgreSQL 18 is the qualified privilege surface, including MAINTAIN. Ordinary
provisioning services retain the PostgreSQL fence startup condition; intentional
maintenance under a fence uses the declared OS-peer control channel explicitly.

## Executed application backups (version 1)

`projects.<name>.backup` declares a user, a dedicated reader group, backup root,
absolute capture/restore/verify/cleanup commands and bounded execution limits.
The systemd service loads credentials from runtime paths. Its timer is independent
of schema migration or backend selection. `lib.applicationBackup = 1` advertises
the interface. Physical cluster backups remain a separate `pgBackup` contract.

Arguments equal to `{backup}` and `{workspace}` are substituted as single argv
elements, without a shell. Capture creates a new directory and writes all artifacts,
plus `capture.json` with exactly `version = 1`, `consistency` (`quiesced` or
`shared_exported_mvcc_snapshot`) and `semantic_sha256`. The application chooses
the canonical complete-record representation whose hash identifies this snapshot.
Restore targets only the newly allocated private workspace. Verify emits exactly
`version = 1`, `status = "verified"` and the same `semantic_sha256` after executing
complete schema/record/revision/sequence comparison. A row count alone is insufficient.
Cleanup must be idempotent, stop and drain every process it started, and is invoked
even after restoration fails. Failed cleanup preserves its private workspace.

`harbor-db-postgres-drill --package POSTGRES restore|cleanup BACKUP WORKSPACE`
supplies a reusable logical PostgreSQL restore adapter for `database.dump`.
It creates `WORKSPACE/socket`, listens only on that private Unix socket at port
55439, and restores database `harbor_restore`. Applications supply their semantic
verifier, which connects to this disposable endpoint. No live database is dropped.

`harbor-db-application-backup --config MANIFEST capture [--attempt ID]` records
every artifact and executable hash, executes restoration and verification, flushes
the accepted tree, then publishes it and advances `LAST_SUCCESS`. An interruption
can leave a valid unpublished point; the previous success remains usable. Attempts
are never overwritten. `inspect BACKUP` rechecks bytes, contracts and freshness.

`certify BACKUP --state PRIVATE_STATE` executes a copied backup on another machine.
The private state directory and persistent `lock` inode must already exist. Its
receipt binds the original acceptance hash, actual machine identity, executables
and semantic identity. Execution on the original machine cannot claim independence.
Copying bytes or trusting an application's unexecuted assertion is insufficient.
Adapters must retain inherited leases through child work and drain descendants
before returning; generated systemd units additionally retain control-group cleanup.
