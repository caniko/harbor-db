{
  pkgs,
  module,
  lifecycleModule,
}: let
  inherit (pkgs) lib;
  inherit (import ./eval-checks.nix {inherit pkgs;}) mkEvalCheck;
  evaluate = modules:
    import (pkgs.path + "/nixos/lib/eval-config.nix") {
      inherit (pkgs.stdenv.hostPlatform) system;
      modules =
        [
          {
            system.stateVersion = "24.11";
            services.postgresql.enable = true;
          }
        ]
        ++ modules;
    };
  defaults = (evaluate [module]).config;
  customization = {
    enable = true;
    user = "monitor-agent";
    group = "monitor-group";
    role = "Observer-Role";
    database = "Observer-Db";
    identMapName = "monitor-map";
  };
  enabled = evaluate [
    module
    {
      services.harbor-db.postgresql.observer = customization;
      services.postgresql = {
        authentication = lib.mkAfter "local all all trust";
        identMap = lib.mkAfter "other-map other-user other-role";
      };
    }
  ];
  inherit (enabled) config;
  pg = config.services.postgresql;
  setup = config.systemd.services.postgresql-setup;
  hbaRecords = authentication: lib.filter (line: line != "" && !lib.hasPrefix "#" line) (map lib.trim (lib.splitString "\n" authentication));
  app = {
    services.harbor-db.projects.app.postgres.provision = {
      enable = true;
      database = "appdb";
      ownerRole = "app_owner";
      runtimeRole = "app_runtime";
      runtimeOsUser = "app-runtime";
    };
  };
  expectedHba = ''
    local Observer-Db Observer-Role peer map=monitor-map
    local replication Observer-Role reject
    local all Observer-Role reject
    host replication Observer-Role all reject
    host all Observer-Role all reject
  '';
  external =
    (enabled.extendModules {
      modules = [{services.harbor-db.postgresql.observer.createOsUser = false;}];
    }).config;
  assertionsPass = prefixes: checked:
    lib.all (a: a.assertion || !lib.any (prefix: lib.hasPrefix prefix a.message) prefixes) checked.assertions;
  observerPrefix = "The harbor-db PostgreSQL observer ";
  observerPasses = assertionsPass [observerPrefix];
  standaloneAssertionsPass = assertionsPass [observerPrefix "Harbor DB " "services.harbor-db.pgBackup"];
  withOverride = overrides: (enabled.extendModules {modules = [overrides];}).config;
  invalid = key: value:
    !observerPasses (withOverride {services.harbor-db.postgresql.observer.${key} = lib.mkForce value;});
in
  mkEvalCheck {
    name = "harbor-db-postgres-observer-eval";
    assertions = [
      {
        name = "disabled-is-inert";
        assertion =
          !defaults.services.harbor-db.postgresql.observer.enable
          && !(defaults.users.users ? harbor-db-observer)
          && defaults.services.postgresql.ensureUsers == []
          && !lib.hasInfix "harbor-db-observer" defaults.services.postgresql.authentication
          && !(evaluate [lifecycleModule]).config.services.harbor-db.postgresql.observer.enable;
        message = "Observer provisioning must be opt-in, including through the lifecycle module.";
      }
      {
        name = "dedicated-system-account";
        assertion =
          config.users.users.monitor-agent.isSystemUser
          && config.users.users.monitor-agent.group == "monitor-group"
          && config.users.users.monitor-agent.extraGroups == []
          && config.users.groups ? monitor-group
          && observerPasses config
          && observerPasses (evaluate [module {services.harbor-db.postgresql.observer.enable = true;}]).config
          && !(external.users.users ? monitor-agent)
          && !(external.users.groups ? monitor-group);
        message = "The optional observer account must have no supplementary groups.";
      }
      {
        name = "reject-shared-or-supplementary-groups";
        assertion =
          invalid "group" "disk"
          && invalid "group" "utmp"
          && lib.all (overrides: !observerPasses (withOverride overrides)) [
            {users.users.monitor-agent.extraGroups = ["wheel"];}
            {users.groups.wheel.members = ["monitor-agent"];}
            {users.groups.monitor-group.members = ["other-user"];}
            {users.groups.monitor-group.gid = config.ids.gids.utmp;}
            {
              services.harbor-db.postgresql.observer = {
                createOsUser = false;
                group = lib.mkForce "utmp";
              };
            }
            {users.users.monitor-agent.group = lib.mkForce "wheel";}
            {users.users.monitor-agent.isSystemUser = lib.mkForce false;}
          ];
        message = "The observer system account must have a dedicated primary group and no supplementary memberships.";
      }
      {
        name = "exclusive-ident-map";
        assertion = let
          passes = identMap: observerPasses (withOverride {services.postgresql.identMap = lib.mkAfter identMap;});
        in
          passes "\n # monitor-map other-user Observer-Role\n"
          && passes ''"other-quoted-map" other-user other-role''
          && !passes "monitor-map other-user Observer-Role"
          && !passes "monitor-map monitor-agent Observer-Role"
          && !passes ''"monitor-map" other-user Observer-Role''
          && !passes "include_if_exists extra-ident.conf"
          && !passes "other-map other-user other-role\\\n# continued";
        message = "The merged ident file must contain only the dedicated mapping under the observer map name.";
      }
      {
        name = "ordered-peer-and-denials";
        assertion =
          lib.concatLines (lib.take 5 (hbaRecords pg.authentication))
          == expectedHba
          && lib.hasInfix "local all all trust" pg.authentication
          && lib.hasPrefix "monitor-map monitor-agent Observer-Role\n" pg.identMap
          && lib.hasInfix "other-map other-user other-role" pg.identMap;
        message = "Observer peer mapping and refusals must precede any fallback rules.";
      }
      {
        name = "effective-hba-requires-observer-prefix";
        assertion = let
          passes = authentication:
            observerPasses (withOverride {services.postgresql.authentication = lib.mkForce authentication;});
        in
          passes (" \n  # comment\n" + expectedHba)
          && !passes "local all all trust"
          && !passes ("local all all trust\n" + expectedHba)
          && !passes ("# continued\\\n" + expectedHba);
        message = "The effective HBA must keep the generated observer rules first, even after an override.";
      }
      {
        name = "reject-overridden-auth-file-paths";
        assertion = let
          rejects = setting: path: let
            overridden = withOverride {services.postgresql.settings.${setting} = lib.mkForce path;};
          in
            overridden.services.postgresql.authentication
            == pg.authentication
            && overridden.services.postgresql.identMap == pg.identMap
            && !observerPasses overridden;
        in
          rejects "hba_file" "/etc/pg_hba.conf"
          && rejects "ident_file" "/etc/pg_ident.conf";
        message = "The observer must reject alternate HBA and ident files even when the generated records are intact.";
      }
      {
        name = "observer-peer-before-application-reject";
        assertion = let
          provision = {
            imports = [./application-provision.nix];
            _module.args.harborDbStoragePackage = pkgs.emptyDirectory;
          };
          ordered = modules: let
            cfg =
              (evaluate (modules
                ++ [
                  {services.harbor-db.postgresql.observer = customization // {database = "appdb";};}
                  app
                ])).config;
            records = hbaRecords cfg.services.postgresql.authentication;
            position = record: lib.lists.findFirstIndex (line: line == record) null records;
            allow = position "local appdb Observer-Role peer map=monitor-map";
            deny = position "local appdb all reject";
            unit = cfg.systemd.services.harbor-db-app-provision;
          in
            allow
            != null
            && deny != null
            && allow < deny
            && standaloneAssertionsPass cfg
            && lib.hasSuffix " apply" unit.serviceConfig.ExecStart
            && !(unit.unitConfig ? ConditionPathExists);
        in
          ordered [module provision] && ordered [provision module];
        message = "The observer peer rule must precede the Harbor application database's catch-all reject.";
      }
      {
        name = "standalone-observer-with-backup";
        assertion = let
          cfg = withOverride {
            imports = [./pg-backup.nix];
            _module.args.harborDbStoragePackage = pkgs.emptyDirectory;
            services.harbor-db.pgBackup = {
              enable = true;
              role = "both";
              source.hostName = "127.0.0.1";
              sourceSettings.replicatorPasswordFile = "/run/secrets/replicator";
              targetSettings.sourceLocalRecovery.enable = true;
            };
          };
        in
          standaloneAssertionsPass cfg
          && cfg.systemd.services.pg-basebackup.serviceConfig.User == "postgres"
          && lib.hasInfix "-pg-basebackup-body" cfg.systemd.services.pg-basebackup.serviceConfig.ExecStart;
        message = "A standalone observer must compose with enabled local backup without lifecycle options.";
      }
      {
        name = "standalone-observer-deduplicates-with-lifecycle";
        assertion = let
          cfg = withOverride {imports = [lifecycleModule];};
        in
          observerPasses cfg && (map (user: user.name) cfg.services.postgresql.ensureUsers == ["Observer-Role"]);
        message = "The exported standalone and lifecycle modules must share one observer declaration.";
      }
      {
        name = "least-privilege-setup";
        assertion =
          map (user: user.name) pg.ensureUsers
          == ["Observer-Role"]
          && setup.serviceConfig.User == "postgres"
          && lib.hasInfix "psql -X -w -v ON_ERROR_STOP=1 -d postgres" setup.script
          && lib.hasInfix ''ALTER ROLE "Observer-Role" LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;'' setup.script
          && lib.hasInfix ''GRANT pg_monitor TO "Observer-Role";'' setup.script
          && lib.hasInfix ''GRANT CONNECT ON DATABASE "Observer-Db" TO "Observer-Role";'' setup.script
          && !lib.hasInfix "ALTER ROLE pg_monitor" setup.script;
        message = "PostgreSQL setup must grant pg_monitor to the login and never alter pg_monitor.";
      }
      {
        name = "reject-hba-and-sql-metacharacters";
        assertion =
          lib.all (invalid "role") ["pg_monitor" "pg_other" "PG_other" "postgres" "public" "replicator" "all" ''bad"name'' "x,y" "+role" (lib.concatStrings (lib.replicate 64 "x"))]
          && lib.all (invalid "database") ["all" "replication" "samerole" "samegroup" "sameuser" "db\nlocal" "db;DROP"]
          && invalid "identMapName" "bad map"
          && invalid "user" "/regex"
          && invalid "group" "x'y";
        message = "Invalid role, database, account and map names must fail assertions.";
      }
    ];
  }
