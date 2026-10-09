{
  config,
  lib,
  pkgs,
  ...
}: let
  inherit (lib) mkOption types;
  projects = lib.filterAttrs (_: p: p.transition.enable) config.services.harbor-db.projects;
  package = import ./postgres-package.nix {inherit pkgs;};
  manifest = name: p:
    pkgs.writeText "harbor-db-${name}-transition.json" (builtins.toJSON {
      version = 1;
      resource = name;
      source_manifest = p.sourceManifest;
      target_manifest = p.targetManifest;
      backup_manifest = p.backupManifest;
      independent_receipt = p.independentReceipt;
      custody_manifest = p.targetCustodyManifest;
      postgres_manifest = p.postgresManifest;
      postgres_socket = p.postgresSocket;
      postgres_port = p.postgresPort;
      barrier_dir = p.barrierDirectory;
      drop_in_root = "/etc/systemd/system.control";
      systemctl = "${pkgs.systemd}/bin/systemctl";
      busctl = "${pkgs.systemd}/bin/busctl";
      runuser = "${pkgs.util-linux}/bin/runuser";
      storage_package = "${package}/bin";
      units = p.runtimeUnits;
      retired_units = p.retiredUnits;
      commands = p.commands;
      executable_files = p.executableFiles;
      timeout_seconds = p.timeoutSeconds;
    });
in {
  options.services.harbor-db.projects = mkOption {
    type = types.attrsOf (types.submodule ({
      name,
      config,
      ...
    }: {
      options.transition = {
        enable = lib.mkEnableOption "explicit application backend transition tooling";
        sourceManifest = mkOption {type = types.strMatching "/nix/store/.*";};
        targetManifest = mkOption {type = types.strMatching "/nix/store/.*";};
        backupManifest = mkOption {type = types.strMatching "/nix/store/.*";};
        targetCustodyManifest = mkOption {
          type = types.nullOr (types.strMatching "/nix/store/.*");
          default = null;
          description = "Target cutover resource entry with transition_manifest omitted, for durable corpus custody publication.";
        };
        postgresManifest = mkOption {
          type = types.nullOr (types.strMatching "/nix/store/.*");
          default = null;
        };
        postgresSocket = mkOption {
          type = types.strMatching "/[A-Za-z0-9_./-]+";
          default = "/run/postgresql";
        };
        postgresPort = mkOption {
          type = types.port;
          default = 5432;
        };
        independentReceipt = mkOption {type = types.strMatching "/[A-Za-z0-9_./-]+";};
        barrierDirectory = mkOption {
          type = types.strMatching "/[A-Za-z0-9_./-]+";
          default = "/var/lib/harbor-db-transitions/${name}";
        };
        runtimeUnits = mkOption {type = types.listOf (types.strMatching "[A-Za-z0-9_@.:-]+\\.service");};
        retiredUnits = mkOption {
          type = types.listOf (types.strMatching "[A-Za-z0-9_@.:-]+\\.service");
          default = [];
        };
        commands = mkOption {
          type = types.attrsOf (types.submodule {
            options = {
              user = mkOption {type = types.strMatching "[a-z_][a-z0-9_-]*";};
              argv = mkOption {type = types.listOf types.str;};
            };
          });
          description = "Declared import, verify-target, verify-source and health workers. Imports must be resumable for the same source snapshot.";
        };
        executableFiles = mkOption {
          type = types.listOf types.str;
          default = [];
        };
        timeoutSeconds = mkOption {
          type = types.ints.between 1 86400;
          default = 1800;
        };
        manifest = mkOption {
          type = types.path;
          readOnly = true;
          default = manifest name config.transition;
        };
      };
    }));
  };
  config = lib.mkIf (projects != {}) {
    assertions =
      lib.mapAttrsToList (name: p: {
        assertion = builtins.attrNames p.transition.commands == ["health" "import" "verify-source" "verify-target"] && p.transition.runtimeUnits != [];
        message = "Harbor DB ${name} transitions require all semantic workers and explicit runtime units.";
      })
      projects;
    systemd.tmpfiles.rules = lib.concatMap (p: [
      "d ${p.transition.barrierDirectory} 0700 root root -"
      "f ${p.transition.barrierDirectory}/lock 0600 root root -"
    ]) (lib.attrValues projects);
    environment.etc = lib.mapAttrs' (name: p: lib.nameValuePair "harbor-db/${name}-transition.json" {source = manifest name p.transition;}) projects;
    environment.systemPackages = [package];
  };
}
