{
  pkgs,
  nativePackage,
  testPackage,
}: let
  lib = pkgs.lib;
  postgres = pkgs.postgresql_18;
  legacyPackage = import ./postgres-package.nix {inherit pkgs;};
  tools = [pkgs.bash pkgs.coreutils pkgs.diffutils pkgs.util-linux pkgs.systemd pkgs.findutils pkgs.gnugrep pkgs.gnutar postgres nativePackage];
  fixtureConfig = builtins.toJSON {
    native_package = toString nativePackage;
    legacy_package = toString legacyPackage;
    postgres_package = toString postgres;
    coreutils_package = toString pkgs.coreutils;
    shell = "${pkgs.bash}/bin/bash";
    tool_roots = map toString tools;
  };
in
  pkgs.testers.runNixOSTest {
    name = "harbor-db-native-source-local-recovery";
    extraDriverArgs = ["--junit-xml" "junit.xml"];
    extraPythonPackages = ps: [
      (ps.buildPythonPackage {
        pname = "harbor-db-test-bridge";
        version = "1";
        src = ../python;
        format = "other";
        dontBuild = true;
        installPhase = ''
          mkdir -p "$out/${ps.python.sitePackages}"
          cp -r harbor_db "$out/${ps.python.sitePackages}/"
        '';
      })
    ];
    defaults = {
      virtualisation = {
        memorySize = lib.mkDefault 1024;
        cores = 2;
      };
      services.timesyncd.enable = lib.mkForce false;
      environment.systemPackages = tools;
      system.extraDependencies = [legacyPackage];
      users.users.postgres.shell = pkgs.bash;
    };
    nodes.primary = {
      imports = [./pg-backup.nix ./postgres-lifecycle.nix];
      _module.args.harborDbStoragePackage = nativePackage;
      virtualisation.memorySize = 2048;
      services.postgresql = {
        enable = true;
        package = postgres;
        dataDir = "/var/lib/postgresql/18";
      };
      services.harbor-db.postgresql.enable = false;
      services.harbor-db.pgBackup = {
        enable = true;
        role = "both";
        source.hostName = "127.0.0.1";
        sourceSettings = {
          listenAddresses = ["127.0.0.1"];
          allowedReplicationHosts = ["127.0.0.1/32"];
          firewallInterface = null;
          replicatorPasswordFile = "/run/disposable-secret";
        };
        targetSettings = {
          package = postgres;
          backupDir = "/srv/pgbackup";
          replicatorPasswordFile = "/run/disposable-secret";
          sourceLocalRecovery.enable = true;
          retain = {
            baseBackupDays = 1;
            walDays = 2;
          };
        };
      };
      systemd.services.disposable-secret = {
        before = ["postgresql.service" "postgresql-setup.service"];
        requiredBy = ["postgresql.service" "postgresql-setup.service"];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          UMask = "0077";
        };
        # Disposable VM-only plaintext, generated at runtime, never a credential in the store.
        script = ''
          od -An -N32 -tx1 /dev/urandom | tr -d ' \n' > /run/disposable-secret
          chown postgres:postgres /run/disposable-secret
          chmod 0400 /run/disposable-secret
        '';
      };
      systemd.tmpfiles.rules = ["d /srv/pgbackup 0700 postgres postgres -"];
      systemd.services.pg-receivewal.wantedBy = lib.mkForce [];
      systemd.timers.pg-basebackup.enable = lib.mkForce false;
    };
    nodes.certifier = {
      users.groups.postgres = {};
      users.users.postgres = {
        isSystemUser = true;
        group = "postgres";
      };
    };
    # The bridge supplies transport only; Rust owns ordering, waits and assertions.
    testScript = ''
      import os, socket, subprocess
      from harbor_db.test_bridge import serve
      control, inherited = socket.socketpair()
      fixture = subprocess.Popen(
          ["${testPackage}/bin/harbor-db-source-local-recovery-fixture",
           "--config", ${builtins.toJSON fixtureConfig},
           "--control-fd", str(inherited.fileno()),
           "--acceptance", os.path.join(os.environ["out"], "source-local-recovery-acceptance.json")],
          pass_fds=(inherited.fileno(),))
      inherited.close()
      try:
          serve(control.fileno(), {"primary": primary, "certifier": certifier})
      finally:
          control.close()
          if fixture.poll() is None:
              try:
                  fixture.wait(timeout=30)
              except subprocess.TimeoutExpired:
                  fixture.kill()
                  fixture.wait(timeout=30)
      assert fixture.returncode == 0, fixture.returncode
    '';
  }
