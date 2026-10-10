# Source-local physical recovery producer

## Decision

Recovery capture runs on the source using its existing PostgreSQL OS account and
local peer SQL endpoint. WAL is received locally, or through a separately
qualified localhost relay, so the existing writer fence remains authoritative.
With `pgBackup.targetSettings.sourceLocalRecovery.enable`, select `role = "both"`
and a loopback `source.hostName`. The module generates SCRAM replication HBA
rules for that address (`localhost` admits both loopback families) without adding
a firewall opening. An empty `sourceSettings.listenAddresses` also derives TCP
listeners for those loopback families; an explicit list remains unchanged.
Source and target still require the replicator password file.
Authenticated orchestration and artifact transport connect the source to an
independent certifier. Fleet endpoints and credentials remain consumer policy.

## Additive repository contract

The legacy `LAST_SUCCESS` ISO timestamp and `base/<backup-ID>/` publication remain
intact. Recovery opts into `source-local-v1`; omitted protocol configuration uses
the existing completed-ID/sidecar contract. The selected capture resides in
`recovery/SELECTED`, referring to immutable `recovery/captures/<capture-ID>.json`.
An immutable `recovery/snapshots/<capture-ID>.json` carries the corresponding
source snapshot. Both are durable before the single selection commit; retries
preserve their bytes and the original completion timestamp. Recovery readers
consume the selected generation's snapshot. The configured legacy snapshot path
continues to locate the evidence lease and legacy-protocol snapshot.
Recovery metadata and retention pins are outside `base/`.
The capture target remains the first `pg_switch_wal()` ending LSN. PostgreSQL's
inclusive recovery target compares WAL record start positions, so finalization
also completes and waits for the following receiver segment containing a genuine
post-target checkpoint record. This makes the frozen target reachable without
depending on a partial WAL carrier or moving the target during a retry.
Managed source-local preparation requires `requireWriterFence = true` at module
evaluation. Read-only independent certification and retired bootstrap enrollment
can disable the local fence requirement while consuming the same bound capture.

A capture references a verified service-produced physical backup and its actual
manifest digest, primary identity, major, timeline and stop LSN. Its fenced record
hashes and durable target LSN cover acknowledged changes after backup completion.
Publication preserves version-1 recovery binding fields and JSON/hash conventions.
New capture bytes produce new evidence hashes; existing receipts are not rewritten.

The source-local producer validates the live retained fence before capture. It
serializes with backup publication, pruning and recovery inspection through the
persistent `locks/mutate` anchor. When taking both backup anchors, the order is
`locks/mutate` then `BACKUP_LOCK`; source finalization takes writer exclusion
before backup anchors and evidence publication afterward. Worker subprocesses
inherit the owned leases. Descriptor close releases authority; anchor files are
never replaced or removed.

An immutable retention pin protects the selected backup and its required WAL
floor before metadata or selection becomes visible. Interrupted publication
retains the pin and immutable intent for explicit retry. Pins outlive ordinary
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
