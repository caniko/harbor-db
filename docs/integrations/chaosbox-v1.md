# Chaosbox v1 integration handoff (harbor-db Gel support)

Target consumer: Chaosbox (native Rust Graphify rewrite, Gel-authoritative).
Scope: harbor-db side only. Chaosbox owns SDL, migrations, roles, queries,
and the `chaosbox db check|migrate --json` commands. simit owns CI/release
generation. Do not copy Chaosbox schema, Jev calls, or graph logic into
harbor-db; the fixture below is a 3-statement toy.

## 1. Tested, pinned versions

| Component | Version | Pin |
|---|---|---|
| Gel server | `7.1` (`7.1+08db576`, full build `7.1+d2025120316.ge0ef1b92d.cv202508070002.r202512051814.tpa4dmxzwgqwxk3tlnzxxo3rnnruw45lyfvtw45i.bofficial.s08db576`) | `docker.io/geldata/gel:7.1@sha256:b7270b0973da6950d01ae0d578c6d38cd8d87fabdd6c4b75a09b74291ad6f3a8` |
| Gel CLI | `7.10.2+feff35c` | flake input `nixpkgs` rev `b1b875982b17dabde9b4a37f3e229e74913e6db3` (`pkgs.gel`); the server image bundles the identical CLI build |
| Plan wire format | `PLAN_VERSION = 1`, unchanged | `src/lib.rs` |
| Backend label | `"gel"` | Rust `Backend::Gel`, Nix `backend = "gel"` |

CLI/server compatibility is same-major (7.x). The exact pair above was
executed together (live test §6). Re-pin procedure (§8) before changing
either side.

## 2. Public interfaces added by this change

Rust (`src/lib.rs`):

- `Backend::Gel` — serialized `"gel"`. Defaults and all existing manifests
  are unchanged (`Backend::default()` is still `Generic`; v1 manifests
  without backend fields still decode). This is a SemVer-minor addition for
  plan authors; Rust consumers with exhaustive `match` on `Backend` add one
  arm. The crate is `0.x` + `publish = false`; see `CHANGELOG.md`.
- No new runner, no Gel client code, no schema parsing. Gel operations use
  the existing `CommandSpec` / `credential_args` / `credential_environment`
  path byte-for-byte.

Nix:

- `nixosModules.gel` (`nix/gel.nix`):
  `services.harbor-db.gel.{cliPackage, image, readyCheck,
  instances.<name>.{enable, port, bindAddress, dataDir, passwordFile,
  tlsCertMode, extraEnvironment, systemdUnit}}`.
  Defaults: `cliPackage = pkgs.gel`, digest-pinned image above,
  `port = 5656`, `bindAddress = "127.0.0.1"`,
  `dataDir = /var/lib/harbor-db-gel/<name>`,
  `tlsCertMode = "generate_self_signed"`.
- `services.harbor-db.projects.<name>.operations.<op>.backend` accepts
  `"gel"` (`nix/module.nix`). New evaluation guard: `postgres.grants.enable`
  requires the `default` operation to use `backend = "postgres"` — Gel
  settings are never silently reinterpreted as PostgreSQL settings, and no
  `psql` GRANT helper ever runs against a Gel operation.
- `checks.gel-eval` (`nix/gel-eval.nix`): digest pin, loopback publish,
  password-as-file, `GEL_DOCKER_APPLY_MIGRATIONS=never`, `"gel"` plan
  rendering, credential hygiene, Gel→migration→runtime ordering,
  grants-guardrail negative case, `readyCheck` mechanism.
- `checks.gel-integration` (`nix/test-gel.nix`): disposable nixosTest that
  boots the pinned server, applies the toy fixture through the generic
  runner, and asserts the §6 matrix except parallel isolation (covered by
  `tests/gel/live.sh`): pending→apply→current, idempotent re-apply, broken
  and incompatible migrations blocking the dependent, reader credential
  separation, wrong-password error, operator-wipe exclusion, secret hygiene.
  The VM preloads the image via `pullImage` (test guests have no registry
  egress) and therefore runs the tag form: `docker load` drops RepoDigests
  so a digest ref cannot resolve locally. Bits are identical; the digest
  pin itself is enforced by `gel-eval` on the module default.

Test facility (`tests/gel/`, no Chaosbox content):

- `toy-chaosbox` — fixture CLI speaking the §4 contract against committed
  `migrations/*.edgeql` (toy type, reader role, seed row).
- `migrations/` — the three committed fixture statements.
- `live.sh` — host-side podman runner for the full matrix; see §6.

## 3. Minimal consumer composition (existing runner model)

```nix
{
  imports = [ inputs.harbor-db.nixosModules.default inputs.harbor-db.nixosModules.gel ];

  services.harbor-db.gel.instances.chaosbox = {
    enable = true;
    port = 56561;
    passwordFile = config.age.secrets.chaosbox-gel-admin.path;
  };
  services.harbor-db.dataDirectories = [{
    path = "/var/lib/harbor-db-gel/chaosbox";
    user = "root"; group = "root"; mode = "0700";
  }];

  services.harbor-db.projects.chaosbox = {
    enable = true;
    # Server availability is its own operation: the container unit being
    # started does not mean Gel accepts connections yet, and neither the
    # project command nor harbor-db retries on its own. The probe below
    # (authenticated, bounded) gates the schema migration.
    operations.ready = {
      enable = true;
      backend = "gel";
      credentials.admin-pw = config.age.secrets.chaosbox-gel-admin.path;
      runner = {
        command = "${config.services.harbor-db.gel.readyCheck}/bin/harbor-db-gel-ready --host 127.0.0.1 --port 56561 --user admin --password-file \"$READY_PW_FILE\" --timeout 300s";
        checkCommand = "${config.services.harbor-db.gel.readyCheck}/bin/harbor-db-gel-ready --host 127.0.0.1 --port 56561 --user admin --password-file \"$READY_PW_FILE\" --timeout 60s";
        credentialEnvironment.READY_PW_FILE = "admin-pw";
      };
      after = [config.services.harbor-db.gel.instances.chaosbox.systemdUnit];
      requires = [config.services.harbor-db.gel.instances.chaosbox.systemdUnit];
    };
    operations.schema = {
      enable = true;
      backend = "gel";
      credentials.admin-creds = config.age.secrets.chaosbox-gel-creds.path;
      runner = {
        package = pkgs.chaosbox;
        executable = "bin/chaosbox";
        args = ["db" "migrate" "--json"];
        checkArgs = ["db" "check" "--json"];
        credentialEnvironment.CHAOSBOX_GEL_CREDENTIALS_FILE = "admin-creds";
      };
      after = [config.services.harbor-db.gel.instances.chaosbox.systemdUnit];
      requires = [config.services.harbor-db.gel.instances.chaosbox.systemdUnit];
      dependsOn = ["ready"];
    };
    runtimeUnits = ["chaosbox.service" "chaosbox-worker.service"];
    serviceConfig.ReadWritePaths = ["/var/lib/chaosbox"];
  };
}
```

Ordering enforced: Gel container started → `ready` probe succeeds
(server accepting authenticated connections) → `harbor-db-chaosbox.service`
(schema apply) → `chaosbox*.service`. Readers/consumers start only after a
successful migration unit; a failed migration fails the unit and blocks them.
Give readers their own credential with a non-superuser Gel role and never
reuse the migration credential for runtime (§4).

## 4. Credential and readiness contract (Chaosbox v1, shared)

- `CHAOSBOX_GEL_CREDENTIALS_FILE` is a **runtime credential-file path**
  delivered via systemd `LoadCredential` →
  `credentialEnvironment` (harbor-db resolves it under
  `$CREDENTIALS_DIRECTORY`; secret contents never enter plans, logs, or the
  store — asserted in `gel-eval`, `test-gel`, and `live.sh`).
- harbor-db never parses the file. Recommended content: a native Gel
  credentials JSON used with `gel --credentials-file` (proven in §6), or a
  DSN file consumed via `GEL_DSN`. Chaosbox owns the format decision.
- Both commands emit one JSON object:
  `contract_version = 1`, `backend = "gel"`, `operation`, `status` in
  `{ready, pending, incompatible, error}`, sanitized `diagnostics`
  (names/counts/versions only — never credential material).
- `check` is read-only: exit `0` = ready, exit `2` = pending, any other
  nonzero = incompatible/error. harbor-db maps exit `0`→current,
  `2`→pending, other-nonzero→failure (surfacing exit `1` with the JSON
  status preserved in output). **Chaosbox must exit 2 for pending** to get
  pending semantics; use distinct non-2 codes (e.g. 3/1) for
  incompatible/error so logs stay diagnosable.
- `migrate` applies committed migrations idempotently and exits `0` only
  after re-verified readiness. It must never wipe, drop/recreate, reset
  migration history, upgrade the server, or run backfills needing
  confirmation — those stay `operator_confirmed` operations run only with
  explicit `--operation … --confirm`.
- Permission model is Gel roles, not PostgreSQL `GRANT`s: migration uses a
  superuser credential; runtime uses a least-privilege role. Proven: a
  non-superuser role runs `SELECT` but gets
  `DisabledCapabilityError: cannot execute DDL commands` (§6, check 12).
- Schema failure blocks dependents at two levels: the migration unit fails
  (systemd ordering) and `depends_on` stops later plan operations.

## 5. Operational rules

- Nix generation rollback is not schema rollback. Migrations are
  forward-only; recovery is forward fix + `gel dump` backups (project-owned,
  e.g. an `operator_confirmed` dump operation), never automatic reversal.
- No automatic restore, wipe, drop/recreate, history reset, or server
  upgrade. `GEL_DOCKER_APPLY_MIGRATIONS=never` is set so container startup
  never applies schema implicitly.
- Listeners are loopback by default; changing `bindAddress` requires
  reviewed TLS/auth. Password auth is mandatory (`passwordFile` is required;
  no trust-auth mode exists in this module).
- State-directory ownership: the container entrypoint runs as root and
  chowns `dataDir` to the server user itself (`edbdocker_ensure_dirs`),
  so a `root:root 0700` host directory works for rootful runtimes
  (verified in the image's entrypoint source). Rootless runtimes skip that
  step — there the directory must already be writable by the mapped server
  uid (the live test maps it with `--userns=keep-id`).

## 6. Real test results (executed 2026-09-18, atlas, x86_64-linux)

`cargo test --locked`: **16 lib + 4 backup tests pass** (14 pre-existing +
2 new Gel serialization tests). `cargo fmt --check`: clean.
`nix-instantiate --parse`: clean for all new/changed Nix files.
`tests/gel/live.sh --stacks 2` (podman, image digest above, repo-built
`harbor-db`): **ALL GEL INTEGRATION CHECKS PASSED — 22/22 per stack, two
stacks in parallel** (ports 56561/56562, isolated data dirs/passwords):

1. server startup + authenticated readiness; 2. authenticated EdgeQL
   returns data; 3. password-from-stdin readiness (the exact `readyCheck`
   flag sequence incl. `--wait-until-available`); 4. read-only check
   reports pending (exit 2), state untouched; 5. check applied nothing;
2. apply migrates, dependent runs;
3. operator wipe excluded from activation (`skipped-manual`); 8. three
   committed migrations applied exactly once; 9. migrated data visible in
   Gel (`ToyItem` count 1); 10. repeated apply idempotent; 11. post-migration
   check current (exit 0); 12. reader credentials pass read-only check;
4. reader migrate denied (`DisabledCapabilityError` permission);
5. incompatible migration surfaces distinctly and blocks; 15. incompatible
   blocks dependent startup (event count unchanged); 16. broken EdgeQL
   fails the apply and blocks the dependent; 17. apply recovers after the
   bad file is removed; 18. wrong password is error (exit 1), never
   ready/pending; 19. admin password absent from all plans and logs
   (present only in credential files); 20. explicit
   `--operation wipe --confirm` runs; 21.–22. container + state dir cleaned
   up. Versions observed: CLI `7.10.2+feff35c`, server `7.1+08db576`.

## 7. CI status (simit `nix flake check`, branch `gel-support`)

- **Evaluation: green.** `nix flake check` evaluates all outputs: the
  `gel-eval` derivation builds, and every Gel plan/unit derivation builds
  (`harbor-db-geltoy-plan.json`, wipe apply/check scripts, setup unit).
  Verified in CI run `35393490927` (and follow-ups).
- **Formatter fix verified.** The `formatter` output referenced
  `harbor-meta.treefmtModules.{nix,toml}`, which no longer exists at the
  locked harbor-meta rev (fallout from the harbor GitHub input migration;
  pre-existing, broke `nix flake check` before any check could run). Fixed
  by inlining `programs.alejandra` + `programs.taplo` next to
  `harbor-rs.treefmtModules.rust`; the check now proceeds past `formatter`.
- **Blocked (pre-existing, not Gel-related): Rust builds fail inside
  harbor-rs sccache.** `harbor-db-deps` fails with
  `harbor-rs-sandbox-sccache rustc -vV (exit status: 75)`. The identical
  failure occurs on `trunk` without any Gel changes (CI run `33169533667`,
  2026-08-28; trunk has been red since the GitHub migration on 2026-08-15).
  No Gel file influences this path (no new Rust deps, lockfile untouched).
  Routed to harbor-rs/infra owners; unblocks `checks.harbor-db`,
  `module-smoke`, `gel-integration` VM run, and the cargo test/clippy/docs
  steps, which all short-circuit behind it.
- `cargo clippy --deny warnings`: additionally unverified locally (driver/
  sysroot mismatch); the change adds no `match` on `Backend` (grep-verified).

## 8. Limitations and re-pin prerequisites

- Supported: `x86_64-linux` only. The image has other-arch manifests;
  `aarch64-linux` is defined but untested — do not advertise it.
- Re-pin server: update `services.harbor-db.gel.image` (tag+digest),
  re-run `tests/gel/live.sh --stacks 2`, record CLI/server strings here.
- Bump CLI: update the flake `nixpkgs` input, confirm `pkgs.gel --version`
  stays same-major with the pinned server, re-run `live.sh`.
- Server major upgrade (e.g. 7→8): treat as explicit migration with a
  `gel dump` backup first; never silent. The toy `REQUIRES-MAJOR`
  mechanism models version-skew detection — Chaosbox should implement the
  equivalent against its committed migrations.
- The `gel` server binary is obtained as the digest-pinned official image,
  not built from source; no `gel server install` download ever runs during
  activation or build, and no developer-local Gel instance is used
  (every test uses isolated ports, data dirs, and passwords).
