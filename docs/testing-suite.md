# Lifecycle qualification suite

The versioned catalog in `tests/suite.toml` records executable tests, exported Nix
checks and production packages, and the public-operation coverage matrix. Loading
the catalog is a static operation: it neither evaluates Nix nor runs tests.

## Run and observe

`harbor-db-test` is built with the `testing` Cargo feature. Its execution interface
accepts a prepared execution specification (`run --spec`) or the catalog
(`run --suite`). Catalog execution identifies the source snapshot and candidate
with `--source`, `--base`, and `--id`, and selects a cumulative `--profile`.
Execution is detached by default; `--foreground` keeps the caller attached.

The execution lifecycle has separate `create`, `worker`, `observe`, `status`,
`watch`, `cancel`, `verify`, and `retain` commands, backed by the durable supervisor
and candidate-bound runner. Use the installed binary's
`--help` and each subcommand's `--help` for its exact argument contract. The worker
owns execution; an observer or watcher is not a second worker and does not gain
authority to restart a missing process. Cancellation and timeouts retain their
own execution reasons. Verification checks retained evidence rather than treating
an observer's disconnect or stdout EOF as successful completion.

`catalog` exposes the expanded static inventory. `plan` exposes selected cases
and their static coverage report. These views are not passing run receipts.

Observers sample resources every 10 seconds and summarize every 30 seconds.
Log activity, heartbeat liveness, and meaningful case progress have independent
timestamps. After five minutes without meaningful progress the observer retains
and emits a `stalled` notification, even if logs and heartbeats are fresh. Quiet
logs have their own diagnostic notification. Reattachment deduplicates both;
notifications never terminate a worker before its declared hard deadline.

Before classifying unexpected worker loss, the observer retains its exact
deciding identity probe in `worker-liveness.json`: saved/observed boot and process
start identities, bounded proc-stat data or the failing read stage and OS error,
and caller process/thread identity. This diagnostic does not replace execution
receipts or signal any persisted PID. Its bytes explain the loss decision;
reattachment does not rerun the worker.
Registration reads `/proc/<pid>/task/<pid>/stat` to bind the process's main task
explicitly. This preserves one start identity across worker/observer threads,
including configured user-mode emulation that synthesizes `/proc/<pid>/stat`
with the calling thread's start time. Boot ID and PID/start-time equality remain
mandatory.

Final writer handover uses `storage::process::exec(command, leases)`: validation
and command construction complete first, then the spawn coordination gate is
held while explicitly selected descriptors become inheritable. A failed exec
restores every original descriptor flag before releasing that gate; restoration
failure is reported ahead of the exec error. Validation workers receive their
explicit lease list through child-local inheritance. The legacy low-level
`Lease::inherit()` and `process::inherit_fd()` retain their process-wide contract
and require callers to exclude concurrent forks until exec or flag restoration.

## Cumulative profiles

| Profile | Included work |
| --- | --- |
| `fast` | Fast native catalog/protocol/evidence checks and baseline Rust tests |
| `integration` | Fast cases, individual Python and native storage/process tests, prerequisites, and Nix evaluation checks |
| `vm` | Integration cases and the declared NixOS VM checks |
| `full` | VM cases and the remaining exported flake checks and production package builds |

Selection is stable and dependency-first. Dependencies must be registered and
belong to the same or an earlier profile; unknown references, cycles, duplicate
IDs, and profile-leaking dependencies invalidate the catalog. Platform metadata
identifies supported systems. Selecting the static full inventory includes both
platforms; the executor must record platform eligibility and execution outcomes
explicitly rather than silently turning an unsupported case into a pass.

## Catalog schema

Catalog version 1 contains `capabilities`, optional shared `defaults`, explicit
`cases`, compact executable `inventories`, compact `nix_inventories`, and reviewed
`coverage` bindings. All schema objects reject unknown fields and enum values.

Shared defaults are resolved at load time. Callers receive complete cases with
ID, source location/symbol, profile, maturity, argv or Nix installable, deadline,
resource budget, platforms, dependencies, artifact metadata, and coverage claims.
An executable inventory explicitly lists selectors. It expands
`argv_prefix + [selector_prefix + selector] + argv_suffix` into one case per
selector; it is not runtime discovery. Rust entries use `--exact`. Python entries
retain their fully qualified unittest selector as `python_migration_id`.

Nix inventories enumerate namespace, systems, and export names. They expand into
local `.#checks.SYSTEM.NAME` or `.#packages.SYSTEM.NAME` metadata. Package aliases
and cached variants remain separately identifiable exports. Aggregate flake
checks provide build/check evidence and do not replace individual test-count
evidence or acquire blanket behavioral coverage.

Deadlines are finite and positive. CPU, memory, and disk budgets are positive;
named exclusive resources describe scheduling constraints. The supervisor enforces
deadlines and locks named exclusive resources across runs under the same base.
The runner currently executes sequentially. Cargo compilation defaults to the
case's declared CPU count via `CARGO_BUILD_JOBS`. CPU, memory, and disk declarations
are planning metadata, not enforced limits or measured usage. Nix builds use the
supervisor's fixed `--max-jobs 1 --cores 2` policy.

## Artifacts and acceptance

`artifacts` contains descriptive log labels. `artifact_specs` contains versioned
acceptance descriptors: case-bound `source`, relative `path`, typed `kind`,
`required`, and an optional expected `sha256`. Paths cannot be absolute or traverse
parents. `Case::resolved_artifacts` resolves them under an absolute private
per-case workspace into supervisor artifact specifications.

Nix cases without explicit descriptors receive a required `nix-outputs.json`
descriptor of kind `nix_outputs`. The executor must produce the validated Nix
output/root manifest; catalog loading does not invent a result or digest.
VM-profile Nix cases also require the build output's `junit.xml`. The worker
retains that document and validates executed cases and outcomes before acceptance.
The NixOS fixtures enable the pinned driver's `--junit-xml junit.xml` option;
they do not synthesize a passing document from a successful build exit. Output
manifests bind both realized outputs and their derivations to validated indirect
GC roots and NAR hashes. Retaining the derivation preserves its source/input graph
in addition to the realized runtime closure.
Acceptance-document discovery examines realized output directories. Retained
regular files, including derivations, remain provenance inputs but are not searched
for child documents. Redirected outputs, multiple matching documents, and invalid
document contents still reject acceptance.
Python prototype execution uses a versioned, retained unittest helper. Its
`TestResult` checks that exactly the named test ran and succeeded, with no skip,
expected failure, unexpected success, or failing subtest. It writes case-bound
semantic JSON; assertion outcomes are not inferred from stdout/stderr. An invalid
selector or nonpassing result produces a failed run, with logs retained.

CLI catalog execution routes registered native Rust selectors through a retained
executor specification and executable. The accepted shape is
`cargo test OPTIONS SELECTOR -- --exact`; a missing exact suffix, `--ignored`
override, or different Cargo command is rejected. The executor runs the original
argv and validates the known libtest harness output: the exact selector must have
an `ok` result and an executed count of one passed, zero failed, and zero ignored.
This adapter produces semantic JSON from the actual harness result, not exit
status alone. It uses the stable harness text contract, not unstable libtest JSON
or a general arbitrary-log parser. Invalid selectors, ignored tests, and actual
test failures do not produce passing acceptance receipts.

The PostgreSQL prerequisite IDs use the same executor with explicit disposable
package metadata: major 18 for `prerequisite.disposable-postgres`, major 17 for
`prerequisite.disposable-postgres-17`. After the original prerequisite command,
the adapter checks executable inventory including `pg_upgrade` and validates
the runtime server's `postgres --version` against the requested major (Nix may
keep `pg_config` in a separate development output). These checks produce their own
semantic assertions; they do not claim a database lifecycle test passed.

The packaged runner includes Cargo, rustc, a linker, the Python prototype adapter,
and the systemd/Nix tools used by detached execution. Its packaged smoke gate runs
an exact dependency-free Cargo selector in a cleared environment and validates the
resulting semantic receipt. An installed runner therefore does not need to inherit
a developer shell to execute that supported Cargo shape. Project dependencies and
application-specific tools still come from the retained candidate's selected inputs.

Library `runner::create` without an executor retains its existing Python behavior;
native/prerequisite argv cases then require explicit artifact producers. Other
unregistered argv commands also need real semantic/JUnit evidence. A zero exit status with zero
selected tests, missing required evidence, malformed evidence, a skipped required
case, or a lost worker is not equivalent to accepted execution. Retained artifact
hashes bind bytes that actually existed; optional expected hashes are not
fabricated hashes of future output.

Argv producers receive `HARBOR_DB_TEST_CASE_ID` (the supervisor-safe identity) and
`HARBOR_DB_TEST_ARTIFACT_DIR` (the absolute private per-case workspace). They should
write the catalog's relative artifact descriptors there. If no required typed
acceptance descriptor exists, the runner requires `acceptance.json` containing:

```json
{"schema":1,"case_id":"case-<sha256>","assertions":[{"name":"actual assertion","passed":true}]}
```

Assertions must be nonempty, uniquely named, and true. JUnit requires nonempty
executed testcases with no failures, errors, or skips and consistent counts. The
supervisor rejects required artifacts that existed before execution without being
refreshed, validates their contents, and retains their bytes and hashes.

## Coverage and maturity

Each public operation has seven required dimensions: positive behavior,
rejection, repetition, concurrency, interruption, recovery, and compatibility.
Every cell is reported as missing, prototype, crystallized, or explicitly
not applicable. N/A declarations need a nonempty reason and may apply to one
operation or an entire capability. Overlapping exemptions and claims that
contradict an exemption are rejected.

Coverage bindings identify a particular case, operation, dimension, and source
assertion. A multi-operation VM test contributes only its reviewed assertions;
its existence does not cover every cell. Library tests do not automatically prove
CLI parsing or exit-code contracts. Unreviewed inventory remains present with a
coverage note while its operation/dimension cells remain missing.

`CoverageReport::is_complete()` measures static evidence coverage and accepts
both prototype and crystallized evidence. `is_crystallized()` additionally
requires mature evidence for every applicable cell. Neither method means a run
passed. Prototype status alone does not disqualify otherwise valid execution.
`runner::verify` additionally requires at least one successfully executed,
artifact-verified selected case for every applicable cell. Missing or nonpassing
cells are never accepted. A run whose case evidence passes but whose coverage is
missing has an `incomplete` qualification verdict.
The committed matrix intentionally retains missing cells; it is not a declaration
that full lifecycle qualification has been achieved.

## Python inventory and promotion

All 174 original Python unittest methods remain individually registered. They
remain executable prototypes with stable migration IDs. Native Rust tests can
supplement particular cells without claiming that all original scenarios have
been migrated. Fixture workers annotated as ignored subprocess helpers are
excluded from ordinary qualification selectors and remain exercised by their
owning tests.

Promote a Python scenario only after reviewing its assertions against a named
native test: the same positive/rejection contract, durable boundaries, independent
checks, lease/concurrency behavior, interruption recovery, and compatibility
requirements must survive. Bind the corresponding operation/dimension evidence,
run the exact native selector with nonzero test-count evidence, and retain the
candidate-bound result. Do not infer promotion from a similar test name, a package
build, an aggregate cargo run, or a broad monolithic test. Preserve original tests
until the migration decision and its evidence are recorded.

Real PostgreSQL cases require `HARBOR_DB_TEST_POSTGRES` to identify an executable
disposable package. Cross-major native upgrade qualification additionally requires
`HARBOR_DB_TEST_POSTGRES_17`. Prerequisite cases remain selected, and missing
acceptance evidence fails closed even if their command exits zero. Missing package
variables prevent creation of an executor-backed prerequisite run. Dependent
cases do not run after a prerequisite execution/evidence failure. The
suite targets disposable fixtures, not an ambient production database.

## Developer checks

The focused catalog gate is:

```sh
direnv exec . env RUSTFLAGS='-Clink-arg=-fuse-ld=mold' \
  cargo test --features testing --test catalog
```

Source-inventory checks compare registered selectors with ordinary annotated Rust
tests and Python AST methods without executing their bodies. Add a selector when
adding a qualification test; exclude ignored worker fixtures explicitly. Reconcile
Nix export names from source declarations without requiring catalog loading to
evaluate the flake. Repository formatting is managed through treefmt.

Sandbox checks provide an immutable configuration fixture, a compile-time numeric
view of its root ownership, a deterministic machine-identity fixture, and a private
temporary directory. These model the Nix sandbox's input and filesystem view;
production adapters still require actual root-owned immutable policy and real
machine identity. Host checks use the approved scratch directory and the real
store-reader tests.

The focused runner/CLI gate is:

```sh
direnv exec . env RUSTFLAGS='-Clink-arg=-fuse-ld=mold' \
  cargo test --locked --all-features --test testing_cli
```

It exercises real Python result transport, coverage acceptance/rejection,
candidate retention, CLI worker/observer dispatch, read-only follower disconnect,
durable cancellation, dependency ordering, real Cargo selector/count validation,
and prerequisite inventory/major validation against disposable fixture tools.
It does not launch VM checks or
assert that the full suite is qualified.

## Library interface

The public module is `harbor_db::testing::catalog`:

```text
Suite::load(path) -> Result<Suite, CatalogError>
Suite::parse(input) -> Result<Suite, CatalogError>
Suite::validate() -> Result<(), CatalogError>
Suite::select(Profile) -> Vec<&Case>
Suite::coverage(Profile) -> CoverageReport
CoverageReport::is_complete() -> bool
CoverageReport::is_crystallized() -> bool
Case::resolved_artifacts(workspace) -> Result<Vec<ArtifactSpec>, CatalogError>
```

Execution metadata is `Execution::Argv { argv, env }` or
`Execution::Nix { installable }`. Structured argv preserves literal arguments;
the catalog does not insert a shell, discover tests, or execute installables.

A catalog run, explicitly attached to the foreground, has this shape:

```sh
harbor-db-test run --suite tests/suite.toml --source /path/to/source \
  --base /path/to/private-runs --id candidate-qualification \
  --profile integration --foreground
```

Omit `--foreground` for detached execution. For a prepared JSON execution
specification, replace `--suite` and `--source` with `--spec /path/to/spec.json`.
`retain SOURCE DESTINATION` creates the source snapshot used for candidate-bound
execution. `worker DIRECTORY`, `status DIRECTORY`, `watch DIRECTORY`,
`cancel DIRECTORY`, and `verify DIRECTORY` act on an existing run directory;
`observe DIRECTORY --once` performs a single observation. `create` takes the same
`--suite`/`--source` or `--spec` options as `run`, but only creates the run.

## Retention, services, and verdicts

The testing package includes `harbor-db-native-supervisor-fixture` and the
x86-64 `native-supervisor` VM check. Version-1 Rust orchestration exercises a
lingering user manager, watcher disconnect, observer restart and crash/restart,
retained executable replacement, cancellation, hard deadlines, interrupted
workers, nonpassing semantic evidence, and a live stale-PID surrogate. Python
transports the VM session and retained artifacts. Its coverage bindings require
actual passing VM evidence; registration alone does not qualify supervision.

`harbor-db-writer-fence-fixture` orchestrates the native writer-fence VM through
the version-1 bridge. It owns command expectations, root-barrier and SQL readback,
bounded waits, three crash/restarts, generation switching, and explicit thaw.
The original Python driver remains the legacy gate; Python fault injection is a
compatibility oracle used by the native gate. Both JUnit and the case-bound
`writer-fence-acceptance.json` are required. Nix import validates these documents
against exactly one realized output and retains the producer's bytes unchanged.

`harbor-db-postgres-transition-fixture` owns the borrowed-fence PostgreSQL
transition scenario. It requires one import across missing-proof retries and
same-count record drift, independent application restoration, and real physical
backup/WAL replay on the separate certifier. Record equality and unchanged receipt
transport admit preparation; abort and retirement retain the borrowed fence until
the PostgreSQL owner explicitly stops and thaws the primary. The native case
requires both driver JUnit and `postgres-transition-acceptance.json`. The original
Python scenario remains the legacy oracle.

`harbor-db-postgres-backup-fixture` drives the actual generated `pg-receivewal`,
`pg-basebackup`, and `pg-backup-prune` units on separate primary and backup hosts.
It requires observed PostgreSQL transfer progress before killing a backup cgroup,
preservation of interrupted private trees and the previous publication, explicit
retry, complete received WAL segments, and successful real manifest verification.
A distinct prune service competes with a live backup lease; the lock inode and
previous marker bytes/inode and tree inventory must remain unchanged. Meaningful
retention must remove an aged third backup while retaining the latest two and both
interrupted trees. The case requires driver JUnit and
`backup-service-acceptance.json`; registration alone is static coverage.

The generated backup service retains its legacy ISO timestamp `LAST_SUCCESS`
contract. Recovery consumes a completed-backup identifier and identity/epoch/LSN
metadata instead. The service fixture verifies bidirectional Python/native marker
byte compatibility, but does not fabricate recovery metadata. Recovery-ready
producer acceptance requires a compatible selector and metadata namespace,
retention support for that namespace, then independent restoration using the
service-created backup and receiver-created WAL with exact record equality.

`tests/backup_interruption.rs` stops the actual native prune CLI at an observed
deletion syscall inside an expired backup, before that deletion executes. While
stopped, the CLI must still own `BACKUP_LOCK` and reject a competing acquisition.
SIGKILL must release authority while preserving the complete backup/WAL byte and
identity inventory. An explicit untraced retry with the same arguments must
remove obsolete data, preserve the latest recovery chain and partial transfers,
and retain the original lock inode. This case uses x86-64 Linux ptrace; it does
not claim AArch64 instrumentation or recovery after partially completed deletion.

`tests/drill_interop.rs` exchanges actual Python/native disposable restored
clusters in both directions. Each private endpoint must return exact nonempty
binary/Unicode records from the same real custom-format dump. The other engine
must refuse an occupied restore without mutation, then successfully clean up
that restored PostgreSQL state. Both cleanup CLIs must reject redirected or
nonprivate workspaces while leaving the original server's carrier bytes,
directory identities and full records intact and queryable. Successful cleanup
must stop the server, preserve essential state, and tolerate an original-engine
repeat without rewriting it. These persisted carriers are PostgreSQL files and
the private socket directory, rather than invented workspace receipts.

Detached worker and observer roles execute a private `service-executable` copy
with a retained SHA-256 binding. Observer restarts reuse those exact bytes even
when the invoking development binary is rebuilt or replaced. Verification and
worker startup reject a changed retained executable. Launch failures retain their
original receipt and stop only observer units successfully created for that run.

For catalog runs, `--base` must be an absolute private storage path outside the
checkout, and `--id` must be a new ASCII alphanumeric/hyphen/underscore identity.
The runner retains tracked and nonignored untracked regular sources, including
local edits, under `BASE/ID-candidate/source`. It excludes `.git`, `.direnv`,
`.envrc`, build targets, `.nix-results` output/GC-root state, Python bytecode, and
the root `.pre-commit-config.yaml` generated development-shell link. Other
symlinks/special sources, including dangling links, are rejected.
The retained inventory covers the entire snapshot, not just selected test files.
Candidate and run identity reuse is rejected.

`BASE/ID-artifacts` retains the catalog bytes, `unittest-v1.py`, per-case
workspaces, and `runner-binding.json`. The binding records profile, original-to-safe
case mapping, and a digest of the case specifications. Source files, inventory,
catalog, helper, and binding are SHA-256-bound run inputs. They are immutable by
binding: changing retained bytes invalidates verification. Catalog IDs and resource
IDs map to `case-` plus the full SHA-256 of the original ID; punctuation collisions
do not merge cases. Cargo build output goes into `BASE/ID-build`, outside the bound
source inventory. Verification rechecks the catalog selection and bindings.
When a caller supplies an executor, its executable bytes are copied into
`BASE/ID-artifacts/harbor-db-test`, made executable, and input-bound. Native and
prerequisite cases retain a bound `executor.json` in their case workspace; the
worker invokes the retained executable as `execute --spec PATH`. Adapter
specifications retain only explicit catalog environment and the documented
non-secret execution variables; credentials are not copied from the caller.

`BASE/ID` is the durable supervisor run directory. It contains case logs, retained
artifact receipts, execution results, terminal state, and observer diagnostics.
`qualification.json` is a derived runner verdict; it is recomputed from retained
inputs and evidence, never trusted as the sole acceptance source.

Detached `run` starts independent `systemd-run --user` worker and observer services
using the retained service executable and absolute run path. This requires a functioning
user service manager. The worker runs once; observer restart only attaches.
Catalog argv environments retain explicit catalog values plus invoking `PATH`,
`CARGO_HOME`, `RUSTFLAGS`, and the two disposable PostgreSQL package variables
when present. The runner sets snapshot Python paths and an external Cargo target
directory. Toolchains must remain available at those paths for detached execution.
Prepared `--spec` runs must supply their own source/input bindings, safe case IDs,
dependency-first ordering, and absolute artifact paths; without a retained catalog
binding their verdict is execution verification, not public-operation coverage.

`watch DIRECTORY` streams one JSON status object per interval; use
`--interval-ms 1000` and optionally `--seconds N` to control sampling. A broken
pipe, time limit, or disconnected watcher only stops that follower. `cancel`
writes a durable request consumed by the worker. `status` and `watch` are read-only.

Foreground `run`, `worker`, and `verify` exit 0 only for a passed runner verdict,
2 for a nonpassing verdict, and 1 for service/configuration errors. Successful
`create` or detached launch means creation/launch succeeded, not qualification.
`observe` returns 0 after successful observation even for a negative verdict, so
systemd's observer restart policy does not loop on ordinary qualification failure.
Its final JSON includes the coverage-aware verdict.

For catalog runs, the runner suppresses the supervisor's execution-only terminal
notice and publishes `qualification-notification.json` plus a best-effort desktop
notification from the observer or foreground caller. CLI workers publish the
coverage-aware verdict without sending a desktop notification. Direct supervisor
library calls still expose execution verification; use `runner::verify` or
`runner::status` when claiming catalog qualification.

Runner library entry points are `create(suite, source, base, id, profile)`,
`create_with_executor(suite, source, base, id, profile, Option<&Path>)`,
`safe_identity(id)`, `verify(run)`, `status(run)`, and
`publish_verification(run, notify)`. The CLI supplies its own absolute executable
to `create_with_executor`. `execute --spec PATH` dispatches a single retained
`ExecutorSpec` through `executor::load` and `executor::execute`; it is normally
invoked by the worker from the retained source directory.
The current full catalog retains missing coverage; a full-profile invocation is
not a declaration that full lifecycle qualification has been achieved.

`native-application-transition` uses `harbor-db-backend-transition-fixture` from
the testing package. The NixOS driver is socket transport; Rust owns the
filesystem lifecycle, cross-language continuations, waits, byte comparisons and
semantic assertions. Acceptance requires both realized-output `junit.xml` and
`backend-transition-acceptance.json`, bound to this particular catalog case.

`native-source-local-recovery` uses `harbor-db-source-local-recovery-fixture` with
generated loopback backup/receiver units and an independent certifier VM. It
requires producer-created immutable capture generations, post-backup full-record
equality, actual missing-WAL replay rejection, explicit same-ID finalization retry,
unchanged certified evidence on repetition, Python/native admission equality and
retained writer exclusion. Its driver JUnit and case-bound
`source-local-recovery-acceptance.json` must both pass. The reader, retention-pin
and publication tests qualify their narrower contracts independently; they do
not substitute for this physical recovery gate. The opt-in repository and
operator boundaries are described in [source-local recovery](source-local-recovery.md).

`native-application-provision` uses `harbor-db-provision-fixture` for actual SQL
permissions, native/Python convergence, deterministic catalog/record readback,
overgrant repair and reboot. Its `provision-acceptance.json` and driver JUnit are
both required exports from the same realized output. Testing fixtures are
feature-gated and excluded from the production lifecycle package.

Sandboxed Cargo checks bind a compile-time `HARBOR_DB_TEST_MACHINE_ID` store
fixture, available only with the `testing` feature, and use the builder's
`TMPDIR` through `HARBOR_DB_TEST_TMPDIR` for disposable PostgreSQL clusters.
The Python compatibility helper reads the same fixture. This qualifies receipt
hashing and same-machine rejection; independent physical-host certification
requires VM evidence. Production lifecycle packages omit `testing`, require
`/etc/machine-id`, and accept no runtime machine-identity override.
Nix maps root-owned configuration input inodes to an unmapped UID in the Cargo
sandbox. Its build hook binds the declared fixture's observed UID as compile-time
`HARBOR_DB_TEST_ROOT_UID`, also available only with `testing`. Both the production
lifecycle package and packaged VM testing binaries omit this binding and enforce
UID 0. Read-only store ancestry, regular-file and alias checks remain required.
Startup-barrier ancestry checks use that same filesystem ownership view; process
privilege checks still require effective UID 0. The transition compatibility
helper maps only this fixture UID back to root in Python's `lstat` observations,
leaving the retained Python validation logic and all journal bytes intact.
