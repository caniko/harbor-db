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

- Native FIFO-manifest admission remains bounded and conservatively preserves
  recovery data; Python's potentially unbounded stream behavior is an approved
  migration exception.
- Passing individual checks does not establish comprehensive lifecycle or
  production acceptance; the catalog and retained qualification verdict remain
  authoritative.
