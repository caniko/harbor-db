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

Roles cannot inherit or be granted to another role. Runtime receives database
CONNECT and schema USAGE, without database CREATE or TEMP privileges. A schema
owned by an unrelated role cannot be taken over implicitly. PostgreSQL's initial
`public` schema owned by `pg_database_owner` is the explicit initialization case.

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
The coordinator forwards its leases to every adapter through inherited descriptors
listed in `HARBOR_DB_LEASE_FDS`; adapters forwarding work must retain them too.
Receipt output is bounded in memory; diagnostic output is discarded rather than
spooled into persistent files. This does not authorize secrets in artifact dumps.

An interrupted point may be retried explicitly with `--retry-incomplete`. Its
manifest and executable hashes must match the original attempt. Cleanup drains
the old disposable restore first; partial bytes and intent are moved into a private
abandoned directory before a fresh capture. Existing accepted points are immutable.

## Backend transitions (version 1)

`projects.<name>.transition` and `lib.applicationBackendTransition = 1` expose the
shared journal engine. Source and target resource manifests identify one authority
directory, retained storage roots and different backend bindings. Applications
supply import, complete source/target parity and post-release health workers.
Commands run under explicit service accounts and receive single argv substitutions
for `{backup}`, `{source}`, `{target}` and, after binding, `{candidate}`.

The root coordinator installs persistent systemd client barriers before stopping
writers and taking the exclusive authority lease. Legacy service units are included
explicitly. PostgreSQL transitions borrow the existing capability-3 writer fence;
only local OS-peer control SQL remains available. An importer must use that control
path, for example a `postgres`-owned worker using `SET ROLE` for the dedicated
schema owner. It cannot open application HBA access or release the borrowed fence.
No transition command starts PostgreSQL or application services.

The sequence is:

1. `plan --candidate CONTRACT [--writer-fence-token TOKEN]` binds a realized
   immutable transition manifest before building the full system closure.
2. `prepare` installs and verifies effective client barriers, drains writers,
   captures/restores the source, and waits for independently executed restore
   evidence. A repeated call uses the same immutable intent and retained snapshot.
3. Import runs once per successful preparation; interrupted imports must resume
   idempotently for that snapshot. Whole-primary PostgreSQL recovery evidence is
   refreshed by the owning recovery coordinator **after** import. Preparation
   retains phase `imported` while that external acceptance is pending; retry does
   not re-import successful target data. Existing physical recovery policy is
   retained, including off-host evidence when required.
4. `bind-candidate --candidate GENERATION` binds the exact realized system after
   successful preparation. Mandatory preflight admits the prepared target with
   writers inhibited. Pre-switch admission additionally compares the actual
   NixOS-supplied generation path, preventing a different system with the same
   manifest from using the receipt.
5. Activate the bound generation using the consumer's existing activation lease.
   `commit` revalidates independent proof, complete parity, target corpus and
   token-bound whole-primary recovery evidence. Authority and target corpus
   custody publish through a journaled compare-and-swap. Client barriers remain.
6. `enable-writes` revalidates evidence and persists the point of no return before
   removing client inhibition. The root-owned startup policy admits only the exact
   released generation, including legacy units without a resource wrapper.
   Retired unit names remain inhibited. The borrowed cluster fence must still be
   released explicitly by its owner; then start clients and run real acceptance.
7. `complete` checks health and authority without requiring equality to old
   records or taking an exclusive lease from the now-running writer.
8. `retire` archives terminal journal evidence before another transition may be
   planned. The root-owned generation startup policy remains retained. Authorizing
   a new backend or generation uses another explicit qualified transition.

Before writer enablement, `abort` requires the exact retained source generation
selected with clients still inhibited and intact source semantic evidence. It
restores old authority/custody using compare-and-swap, preserving target bytes.
After writer enablement, abort is forbidden even if no write has yet been observed:
reversal requires a fresh reverse transition. Release and abort can resume after
an interruption; neither rewrites foreign identity or startup policy.

Consumers using mandatory filesystem/corpus admission provide
`targetCustodyManifest` (a target resource entry without `transition_manifest`) and
set `cutover.resources.<name>.transition_manifest` to the generated transition
manifest. The shared engine publishes target custody at authority commit; normal
startup continues through the existing resource/cutover guard. Database-owned
corpus references are queried read-only and validated against complete target bytes.

Qualification retains all existing Harbor DB gates and adds dedicated provisioning
evaluation and three systemd/VM application gates. Native regressions cover changed
ownership/default/column privileges, real PostgreSQL restores, failed publication,
changed evidence, surviving child leases, interrupted authority commit, generation
binding, source-preserving abort and forbidden rollback after writer enablement.
