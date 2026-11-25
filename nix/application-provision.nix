{
  config,
  lib,
  pkgs,
  options,
  harborDbStoragePackage,
  ...
}: let
  inherit (lib) mkOption types;
  identifier = types.strMatching "[a-z_][a-z0-9_]{0,62}";
  unit = types.strMatching "[A-Za-z0-9_@.:-]+\\.service";
  projects = lib.filterAttrs (_: project: project.postgres.provision.enable) config.services.harbor-db.projects;
  policies = lib.mapAttrs (_: project: project.postgres.provision) projects;
  safeProjectName = name: builtins.match "[A-Za-z0-9_-]{1,128}" name != null;
  # Keep malformed names out of typed systemd fields so the project assertion
  # reports the error before any PostgreSQL or unit configuration is generated.
  renderedPolicies = lib.filterAttrs (name: _: safeProjectName name) policies;
  orderingEdges = lib.concatLists (lib.mapAttrsToList (name: p: let
    provision = "harbor-db-${name}-provision.service";
    permissions = "harbor-db-${name}-permissions.service";
  in
    [["postgresql.service" provision] ["postgresql-setup.service" provision] [provision permissions]]
    ++ map (schema: [provision schema]) p.schemaUnits
    ++ map (schema: [schema permissions]) p.schemaUnits
    ++ map (runtime: [permissions runtime]) p.runtimeUnits)
  renderedPolicies);
  orderingUnits = lib.unique (lib.concatLists orderingEdges);
  ordering = lib.toposort (before: after: lib.elem [before after] orderingEdges) orderingUnits;
  package = harborDbStoragePackage;
  primaryPort = config.services.postgresql.settings.port;
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
        inherit (policy) port;
        control_role = "postgres";
        lock_file = "/var/lib/harbor-db-provision/lock";
      };
    });
  command = name: policy: "${package}/bin/harbor-db-provision --config ${manifest name policy}";
  fenceEnabled = lib.hasAttrByPath ["services" "harbor-db" "postgresql" "enable"] options && config.services.harbor-db.postgresql.enable;
  gate = lib.optionalAttrs fenceEnabled {
    ConditionPathExists = ["!${config.services.harbor-db.postgresql.stateDir}/writer-fence.json"];
  };
in {
  imports = [./storage-package-argument.nix];
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
          description = "Exact privileges on declared tables; post-schema reconcile/check require them all before admitting runtime.";
        };
        socketDirectory = mkOption {
          type = types.strMatching "/[A-Za-z0-9_./-]+";
          default = "/run/postgresql";
        };
        port = mkOption {
          type = types.port;
          default = primaryPort;
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
    assertions =
      [
        {
          assertion = !(ordering ? cycle);
          message = "Harbor DB provisioning ordering must be acyclic across all enabled projects.";
        }
        {
          assertion = config.services.postgresql.enable;
          message = "Harbor DB application provisioning requires the host PostgreSQL service.";
        }
        {
          assertion = lib.intersectLists (map (p: p.ownerOsUser) (lib.attrValues policies)) (map (p: p.runtimeOsUser) (lib.attrValues policies)) == [];
          message = "Harbor DB runtime OS users must not map to any application owner role across enabled projects.";
        }
        {
          assertion = lib.all (p: !(lib.elem p.runtimeOsUser ["root" "postgres"])) (lib.attrValues policies);
          message = "Harbor DB runtime OS users must not use root or postgres control accounts.";
        }
        {
          assertion = lib.length (lib.unique (map (p: p.database) (lib.attrValues policies))) == lib.length (lib.attrValues policies);
          message = "Harbor DB application databases must be unique.";
        }
        {
          assertion = let roles = lib.concatMap (p: [p.ownerRole p.runtimeRole]) (lib.attrValues policies); in lib.length (lib.unique roles) == lib.length roles;
          message = "Harbor DB application roles must have one dedicated owner.";
        }
      ]
      ++ lib.flatten (lib.mapAttrsToList (name: p: [
          {
            assertion = safeProjectName name;
            message = "Harbor DB ${name} provision project name must contain 1..128 ASCII letters, digits, underscores or hyphens.";
          }
          {
            assertion = lib.intersectLists p.schemaUnits p.runtimeUnits == [];
            message = "Harbor DB ${name} schemaUnits and runtimeUnits must be disjoint.";
          }
          {
            assertion = p.ownerOsUser != p.runtimeOsUser;
            message = "Harbor DB ${name} ownerOsUser and runtimeOsUser must differ to isolate owner and runtime peer access.";
          }
        ])
        policies);
    systemd.tmpfiles.rules = ["d /var/lib/harbor-db-provision 0700 postgres postgres -" "f /var/lib/harbor-db-provision/lock 0600 postgres postgres -"];
    environment.systemPackages = [package];
    services.postgresql.identMap = lib.mkBefore ("harbor-control postgres postgres\n"
      + lib.concatStringsSep "\n" (lib.mapAttrsToList (name: p: ''
          harbor-${name}-runtime ${p.runtimeOsUser} ${p.runtimeRole}
          harbor-${name}-owner ${p.ownerOsUser} ${p.ownerRole}
        '')
        renderedPolicies));
    services.postgresql.authentication = lib.mkBefore (lib.concatStringsSep "\n" (lib.mapAttrsToList (name: p: ''
        local ${p.database} ${p.runtimeRole} peer map=harbor-${name}-runtime
        local ${p.database} ${p.ownerRole} peer map=harbor-${name}-owner
        local ${p.database} postgres peer map=harbor-control
        local ${p.database} all reject
        host ${p.database} all all reject
      '')
      renderedPolicies));
    environment.etc = lib.mapAttrs' (name: p: lib.nameValuePair "harbor-db/${name}-provision.json" {source = manifest name p;}) renderedPolicies;
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
    in
      lib.mkMerge [
        {
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
                  ExecStart = "${command name p} reconcile";
                  ExecStartPost = "${command name p} check";
                };
            };
        }
        (lib.genAttrs (map (lib.removeSuffix ".service") p.runtimeUnits) (_: {unitConfig = gate;}))
      ])
    renderedPolicies);
  };
}
