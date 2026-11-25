{
  config,
  lib,
  pkgs,
  ...
}: let
  inherit (lib) mkEnableOption mkIf mkOption types;
  cfg = config.services.harbor-db.postgresql.observer;
  pg = config.services.postgresql;
  safeName = value: builtins.match "[a-zA-Z_][a-zA-Z0-9_-]{0,62}" value != null;
  safeRole =
    safeName cfg.role
    && !(lib.hasPrefix "pg_" (lib.toLower cfg.role))
    && !(builtins.elem (lib.toLower cfg.role) ["all" "postgres" "public" "replicator"]);
  safeDatabase =
    safeName cfg.database
    && !(builtins.elem (lib.toLower cfg.database) ["all" "replication" "samerole" "samegroup" "sameuser"]);
  observerHba = [
    "local ${cfg.database} ${cfg.role} peer map=${cfg.identMapName}"
    "local replication ${cfg.role} reject"
    "local all ${cfg.role} reject"
    "host replication ${cfg.role} all reject"
    "host all ${cfg.role} all reject"
  ];
  # A comment ending in a backslash consumes the next physical line. Do
  # not skip it when verifying that the generated records really come first.
  hbaRecords =
    lib.filter (line: line != "" && !(lib.hasPrefix "#" line && !lib.hasSuffix "\\" line))
    (map lib.trim (lib.splitString "\n" pg.authentication));
  observerMapping = "${cfg.identMapName} ${cfg.user} ${cfg.role}";
  identLines = lib.splitString "\n" pg.identMap;
  identRecords = lib.filter (line: builtins.match "[ \t\r,]*(#.*)?" line == null) identLines;
  # PostgreSQL accepts quoted map tokens and includes. Normalize the former
  # for comparison; reject includes and continuations we cannot verify here.
  identMapFor = line: let
    match = builtins.match "[ \t\r,]*(([^ \t\r,#\"]|\"[^\"]*\")+)([ \t\r,#].*)?" line;
  in
    if match == null
    then null
    else lib.replaceStrings ["\""] [""] (lib.head match);
  identMapIsExclusive =
    lib.all (line: builtins.match ".*\\\\\r*" line == null) identLines
    && lib.all (line: let
      name = identMapFor line;
    in
      name
      != null
      && !lib.hasPrefix "@" name
      && !(builtins.elem name ["include" "include_if_exists" "include_dir"]))
    identRecords
    && lib.filter (line: identMapFor line == cfg.identMapName) identRecords == [observerMapping];
  sharedGroups = [
    "root"
    "wheel"
    "disk"
    "postgres"
    "shadow"
    "adm"
    "docker"
    "kvm"
    "libvirtd"
    "systemd-journal"
    "video"
    "render"
    "audio"
    "input"
    "tty"
    "dialout"
    "plugdev"
    "users"
    "nogroup"
    "nobody"
  ];
  osGroup = config.users.groups.${cfg.group} or {};
  reservedGroups = config.ids.gids;
  osUser = config.users.users.${cfg.user};
in {
  options.services.harbor-db.postgresql.observer = {
    enable = mkEnableOption "a dedicated peer-only PostgreSQL activity observer";
    user = mkOption {
      type = types.str;
      default = "harbor-db-observer";
      description = "Dedicated operating-system user permitted to peer-authenticate as the observer role.";
    };
    group = mkOption {
      type = types.str;
      default = "harbor-db-observer";
      description = "Primary group of the dedicated observer user.";
    };
    createOsUser = mkOption {
      type = types.bool;
      default = true;
      description = "Create the observer system account and group. Disable only for an externally managed, dedicated account.";
    };
    role = mkOption {
      type = types.str;
      default = "harbor_db_observer";
      description = "Dedicated PostgreSQL login role granted pg_monitor. Do not reuse an application or replication role.";
    };
    database = mkOption {
      type = types.str;
      default = "postgres";
      description = "Existing database the observer may connect to via its local PostgreSQL socket.";
    };
    identMapName = mkOption {
      type = types.str;
      default = "harbor-db-observer";
      description = "Dedicated pg_ident.conf map for the observer account.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = pg.enable;
        message = "The harbor-db PostgreSQL observer requires services.postgresql.enable.";
      }
      {
        assertion = safeName cfg.user && !(builtins.elem cfg.user ["root" "postgres"]);
        message = "The harbor-db PostgreSQL observer user must be a safe, dedicated system username.";
      }
      {
        assertion = safeName cfg.group && !(builtins.elem cfg.group ["root" "postgres"]);
        message = "The harbor-db PostgreSQL observer group must be a safe, dedicated system group.";
      }
      {
        assertion = safeRole;
        message = "The harbor-db PostgreSQL observer role must be a safe, nonreserved PostgreSQL role.";
      }
      {
        assertion = safeDatabase;
        message = "The harbor-db PostgreSQL observer database must be a safe, literal HBA database name.";
      }
      {
        assertion = safeName cfg.identMapName;
        message = "The harbor-db PostgreSQL observer identMapName must be a safe, literal map name.";
      }
      {
        assertion = lib.take (lib.length observerHba) hbaRecords == observerHba;
        message = "The harbor-db PostgreSQL observer requires its generated records at the start of the effective HBA.";
      }
      {
        assertion = identMapIsExclusive;
        message = "The harbor-db PostgreSQL observer requires exactly its generated mapping under its ident map name, with no unverifiable includes or continuations.";
      }
      # Match the store path strings generated by the NixOS PostgreSQL module.
      {
        assertion = !pg.enable || (pg.settings.hba_file or null) == "${pkgs.writeText "pg_hba.conf" pg.authentication}";
        message = "The harbor-db PostgreSQL observer requires services.postgresql.settings.hba_file to use the file generated from services.postgresql.authentication.";
      }
      {
        assertion = !pg.enable || (pg.settings.ident_file or null) == "${pkgs.writeText "pg_ident.conf" pg.identMap}";
        message = "The harbor-db PostgreSQL observer requires services.postgresql.settings.ident_file to use the file generated from services.postgresql.identMap.";
      }
      {
        assertion =
          !(builtins.elem cfg.group sharedGroups)
          && !(builtins.hasAttr cfg.group reservedGroups)
          && !(builtins.elem (osGroup.gid or null) (lib.attrValues reservedGroups))
          && (
            !cfg.createOsUser
            || (
              osUser.isSystemUser
              && osUser.group == cfg.group
              && lib.all (member: member == cfg.user) osGroup.members
              && lib.all (user: user.group != cfg.group && !(builtins.elem cfg.group user.extraGroups))
              (lib.attrValues (removeAttrs config.users.users [cfg.user]))
            )
          );
        message = "The harbor-db PostgreSQL observer user must be a system user with an unshared, unprivileged primary group.";
      }
      {
        assertion =
          !cfg.createOsUser
          || (osUser.extraGroups
            == []
            && lib.all (group: !(builtins.elem cfg.user group.members))
            (lib.attrValues (removeAttrs config.users.groups [cfg.group])));
        message = "The harbor-db PostgreSQL observer user must not have supplementary group memberships.";
      }
    ];

    users.groups = mkIf cfg.createOsUser {${cfg.group} = {};};
    users.users = mkIf cfg.createOsUser {
      ${cfg.user} = {
        isSystemUser = true;
        inherit (cfg) group;
      };
    };

    services.postgresql = {
      ensureUsers = [{name = cfg.role;}];
      identMap = lib.mkBefore ''
        ${observerMapping}
      '';
      authentication = lib.mkOrder 400 (lib.concatLines observerHba);
    };

    # NixOS runs postgresql-setup as postgres and creates ensureUsers first.
    systemd.services.postgresql-setup.script = lib.mkAfter ''
      ${pg.finalPackage}/bin/psql -X -w -v ON_ERROR_STOP=1 -d postgres <<'HARBOR_OBSERVER_SQL'
      ALTER ROLE "${cfg.role}" LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
      GRANT pg_monitor TO "${cfg.role}";
      GRANT CONNECT ON DATABASE "${cfg.database}" TO "${cfg.role}";
      HARBOR_OBSERVER_SQL
    '';
  };
}
