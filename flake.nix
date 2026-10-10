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
    configurationFixture = pkgs: let
      policy = pkgs.writeText "harbor-db-configuration-fixture.json" (builtins.toJSON {
        version = 1;
        resource = "configuration-fixture";
      });
      cutover = pkgs.writeText "harbor-db-cutover-configuration-fixture.json" (builtins.toJSON {
        version = 1;
        enforced = true;
        host = "fixture";
        resources = {};
      });
    in
      pkgs.runCommand "harbor-db-configuration-fixture" {} ''
        mkdir "$out"
        ln -s ${policy} "$out/postgresql.json"
        ln -s ${cutover} "$out/cutover.json"
        ln -s /proc/sys/kernel/ostype "$out/mutable.json"
        ln -s /dev/null "$out/special.json"
      '';
    cargoSource = pkgs: craneLib:
      pkgs.lib.cleanSourceWith {
        src = pkgs.lib.cleanSource ./.;
        filter = path: type:
          (craneLib.filterCargoSources path type)
          # The catalog and retained-baseline tests inspect the public Nix/CI
          # contracts as well as compiled Rust and Python source. Keep the real
          # producer files in sandboxed Cargo checks instead of weakening those
          # assertions or discovering workstation files at test execution time.
          || path == "${toString ./.}/flake.nix"
          || pkgs.lib.hasPrefix "${toString ./.}/nix/" path
          || pkgs.lib.hasPrefix "${toString ./.}/docs/" path
          || pkgs.lib.hasPrefix "${toString ./.}/.github/" path
          || ((pkgs.lib.hasPrefix "${toString ./.}/tests/" path
              || pkgs.lib.hasPrefix "${toString ./.}/python/" path)
            && !(pkgs.lib.hasInfix "__pycache__" path)
            && !(pkgs.lib.hasSuffix ".pyc" path));
      };
    mkCargoArgs = pkgs: craneLib: {
      src = cargoSource pkgs craneLib;
      pname = "harbor-db";
      version = "0.1.0";
      strictDeps = true;
      cargoExtraArgs = "--locked";
      postPatch = ''
        for source in tests/*.rs; do
          if test -f "$source"; then
            substituteInPlace "$source" --replace-warn '/bin/sh' '${pkgs.runtimeShell}'
          fi
        done
      '';
    };
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
    nixosModules.native-storage = {pkgs, ...}: {
      imports = [self.nixosModules.default];
      _module.args.harborDbStoragePackage = self.packages.${pkgs.system}.storage-lifecycle-rust;
    };

    packages = forAllSystems ({
      pkgs,
      craneLib,
      ...
    }: let
      commonArgs =
        mkCargoArgs pkgs craneLib
        // {
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
      harbor-db = craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
          doCheck = false;
        });
      testingArgs =
        commonArgs
        // {
          pname = "harbor-db-test";
          cargoExtraArgs = "--locked --features testing --bin harbor-db-test --bin harbor-db-native-supervisor-fixture --bin harbor-db-writer-fence-fixture --bin harbor-db-backend-transition-fixture --bin harbor-db-provision-fixture --bin harbor-db-postgres-transition-fixture --bin harbor-db-postgres-backup-fixture --bin harbor-db-source-local-recovery-fixture";
          meta = commonArgs.meta // {mainProgram = "harbor-db-test";};
        };
      testingArtifacts = craneLib.buildDepsOnly testingArgs;
      harbor-db-test = craneLib.buildPackage (testingArgs
        // {
          cargoArtifacts = testingArtifacts;
          doCheck = false;
          nativeBuildInputs = [pkgs.makeWrapper];
          postInstall = ''
            mkdir -p "$out/share/harbor-db-test"
            cp -r python tests "$out/share/harbor-db-test/"
            chmod -R u+rwX "$out/share/harbor-db-test"
            find "$out/share/harbor-db-test" -name __pycache__ -type d -exec rm -rf {} +
            cp tests/suite.toml "$out/share/harbor-db-test/suite.toml"
            wrapProgram "$out/bin/harbor-db-test" \
              --prefix PATH : ${pkgs.lib.makeBinPath [pkgs.nix pkgs.systemd pkgs.gitMinimal pkgs.python3 pkgs.cargo pkgs.rustc pkgs.stdenv.cc pkgs.bash pkgs.coreutils pkgs.util-linux]} \
              --set PYTHONPATH "$out/share/harbor-db-test/python:$out/share/harbor-db-test/tests"
          '';
        });
      harbor-db-cached = buildCache.withRustCache {package = harbor-db;};
    in {
      inherit harbor-db harbor-db-cached harbor-db-test;
      storage-lifecycle-rust = harbor-db.overrideAttrs (old: {
        meta = old.meta // {mainProgram = "harbor-db-postgres";};
      });
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
      commonArgs =
        mkCargoArgs pkgs craneLib
        // {
          cargoExtraArgs = "--locked --all-features";
          nativeBuildInputs = [pkgs.postgresql_18 pkgs.postgresql_17 pkgs.python3 pkgs.gitMinimal pkgs.util-linux pkgs.bash pkgs.coreutils];
          HARBOR_DB_TEST_POSTGRES = pkgs.postgresql_18;
          HARBOR_DB_TEST_POSTGRES_17 = pkgs.postgresql_17;
          HARBOR_DB_TEST_CONFIG_FIXTURE = configurationFixture pkgs;
          HARBOR_DB_TEST_MACHINE_ID = pkgs.writeText "harbor-db-test-machine-id" "00000000000000000000000000000001\n";
          preBuild = ''
            export HARBOR_DB_TEST_ROOT_UID=$(${pkgs.coreutils}/bin/stat -Lc %u "$HARBOR_DB_TEST_CONFIG_FIXTURE/postgresql.json")
          '';
          preCheck = ''
            export HARBOR_DB_TEST_TMPDIR="$TMPDIR"
          '';
        };
      inherit (commonArgs) src;
      cargoArtifacts = craneLib.buildDepsOnly commonArgs;
      nativePackage = self.packages.${pkgs.system}.storage-lifecycle-rust;
      nativeModule = self.nixosModules.native-storage;
    in
      {
        native-package-eval = pkgs.callPackage ./nix/module-eval.nix {module = nativeModule;};
        native-storage-eval = pkgs.callPackage ./nix/native-storage-eval.nix {
          module = nativeModule;
          inherit nativePackage;
        };
        native-cli-smoke = pkgs.callPackage ./nix/native-cli-smoke.nix {
          package = nativePackage;
          testPackage = self.packages.${pkgs.system}.harbor-db-test;
        };
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
        postgres-lifecycle-test = import ./nix/test-python-regressions.nix {
          inherit pkgs;
          pythonSource = ./python;
          checkName = "postgres-lifecycle-test";
        };
        postgres-lifecycle-oracle-test = pkgs.callPackage ./nix/test-postgres-oracle.nix {};
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
            cargoDocExtraArgs = "--no-deps";
          });
        cargo-clippy = craneLib.cargoClippy (commonArgs
          // {
            inherit cargoArtifacts;
            cargoExtraArgs = "--all-targets --all-features --locked";
            cargoClippyExtraArgs = "-- -D warnings";
          });
      }
      // pkgs.lib.optionalAttrs (pkgs.system == "x86_64-linux") {
        native-supervisor = pkgs.callPackage ./nix/test-native-supervisor.nix {
          testPackage = self.packages.${pkgs.system}.harbor-db-test;
        };
        native-postgres-backup-service = pkgs.callPackage ./nix/test-postgres-backup-service.nix {
          inherit nativePackage;
          testPackage = self.packages.${pkgs.system}.harbor-db-test;
        };
        native-source-local-recovery = pkgs.callPackage ./nix/test-source-local-recovery.nix {
          inherit nativePackage;
          testPackage = self.packages.${pkgs.system}.harbor-db-test;
        };
        native-postgres-crash-rollback = pkgs.callPackage ./nix/test-postgres-lifecycle.nix {inherit nativePackage;};
        native-postgres-interrupted-upgrade = pkgs.callPackage ./nix/test-postgres-upgrade.nix {inherit nativePackage;};
        native-postgres-recovery-acceptance = pkgs.callPackage ./nix/test-postgres-recovery.nix {inherit nativePackage;};
        native-postgres-writer-fence = pkgs.callPackage ./nix/test-postgres-writer-fence.nix {
          inherit nativePackage;
          testPackage = self.packages.${pkgs.system}.harbor-db-test;
        };
        native-application-provision = pkgs.callPackage ./nix/test-application-provision.nix {
          module = nativeModule;
          inherit nativePackage;
          testPackage = self.packages.${pkgs.system}.harbor-db-test;
        };
        native-application-backup = pkgs.callPackage ./nix/test-application-backup.nix {module = nativeModule;};
        native-application-transition = pkgs.callPackage ./nix/test-application-transition.nix {
          module = nativeModule;
          inherit nativePackage;
          testPackage = self.packages.${pkgs.system}.harbor-db-test;
        };
        native-application-postgres-transition = pkgs.callPackage ./nix/test-application-transition.nix {
          module = nativeModule;
          inherit nativePackage;
          testPackage = self.packages.${pkgs.system}.harbor-db-test;
          withPostgres = true;
        };
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
