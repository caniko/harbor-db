{pkgs}: let
  manifest = builtins.fromTOML (builtins.readFile ../Cargo.toml);
in
  pkgs.rustPlatform.buildRustPackage {
    pname = "harbor-db-storage";
    inherit (manifest.package) version;
    src = pkgs.lib.cleanSource ../.;
    cargoLock.lockFile = ../Cargo.lock;
    cargoBuildFlags = ["--bins"];
    doCheck = false;
    disallowedReferences = [pkgs.python3];
    passthru.harborDbRuntime = "rust";
    meta = {
      description = "Native crash-safe storage lifecycle and fenced recovery";
      homepage = "https://github.com/caniko/harbor-db";
      license = pkgs.lib.licenses.asl20;
      mainProgram = "harbor-db-postgres";
      platforms = pkgs.lib.platforms.linux;
    };
  }
