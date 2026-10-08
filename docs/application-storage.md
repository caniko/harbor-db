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
