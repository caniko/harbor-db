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
An enabled unguarded primary is a configuration error. Existing PostgreSQL
identity, staged-upgrade, recovery snapshot and independent restore acceptance
engines remain authoritative. Early admission validates their current bindings
and evidence freshness. It deliberately defers backup byte verification to the
activation gate, which runs the existing full recovery admission.

Candidate-owned `postgresCompatibilityChecks` run cheap read-only SQL against
the live primary before building. Each query must return exactly one true value;
unsupported schemas or failed migrations block admission. The accepted recovery
policy still defines its backup age and restore requirements.

## Historical filesystem state

Declare each authoritative corpus as a filesystem resource. Its authority state
must be outside the corpus. Set every `dataDirectories` entry at or beneath a
custody root to `create = false`; missing historical storage is an error. Explicit initialization
of genuinely new state remains a separate policy choice.

Database inventory queries may additionally return `size`, `sha256`,
`git_repository = true` and `git_has_commits` for each path. Certification checks
both source and independent restore against those requirements. Git repositories
must be bare and pass `git fsck --full --strict`; database-nonempty repositories
must resolve `HEAD` to a commit. Shallow history, partial clones and external
object alternates cannot claim complete custody. The immutable `git_executable`
is part of the resource contract. Activation repeats these integrity checks.

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
