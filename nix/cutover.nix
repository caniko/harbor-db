{
  config,
  lib,
  options,
  pkgs,
  ...
}: let
  inherit (lib) mkOption types;
  cfg = config.services.harbor-db.cutover;
  postgresDeclared = options.services.harbor-db ? postgresql;
  postgresEnabled = postgresDeclared && config.services.harbor-db.postgresql.enable;
  resources =
    cfg.resources
    // lib.optionalAttrs postgresEnabled {
      postgresql = {
        kind = "postgres";
        user = "postgres";
        config = toString config.environment.etc."harbor-db/postgresql.json".source;
        runtime_units = [];
        compatibility_checks = cfg.postgresCompatibilityChecks;
        corpus_checks = lib.filterAttrs (_: checks: checks != []) (lib.mapAttrs (_: entry: entry.database_inventory_checks) cfg.resources);
        socket_dir =
          if config.services.harbor-db.postgresql.switchAdoption != null
          then config.services.harbor-db.postgresql.switchAdoption.socketDir
          else "/run/postgresql";
        port =
          if config.services.harbor-db.postgresql.switchAdoption != null
          then config.services.harbor-db.postgresql.switchAdoption.port
          else 5432;
      };
    };
  manifest = pkgs.writeText "harbor-db-cutover.json" (builtins.toJSON {
    version = 1;
    enforced = cfg.enable;
    host = config.networking.hostName;
    checker = "${cfg.package}/bin/harbor-db-cutover";
    timeout_seconds = cfg.timeoutSeconds;
    activation_timeout_seconds = cfg.activationTimeoutSeconds;
    inherit resources;
  });
  protectedDirectories = lib.concatMap (entry:
    if entry.kind == "filesystem"
    then entry.authority.directories
    else []) (lib.attrValues resources);
  requireExisting = rule: let
    matched = builtins.match "([dDvqQ][^[:space:]]*)[[:space:]]+('([^']*)'|\"([^\"]*)\"|([^[:space:]]+))([[:space:]].*)" rule;
    path =
      if matched == null
      then null
      else if builtins.elemAt matched 2 != null
      then builtins.elemAt matched 2
      else if builtins.elemAt matched 3 != null
      then builtins.elemAt matched 3
      else builtins.elemAt matched 4;
  in
    if cfg.enable && matched != null && lib.elem path protectedDirectories
    then "z ${builtins.elemAt matched 1}${builtins.elemAt matched 5}"
    else rule;
  declaredDirectories =
    if options.services.harbor-db ? dataDirectories
    then config.services.harbor-db.dataDirectories
    else [];
  guardService = name: service: let
    selected = lib.filterAttrs (_: entry: lib.elem "${name}.service" entry.runtime_units) cfg.resources;
    names = lib.attrNames selected;
    wrap = command:
      if command == ""
      then command
      else if command == startupCheck
      then command
      else if !(lib.hasPrefix "/" command)
      then throw "Harbor-DB guarded writer command must use an absolute executable without systemd privilege/argv modifiers: ${name}"
      else "${cfg.package}/bin/harbor-db-cutover serve --contract ${manifest} --host ${config.networking.hostName} --resource ${lib.head names} -- ${command}";
    startupCheck = "+${cfg.package}/bin/harbor-db-cutover check --contract ${manifest} --host ${config.networking.hostName} --phase startup";
    commands = lib.filterAttrs (key: _: lib.elem key ["ExecCondition" "ExecStartPre" "ExecStart" "ExecStartPost" "ExecReload" "ExecStop" "ExecStopPost"]) service;
  in
    if !cfg.enable || names == []
    then service
    else if lib.length names != 1
    then throw "Harbor-DB writer unit must belong to exactly one filesystem authority: ${name}"
    else
      service
      // lib.mapAttrs (_: command:
        if builtins.isList command
        then map wrap command
        else wrap command)
      commands;
in {
  # Application modules often emit boot-only `d` rules with no initialization
  # option. Enforced historical roots retain permission repair, never creation.
  options.systemd.tmpfiles.rules = mkOption {apply = rules: map requireExisting rules;};
  options.systemd.services = mkOption {
    type = types.attrsOf (types.submodule ({name, ...}: {
      options.serviceConfig = mkOption {apply = guardService name;};
    }));
  };
  options.services.harbor-db.cutover = {
    enable = lib.mkEnableOption "mandatory rebuild and activation cutover admission";
    package = mkOption {
      type = types.package;
      default = import ./postgres-package.nix {inherit pkgs;};
      description = "Harbor-DB cutover and existing storage/recovery engines.";
    };
    timeoutSeconds = mkOption {
      type = types.ints.between 1 300;
      default = 30;
      description = "Total read-only early inspection budget; expiry blocks rebuild admission.";
    };
    activationTimeoutSeconds = mkOption {
      type = types.ints.between 1 3600;
      default = 900;
      description = "Bounded byte-verification and custody certification budget.";
    };
    resources = mkOption {
      type = types.attrsOf (types.submodule ({name, ...}: {
        options = {
          kind = mkOption {
            type = types.enum ["filesystem"];
            default = "filesystem";
          };
          user = mkOption {type = types.str;};
          login_shell = mkOption {
            type = types.nullOr (types.strMatching "/.*");
            default = null;
            description = "Underlying immutable shell for a consumer using harbor-db-cutover-shell as its login shell; SSH commands retain the same authority lease.";
          };
          git_executable = mkOption {
            type = types.strMatching "/.*";
            default = "${pkgs.gitMinimal}/bin/git";
            description = "Immutable Git executable for database-declared bare-repository integrity checks during certification and activation.";
          };
          runtime_units = mkOption {type = types.listOf types.str;};
          database_resource = mkOption {
            type = types.nullOr types.str;
            default = null;
          };
          database_inventory_checks = mkOption {
            type = types.listOf (types.submodule {
              options = {
                root = mkOption {type = types.ints.unsigned;};
                database = mkOption {type = types.str;};
                sql = mkOption {type = types.lines;};
              };
            });
            default = [];
            description = "Database-owned corpus paths: read-only SQL returns a JSON array of {path, directory} entries relative to the indexed authority root.";
          };
          custody_file = mkOption {type = types.strMatching "/.*";};
          transition_manifest = mkOption {
            type = types.nullOr (types.strMatching "/nix/store/.*");
            default = null;
            description = "Prepared backend transition admission; ordinary startup still requires committed authority and explicit writer release.";
          };
          max_age_seconds = mkOption {
            type = types.ints.positive;
            default = 172800;
          };
          authority = mkOption {
            type = types.submodule {
              options = {
                resource = mkOption {
                  type = types.str;
                  default = name;
                };
                state_dir = mkOption {type = types.strMatching "/.*";};
                directories = mkOption {type = types.listOf (types.strMatching "/.*");};
                binding = mkOption {type = types.attrsOf types.str;};
                required_files = mkOption {
                  type = types.listOf (types.strMatching "/.*");
                  default = [];
                };
                required_mounts = mkOption {
                  type = types.listOf types.str;
                  default = [];
                };
                consumer_command = mkOption {
                  type = types.listOf types.str;
                  default = [];
                };
              };
            };
          };
        };
      }));
      default = {};
      description = "Existing filesystem corpora whose source/restore equality must be certified. PostgreSQL is enrolled automatically when guarded lifecycle is enabled.";
    };
    postgresCompatibilityChecks = mkOption {
      type = types.listOf (types.submodule {
        options = {
          database = mkOption {type = types.str;};
          sql = mkOption {type = types.lines;};
        };
      });
      default = [];
      description = "Cheap candidate-owned read-only schema/recovery compatibility queries, each returning exactly one PostgreSQL true value.";
    };
    manifest = mkOption {
      type = types.path;
      readOnly = true;
      description = "Exact candidate cutover contract, exported for early rebuild admission.";
    };
    bundle = mkOption {
      type = types.package;
      readOnly = true;
      description = "Small production guard closure; may be realized before the system closure.";
    };
    operatorUsers = mkOption {
      type = types.listOf types.str;
      default = [];
      description = "Users permitted to run the installed read-only contract, with exact argv only.";
    };
  };
  config = lib.mkMerge [
    {
      services.harbor-db.cutover.manifest = manifest;
      services.harbor-db.cutover.bundle = pkgs.runCommand "harbor-db-cutover-contract" {} ''
        mkdir -p "$out"
        ln -s ${manifest} "$out/manifest.json"
        ln -s ${cfg.package}/bin/harbor-db-cutover "$out/checker"
      '';
    }
    (lib.mkIf cfg.enable {
      assertions = [
        {
          assertion = !(cfg.resources ? postgresql);
          message = "Harbor-DB reserves the postgresql cutover resource for automatic lifecycle enrollment.";
        }
        {
          assertion = !config.services.postgresql.enable || postgresEnabled;
          message = "Harbor-DB cutover admission requires adopted PostgreSQL lifecycle enrollment for every enabled primary.";
        }
        {
          assertion = lib.all (entry: !(lib.elem entry.path protectedDirectories) || !(entry.create or true)) declaredDirectories;
          message = "Harbor-DB adopted corpus roots must use dataDirectories.create = false; missing historical data may never be initialized.";
        }
        {
          assertion = lib.all (entry: entry.database_resource == null || builtins.hasAttr entry.database_resource resources) (lib.attrValues cfg.resources);
          message = "Harbor-DB filesystem custody must reference an enrolled database resource.";
        }
      ];
      environment.etc."harbor-db/cutover.json".source = manifest;
      environment.systemPackages = [cfg.package];
      system.preSwitchChecks."00---harbor-db-cutover" = ''
        ${cfg.package}/bin/harbor-db-cutover check --contract ${manifest} --host ${lib.escapeShellArg config.networking.hostName} --phase activate --candidate "$1" || exit $?
      '';
      systemd.services = lib.mkMerge (lib.mapAttrsToList (_: entry:
        lib.genAttrs (map (lib.removeSuffix ".service") entry.runtime_units) (_: {
          serviceConfig.ExecStartPre = lib.mkBefore ["+${cfg.package}/bin/harbor-db-cutover check --contract ${manifest} --host ${config.networking.hostName} --phase startup"];
        }))
      cfg.resources);
      security.sudo.extraRules = lib.optional (cfg.operatorUsers != []) {
        users = cfg.operatorUsers;
        commands = [
          {
            command = "${cfg.package}/bin/harbor-db-cutover check --contract /etc/harbor-db/cutover.json --host ${config.networking.hostName} --phase preflight";
            options = ["NOPASSWD"];
          }
        ];
      };
    })
  ];
}
