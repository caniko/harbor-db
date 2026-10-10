{pkgs}: let
  inherit (pkgs) lib;
  inherit (import ./eval-checks.nix {inherit pkgs;}) mkEvalCheck;
  eval = import "${pkgs.path}/nixos/lib/eval-config.nix" {
    system = "x86_64-linux";
    modules = [
      ./pg-backup.nix
      {
        system.stateVersion = "24.11";
        services.harbor-db.pgBackup = {
          enable = true;
          role = "target";
          source.hostName = "10.0.0.2";
          targetSettings.replicatorPasswordFile = "/run/secrets/pg-replicator-password";
        };
      }
    ];
  };
  sourceEval = import "${pkgs.path}/nixos/lib/eval-config.nix" {
    system = "x86_64-linux";
    modules = [
      ./pg-backup.nix
      {
        system.stateVersion = "24.11";
        services.postgresql.enable = true;
        services.harbor-db.pgBackup = {
          enable = true;
          role = "source";
          source.hostName = "10.0.0.1";
          sourceSettings = {
            listenAddresses = ["10.0.0.1"];
            allowedReplicationHosts = ["10.0.0.2/32"];
            replicatorPasswordFile = "/run/secrets/pg-replicator-password";
          };
        };
      }
    ];
  };
  receive = eval.config.systemd.services.pg-receivewal.serviceConfig;
  base = eval.config.systemd.services.pg-basebackup.script;
  sourcePostgresql = sourceEval.config.services.postgresql;
  localEval = import "${pkgs.path}/nixos/lib/eval-config.nix" {
    system = "x86_64-linux";
    modules = [
      ./pg-backup.nix
      {
        system.stateVersion = "24.11";
        services.postgresql.enable = true;
        services.harbor-db.pgBackup = {
          enable = true;
          role = "both";
          source.hostName = "127.0.0.1";
          sourceSettings = {
            replicatorPasswordFile = "/run/secrets/pg-replicator-password";
          };
          targetSettings.sourceLocalRecovery.enable = true;
        };
      }
    ];
  };
  localBackup = localEval.config.systemd.services.pg-basebackup.script;
  ipv6Local =
    (localEval.extendModules {
      modules = [{services.harbor-db.pgBackup.source.hostName = lib.mkForce "::1";}];
    }).config;
  localhost =
    (localEval.extendModules {
      modules = [{services.harbor-db.pgBackup.source.hostName = lib.mkForce "localhost";}];
    }).config;
in
  mkEvalCheck {
    name = "harbor-db-pg-backup-eval";
    resultMessage = "harbor-db PostgreSQL backup creates slots before streaming and verifies base backups";
    assertions = [
      {
        name = "flush-wal-on-receipt";
        assertion = lib.hasInfix "--synchronous" receive.ExecStart;
        message = "WAL must be flushed on receipt, including partial segments.";
      }
      {
        name = "slot-create-is-pre-start";
        assertion = lib.hasInfix "--create-slot" (lib.concatStringsSep "\n" receive.ExecStartPre) && !(lib.hasInfix "--create-slot" receive.ExecStart);
        message = "pg_receivewal slot creation must be a one-shot ExecStartPre";
      }
      {
        name = "restart-policy";
        assertion = receive.Restart == "on-failure" && receive.RestartSteps == 6 && receive.RestartMaxDelaySec == "5min";
        message = "continuous WAL streaming must restart after failure";
      }
      {
        name = "source-replication-hba";
        assertion = lib.hasInfix "host replication replicator 10.0.0.2/32 scram-sha-256" sourcePostgresql.authentication;
        message = "source role must render the allowed replication host in pg_hba.conf";
      }
      {
        name = "source-replication-firewall";
        assertion = lib.elem 5432 sourceEval.config.networking.firewall.interfaces.wg-home.allowedTCPPorts;
        message = "source role must open PostgreSQL on the configured replication interface";
      }
      {
        name = "verify-backup";
        assertion = lib.hasInfix "pg_verifybackup" base;
        message = "base backups must pass pg_verifybackup before publication";
      }
      {
        name = "partial-publish";
        assertion = lib.hasInfix ".partial" base && lib.hasInfix "publish-tree \"$partial_dir\" \"$date_dir\"" base;
        message = "base backups must publish through a partial directory and atomic rename";
      }
      {
        name = "source-local-both-roles";
        assertion = !(builtins.any (a: lib.hasPrefix "services.harbor-db.pgBackup" a.message) (builtins.filter (a: !a.assertion) localEval.config.assertions)) && lib.hasInfix "host replication replicator 127.0.0.1/32 scram-sha-256" localEval.config.services.postgresql.authentication && lib.hasInfix "-h 127.0.0.1" localEval.config.systemd.services.pg-receivewal.serviceConfig.ExecStart && !(localEval.config.networking.firewall.interfaces ? wg-home);
        message = "Source-local reception must configure both source credentials and generated receiver services without opening a replication firewall.";
      }
      {
        name = "source-local-ipv6-replication-hba";
        assertion = lib.hasInfix "host replication replicator ::1/128 scram-sha-256" ipv6Local.services.postgresql.authentication;
        message = "IPv6 loopback reception must have a generated authenticated replication rule.";
      }
      {
        name = "source-local-localhost-replication-hba";
        assertion = lib.all (host: lib.hasInfix "host replication replicator ${host} scram-sha-256" localhost.services.postgresql.authentication) ["127.0.0.1/32" "::1/128"];
        message = "localhost reception must admit both loopback address families.";
      }
      {
        name = "source-local-persistent-mutation-before-publication";
        assertion = lib.hasInfix ''exec 8<>"$backup_root/locks/mutate"'' localBackup && lib.hasInfix ''flock -n 8'' localBackup && !(lib.hasInfix ''locks/mutate'' base);
        message = "Opt-in backup publication must own the existing recovery mutation inode before its legacy anchor; legacy publication remains unchanged.";
      }
      {
        name = "source-local-private-recovery-namespace";
        assertion = lib.elem "f /var/backups/pgbackup/127.0.0.1/recovery/PROTOCOL 0600 postgres postgres - source-local-v1" localEval.config.systemd.tmpfiles.rules && lib.elem "f /var/backups/pgbackup/127.0.0.1/locks/mutate 0600 postgres postgres -" localEval.config.systemd.tmpfiles.rules;
        message = "The capture namespace and mutation inode must be provisioned outside completed backup directories with private ownership.";
      }
    ];
  }
