# Cutover admission

Enable `services.harbor-db.cutover.enable` on every deployment target. Harbor-DB
exports a small `services.harbor-db.cutover.bundle` containing its checker and
exact host contract. The deployment frontend checks installed readiness before
Nix evaluation, checks the selected candidate contract before system realization,
and reruns candidate admission under the target's activation lease before changing
the system profile. Resumes and delegated builds retain these checks.

Manifest symlinks in Nix guard bundles and `/etc` are resolved once before
dispatch. Every service-user worker receives that same canonical target;
regular-file reads retain their no-follow protection.

PostgreSQL automatically enrolls when its Harbor-DB lifecycle guard is enabled.
An enabled unguarded primary is a configuration error, as is an enrolled primary
without `services.harbor-db.postgresql.recovery`, because every non-startup phase
requires recovery admission. Existing PostgreSQL
identity, staged-upgrade, recovery snapshot and independent restore acceptance
engines remain authoritative. Early admission validates their current bindings
and evidence freshness. It deliberately defers backup byte verification to the
activation gate, which runs the existing full recovery admission.

Candidate-owned `postgresCompatibilityChecks` run cheap read-only SQL against
the live primary before building. Each query must return exactly one true value;
unsupported schemas or failed migrations block admission. The accepted recovery
policy still defines its backup age and restore requirements.
While first-rollout `switchAdoption` is configured, automatic PostgreSQL enrollment
uses that request's explicit local endpoint and records its identifier as
`switch_adoption_identifier`. The cutover gate runs before recovery evidence
admission and the adoption hook. For an unadopted cluster, only the `preflight` and
`activate` phases defer the adopted-identity record to that hook. The pending
identifier must equal the recovery policy's, and the live and physical identity,
writer-fence startup state, compatibility, recovery and corpus checks still run.
The gate never adopts. `startup`, `certify` and service startup stay strict, and
once `identity.json` exists every phase verifies it. After the one-shot request is retired,
cutover checks retain `/run/postgresql` and use the effective
`services.postgresql.settings.port`, including non-default service ports.

## Historical filesystem state

Declare each authoritative corpus as a filesystem resource. Every authority state
directory and corpus root must be globally pairwise disjoint, including across
resources. Equality and containment in either direction reject during Nix and
native/Python manifest admission before authority locks or state writes. This
keeps one resource's identity and transition journals outside another resource's
certified corpus and exclusive lease. Set every `dataDirectories` entry at or beneath a
custody root to `create = false`; missing historical storage is an error. Explicit initialization
of genuinely new state remains a separate policy choice.
Each `custody_file` must be a dedicated direct child of its authority directory.
The names `identity.json`, `transition.json` and `lock` are reserved authority
controls. Nix and both runtime manifest readers reject them, and direct
certification checks the receipt path before taking a lease or adopting storage.
Rejected paths preserve existing authority bytes/inodes and do not publish root
identity markers.

With custody enforcement enabled, authority roots, data directory paths and
directory-creating tmpfiles paths must use normalized absolute spelling. Dot and
parent components, repeated separators and trailing separators are rejected
before creation; this comparison does not depend on the historical path existing.
Managed PostgreSQL preparation applies the same normalized-path admission to
provisioned directories, writable preparation paths and the protected primary
and authority roots before containment checks or root-run directory creation.

Global namespace admission includes every filesystem corpus and authority-state
root plus the data, lifecycle-state and startup-inhibition paths read from each
enrolled PostgreSQL policy. All must be pairwise disjoint in both containment
directions. The startup namespace remains protected during inode inspection too,
so marker/policy writes cannot invalidate a filesystem custody receipt. Before
dispatching user-scoped inspection or certification workers, the coordinator also
compares complete inode inventories across these namespaces, including directory
roots and mounted descendants. Hard links, root bind mounts and nested empty
directory aliases cannot give shared storage separate resource leases. Traversal
uses the coordinator's bounded inspection budget, including every consumer
validator expansion. Each validator receives only the remaining total budget.
Every enrolled filesystem snapshot is verified against its adopted authority,
including resources not selected for certification. Prepared transitions verify
their recorded source/target authorities and protect both live and target roots.
The coordinator passes expanded snapshots through bounded worker stdin (64 MiB
aggregate limit), avoiding per-argument limits for large consumer contracts.
Workers require exact namespace and binding fields; required files may be added
and counters may advance, while missing files and regressed witnesses reject.
Direct certification workers share one activation deadline through snapshot
input, namespace expansion, consumer revalidation and final publication checks.
Worker/serve processes retain
their own authority lease and startup guard without requiring read access to
other applications' private corpora.

Database inventory queries may additionally return `size`, `sha256`,
`git_repository = true` and `git_has_commits` for each path. Certification checks
both source and independent restore against those requirements. Git repositories
must be bare and pass `git fsck --full --strict`; database-nonempty repositories
must resolve `HEAD` to a commit. Shallow history, partial clones and external
object alternates cannot claim complete custody. The immutable `git_executable`
is part of the resource contract. Nix admits only normalized store paths and
realizes the manifest after checking that each resolves to a regular store file.
Both engines validate a root-owned, read-only, executable store file and its
immutable alias chain before execution, including external manifests. Mutable
runtime aliases to a store executable are rejected. Activation repeats these
integrity checks.

Cutover capability version 3 guards all seven declared systemd writer phases
(`ExecCondition`, `ExecStartPre`, `ExecStart`, `ExecStartPost`, `ExecReload`,
`ExecStop` and `ExecStopPost`) with the authority lease. This includes conditions
before startup admission and reloads invoked independently of startup. Applications
that use system OpenSSH must also set their service user's shell to the packaged
`harbor-db-cutover-shell` and declare the underlying absolute `login_shell`.
This shell selects the resource by the authenticated effective user, validates
startup custody, then execs the real shell with the shared lease inherited.
It covers externally invoked Git commands and hooks when the web service is
stopped. Operator-launched maintenance must use the same guarded entrypoints.

Application-provided boot tmpfiles directory-creation rules at or beneath
guarded roots are converted to permission-only rules. Descendants cannot create
missing parent roots; sibling paths remain independent. This also covers applications
whose upstream module has no require-existing initialization option.

The `harbor-db-cutover certify` command compares every regular file's SHA-256,
relative path, mode and size with an independently restored corpus. It rejects
missing roots, empty substitute directories, redirected roots, symbolic links,
special files, overlapping source/restore roots and changed source data. It uses
the existing resource adoption engine after equality succeeds, then durably
publishes its receipt. Equal record counts or equal file sizes do not establish
custody.

Restored roots, files and directories must have inodes independent of the union
of every enrolled corpus, authority, PostgreSQL primary, lifecycle and startup
namespace's inventory. The coordinator rejects lexical containment and complete
inode overlap before dispatching a certifying worker. A bind mount of another source root or
one of its entries cannot certify a restore, even with identical inventories,
disjoint path spelling and a regular-file link count of one.

For a resource with `database_resource = "postgresql"`, certification also
requires executed recovery evidence and live records matching the accepted
database snapshot. The filesystem receipt binds that snapshot digest. A changed
backup, record contract or database snapshot requires a new coordinated proof.

Certification is an explicitly mutating operator operation. Establish the
application consistency window and stop all declared writer units first. The
command does not stop services, restore storage, initialize roots, or fabricate
database receipts. Prepare the private authority directory with its service-user
ownership before certification. Supply restored roots in the declared source-root
order:

```text
harbor-db-cutover certify --contract <immutable-manifest> --host <host> \
  --resource <resource> --identity <independently-recorded-corpus-id> \
  --restore-root <restored-root-1> --restore-root <restored-root-2>
```

Read-only checks never write receipts or adoption markers. Early filesystem checks
compare root identity, required files, receipt binding, freshness and file metadata;
activation additionally hashes the corpus. Failure produces a resource-specific
blocked JSON report. Preserve the historical corpus and failed evidence while
repairing its exact custody problem.

## Rollout

Before the first installation, qualify and realize the candidate guard bundle
separately. Supply its immutable store path through the deployment frontend's
`--cutover-contract` option. This selects an early checker, not an admission
bypass: the evaluated candidate is checked independently, and activation reruns
the candidate guard.

Service startup guards preserve existing resource authority and certified root
identity while allowing ordinary application writes. Deployment admission
and startup have separate phase contracts; a routine service restart must not
be treated as a new restore certification.

Declared filesystem writer units retain the resource's shared lifetime lease
through `harbor-db-cutover serve`, including the actual `ExecStart` process.
Certification holds the exclusive lease through byte comparison and receipt
publication. Guarded commands require an absolute executable and reject systemd
privilege/argv modifiers rather than silently weakening this ownership.

`database_inventory_checks` bind application rows to corpus completeness: each
query returns a JSON array of `{path, directory}` objects under its indexed
authority root. Certification requires every referenced file/directory in both
matching corpora; rebuild admission also requires these live database paths to
match the certified requirements. Database snapshot equality alone cannot prove
that an identically incomplete source and restore contain all referenced files.

The default early budget is 30 seconds total; exceeding it blocks admission. The
healthy-local sub-five-second target must be measured against the real inventory.
The activation/deep-verification budget is separate and bounded at 900 seconds.
