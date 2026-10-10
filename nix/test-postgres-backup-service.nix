{
  pkgs,
  nativePackage,
  testPackage,
}: let
  inherit (pkgs) lib;
  postgres = pkgs.postgresql_18;
  legacyPackage = import ./test-python-package.nix {inherit pkgs;};
  bridge = import ./test-native-bridge.nix {inherit pkgs;};
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
    extraPythonPackages = ps: [(bridge.package ps)];
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
    testScript = bridge.script {
      fixture = "${testPackage}/bin/harbor-db-postgres-backup-fixture";
      arguments = ["--config" fixtureConfig];
      nodes = ["primary" "backup"];
      artifact = "backup-service-acceptance.json";
    };
  }
