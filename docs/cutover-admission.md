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

## Rollout CLI

`services.harbor-db.postgresql.rollout` (opt-in) renders
`/etc/harbor-db/postgresql-rollout.json`. It binds the fenced maintenance
manifest, the preparation argv, the client inventory (defaults to
`writerFence.blockedUnits`), setup units, WAL receivers, publishers to pause and
the independent executor. One resumable journal at the fixed root
`/var/lib/harbor-db/postgresql-rollout` sequences the existing verbs:

```console
# harbor-db-postgres --config /etc/harbor-db/postgresql-rollout.json rollout next
```

| Step | Phase after | What it does |
| --- | --- | --- |
| `fence` | `fenced` | inhibit startup, pause publishers, stop clients and the primary, open the fence, start the primary, **wait for setup units to settle**, inspect the live fence |
| `prepare` | `prepared` | run `prepare-recovery` with receivers stopped; stops at the independent-certification hand-off and records the export and its SHA-256 |
| `certify` | (executor) | as the declared unprivileged executor, run its restore/certify argv on the transported export (`--export`, `--export-sha256` and `--workspace` are appended; the argv must write its receipt to `<workspace>/off-host-recovery.json`) |
| `import --receipt --sha256` | `imported` | stage the digest-verified receipt and re-run preparation to admission |
| `adopt` | `adopted` | re-verify live records against the accepted snapshot, then `adopt-live` and `check` |
| `activated` | `activated` | require the running generation to carry this manifest, then record that it is active under the fence |
| `accept` | `activated` | after verifying the deployment (the running generation carries this manifest): write the acceptance thaw checks, root 0600 at `<journal root>/acceptance.json`: `{"version": 1, "status": "accepted", "host", "system_identifier", "manifest_sha256", "token" (the fence token), "snapshot_sha256" (of the record snapshot's bytes)}` |
| `thaw --acceptance` | `thawed` | require the running generation to carry this manifest; verify a root-owned 0400/0600 acceptance (in a directory only root can write) with exactly those fields; re-admit recovery (`inspect-recovery`, the cutover checker); close the fence, restart the primary, the clients it stopped and declared clients whose start the fence skipped, lift the pause |
| `abort` | `aborted` | before activation, or after rolling back a failed deployment (an installed candidate, adopted or activated, must be thawed instead): close (or confirm closed) the fence, restart the primary and recorded clients, lift the pause; evidence is kept |
| `archive` | (none) | archive a thawed or aborted journal and drop its GC root, without fencing |

`rollout next` performs the next local step or prints the exact external action
(executor certification, activation, thaw). From `prepared` it names the
manifest closure copy (`nix copy --to ssh-ng://<executor> <manifest>`), because
the executor runs the Harbor and certify argv that closure contains, as well as
the export transfer. Every step is idempotent; rerun it after an interruption.
After a thawed or aborted rollout `next` only reports: a new rollout starts only
with an explicit `rollout fence` (which archives the finished journal first),
and `rollout archive` retires one without fencing.

Thaw re-admits the recovery evidence, which must therefore still be within its
`maxAgeSeconds` of `prepare`. If the deployment fails verification, or the
evidence ages out first, roll back to the previous generation as the boot
default (`nixos-rebuild switch --rollback`, or `boot --rollback`), then `rollout
abort`. Abort is allowed once the candidate is neither installed, nor the system
profile's (the next boot's) generation, nor the booted one; a `test` rollback
alone would boot the candidate next, unfenced and without thaw's checks. After thaw, retire
the rollout within that same window: activate a generation without `rollout`
(and `recoveryPreparation`) and with `recovery.requireWriterFence = false`, or
the consumer's equivalent. A generation that still declares `rollout` requires
a live writer fence for its recovery admission, so it cannot activate once the
fence is closed. Then run `rollout archive`; never start a fresh rollout to get
past a thawed one.
Only the manifest that opened the journal steps or unwinds it (`abort` and
`thaw` included): every policy its fence, inhibition, clients and slot were made
under is that manifest's. Another manifest refuses and names the opener to rerun
with (`--config <opening manifest>`, or canix `--candidate <generation>`); the
opener is kept alive by the GC root `/nix/var/nix/gcroots/harbor-db-postgresql-rollout`
from the journal's creation until it is archived. The root's directory is synced
before the journal is first written, and again after archiving removes it. Only a finished journal,
which leaves nothing open, may be archived by any manifest: a thawed one, or a
fully unwound abort whose receipt (if any) is already retired.
The journal ignores unknown fields and omits empty ones, and new fields are
optional and additive (only an incompatible change bumps its `version`), so a
rollback generation can still read and abort a newer coordinator's journal.

Exclusion and recovery:

- **Foreign fences.** Harbor resumes any prepared writer fence, so `fence`
  refuses (before touching any unit or writing a journal) a live fence or
  interrupted fence selector whose token this journal does not own; finish it
  with the coordinator that opened it. An `abort` interrupted between
  inhibition and fence-open never adopts such a fence either: Harbor releases
  startup only at a fence boundary, so while the shared inhibition is still
  held it names both tokens and asks for the other fence to be finished first;
  once that coordinator has released the inhibition, the retried `abort`
  restarts the primary this rollout stopped.
- **Interrupted fence opening.** The journal records the intent to open a fence
  before `fence-open` and the token after it. Harbor mints the token, so if the
  coordinator dies in between and a fence is live on retry, nothing proves it
  is this rollout's: `fence`, `next` and `abort` refuse, naming the token, until
  the operator confirms no other coordinator ran (for example a legacy
  writer-fence acquisition) and reruns with `--adopt-fence-token <token>`. With
  no live fence the retry simply opens one. The flag is refused once a token is
  recorded.
- **Execution lock.** `executionLock` names an existing host lock file (for
  example `/run/lock/canix/switch.lock`) in a real directory chain only root can
  modify, so its inode cannot be swapped. Every mutating step holds a
  non-blocking exclusive `flock` on the inode it opened without following a
  symlink, besides the journal's `rollout.lock`; contention names the holder
  PID. `status` takes no lock.
- **Preparation worker.** Preparation runs as the transient
  `harbor-db-rollout-prepare.service` with a one-day bound. Termination
  signals are deferred while it runs: an interrupt stops the unit, restores the
  receivers and fails the step. While that unit is active (for example an
  orphan of a killed coordinator) every mutating step, `abort` and `thaw`
  included, is refused before any side effect, so no fence closes and no
  writer starts under it; `status` and `next` say so. A never-loaded or
  collected unit reads `LoadState=not-found` and is idle.
- **Bounded commands.** Every `systemctl` and Harbor command runs in its own
  process group with a bound (600 and 1800 seconds). Cluster-owned verbs drop
  to the `postgres` account in-process.
- **Unwinding.** Once `abort` or `thaw` has closed the fence and verified the
  normal primary, the journal records it; a retry only returns clients,
  receivers and publishers and never restarts the primary again. Every client,
  receiver and publisher is attempted before the first failure is reported.
- **Lost journal.** Without a journal, `abort` names Harbor's live fence token
  (verify it with `inspect-fence --token`). `rollout abort --adopt-fence-token
  <token>` then unwinds that fence through the normal close, restarting every
  declared receiver and paused publisher. With no live fence but a held startup
  inhibition (which keeps the primary from starting), `abort` names its token,
  and `rollout abort --adopt-startup-token <token>` completes the fence boundary
  Harbor releases startup at and unwinds it. The lost journal's stopped clients
  are unknown, so re-activate the current generation afterwards.
- **Crash windows.** Every external change is journaled before it is made, or
  recovered from Harbor's own record on retry: the inhibition intent (its token
  is recovered from the held inhibition), the fence-opening intent, stopped
  clients, publishers and receivers, and the staged receipt. Provisioned
  directories appear only complete: they are made, permissioned and owned under
  a private name and then renamed into place.
- **One role per unit.** Clients (with retired clients), paused publishers,
  receivers, setup units and `postgresql.service` may not overlap, templates
  included, both in Nix and when the manifest is loaded: a retired client that
  is also a paused publisher would be restarted when the pause lifts.
  Only clients (and retired clients), enumerated from active units, may be
  `@.service` templates; publishers, receivers and setup units name units. A
  provisioned path has one owner and mode contract (the export root is never
  also a generic provision directory).
- **Setup.** Fencing requires every setup unit to settle successfully: a
  `failed` one fails the fence with the window retained (fix and restart it,
  then rerun); unwinding only waits for them.
- **Preparation outcome.** The worker's outcome (phase, export or admission) is
  journaled before the receivers it stopped are restored, so a failed restore
  never reruns preparation or its export command; any later step first
  restores the pending receivers.
  A worker that finished before its outcome was journaled (the coordinator
  was killed in between) is never relaunched either: before launching, `prepare`
  and `import` recover it from Harbor's evidence, admission (`inspect-recovery`
  ready) as `imported`, a fence-bound snapshot with its preparation binding and
  export plan as `prepared`.
  The import's preparation reruns the export command, so admission is recorded
  as `imported` only if the export plan, read through the pinned export
  directory, still has the certified digest. If the exporter rewrote it, the
  step fails closed and the window is retained: restore the certified export
  and rerun `import`, or abort. A recovered admission is held to the same
  check.
- **Disjoint roots.** Both Nix and the manifest load require the PostgreSQL
  data directory, the identity state directory (which also holds the writer
  fence), the startup inhibition state directory, the journal root, the import
  slot's directory, the export root, the writable paths and the provisioned
  directories to be pairwise disjoint. The only intended nestings are a
  provisioned directory inside a writable path or inside another provisioned
  one (evidence in its backup root), and the export root inside a writable
  path. Such an export root is written through that root; only one outside
  every writable root is its own sandbox entry, and its ancestry must then be
  modifiable only by root. The recovery evidence directories (snapshot, receipt and off-host
  receipt), the recovery backup root (which the backup command fills) and the
  provisioned directories must lie inside the writable paths.
- **Root writes.** Root never resolves a pathname below a directory another
  account can write. Paths are walked by opening each component from its
  parent's descriptor without following symlinks, and each component is
  checked through its own descriptor. Provisioning, including below the
  postgres-owned backup root (evidence), creates, permissions, owns and renames
  relative to the pinned parent descriptor, and never replaces an existing
  entry. Every other root write (journal root and archives, staged and retired
  receipts, the import slot, pause drop-ins, the GC root) is in a directory
  whose whole ancestry only root can modify, which the fence preflight checks.
- **Load-time inputs.** Every local executable a step runs (`harbor`,
  `systemctl`, `systemd-run` and thaw's cutover checker) must be an executable
  regular store file reached without a mutable alias, and the worker's
  preparation config must be a store file. The executor command runs on the
  executor host, so it is checked only lexically: a host other than the primary
  (certification is independent only elsewhere), a user other than root
  (`certify` refuses root), an absolute program and non-empty arguments.
- **One maintenance manifest.** With a rollout, every guard and service of the
  generation that inspects recovery, fence or startup state renders from
  `rollout.maintenance`, the manifest the coordinator fences, prepares and
  adopts under. That covers the pre-switch recovery and adoption checks,
  PostgreSQL's start `check` and `serve`, and the recovery-check service. The
  upgrade service keeps the module's own manifest.
- **Preflight.** Anything static that could fail a later step once the fence
  is held is checked by `fence` before any side effect (and the worker's part
  again by `prepare`):
  - every declared mount is present: the worker's and the maintenance
    manifest's own, which every Harbor verb (`fence-open` first, and the abort
    that must reopen) requires;
  - every name a later step resolves exists: the `postgres` account and group,
    each provisioned directory's group (including directories still to be
    created) and the worker's supplementary groups;
  - while the primary runs, `inspect-live` answers on the manifest's socket and
    port with its system identifier, as `inspect-fence` must once the fence is
    open;
  - each provisioned directory, and the identity state directory that Harbor's
    fence and adoption write as postgres (`postgres:postgres` 0700), either
    already has its declared owner, group and mode (an existing one, perhaps a
    mountpoint, is never re-owned) or is creatable under trusted ancestry;
  - every writable path, evidence directory, the recovery backup root and the
    export root exists or is provisioned (systemd fails a missing writable
    path, and Harbor creates no evidence directory), is reached without a
    redirected component (walked no-follow below its writable root, which
    postgres may own) and is writable by the worker's uid or groups (mode bits;
    ACLs are not considered);
  - every directory postgres must traverse to reach these grants it search
    permission;
  - every writable sandbox path, the required mounts and an export root outside
    every writable root (one inside is written through that root) lie on a
    chain no other user can swap;
  - the journal root, the startup inhibition state directory and the drop-in
    root are root's and writable by no one else, or creatable;
  - the import slot's directory is (or can be created) private to root, and
    the slot is empty;
  - no record snapshot, `preparation.json` or receipt from another writer
    window (or none) is present.

  `fence` creates the inhibition state's missing parents searchable before
  Harbor would create them root-private, and `import` never creates the slot
  directory below an absent mount. Before `prepare` records `prepared`, the
  snapshot must be bound to this rollout's fence and `preparation.json` must
  be its backup binding. Stale evidence is never moved implicitly: move it
  aside first.
- **Receipt size.** Transported and staged receipts are bounded by the
  recovery importer's own 16 MiB limit; Harbor's small control records and the
  acceptance by 1 MiB.
- **Pinned reads.** Files other writers control (a transported receipt, the
  acceptance, preparation evidence, Harbor's fence selector and records) are
  read through one descriptor opened without following a symlink or blocking,
  checked as a bounded regular file.
- **Starting again.** At its terminal transition a rollout moves a staged
  receipt out of the import slot into the journal root, so the next rollout's
  `prepare` never meets it as unrecorded. The next `fence` or `next` archives a
  finished rollout and starts afresh: a thawed one, or an abort once its reopen
  is recorded, its receipt retired and no Harbor fence is held, under any
  manifest (an older abort whose slot was never retired: only its opener). A
  slot that does not hold the recorded receipt is kept aside as
  `receipt.rejected.<n>.json` instead of holding the rollout. Harbor stages the
  slot itself atomically (a synced temporary linked into place, never
  replacing), and `import` creates its private directory. While `prepared`,
  `import` with a new receipt sets the recorded one (which failed admission)
  aside and stages the new one; it still refuses a slot it never recorded.
  The journal and its receipt share one collision-free id,
  `rollout.<n>.{thawed,aborted,receipt}.json`, the next sequence number,
  published with no-replace links so no archive is ever overwritten. `status`
  lists the archives.
- **Start limits.** Every coordinator start (primary, clients, receivers,
  publishers) runs `systemctl reset-failed` first, so repeated cycles never
  leave a unit down on its start limit. Clients, receivers and publishers are
  started without waiting for them to finish (`--no-block`; a oneshot backup
  can run for an hour). Only a failed start is an error: a unit running, ended,
  or still queued behind another unit's job after `setupSettleSeconds` counts as
  started. A unit to restore that the running generation does not load (one
  the candidate removed, or only it brings) is skipped, as the pause and the
  primary's stop skip it, and listed in the journal's `unloaded`. A queued job
  reads as an empty `Job` value; `0` (the D-Bus no-job id) counts as none.
  Each unit started (or skipped) is journaled at once, so a retried unwind
  restarts only what failed, never a oneshot that already ran.
  Only the primary's start waits, for readiness and its setup units.
- **Hard dependents.** Before every primary stop, the unsettled services in
  its reverse Requires/BindsTo closure (through targets such as
  `postgresql.target`) are recorded, as clients or as pending receivers, so the
  stop's propagation is undone; retired clients, setup units and paused
  publishers keep their own handling.
- **What is restored.** One rule decides which units are recorded before
  being stopped and restored afterwards: clients, receivers, publishers and
  hard dependents alike. Any unit that is not settled (`inactive` or `failed`)
  counts, including one still `deactivating` (perhaps restarting), whose stop
  the rollout would otherwise complete without restoring it.
- **Reported export.** The `backup_id` the (postgres-written) preparation
  evidence names must be a repository identifier directly below the export
  root; the export is reached and its plan hashed without following symlinks.
- **Receivers.** Every receiver preparation stops is recorded and restarted.
- **Output.** Each step prints exactly one JSON status document on stdout; the
  preparation worker's and the executor command's own output goes to stderr.
- **Publishers.** Publishers that were running when the pause began are
  journaled and restarted (after `reset-failed`) once `abort` or `thaw` lifts
  the pause; idle ones keep waiting for their own triggers. A publisher the
  running generation does not load (the candidate brings it) is left alone.
  A loaded publisher whose start the pause itself skipped (its last condition
  check failed after the pause marker was written; the marker is written once,
  so its mtime is when the pause began) is started too. One whose condition
  failed before the pause, or that never ran, is not.
- **Repository protocol.** The coordinator requires the legacy recovery
  repository protocol (checked in Nix and at load), as source-local preparation
  leaves no preparation binding to hand off.
- **Mounts and systemd values.** Every directory the coordinator provisions
  (and its journal root) is created only after the declared mounts covering
  it are mounted. Paths, mounts, groups and the import slot rendered into the
  worker's systemd properties must not contain whitespace, quotes, `\`, `%`
  (or `:` in the import slot), both in Nix and when the manifest is loaded.
- **No swappable components.** systemd resolves symlinks in `ReadWritePaths`,
  so before the worker starts every writable path, the export root and every
  required mount must be reached through real directories (never a symlink)
  that only root can modify; below a sticky directory a component must be
  root's too. Provisioned directories follow the same rule, except that one may
  sit inside another declared provisioned directory (an evidence directory in
  a backup root); they are made by `mkdir` and permissioned through a
  no-follow descriptor, never through a path. They are provisioned shallowest
  first, so a declared parent always gets its own owner and mode.
- **Activation.** A rollout-enabled generation does not install the legacy
  `00-0-harbor-db-postgresql-prepare` pre-switch hook: the coordinator already
  prepared, imported and adopted under the fence. Every generation of the
  module installs `00-0-0-harbor-db-postgresql-rollout-guard`, sorted before
  any preparation, which refuses activation that would strand an open rollout:
  one between fence and adoption, with an interrupted fence or unfinished
  abort, adopted or activated under another rollout manifest, or still
  unwinding. With `rollout` it runs `rollout activation-check`; without it (for
  example after the rollout is retired, or a generation rolled back to) it runs
  the manifest-free `rollout-guard`, which reads only the fixed journal. That
  lets in a thawed, fully aborted or never-started rollout, and an adopted or
  activated one being rolled back (abort then unwinds it).
  `00-harbor-db-postgresql-recovery` still requires accepted recovery evidence.
- **Digests.** `certify` prints `receipt_sha256`, the digest of the receipt it
  produced, computed on the executor; that (not the export's SHA-256, which
  `certify` itself takes) is what `rollout import --sha256` expects.
- **Status.** `rollout status` needs read access to the root-owned journal; an
  unreadable journal is an error, never an idle rollout.

### Executor transport hook

With `rollout.executor.transport` set (one immutable store executable, checked
by the module and again when the manifest loads; wrap arguments in a store
script), `rollout next` from `prepared` performs the hand-off itself. Harbor
runs the hook as the transient root unit `harbor-db-rollout-transport.service`
(`KillMode=control-group`, bounded to eight hours) in `/`, with systemd's clean
environment plus `HOME=/root`, `LANG=C` and the caller's `SSH_AUTH_SOCK`; the
hook sets anything else, its `PATH` included. It appends:

| Argument | Value |
| --- | --- |
| `--config` | the opening rollout manifest (a GC-rooted store path; the executor needs its closure) |
| `--export` | the recorded export directory |
| `--export-sha256` | the recorded SHA-256 of its `recovery-export.json` |
| `--receipt-dir` | a fresh root-private directory for this attempt |

The hook copies the manifest closure and the export to the executor, runs
`rollout certify` there as the declared executor, writes the receipt to
`<receipt-dir>/off-host-recovery.json` (mode `0600`) and prints on stdout only
the `receipt_sha256` that certify printed on the executor, never a digest of the
copy. Its stderr streams to the caller. Harbor holds the receipt to import's
rules (a coordinator-owned private regular file, bounded, a JSON object equal to
that digest), journals it with the export digest it certifies, then imports it
exactly as `rollout import` does.

- **Interruption.** Termination signals are deferred while the hook runs: an
  interrupt or the deadline stops its unit (and every local descendant). While
  that unit is active (an orphan of a killed coordinator) every step refuses,
  and `status` names it.
- **Remote work.** Stopping the unit ends only local processes: the hook must
  end its executor-side work when it dies (for example `ssh -tt`, or a remote
  `timeout`) and must not leave background processes behind. After any failure
  Harbor starts a fresh attempt with a new `--receipt-dir`; a failed attempt's
  directory is removed, and the last one once its receipt is imported.
- **Resuming.** A journaled receipt that still certifies the recorded export is
  imported again (import reuses identical staged bytes); a lost one whose
  recorded receipt is staged is readmitted; otherwise the export is certified
  again. Before a fresh hand-off the fence and the evidence binding are checked.
- **Refused receipts.** If Harbor refuses the recorded receipt, `next` retries
  its admission; `status` then names the manual route (`rollout certify` on the
  executor, `rollout import` here, which sets the recorded receipt aside).
- **Compatibility.** The manifest carries `transport` only when set; a manifest
  with a hook needs a hook-aware coordinator to load or abort it. The journal's
  `transported` entry is written only when present.
