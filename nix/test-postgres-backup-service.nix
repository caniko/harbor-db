{
  pkgs,
  nativePackage,
  testPackage,
}: let
  lib = pkgs.lib;
  postgres = pkgs.postgresql_18;
  legacyPackage = import ./postgres-package.nix {inherit pkgs;};
  tools = [pkgs.bash pkgs.coreutils pkgs.diffutils pkgs.util-linux pkgs.systemd pkgs.findutils pkgs.gnugrep postgres nativePackage];
  fixtureConfig = builtins.toJSON {
    native_package = toString nativePackage;
    legacy_package = toString legacyPackage;
    postgres_package = toString postgres;
    shell = "${pkgs.bash}/bin/bash";
    tool_roots = map toString tools;
  };
in
  pkgs.testers.runNixOSTest {
    name = "harbor-db-native-postgres-backup-service";
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
      imports = [./pg-backup.nix];
      _module.args.harborDbStoragePackage = nativePackage;
      virtualisation = {
        memorySize = 1024;
        cores = 2;
      };
      environment.systemPackages = tools;
      services.harbor-db.pgBackup = {
        enable = true;
        source.hostName = "192.168.1.10";
      };
    };
    nodes.primary = {
      networking.interfaces.eth1.ipv4.addresses = lib.mkForce [
        {
          address = "192.168.1.10";
          prefixLength = 24;
        }
      ];
      services.postgresql = {
        enable = true;
        package = postgres;
      };
      services.harbor-db.pgBackup = {
        role = "source";
        sourceSettings = {
          listenAddresses = ["192.168.1.10"];
          allowedReplicationHosts = ["192.168.1.20/32"];
          firewallInterface = "eth1";
          replicatorPasswordFile = "/run/backup-credential/password";
          maxWalSenders = 6;
          maxReplicationSlots = 4;
        };
      };
      systemd.services.backup-credential = {
        before = ["postgresql.service" "postgresql-setup.service"];
        requiredBy = ["postgresql.service" "postgresql-setup.service"];
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          UMask = "0077";
        };
        script = ''
          install -d -m 0700 -o postgres -g postgres /run/backup-credential
          od -An -N32 -tx1 /dev/urandom | tr -d ' \n' > /run/backup-credential/password
          chown postgres:postgres /run/backup-credential/password
          chmod 0400 /run/backup-credential/password
        '';
      };
    };
    nodes.backup = {
      networking.interfaces.eth1.ipv4.addresses = lib.mkForce [
        {
          address = "192.168.1.20";
          prefixLength = 24;
        }
      ];
      users.groups.postgres = {};
      users.users.postgres = {
        isSystemUser = true;
        group = "postgres";
      };
      system.extraDependencies = [legacyPackage];
      systemd.tmpfiles.rules = ["d /srv/pgbackup 0700 postgres postgres -"];
      services.harbor-db.pgBackup = {
        role = "target";
        targetSettings = {
          package = postgres;
          backupDir = "/srv/pgbackup";
          replicatorPasswordFile = "/run/backup-credential/password";
          baseBackup.maxRate = "1M";
          retain = {
            baseBackupDays = 1;
            walDays = 2;
          };
        };
      };
      systemd.services.pg-receivewal.wantedBy = lib.mkForce [];
      systemd.timers.pg-basebackup.enable = lib.mkForce false;
    };
    # Python supplies only node objects and the version-1 transport.
    testScript = ''
      import os, socket, subprocess
      from harbor_db.test_bridge import serve
      control, inherited = socket.socketpair()
      fixture = subprocess.Popen(
          ["${testPackage}/bin/harbor-db-postgres-backup-fixture",
           "--config", ${builtins.toJSON fixtureConfig},
           "--control-fd", str(inherited.fileno()),
           "--acceptance", os.path.join(os.environ["out"], "backup-service-acceptance.json")],
          pass_fds=(inherited.fileno(),))
      inherited.close()
      try:
          serve(control.fileno(), {"primary": primary, "backup": backup})
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
