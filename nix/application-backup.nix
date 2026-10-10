{
  config,
  lib,
  pkgs,
  harborDbStoragePackage,
  ...
}: let
  inherit (lib) mkOption types;
  projects = lib.filterAttrs (_: p: p.backup.enable) config.services.harbor-db.projects;
  package = harborDbStoragePackage;
  manifest = name: backup:
    pkgs.writeText "harbor-db-${name}-backup.json" (builtins.toJSON {
      version = 1;
      resource = name;
      root = backup.directory;
      inherit (backup) commands;
      executable_files = backup.executableFiles;
      timeout_seconds = backup.timeoutSeconds;
      maximum_age_seconds = backup.maximumAgeSeconds;
    });
in {
  imports = [./storage-package-argument.nix];
  options.services.harbor-db.projects = mkOption {
    type = types.attrsOf (types.submodule {
      options.backup = {
        enable = lib.mkEnableOption "executed application backup and disposable restore acceptance";
        user = mkOption {type = types.strMatching "[a-z_][a-z0-9_-]*";};
        group = mkOption {type = types.strMatching "[a-z_][a-z0-9_-]*";};
        directory = mkOption {type = types.strMatching "/[A-Za-z0-9_./-]+";};
        commands = mkOption {
          type = types.attrsOf (types.listOf types.str);
          description = "Absolute capture/restore/verify/cleanup argv. Exact {backup} and {workspace} arguments are replaced without a shell.";
        };
        executableFiles = mkOption {
          type = types.listOf types.str;
          default = [];
          description = "Additional immutable adapter/interpreter files whose hashes are bound to acceptance.";
        };
        timeoutSeconds = mkOption {
          type = types.ints.between 1 86400;
          default = 1800;
        };
        maximumAgeSeconds = mkOption {
          type = types.ints.between 1 86400;
          default = 3600;
        };
        readers = mkOption {
          type = types.listOf types.str;
          default = [];
        };
        calendar = mkOption {
          type = types.str;
          default = "*:0/15";
        };
        requires = mkOption {
          type = types.listOf types.str;
          default = [];
        };
        credentials = mkOption {
          type = types.attrsOf types.str;
          default = {};
          description = "Systemd credential names mapped to runtime paths, never secret contents or Nix path coercion.";
        };
      };
    });
  };
  config = lib.mkIf (projects != {}) {
    assertions = lib.concatLists (lib.mapAttrsToList (name: p: [
        {
          assertion = builtins.match "[A-Za-z0-9_-]{1,128}" name != null;
          message = "Harbor DB ${name} backup project name must contain 1..128 ASCII letters, digits, underscores or hyphens, matching the runtime resource name.";
        }
        {
          assertion = builtins.attrNames p.backup.commands == ["capture" "cleanup" "restore" "verify"];
          message = "Harbor DB ${name} backup requires capture, restore, verify and cleanup.";
        }
        {
          assertion = lib.all (argv: argv != [] && lib.hasPrefix "/" (builtins.head argv)) (lib.attrValues p.backup.commands);
          message = "Harbor DB ${name} backup commands must be absolute.";
        }
        {
          assertion = lib.all (stage: let
            argv = p.backup.commands.${stage} or [];
          in
            lib.elem "{backup}" argv && (stage == "capture" || lib.elem "{workspace}" argv)) ["capture" "restore" "verify" "cleanup"];
          message = "Harbor DB ${name} backup command placeholders require a literal {backup} argument in every stage and {workspace} in restore, verify and cleanup.";
        }
        {
          assertion =
            lib.all (credential:
              builtins.match "[A-Za-z0-9_.-]{1,255}" credential != null && !(lib.elem credential ["." ".."])) (builtins.attrNames p.backup.credentials);
          message = "Harbor DB ${name} backup credential names must be filename-safe ASCII IDs of 1..255 bytes, excluding '.' and '..'.";
        }
        {
          assertion = lib.all (path: lib.hasPrefix "/" path && !(lib.hasPrefix "/nix/store/" path)) (lib.attrValues p.backup.credentials);
          message = "Harbor DB backup credentials must be runtime paths outside the store.";
        }
      ])
      projects);
    environment.systemPackages = [package];
    environment.etc = lib.mapAttrs' (name: p: lib.nameValuePair "harbor-db/${name}-backup.json" {source = manifest name p.backup;}) projects;
    users.groups = lib.listToAttrs (map (p: lib.nameValuePair p.backup.group {}) (lib.attrValues projects));
    users.users = lib.mkMerge (lib.concatMap (p: map (reader: {${reader}.extraGroups = [p.backup.group];}) p.backup.readers) (lib.attrValues projects));
    systemd.tmpfiles.rules = lib.concatMap (p: [
      "d ${p.backup.directory} 2750 ${p.backup.user} ${p.backup.group} -"
      "f ${p.backup.directory}/lock 0640 ${p.backup.user} ${p.backup.group} -"
    ]) (lib.attrValues projects);
    systemd.services = lib.mapAttrs' (name: p:
      lib.nameValuePair "harbor-db-${name}-backup" {
        requires = p.backup.requires;
        after = p.backup.requires;
        serviceConfig = {
          Type = "oneshot";
          User = p.backup.user;
          Group = p.backup.group;
          UMask = "0077";
          ExecStart = "${package}/bin/harbor-db-application-backup --config ${manifest name p.backup} capture";
          TimeoutStartSec = "${toString (4 * p.backup.timeoutSeconds + 60)}s";
          NoNewPrivileges = true;
          ProtectSystem = "strict";
          ProtectHome = true;
          PrivateTmp = true;
          ReadWritePaths = [p.backup.directory];
          LoadCredential = lib.mapAttrsToList (key: path: "${key}:${path}") p.backup.credentials;
        };
      })
    projects;
    systemd.timers = lib.mapAttrs' (name: p:
      lib.nameValuePair "harbor-db-${name}-backup" {
        wantedBy = ["timers.target"];
        timerConfig = {
          OnCalendar = p.backup.calendar;
          RandomizedDelaySec = "2min";
          Persistent = true;
        };
      })
    projects;
  };
}
