# Changelog

All notable changes to harbor-db are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[SemVer](https://semver.org/). The crate is currently `0.x` and
`publish = false`, so API breaks are minor bumps, not major releases.

## [Unreleased]

### Fixed

- `formatter` output referenced `harbor-meta.treefmtModules.{nix,toml}`,
  which no longer exists at the locked harbor-meta rev (fallout from the
  harbor GitHub input migration; broke `nix flake check` evaluation as a
  whole). Replaced with inline `programs.alejandra` + `programs.taplo`
  next to `harbor-rs.treefmtModules.rust`.

### Added

- `Backend::Gel` (`"gel"`) lifecycle-operation backend for Gel-backed Rust
  applications. Serialized plans are unchanged (`PLAN_VERSION = 1`);
  existing manifests decode as before and the default stays `generic`.
  Downstream Rust code with exhaustive `match` on `Backend` needs a new arm.
- `services.harbor-db.projects.<name>.operations.<op>.backend = "gel"` Nix
  option, with an evaluation guard: `postgres.grants` requires the default
  operation to use `backend = "postgres"`, so Gel settings are never silently
  reinterpreted as PostgreSQL settings.
- `nixosModules.gel` (`services.harbor-db.gel`): digest-pinned Gel server
  image (server 7.1), loopback-only publish by default, password delivered as
  a mounted file (never in the store), `GEL_DOCKER_APPLY_MIGRATIONS=never`,
  per-instance `systemdUnit` for project ordering, and an authenticated
  bounded `readyCheck` probe (`--password-from-stdin` + real query).
- `checks.gel-eval`: evaluation test for the Gel module, backend rendering,
  credential hygiene, ordering, and the grants guardrail.
- `checks.gel-integration`: disposable Gel integration test (pinned server,
  toy project-owned migrations, idempotency, pending/incompatible/error
  semantics, dependent blocking, credential separation, operator gating).
- `tests/gel/live.sh` + `tests/gel/toy-chaosbox`: host-side (podman) runner
  for the same matrix, runnable without Nix.
- `docs/integrations/chaosbox-v1.md`: pinned Chaosbox consumer handoff.

### Compatibility

- Tested pair: Gel server 7.1
  (`docker.io/geldata/gel:7.1@sha256:b7270b0973da6950d01ae0d578c6d38cd8d87fabdd6c4b75a09b74291ad6f3a8`)
  with Gel CLI 7.10.2 (nixpkgs `pkgs.gel`, same version the image bundles).
- Target platform: `x86_64-linux`.
