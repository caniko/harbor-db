{
  harbor-meta,
  harbor-rs,
}: _: {
  imports = [
    harbor-meta.treefmtModules.nix
    harbor-meta.treefmtModules.toml
    harbor-rs.treefmtModules.rust
  ];
  projectRootFile = "flake.nix";
  settings.excludes = [
    "templates/**"
    "tests/fixtures/**"
    "checks/fixtures/**"
    "tests/pr14-baseline.toml"
    ".github/workflows/*.yaml"
  ];
}
