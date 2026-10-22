# Source-local physical recovery producer

The lifecycle `stateDir` must be a dedicated normalized absolute directory.
The filesystem root, relative paths, dot components, repeated or trailing
separators and whitespace/control characters reject before tmpfiles can assign
PostgreSQL ownership or permissions.

## Decision

Recovery capture runs on the source using its existing PostgreSQL OS account and
local peer SQL endpoint. WAL is received locally, or through a separately
qualified localhost relay, so the existing writer fence remains authoritative.
With `pgBackup.targetSettings.sourceLocalRecovery.enable`, select `role = "both"`
and a loopback `source.hostName`, with `targetSettings.receiveWal.enable = true`
for continuous post-target WAL coverage and `targetSettings.baseBackup.enable = true`
for fresh verified base-backup production. Disabling either operation fails
evaluation because capture finalization requires both. Independent WAL-only
targets remain supported outside source-local recovery. The module generates SCRAM replication HBA
rules for that address (`localhost` admits both loopback families) without adding
a firewall opening. An empty `sourceSettings.listenAddresses` also derives TCP
listeners for those loopback families; explicit lists also retain the required
source-local loopbacks, without duplicate addresses.
Source and target still require the replicator password file.
Source-local WAL reception requires and starts after the local primary and its
role/password setup service. Each local base backup requires and starts after
the primary and WAL receiver, whose pre-start step establishes the continuous
receiver slot. This also orders persistent timer catch-up during boot. Remote
targets retain their independent service dependencies.
Source-local recovery requires at least two replication slots and three WAL
senders: the continuous receiver holds one slot/sender while a streaming base
backup uses the second slot and two concurrent connections. Evaluation checks
both `sourceSettings` and the effective PostgreSQL settings, so an override
cannot silently reduce this capacity.
Every source-only or combined source role also requires at least one effective
WAL sender and replication slot when source-local recovery is disabled. Target-only
hosts retain independent source settings; effective zero/negative capacity on a
local source is rejected before generating an unusable replication service.
The effective `services.postgresql.settings.wal_level` must be `replica` or
`logical`. `minimal` cannot produce streaming physical recovery and PostgreSQL
will not start with nonzero WAL senders; source-role evaluation rejects it even
when selected by an explicit PostgreSQL override.
Every enabled WAL-receiver and base-backup slot name must contain 1–63 lowercase
ASCII letters, digits or underscores. Names are validated before producer
services are admitted, including for independent targets. The length bound
prevents PostgreSQL clients from silently truncating different names to the same
slot. When both operations are enabled, their validated names must differ.

The source-local plain-format producer supports only the primary's built-in
tablespaces. Source setup, receiver startup and each base-backup startup inspect
the actual `PGDATA/pg_tblspc` directory before replication. Missing, redirected,
unreadable or nonempty inventories reject. External tablespaces require a
separately qualified mapping/restore contract; they are never copied back into
authoritative same-host locations by this mode. The gate reproduces the upstream
unmapped same-host failure with a real PostgreSQL 18 tablespace and proves
rejection leaves primary records queryable.
All recovery snapshot and receipt outputs reserve a separate evidence namespace,
including the default legacy protocol.
They cannot select `recovery/`, `base/`, `wal/`, `locks/`, `BACKUP_LOCK`,
`LAST_SUCCESS` or the snapshot parent's `recovery.lock`, `preparation.lock` and
`preparation.json`. Nix and both runtime
engines reject these aliases before publication, including off-host receipt
imports. The dedicated `<backupRoot>/evidence/` subtree remains supported.
The repository root, snapshot, local-receipt and off-host-receipt paths exclude the declared
PostgreSQL data, lifecycle-state and startup-inhibition trees in both containment
directions. Nix and both runtimes reject these paths before capture, preparation
or receipt publication, including under the legacy protocol. Recovery evidence
cannot replace cluster control files, authority identity or fence/startup policy.
Every generated physical backup root must also be outside all enabled custody
corpus/authority namespaces and enrolled PostgreSQL data, lifecycle-state and
startup-inhibition roots, including on target-only hosts. Equality and either
containment direction reject before WAL reception, publication or pruning can
mutate certified storage; disjoint per-source siblings remain supported.
A read-only fence-only policy may omit a backup repository. Evidence-producing
operations still require an explicit root, as does source-local policy admission.

Writer-fence `setupUnits` must be unique and exclude `postgresql.service`, which
runtime inhibition already includes as the primary unit. Evaluation rejects
duplicate/primary entries before deployment; empty lists and distinct extra setup
units remain supported.
When a backup target shares a host with an enabled primary, every derived
per-source backup root must be normalized and disjoint from the primary data
directory in both containment directions. Equality, nested outputs and ambiguous
path spellings fail evaluation before backup services can copy into their own
input tree. A shared parent is permitted when the derived root and primary are
disjoint siblings; a separate target without an enabled primary remains supported.
When the PostgreSQL lifecycle module is present, source-local recovery enrolls
`replicator` in `writerFence.replicationRoles`, preserving configured peer roles
without duplicates. A forced policy that removes `replicator` from an enabled
writer fence fails evaluation before the receiver can lose replication admission.
Authenticated orchestration and artifact transport connect the source to an
independent certifier. Fleet endpoints and credentials remain consumer policy.

## Additive repository contract

The legacy `LAST_SUCCESS` ISO timestamp and `base/<backup-ID>/` publication remain
intact. Recovery opts into `source-local-v1`; omitted protocol configuration uses
the existing completed-ID/sidecar contract. The selected capture resides in
`recovery/SELECTED`, referring to immutable `recovery/captures/<capture-ID>.json`.
An immutable JSON `recovery/captures/<capture-ID>.wal-inventory` maps every required WAL
segment filename to its streamed SHA-256 digest. The corresponding immutable
`recovery/snapshots/<capture-ID>.json` binds the inventory carrier's exact bytes
through `wal_inventory_sha256`, alongside the metadata binding. These carriers
are durable before the single selection commit; retries
preserve their bytes and the original completion timestamp. Recovery readers
consume the selected generation's snapshot. The configured legacy snapshot path
continues to locate the evidence lease and legacy-protocol snapshot.
Recovery metadata and retention pins are outside `base/`.
Final publication rechecks both the frozen completion timestamp and the physical
manifest's age after synchronizing immutable carriers and selector scratch, just
before replacing `SELECTED`. An expired attempt preserves the previous selector's
bytes and inode plus its own immutable pin. Expired pins cannot be rewritten to
refresh their timestamp; a new capture ID and a sufficiently fresh backup are needed.
The capture target remains the first `pg_switch_wal()` ending LSN. PostgreSQL's
inclusive recovery target compares WAL record start positions, so finalization
also completes and waits for the following receiver segment containing a genuine
post-target checkpoint record. This makes the frozen target reachable without
depending on a partial WAL carrier or moving the target during a retry.
Selection rechecks the manifest-derived external WAL range through that following
segment. Every required carrier must remain a complete regular file with exactly
the captured bytes; removal, truncation, redirection or same-length corruption
invalidates readiness even after certification. Both local and independent
receipts bind the inventory. Captures lacking that byte-level binding must be
recaptured and recertified; readers do not invent or upgrade accepted evidence.
Local restore acceptance must name the declared source host, while off-host
acceptance must name an independent host. Copying an independent receipt into
both evidence paths cannot satisfy the two execution requirements.
Snapshot, local receipt and off-host receipt paths must be distinct at admission.
Only a certifier identified as an independent host may select the off-host path
directly as its output; local inspection, capture and import enforce distinctness.
Managed source-local preparation requires `requireWriterFence = true` at module
evaluation. Read-only independent certification and retired bootstrap enrollment
can disable the local fence requirement while consuming the same bound capture.

A capture references a verified service-produced physical backup and its actual
manifest digest, primary identity, major, timeline and stop LSN. Its fenced record
hashes and durable target LSN cover acknowledged changes after backup completion.
Publication preserves version-1 recovery binding fields and JSON/hash conventions.
New capture bytes produce new evidence hashes; existing receipts are not rewritten.
The immutable capture metadata and source snapshot also bind `source_machine_sha256`,
the digest of the installed machine identity. Certification records and validates
`executor_machine_sha256` against that source binding, in addition to descriptive
hostnames. Off-host acceptance requires a distinct valid machine digest; a UTS
namespace or renamed hostname on the source cannot establish independence.
Capture retries preserve the machine binding. Captures predating this binding
require a new capture identifier and fresh certification; legacy local snapshots
remain locally admissible but cannot establish off-host recovery without a fresh
machine-bound source snapshot. Independent machines must have distinct installed
machine identities, including restored/cloned operating-system images.

The exported native lifecycle package is statically linked: its smoke check
rejects an ELF interpreter or dynamic dependency on every operator executable.
An unprivileged certifier's `LD_PRELOAD` cannot interpose namespace admission,
hostname/machine-id reads or receipt publication. Direct Cargo development builds
and the Python cross-engine peer are not production packages.

Installed machine identity admission requires Linux 6.13 or newer with pidfd
namespace ioctls. Both engines obtain the actual calling thread's user, mount and
UTS namespaces through its kernel pidfd; the user namespace and the mount/UTS
owning user namespaces must be the initial host user namespace. Root-created
systemd private mount namespaces remain admissible. Caller-created user/mount/UTS
namespaces are rejected before hashing `/etc/machine-id`, even when that file,
`/proc/self/uid_map`, or the entire PID/proc view has been substituted. Unsupported
kernel interfaces fail closed. Namespace identifiers and ioctl numbers use the
[Linux pidfd UAPI](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/pidfd.h)
and [nsfs UAPI](https://github.com/torvalds/linux/blob/v6.18/include/uapi/linux/nsfs.h).

The source-local producer validates the live retained fence before capture. It
serializes with backup publication, pruning and recovery inspection through the
persistent `locks/mutate` anchor. When taking both backup anchors, the order is
`locks/mutate` then `BACKUP_LOCK`; source finalization takes writer exclusion
before backup anchors and evidence publication afterward. Worker subprocesses
inherit the owned leases. Descriptor close releases authority; anchor files are
never replaced or removed.

An immutable retention pin protects the selected backup and its required WAL
floor before metadata or selection becomes visible. Interrupted publication
retains the pin and immutable intent for explicit retry. After WAL reception,
the completed inventory freezes separately without rewriting the earlier pin.
An accepted retry preserves that inventory and rejects changed WAL. Pins outlive ordinary
age/top-two retention. Malformed or uncertain pin state preserves the repository.

## Capture and acceptance boundaries

1. Complete the real generated base-backup service and verify its manifest.
2. Acknowledge subsequent inserts, updates and deletes at the source.
3. Establish and inspect the existing writer fence.
4. Capture deterministic full-record hashes and a durable WAL target under that
   fence. Administrative WAL switching is separate from read-only record queries.
5. Require complete receiver-produced WAL through the target before publishing
   the capture and source snapshot.
6. Transport the selected physical backup, WAL, metadata and source snapshot
   unchanged to the independent certifier.
7. Replay there, compare exact nonempty records including the post-backup changes,
   and import the bound receipt before admitting preparation.
   Certification inventories device/inode identities for the restored cluster and
   available authoritative/selected-backup trees. Shared files or directories reject
   even when hard links or bind mounts preserve different canonical path spelling;
   redirected or special backup/restored entries reject too. Generated primary
   configuration links may target regular files; those target inodes join the
   protected set. Root inode aliases reject before scanning a bind-mounted primary
   as a disposable copy. The authoritative tree may be
   absent on the independent host; the transported selected backup remains protected.
8. Keep the retained fence until explicit stopped-primary thaw. Capture and
   certification do not start, stop or thaw the primary.

Negative qualification must include missing necessary WAL, metadata/capture drift,
competing prune/publication authority and an interrupted finalizer's explicit retry.
Schema-86 acceptance is preserved; schema 87 remains a separate qualification.

## Implementation and qualification status

The generated backup-service lifecycle is executed and qualified. The separate
source-local producer protocol is being implemented and requires its own passing
independent-restore gate before it can establish recovery-ready admission.

The byte-frozen PR14 Python runtime remains an independently hashed oracle under
`tests/oracles/pr14/`. Its baseline hashes and all 174 original regression methods
remain intact. The source-local extensions carry separate exact hashes and
required gates in `tests/runtime-extensions.toml`; declaration is static admission,
not execution qualification. CI runs both the retained oracle and extended Python
adapter. Physical recovery interoperability still requires the distinct-host gate.
