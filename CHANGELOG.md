# Changelog

## Unreleased

### Added

- Rust-default storage lifecycle commands with durable fencing, recovery,
  application transitions, and coordinated writer lease inheritance. Flake and
  direct module imports share native package selection; Python is a test-only
  migration peer.
- Catalog-driven, candidate-bound lifecycle qualification with detached workers,
  observer reattachment, cancellation, resource diagnostics, and retained
  semantic and JUnit evidence.
- Native NixOS package and VM checks, including physical PostgreSQL transitions,
  generated backup services, and supervisor interruption cases.
- Opt-in source-local recovery captures and retention pins, with independent
  certification and an explicitly hashed runtime-extension contract. The original
  PR #14 Python runtime is retained as a frozen oracle.

### Fixed

- Keep the generated recovery-preparation pre-switch check accepted by
  ShellCheck 0.11, which flagged its trap-only cleanup function (SC2329) and
  failed consumers' `pre-switch-checks` builds.
- Preserve graceful shutdown and post-stop hooks while transitions inhibit startup.
- Bind runtime credential sources into version-2 backup policies so forced transition
  captures receive private worker credentials without depending on a service environment.
- Enroll source-local WAL receivers in the writer fence's physical replication policy,
  preserving peer roles and rejecting overrides that remove the required receiver.
- Require application backup storage mounts before scheduled capture, including `nofail`
  and masked declared mounts, to prevent publishing hidden restore points on the parent filesystem.
- Publish version-2 application backups onto fresh root-owned inodes while running
  adapters unprivileged with private credentials and workspaces. Backup accounts
  cannot chmod or replace accepted bytes, including through retained staging handles.
- Recheck frozen capture freshness after carrier and selector scratch synchronization,
  preserving previous selection and immutable pins when final publication expires.
- Reject duplicate or nested restored corpus roots and overlapping transition
  source/target paths before identity adoption, journal publication or import.
- Publish freshly reverified final target custody at commit while retaining every
  prepared content and binding comparison.
- Include required source-local loopback listeners alongside explicit addresses.
- Protect generated primary configuration target inodes during recovery isolation
  and reject root bind-mount aliases before scanning disposable restore storage.
- Bind the hard-link regression's Python peer to the immutable machine-ID
  fixture used by packaged Cargo tests, including isolated AArch64 execution.
- Recheck every complete retained external WAL segment through the post-target
  carrier when selecting source-local recovery evidence, and require local
  acceptance to have executed on the declared source host.
- Keep reactive restore operations out of normal apply, and admit confirmed
  prerequisites before any dependent or unrelated operation can write.
- Reject externally hard-linked custody files and database-query root overrides
  in both cutover and transition inventory.
- Run transition captures as the validated backup-root owner with an aggregate
  deadline covering all separately bounded backup stages and retry cleanup.
- Reject transition-only project names outside the runtime resource grammar
  during NixOS evaluation.
- Reject hard-linked application backup artifacts before publication permission
  changes and during restore-point inspection in both engines.
- Reject ancestor/descendant backup roots and overlapping transition barriers,
  including backup/barrier overlap; each active or retired writer has one owner.
- Require unique normalized backup roots across enabled projects, preserving
  one owner for each lock, attempt and success-marker namespace.
- Bind every custody runtime writer to an active or retired transition barrier
  before preparation and inventory in both engines.
- Reject ambiguous PostgreSQL preparation and protected paths before writable
  containment checks and root-run provisioning.
- Synchronize the final reader permissions on `LAST_SUCCESS` before publishing
  its inode in both backup engines; failed file sync preserves the prior marker.
- Require optional PostgreSQL and custody transition policies to be immutable
  store references before operator dispatch, preserving recorded contract bytes.
- Reject empty or relative transition worker argv during NixOS evaluation,
  before publishing unusable transition tooling.
- Reject ambiguous custody roots, activation directories and directory-creating
  tmpfiles paths before they can bypass historical-root protection.
- Reject backup manifests with missing literal stage placeholders or invalid
  credential IDs during evaluation, before installing a permanently failing timer.
- Reject non-UTF-8 application backup artifact paths in both engines, preserving
  complete digest inventories and preventing lossy filename collisions.
- Allow real PostgreSQL initialization under registered AArch64 emulation in the
  owning-deadline fixture while still requiring its delayed restore to time out.
- Publish the controlled restore worker's complete PID atomically before exposing
  the cancellation barrier, eliminating an observed empty-carrier race.
- Coordinate the oversized detached-launcher fixture's exec with parallel workers
  so inherited writable descriptors cannot cause a transient text-busy failure.
- Reject enabled backup project names that cannot be represented by the runtime
  resource contract, including dotted, at-sign and overlength names.
- Give logical restoration the owning backup contract's complete bounded budget
  instead of a 120-second per-child probe limit; retain explicit standalone
  budgets, deadline failure cleanup and cross-engine cancellation cleanup.
- Require declared tables during post-schema `reconcile` and read-only `check`,
  while preserving pre-schema `apply`. Generated permissions units reject
  incomplete schema migrations before admitting runtime services.
- Stream detached executable retention and complete-file binding with a separate
  finite bound, admitting debug binaries larger than the evidence-carrier limit.
- Retry occupied evidence temporary names without replacing interrupted inodes;
  retain the invoking tool `PATH` for detached worker and observer services.
- Require existing custody roots and descendants in boot tmpfiles and activation
  directory setup, preserving permission repair and sibling directory creation.
- Generate authenticated loopback replication rules for source-local recovery,
  including both address families for `localhost`, without opening the firewall.
- Derive source-local TCP listeners when no explicit replication listeners are
  configured; exercise the physical recovery fixture with those defaults.
- Probe generated cutover writer leases independently of the retired production
  Python package, retaining condition/reload argv, exit-code and exclusion checks.
- Preserve literal serde JSON discriminator objects and exact large integers in
  receipts and journals; reject malformed or unsupported strings before mutation.
- Share JSON token scanning, UID lookup and prepared-transition discovery; parse
  recovery preparation commands once before side effects and retain explicit
  evidence lease scopes. Native VM fixtures share transport and cleanup while
  preserving their acceptance artifacts.
- Retain the CI contract in filtered Cargo sources so packaged migration checks
  can validate `simit.toml`.
- Stream pinned physical-manifest hashes during pruning in both runtimes and
  retry capture scratch collisions while preserving interrupted inodes.
- Clear undeclared variables from supervised argv cases; test fixtures declare
  their required execution path explicitly.
- Revalidate interrupted writer release in both runtimes while retaining source
  backup pins and borrowed fence leases; inhibit premature completion/retirement.
- Let managed backup, restore and export run for their owning unit's lifetime
  instead of imposing the short worker-probe deadline.
- Coordinate process creation and final writer exec with lease acquisition and
  release, restoring descriptor flags after failed exec.
- Bind supervisor liveness to the explicit main task on emulated AArch64 and
  retain deciding probes before interruption classification.
- Preserve retention compatibility for FIFO lock anchors and unused surrogate
  fields while validating required fields before deleting recovery data.
- Isolate disposable physical recovery authentication from primary-local fence
  selectors while retaining copied configuration bytes and explicit thaw.
- Resolve the selected immutable snapshot during source-local cutover and
  application transitions in both runtimes, including transition resume.
- Stream physical backup manifests without imposing the small metadata-carrier
  limit, and preserve interrupted atomic writes when temporary names collide.
- Exclude the generated development-shell hook link from candidate retention
  while rejecting dangling source links. Synchronize the reboot fence fixture
  with completion of PostgreSQL setup before strict live-session inspection.
- Reject unfenced managed source-local preparation during evaluation while
  preserving read-only certification and retired enrollment policies.
- Capture the expected source-local WAL rejection diagnostic in the fixture and
  read host identity through the available kernel interface.
- Complete the post-target replay-stop WAL segment before selecting a source-local
  capture, preserving its original switch LSN and immutable retry bytes.
- Keep all features enabled once in the documentation build and use the supported
  NixOS test interface for the module smoke gate and its JUnit output.

### Compatibility

- Generated application backups advertise contract version 2 and require a fresh
  accepted capture under the new manifest. Version-1 library fixtures retain their
  historical owner-controlled publication behavior.
- Native FIFO-manifest admission remains bounded and conservatively preserves
  recovery data; Python's potentially unbounded stream behavior is an approved
  migration exception.
- Passing individual checks does not establish comprehensive lifecycle or
  production acceptance; the catalog and retained qualification verdict remain
  authoritative.
