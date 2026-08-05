{
  description = "db-harbor - secure generic lifecycle plans and NixOS systemd wiring";

  inputs = {
    rs-harbor.url = "git+https://codefloe.com/caniko/rs-harbor.git?ref=trunk&rev=7fa1c2104dab4e1dbaa1aaa6df84bba815aa282d";
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    rs-harbor,
    nixpkgs,
    crane,
  }: let
    systems = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    forAllSystems = f:
      nixpkgs.lib.genAttrs systems (system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import rs-harbor.inputs.rust-overlay)];
        };
        toolchain = rs-harbor.lib.mkToolchain {
          inherit pkgs;
          toolchainProfile = "stable";
        };
      in
        f {
          inherit system pkgs toolchain;
          craneLib = toolchain.craneLib;
        });
  in {
    nixosModules.db-harbor = {
      lib,
      pkgs,
      ...
    }: {
      imports = [(import ./nix/module.nix)];
      services.db-harbor.package = lib.mkDefault self.packages.${pkgs.system}.db-harbor;
    };
    nixosModules.pg-backup = import ./nix/pg-backup.nix;
    nixosModules.default = self.nixosModules.db-harbor;

    packages = forAllSystems ({
      pkgs,
      craneLib,
      ...
    }: let
      commonArgs = {
        src = craneLib.cleanCargoSource ./.;
        pname = "db-harbor";
        version = "0.1.0";
        strictDeps = true;
        cargoExtraArgs = "--locked";
        meta = {
          description = "Secure generic lifecycle plans and deployment orchestration for services";
          homepage = "https://codeberg.org/caniko/migrationix";
          license = pkgs.lib.licenses.asl20;
          mainProgram = "db-harbor";
        };
      };
      cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      buildCache = rs-harbor.lib.mkBuildCachePolicy {
        inherit pkgs;
        sccachePackage = rs-harbor.packages.${pkgs.stdenv.hostPlatform.system}.sccache;
        cacheRoot = null;
        namespaceScope = "canix-rust";
        namespaceGeneration = 5;
      };
      db-harbor = buildCache.withRustCache {
        package = craneLib.buildPackage (commonArgs // {inherit cargoArtifacts;});
      };
    in {
      inherit db-harbor;
      # Same derivation: exports both db-harbor and the standalone
      # home-manager-backup bin. mainProgram lets `lib.getExe` resolve the
      # backup helper on both supported architectures.
      home-manager-backup =
        db-harbor
        // {
          meta = db-harbor.meta // {mainProgram = "home-manager-backup";};
        };
      default = db-harbor;
    });

    checks = forAllSystems ({
      pkgs,
      craneLib,
      ...
    }: let
      src = craneLib.cleanCargoSource ./.;
      commonArgs = {
        inherit src;
        pname = "db-harbor";
        version = "0.1.0";
        strictDeps = true;
        cargoExtraArgs = "--locked";
      };
      cargoArtifacts = craneLib.buildDepsOnly commonArgs;
    in {
      module-eval = pkgs.callPackage ./nix/module-eval.nix {
        module = import ./nix/module.nix;
      };
      module-smoke = pkgs.callPackage ./nix/test-module.nix {
        module = self.nixosModules.default;
      };
      pg-backup-eval = pkgs.callPackage ./nix/pg-backup-eval.nix {};
      db-harbor = self.packages.${pkgs.stdenv.hostPlatform.system}.db-harbor;
      cargo-fmt = craneLib.cargoFmt {
        inherit src;
        pname = "db-harbor";
      };
      cargo-test = craneLib.cargoTest (commonArgs
        // {
          inherit cargoArtifacts;
          cargoExtraArgs = "--all-targets --all-features --locked";
        });
      cargo-clippy = craneLib.cargoClippy (commonArgs
        // {
          inherit cargoArtifacts;
          cargoExtraArgs = "--all-targets --all-features --locked";
          cargoClippyExtraArgs = "-- -D warnings";
        });
    });

    formatter = forAllSystems ({pkgs, ...}: pkgs.alejandra);

    devShells = forAllSystems ({pkgs, ...}: let
      packages = [
        pkgs.alejandra
        pkgs.cargo
        pkgs.cargo-nextest
        pkgs.clippy
        pkgs.gcc
        pkgs.nixd
        pkgs.rustc
        pkgs.rustfmt
      ];
    in {
      default = pkgs.mkShell {inherit packages;};
      docs = pkgs.mkShell {inherit packages;};
    });
  };
}
