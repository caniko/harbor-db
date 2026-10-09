{
  description = "harbor-db - secure generic lifecycle plans and NixOS systemd wiring";

  inputs = {
    harbor-rs.url = "git+https://github.com/caniko/harbor-rs.git?ref=trunk&rev=05cc4f162b55fa904b687db1821e2463fa813e50";
    rs-harbor.follows = "harbor-rs";
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    harbor-rs,
    nixpkgs,
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
          cache.enable = false;
        };
      in
        f {
          inherit system pkgs toolchain;
          inherit (toolchain) craneLib;
        });
  in {
    lib.postgresRecoveryReadiness = 1;
    lib.postgresRecoveryPreparation = 1;
    lib.postgresWriterFence = 3;
    lib.applicationProvisioning = 1;
    lib.applicationBackup = 1;
    lib.applicationBackendTransition = 1;
    lib.cutoverPreflight = 3;
    nixosModules.harbor-db = {
      lib,
      pkgs,
      ...
    }: {
      key = "${./nix/module.nix}:flake-wrapper";
      imports = [
        ./nix/module.nix
        ./nix/postgres-lifecycle.nix
        ./nix/cutover.nix
        (lib.mkAliasOptionModule ["services" "db-harbor"] ["services" "harbor-db"])
      ];
      services.harbor-db.package = lib.mkDefault self.packages.${pkgs.system}.harbor-db;
    };
    nixosModules.pg-backup = import ./nix/pg-backup.nix;
    nixosModules.postgres-lifecycle = ./nix/postgres-lifecycle.nix;
    nixosModules.cutover = ./nix/cutover.nix;
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
      buildCache = harbor-rs.lib.mkBuildCachePolicy {
        inherit pkgs;
        sccachePackage = harbor-rs.packages.${pkgs.stdenv.hostPlatform.system}.sccache;
        # The pinned wrapper needs an explicit disk transport when a hosted
        # runner has no Redis socket. It still prefers the host Redis transport
        # and admits only the versioned nixbld-owned, group-writable namespace.
        cacheRoot = "/var/cache/sccache";
        namespaceScope = "canix-rust";
        namespaceGeneration = 5;
      };
      harbor-db = craneLib.buildPackage (commonArgs // {inherit cargoArtifacts;});
      harbor-db-cached = buildCache.withRustCache {package = harbor-db;};
    in {
      inherit harbor-db harbor-db-cached;
      postgres-lifecycle = import ./nix/postgres-package.nix {inherit pkgs;};
      storage-lifecycle = import ./nix/postgres-package.nix {inherit pkgs;};
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
      application-provision-eval = pkgs.callPackage ./nix/application-provision-eval.nix {module = self.nixosModules.default;};
      application-provision = pkgs.callPackage ./nix/test-application-provision.nix {module = self.nixosModules.default;};
      application-backup = pkgs.callPackage ./nix/test-application-backup.nix {module = self.nixosModules.default;};
      application-transition = pkgs.callPackage ./nix/test-application-transition.nix {module = self.nixosModules.default;};
      application-postgres-transition = pkgs.callPackage ./nix/test-application-transition.nix {
        module = self.nixosModules.default;
        withPostgres = true;
      };
      module-smoke = pkgs.callPackage ./nix/test-module.nix {
        module = self.nixosModules.default;
      };
      pg-backup-eval = pkgs.callPackage ./nix/pg-backup-eval.nix {};
      cutover-eval = pkgs.callPackage ./nix/cutover-eval.nix {
        module = self.nixosModules.default;
      };
      postgres-lifecycle-eval = pkgs.callPackage ./nix/postgres-lifecycle-eval.nix {
        module = self.nixosModules.default;
        lifecycleModule = self.nixosModules.postgres-lifecycle;
      };
      postgres-crash-rollback = pkgs.callPackage ./nix/test-postgres-lifecycle.nix {};
      postgres-interrupted-upgrade = pkgs.callPackage ./nix/test-postgres-upgrade.nix {};
      postgres-recovery-acceptance = pkgs.callPackage ./nix/test-postgres-recovery.nix {};
      postgres-writer-fence = pkgs.callPackage ./nix/test-postgres-writer-fence.nix {};
      postgres-lifecycle-test =
        pkgs.runCommand "harbor-db-postgres-lifecycle-test" {
          nativeBuildInputs = [pkgs.python3 pkgs.gitMinimal pkgs.postgresql_18];
          HARBOR_DB_TEST_POSTGRES = pkgs.postgresql_18;
        } ''
          PYTHONPATH=${./python} python3 -B -m unittest discover -s ${./tests} -p 'test_*.py'
          touch "$out"
        '';
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
      cargo-doc = craneLib.cargoDoc (commonArgs
        // {
          inherit cargoArtifacts;
          cargoDocExtraArgs = "--no-deps --all-features";
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
