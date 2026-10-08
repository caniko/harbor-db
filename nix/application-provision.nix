{
  config,
  lib,
  pkgs,
  options,
  ...
}: let
  inherit (lib) mkOption types;
  identifier = types.strMatching "[a-z_][a-z0-9_]{0,62}";
  unit = types.strMatching "[A-Za-z0-9_@.:-]+\\.service";
  projects = lib.filterAttrs (_: project: project.postgres.provision.enable) config.services.harbor-db.projects;
  policies = lib.mapAttrs (_: project: project.postgres.provision) projects;
  package = import ./postgres-package.nix {inherit pkgs;};
  manifest = name: policy:
    pkgs.writeText "harbor-db-${name}-provision.json" (builtins.toJSON {
      version = 1;
      policy = {
        inherit (policy) database schema tables;
        owner_role = policy.ownerRole;
        runtime_role = policy.runtimeRole;
        table_privileges = policy.tablePrivileges;
        sequence_privileges = policy.sequencePrivileges;
      };
      endpoint = {
        package = toString config.services.postgresql.package;
        socket_dir = policy.socketDirectory;
        port = policy.port;
        control_role = "postgres";
        lock_file = "/var/lib/harbor-db-provision/lock";
      };
    });
  command = name: policy: "${package}/bin/harbor-db-provision --config ${manifest name policy}";
  fenceEnabled = options.services.harbor-db ? postgresql && config.services.harbor-db.postgresql.enable;
  gate = lib.optionalAttrs fenceEnabled {
    ConditionPathExists = ["!${config.services.harbor-db.postgresql.stateDir}/writer-fence.json"];
  };
in {
  options.services.harbor-db.projects = mkOption {
    type = types.attrsOf (types.submodule {
      options.postgres.provision = {
        enable = lib.mkEnableOption "dedicated application database and owner/runtime privileges";
        database = mkOption {type = identifier;};
        ownerRole = mkOption {type = identifier;};
        runtimeRole = mkOption {type = identifier;};
        runtimeOsUser = mkOption {type = types.strMatching "[a-z_][a-z0-9_-]*";};
        ownerOsUser = mkOption {
          type = types.strMatching "[a-z_][a-z0-9_-]*";
          default = "root";
        };
        schema = mkOption {
          type = identifier;
          default = "public";
        };
        tablePrivileges = mkOption {
          type = types.listOf (types.enum ["SELECT" "INSERT" "UPDATE" "DELETE" "TRUNCATE" "REFERENCES" "TRIGGER" "MAINTAIN"]);
          default = ["SELECT"];
        };
        sequencePrivileges = mkOption {
          type = types.listOf (types.enum ["USAGE" "SELECT" "UPDATE"]);
          default = ["USAGE" "SELECT"];
        };
        tables = mkOption {
          type = types.attrsOf (types.listOf (types.enum ["SELECT" "INSERT" "UPDATE" "DELETE" "TRUNCATE" "REFERENCES" "TRIGGER" "MAINTAIN"]));
          default = {};
          description = "Exact privileges on existing named tables; apply after explicit owner migrations before admitting runtime.";
        };
        socketDirectory = mkOption {
          type = types.strMatching "/[A-Za-z0-9_./-]+";
          default = "/run/postgresql";
        };
        port = mkOption {
          type = types.port;
          default = 5432;
        };
        schemaUnits = mkOption {
          type = types.listOf unit;
          default = [];
          description = "Explicit schema services ordered after provisioning and before privilege reconciliation.";
        };
        runtimeUnits = mkOption {
          type = types.listOf unit;
          default = [];
        };
      };
    });
  };
  config = lib.mkIf (projects != {}) {
    assertions = [
      {
        assertion = config.services.postgresql.enable;
        message = "Harbor DB application provisioning requires the host PostgreSQL service.";
      }
      {
        assertion = lib.length (lib.unique (map (p: p.database) (lib.attrValues policies))) == lib.length (lib.attrValues policies);
        message = "Harbor DB application databases must be unique.";
      }
      {
        assertion = let roles = lib.concatMap (p: [p.ownerRole p.runtimeRole]) (lib.attrValues policies); in lib.length (lib.unique roles) == lib.length roles;
        message = "Harbor DB application roles must have one dedicated owner.";
      }
    ];
    systemd.tmpfiles.rules = ["d /var/lib/harbor-db-provision 0700 postgres postgres -" "f /var/lib/harbor-db-provision/lock 0600 postgres postgres -"];
    environment.systemPackages = [package];
    services.postgresql.identMap = lib.mkBefore (lib.concatStringsSep "\n" (lib.mapAttrsToList (name: p: ''
        harbor-${name}-runtime ${p.runtimeOsUser} ${p.runtimeRole}
        harbor-${name}-owner ${p.ownerOsUser} ${p.ownerRole}
      '')
      policies));
    services.postgresql.authentication = lib.mkBefore (lib.concatStringsSep "\n" (lib.mapAttrsToList (name: p: ''
        local ${p.database} ${p.runtimeRole} peer map=harbor-${name}-runtime
        local ${p.database} ${p.ownerRole} peer map=harbor-${name}-owner
      '')
      policies));
    environment.etc = lib.mapAttrs' (name: p: lib.nameValuePair "harbor-db/${name}-provision.json" {source = manifest name p;}) policies;
    systemd.services = lib.mkMerge (lib.mapAttrsToList (name: p: let
        provision = "harbor-db-${name}-provision";
        permissions = "harbor-db-${name}-permissions";
        common = {
          unitConfig = gate;
          serviceConfig = {
            Type = "oneshot";
            User = "postgres";
            UMask = "0077";
            TimeoutStartSec = "2min";
            NoNewPrivileges = true;
          };
        };
      in {
        ${provision} =
          common
          // {
            requires = ["postgresql.service" "postgresql-setup.service"];
            after = ["postgresql.service" "postgresql-setup.service"];
            before = p.schemaUnits;
            requiredBy = p.schemaUnits;
            serviceConfig = common.serviceConfig // {ExecStart = "${command name p} apply";};
          };
        ${permissions} =
          common
          // {
            requires = ["${provision}.service"] ++ p.schemaUnits;
            after = ["${provision}.service"] ++ p.schemaUnits;
            before = p.runtimeUnits;
            requiredBy = p.runtimeUnits;
            serviceConfig =
              common.serviceConfig
              // {
                ExecStart = "${command name p} apply";
                ExecStartPost = "${command name p} check";
              };
          };
      })
      policies);
  };
}
