{
  description = "harbor-db - secure generic lifecycle plans and NixOS systemd wiring";

  inputs = {
    harbor-rs.url = "git+https://github.com/caniko/harbor-rs.git?ref=trunk&rev=fac8049316846e0ef1c1e6acd92aed7a337b333a";
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    harbor-meta.follows = "harbor-rs/harbor-meta";
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    harbor-rs,
    harbor-meta,
    treefmt-nix,
    nixpkgs,
    crane,
    ...
  }: let
    systems = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    forAllSystems = f:
      nixpkgs.lib.genAttrs systems (system: let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [(import harbor-rs.inputs.rust-overlay)];
        };
        toolchain = harbor-rs.lib.mkToolchain {
          inherit pkgs;
          toolchainProfile = "stable";
        };
      in
        f {
          inherit system pkgs toolchain;
          craneLib = toolchain.craneLib;
        });
  in {
    nixosModules.harbor-db = {
      lib,
      pkgs,
      ...
    }: {
      imports = [
        (import ./nix/module.nix)
        (lib.mkAliasOptionModule ["services" "db-harbor"] ["services" "harbor-db"])
      ];
      services.harbor-db.package = lib.mkDefault self.packages.${pkgs.system}.harbor-db;
    };
    nixosModules.pg-backup = import ./nix/pg-backup.nix;
    nixosModules.gel = import ./nix/gel.nix;
    nixosModules.db-harbor = self.nixosModules.harbor-db;
    nixosModules.default = self.nixosModules.harbor-db;

    packages = forAllSystems ({
      pkgs,
      craneLib,
      ...
    }: let
      commonArgs = {
        src = craneLib.cleanCargoSource ./.;
        pname = "harbor-db";
        version = "0.1.0";
        strictDeps = true;
        cargoExtraArgs = "--locked";
        meta = {
          description = "Secure generic lifecycle plans and deployment orchestration for services";
          homepage = "https://github.com/caniko/harbor-db";
          license = pkgs.lib.licenses.asl20;
          mainProgram = "harbor-db";
        };
      };
      cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      # Plain crane build, no sccache wrapper: the harbor-rs sandbox wrapper
      # hard-fails (exit 75) on runners without the Canix-managed cache
      # transport (no Redis socket, no host cache root), which is every
      # public CI runner. mkToolchain defaults to an unwrapped craneLib for
      # the same reason; simit-generated flakes never wrap. Dev shells invoke
      # cargo directly and are unaffected.
      harbor-db = craneLib.buildPackage (commonArgs // {inherit cargoArtifacts;});
    in {
      inherit harbor-db;
      db-harbor = harbor-db;
      # Same derivation: exports both harbor-db and the standalone
      # home-manager-backup bin. mainProgram lets `lib.getExe` resolve the
      # backup helper on both supported architectures.
      home-manager-backup =
        harbor-db
        // {
          meta = harbor-db.meta // {mainProgram = "home-manager-backup";};
        };
      default = harbor-db;
    });

    checks = forAllSystems ({
      pkgs,
      craneLib,
      ...
    }: let
      src = craneLib.cleanCargoSource ./.;
      commonArgs = {
        inherit src;
        pname = "harbor-db";
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
      gel-eval = pkgs.callPackage ./nix/gel-eval.nix {
        module = import ./nix/module.nix;
        gelModule = self.nixosModules.gel;
      };
      gel-integration = pkgs.callPackage ./nix/test-gel.nix {
        harborModule = self.nixosModules.default;
        gelModule = self.nixosModules.gel;
      };
      harbor-db = self.packages.${pkgs.stdenv.hostPlatform.system}.harbor-db;
      cargo-fmt = craneLib.cargoFmt {
        inherit src;
        pname = "harbor-db";
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

    formatter = forAllSystems ({
      pkgs,
      toolchain,
      ...
    }:
      (treefmt-nix.lib.evalModule pkgs {
        # harbor-meta no longer ships treefmtModules (input follows
        # harbor-rs/harbor-meta); keep the previous coverage inline:
        # alejandra for Nix (repo standard), taplo for TOML.
        imports = [harbor-rs.treefmtModules.rust];
        programs.alejandra.enable = true;
        programs.taplo.enable = true;
        projectRootFile = "flake.nix";
        programs.rustfmt.package = toolchain.rustToolchain;
      }).config.build.wrapper);

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
