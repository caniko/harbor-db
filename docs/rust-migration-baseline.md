# Rust migration: PR #14 baseline

The runtime port targets the **complete** open [PR #14](https://github.com/caniko/harbor-db/pull/14)
at `4b2850507a2e9bdfe198caf9178ea7b99ffb03a5`, based on
`38ebf2cfbca678cc1ab79a96d6968002fb382b5e`. Its exact-head
[qualification run](https://github.com/caniko/harbor-db/actions/runs/37910539266)
passed all 22 jobs. That result qualifies the original Python/Nix composition;
Rust candidates need their own retained results.

`tests/pr14-baseline.toml` records the complete changed-file inventory, source
SHA-256 digests, all 174 original Python method identities, all 22 CI gates,
and each of the 16 Python package modules' Rust counterparts. The method
inventory is represented by its count and digest over sorted, newline-separated
fully qualified method IDs. The frozen test sources reproduce that inventory.

The baseline test rejects missing original cases, unmapped runtime modules,
unregistered public commands, modified legacy parity oracles, and PR gates
removed from either the catalog or Simit configuration. Newly added prototype
cases and new qualification gates may extend this baseline. Updating the pinned
PR head requires a deliberate source-delta review and new baseline evidence.

## Required behavioral parity

| Contract | Rust engines |
| --- | --- |
| Token-bound stopped-primary fencing, SQL exclusion including superusers, physical replication, explicit thaw | `writer_fence`, `pg_core` |
| Persistent primary/setup barriers, effective systemd readback, legacy generations and reboot, separate startup release | `startup_inhibition` |
| Capture, reusable preparation, adoption and bootstrap under one live fence; independent snapshot certification | `recovery`, `postgres` |
| Persistent lock inodes, writer/child lease inheritance, synchronized publication and bounded workers | `durable`, `process` |
| Exact role ownership and current/default/column/grant privileges; read-only drift | `provision` |
| Executed semantic backup acceptance, immutable publication, independent identity, retry and cleanup retention | `application_backup`, `backup` |
| Disposable logical restore through a private socket with deterministic cleanup | `postgres_drill` |
| Preparation/import/candidate/commit/writer-enable/complete/abort/retirement phases; generation binding and legacy-client gates | `application_transition`, `transition_manifest` |
| Journaled authority and custody, borrowed primary fence and retained post-import recovery obligations | `cutover`, `custody`, `resource` |

Static mappings establish scope. Runtime acceptance additionally requires
candidate-bound passing evidence for every applicable coverage cell, bidirectional
Python/Rust receipt and resume compatibility, and the declared native and VM
profiles. Missing coverage is reported as missing; passing the baseline test
does not qualify a runtime rewrite or a production deployment.

Native integration tests execute the retained Python modules in both directions
for filesystem authority and stopped writer fences. They compare complete
records, exact HBA and journal bytes, same-token retries, and closed-boundary
readback. A Python thaw interrupted after restoring the original selector is
finished explicitly by Rust; startup remains inhibited during the closing phase.
Executed application backups also retry failed restoration in both producer
directions while retaining abandoned evidence, immutable intent, and the completed
restore point's acceptance digest. Transition tests compare intent and exact
journal bytes for all 13 durable phases, including retained unknown obligations;
Fenced recovery snapshot and certification producers now compare exact bytes in
both directions against disposable physical-identity/SQL adapters, including
CRLF-normalized records. Both managed preparers reuse the other implementation's
receipt with backup and restore adapters forbidden, while retaining the live
fence token and leaving adoption explicit. Root-only phase execution still needs
its VM evidence. These are particular
compatibility cells, rather than promotion of all 174 Python scenarios.

Explicit configuration readers admit deployed aliases only when their resolved
target is a root-owned read-only Nix store file. The cutover dispatcher binds
workers to that validated resolved path, and its CLI admission regression checks
both the deployed immutable alias and rejection of a mutable alias. Nested
transition, database and custody policies use the same configuration boundary;
journal, receipt, selector and lock readers remain strict about redirects.

The native filesystem backend-transition VM has a Rust-owned protocol fixture.
It requires real cross-language continuation: native plan, Python capture,
crash/restart with effective barriers, native preparation after independent
restore, Python re-preparation without another import, native generation binding,
Python commit, native explicit write enablement, Python completion and native
retirement. Full peer status records and read-only journal/receipt bytes must
agree. Its passing JUnit and case-bound `backend-transition-acceptance.json` are
both mandatory. Registering this fixture does not qualify it; its exact retained
candidate must execute and validate both exports.

Native provisioning likewise uses a Rust-owned VM fixture and requires
`provision-acceptance.json` plus JUnit. Python and Rust must converge the same
immutable policy, agree on actual overgrant rejection, retain complete SQL
privilege/state projections through repeated apply, and preserve acknowledged
records and permissions through reboot. Retention compatibility includes signed
pre-epoch backup timestamps and cross-language continuation under unchanged
persistent lock inodes. When no eligible WAL segments exist, both engines finish
after the synchronized base-backup decision without synchronizing or changing
the ignored WAL/partial-transfer entries.

The retention audit found three legacy malformed-input acceptance differences.
The retention-specific lock reader now admits regular or FIFO anchors from the
opened descriptor, without waiting for a peer, reading stream data, or replacing
the inode. Generic authority locks still require regular files. A retention-only
JSON projection validates complete UTF-8/JSON syntax, matches decoded required
keys with last-member semantics, and postpones decoding until the final selected
value. Token boundaries come from the validating `IgnoredAny` stream decoder;
selected timelines and LSNs use typed scalar decoding. The shared serde_json
`raw_value` feature is deliberately disabled, because it coerces a literal
private-sentinel object into its payload in generic `Value` readers. Regressions
require those objects to retain their literal interpretation and require
object-wrapped retention fields to preserve recovery data before deletion.
This admits unused escaped high/low surrogates without rewriting manifest
bytes or changing the generic receipt/hash codec. Actual Python/native CLI
regressions require eligible deletion, intact required recovery data and repeated
peer continuation; refusal regressions still require preservation on malformed
syntax, invalid final fields, excessive native nesting and redirects.

### Approved FIFO-manifest exception

The operator chose bounded manifest admission on 2026-10-10. A FIFO
`backup_manifest` therefore makes native pruning return promptly without
deleting recovery data. Frozen Python can wait indefinitely for a writer or EOF
and may prune after receiving a valid JSON stream. That behavior is an explicit
migration exception; full unqualified Python-contract parity is not claimed.
This decision covers manifest streams only. FIFO lock-anchor and unused-surrogate
retention acceptance have separate positive/rejection regressions. Generic
surrogate-bearing receipt representation and canonical hashing are not established
by this retention-only projection.

### Receipt JSON admission

Native receipt and journal readers decode JSON containers structurally. Literal
objects named `$serde_json::private::Number` or `$serde_json::private::RawValue`
remain objects; large integer values remain exact. Decoded duplicate keys use
the final member, including escaped spellings of the same key. The canonical
encoder retains Python-compatible spacing, sorted keys and ASCII escaping.

Surviving lone-surrogate keys or values, invalid UTF-8, malformed syntax,
trailing data and excessive nesting are rejected before mutation. Rejected
receipt bytes remain untouched. Retention's projection of unused manifest
fields has its separately qualified admission policy described above.

The native lease engine coordinates authority acquisition with worker fork/exec
handshakes. This prevents unrelated children from temporarily retaining an
otherwise closed close-on-exec lock descriptor. The coordination covers the
spawn handshake; execution continues concurrently and genuine persistent lease
contention still fails immediately. Cutover fixture subprocesses use that same
handshake, including their output/status adapters and long-lived writer launches;
direct test forks would bypass lease-drop coordination. A concurrent regression
performs 10,000 exclusive-close/immediate-shared acquisitions while three worker
streams exercise bounded, piped-output and inherited-stdio execution. An unrelated
child stays alive after exec while the parent releases its original lease and a
new owner immediately acquires it; real parent and writer contention still require
`WouldBlock`.

Generic worker launches also require Rust's explicit fork/exec error-pipe
handshake. QEMU user-mode drops the `CLONE_VM`/`CLONE_VFORK` synchronization used
by the libc `posix_spawn` fast path, which otherwise permits a parent to return
before an unrelated child discards its copied lease descriptors. A no-op,
async-signal-safe child hook selects Rust's explicit handshake while the spawn
gate remains held. Lease release still closes its descriptor without unlocking
the shared open-file description; inherited authorized workers retain authority.
AArch64 acceptance requires the actual emulated regression to pass separately.
Supervisor and transition test launch adapters use the same coordinated helper
when they can overlap source authority leases; their explicit inherited-worker
lease checks remain active.

Run the admission regression with the pinned project environment:

```sh
harbor-db-test check-baseline --source . --suite tests/suite.toml \
  --baseline tests/pr14-baseline.toml

direnv exec . env RUSTFLAGS='-Clink-arg=-fuse-ld=mold' \
  cargo test --locked --features testing --test migration_baseline
```
