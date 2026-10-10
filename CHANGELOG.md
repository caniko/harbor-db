# Changelog

## Unreleased

### Added

- Opt-in Rust storage lifecycle commands with durable fencing, recovery,
  application transitions, and coordinated writer lease inheritance. Python
  remains the default lifecycle package.
- Catalog-driven, candidate-bound lifecycle qualification with detached workers,
  observer reattachment, cancellation, resource diagnostics, and retained
  semantic and JUnit evidence.
- Native NixOS package and VM checks, including physical PostgreSQL transitions,
  generated backup services, and supervisor interruption cases.
- Opt-in source-local recovery captures and retention pins, with independent
  certification and an explicitly hashed runtime-extension contract. The original
  PR #14 Python runtime is retained as a frozen oracle.

### Fixed

- Coordinate process creation and final writer exec with lease acquisition and
  release, restoring descriptor flags after failed exec.
- Bind supervisor liveness to the explicit main task on emulated AArch64 and
  retain deciding probes before interruption classification.
- Preserve retention compatibility for FIFO lock anchors and unused surrogate
  fields while validating required fields before deleting recovery data.
- Isolate disposable physical recovery authentication from primary-local fence
  selectors while retaining copied configuration bytes and explicit thaw.

### Compatibility

- Native FIFO-manifest admission remains bounded and conservatively preserves
  recovery data; Python's potentially unbounded stream behavior is an approved
  migration exception.
- Passing individual checks does not establish comprehensive lifecycle or
  production acceptance; the catalog and retained qualification verdict remain
  authoritative.
