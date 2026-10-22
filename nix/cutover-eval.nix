{
  pkgs,
  module,
}: let
  inherit (pkgs) lib;
  literalArgument = ''literal; argument "with quotes"'';
  phaseWriter = pkgs.writeText "harbor-db-cutover-phase-writer.py" ''
    import json
    import sys
    from pathlib import Path

    phase = sys.argv[1]
    ready = Path(sys.argv[2])
    pending = ready.with_suffix(".tmp")
    pending.write_text(json.dumps({"phase": phase, "arguments": sys.argv[3:]}))
    pending.replace(ready)
    sys.stdin.read(1)
    raise SystemExit({"ExecCondition": 17, "ExecReload": 23, "ExecStop": 31, "ExecStopPost": 37}[phase])
  '';
  phaseCommand = phase: "${pkgs.python3}/bin/python3 ${phaseWriter} ${phase} HARBOR_DB_PHASE_READY ${lib.escapeShellArg literalArgument}";
  descendantRules = [
    "d /srv/history/cache 0750 archive archive - -"
    "D '/srv/history/cache/nested' 0750 archive archive - -"
    "v \"/srv/history/volumes\" 0750 archive archive - -"
    "q /srv/history/quotas 0750 archive archive - -"
    "Q /srv/history/quotas/nested 0750 archive archive - -"
  ];
  siblingRule = "d /srv/history-other/cache 0750 archive archive - -";
  # Preserve the input path during cold read-only flake qualification.
  eval = import (pkgs.path + "/nixos/lib/eval-config.nix") {
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      module
      {
        system.stateVersion = "26.05";
        networking.hostName = "fixture";
        services.harbor-db.cutover = {
          enable = true;
          operatorUsers = ["operator"];
          resources.history = {
            user = "archive";
            runtime_units = ["archive.service"];
            custody_file = "/var/lib/authority/custody.json";
            authority = {
              state_dir = "/var/lib/authority";
              directories = ["/srv/history"];
              binding.backend = "files";
            };
          };
        };
        services.harbor-db.dataDirectories = [
          {
            path = "/srv/history";
            user = "archive";
            group = "archive";
            create = false;
          }
        ];
        systemd.tmpfiles.rules = ["d '/srv/history' 0750 archive archive - -" "d \"/srv/history\" 0750 archive archive - -" siblingRule] ++ descendantRules;
        systemd.services.archive.serviceConfig.ExecStart = "${pkgs.coreutils}/bin/sleep infinity";
        systemd.services.archive.serviceConfig.ExecStartPre = ["${pkgs.coreutils}/bin/true"];
        systemd.services.archive.serviceConfig.ExecCondition = [(phaseCommand "ExecCondition")];
        systemd.services.archive.serviceConfig.ExecReload = phaseCommand "ExecReload";
        systemd.services.archive.serviceConfig.ExecStartPost = ["${pkgs.coreutils}/bin/true"];
        systemd.services.archive.serviceConfig.ExecStop = phaseCommand "ExecStop";
        systemd.services.archive.serviceConfig.ExecStopPost = [(phaseCommand "ExecStopPost")];
      }
    ];
  };
  inherit (eval) config;
  unsafe =
    (eval.extendModules {
      modules = [{services.harbor-db.dataDirectories = lib.mkForce [{path = "/srv/history";}];}];
    }).config;
  unsafeDescendant =
    (eval.extendModules {
      modules = [{services.harbor-db.dataDirectories = lib.mkForce [{path = "/srv/history/cache";}];}];
    }).config;
  disabled =
    (eval.extendModules {
      modules = [{services.harbor-db.cutover.enable = lib.mkForce false;}];
    }).config;
  # NixOS diagnostics may reference attributes that exist only on failure.
  # Keep successful assertions' messages lazy, as NixOS itself does.
  harborFailures = assertions: lib.filter (item: !item.assertion && lib.hasPrefix "Harbor-DB" item.message) assertions;
  custodyAccepted = overrides: let
    selected = (eval.extendModules {modules = [overrides];}).config;
    result = builtins.tryEval (builtins.deepSeq selected.systemd.tmpfiles.rules (harborFailures selected.assertions == []));
  in
    result.success && result.value;
  succeeds = harborFailures config.assertions == [];
  commandPhases = ["ExecCondition" "ExecStartPre" "ExecStart" "ExecStartPost" "ExecReload"];
  shutdownPhases = ["ExecStop" "ExecStopPost"];
  commandPhaseFixture = pkgs.writeText "harbor-db-cutover-command-phases.json" (builtins.toJSON {
    manifest = config.services.harbor-db.cutover.manifest;
    checker = "${config.services.harbor-db.cutover.package}/bin/harbor-db-cutover";
    inherit literalArgument;
    commands = lib.genAttrs (commandPhases ++ shutdownPhases) (phase: lib.toList config.systemd.services.archive.serviceConfig.${phase});
  });
  tmpfilesFixture = pkgs.writeText "harbor-db-cutover-tmpfiles.conf" (lib.concatStringsSep "\n" (map
    (lib.replaceStrings ["archive archive"] ["- -"])
    (lib.filter (lib.hasInfix "/srv/history") config.systemd.tmpfiles.rules)));
in
  (import ./eval-checks.nix {inherit pkgs;}).mkEvalCheck {
    name = "harbor-db-cutover-eval";
    assertions = [
      {
        name = "postgres-cutover-port-survives-switch-adoption-retirement";
        message = "The generated automatic PostgreSQL resource follows effective service ports once adoption is retired and retains an explicit first-rollout endpoint while active.";
        assertion = let
          resource = port: adoption: let
            selected =
              (eval.extendModules {
                modules = [
                  {
                    services.postgresql = {
                      enable = true;
                      settings.port = lib.mkForce port;
                    };
                    services.harbor-db.postgresql = {
                      enable = true;
                      switchAdoption = lib.mkForce adoption;
                    };
                  }
                ];
              }).config;
          in
            (builtins.fromJSON (builtins.unsafeDiscardStringContext selected.services.harbor-db.cutover.manifest.text)).resources.postgresql;
          active = resource 55432 {
            systemIdentifier = "12345";
            socketDir = "/run/first-rollout";
            port = 55433;
          };
        in
          active.port
          == 55433
          && active.socket_dir == "/run/first-rollout"
          && lib.all (port: let
            retired = resource port null;
          in
            retired.port == port && retired.socket_dir == "/run/postgresql") [5432 55432 6543];
      }
      {
        name = "custody-receipts-reserve-authority-controls";
        message = "Every filesystem receipt is a dedicated direct authority child and cannot replace identity, transition journal or lock.";
        assertion = let
          accepted = path: let
            selected = (eval.extendModules {modules = [{services.harbor-db.cutover.resources.history.custody_file = lib.mkForce path;}];}).config;
          in
            lib.all (a: a.assertion || !lib.hasInfix "custody receipts" a.message) selected.assertions;
        in
          accepted "/var/lib/authority/custody.json"
          && !accepted "/var/lib/outside.json"
          && lib.all (name: !accepted "/var/lib/authority/${name}") ["identity.json" "transition.json" "lock"];
      }
      {
        name = "git-integrity-checker-is-an-immutable-store-file";
        message = "Filesystem custody must reject mutable or non-normalized Git checker paths while retaining the default immutable Git executable.";
        assertion =
          custodyAccepted {}
          && !custodyAccepted {services.harbor-db.cutover.resources.history.git_executable = lib.mkForce "/var/lib/archive/git";}
          && !custodyAccepted {services.harbor-db.cutover.resources.history.git_executable = lib.mkForce "/nix/store/00000000000000000000000000000000-tools/../git";};
      }
      {
        name = "filesystem-root-custody-already-rejects-global-admission";
        message = "The filesystem root contains the declared authority state and must fail global namespace admission before installing host-wide tmpfiles custody hooks.";
        assertion = let
          override.services.harbor-db.cutover.resources.history.authority.directories = lib.mkForce ["/"];
          selected = (eval.extendModules {modules = [override];}).config;
        in
          !custodyAccepted override
          && lib.any (a: !a.assertion && lib.hasInfix "custody roots and authority namespaces must be globally pairwise disjoint" a.message) selected.assertions;
      }
      {
        name = "custody-authority-and-corpus-namespaces-are-isolated";
        message = "An authority state directory cannot share or contain another authority/corpus or be nested inside one, before any namespace ownership is emitted.";
        assertion = let
          peer = {
            user = "archive";
            custody_file = "/var/lib/peer-authority/custody.json";
            authority = {
              state_dir = "/var/lib/peer-authority";
              directories = ["/srv/peer-history"];
              binding.backend = "files";
            };
          };
          accepted = override: let
            entry = lib.recursiveUpdate peer override;
          in
            custodyAccepted {services.harbor-db.cutover.resources.peer = entry // {custody_file = "${entry.authority.state_dir}/custody.json";};};
        in
          accepted {}
          && lib.all (state: !accepted {authority.state_dir = state;}) ["/srv/history" "/srv/history/state" "/srv" "/var/lib/authority" "/var/lib/authority/nested" "/var/lib"]
          && lib.all (directory: !accepted {authority.directories = [directory];}) ["/var/lib/authority" "/var/lib/authority/corpus" "/var/lib"]
          && accepted {authority.state_dir = "/var/lib/authority-sibling";};
      }
      {
        name = "filesystem-custody-excludes-enrolled-postgresql-storage";
        message = "Every filesystem authority/corpus must be disjoint from the automatically enrolled PostgreSQL primary, lifecycle and startup-inhibition roots.";
        assertion = let
          accepted = kind: path:
            custodyAccepted {
              services.postgresql.enable = true;
              services.postgresql.dataDir = "/srv/postgres-primary";
              services.harbor-db.postgresql = {
                enable = true;
                stateDir = "/srv/postgres-state";
                writerFence.startupStateDir = "/srv/postgres-startup";
              };
              services.harbor-db.cutover.resources.history.authority.${kind} = lib.mkForce (
                if kind == "directories"
                then [path]
                else path
              );
              services.harbor-db.cutover.resources.history.custody_file = lib.mkForce (
                if kind == "state_dir"
                then "${path}/custody.json"
                else "/var/lib/authority/custody.json"
              );
            };
        in
          lib.all (kind:
            accepted kind "/srv/postgres-primary-sibling"
            && lib.all (path: !accepted kind path) ["/srv/postgres-primary" "/srv/postgres-primary/nested" "/srv/postgres-state" "/srv/postgres-state/nested" "/srv/postgres-startup" "/srv/postgres-startup/nested" "/srv"])
          ["directories" "state_dir"];
      }
      {
        name = "ambiguous-custody-paths-fail-before-directory-creation";
        assertion = lib.all (path:
          !custodyAccepted {services.harbor-db.dataDirectories = lib.mkForce [{inherit path;}];}
          && !custodyAccepted {systemd.tmpfiles.rules = lib.mkForce ["d '${path}' 0750 - - - -"];})
        ["/srv/./history/cache" "/srv/other/../history/cache" "/srv//history/cache" "/srv/history/cache/"];
        message = "non-normalized directory paths must not bypass historical custody protection";
      }
      {
        name = "custody-roots-are-globally-disjoint";
        assertion =
          lib.all (directory:
            !custodyAccepted {
              services.harbor-db.cutover.resources.peer = {
                user = "archive";
                custody_file = "/var/lib/peer-authority/custody.json";
                authority = {
                  state_dir = "/var/lib/peer-authority";
                  directories = [directory];
                  binding.backend = "files";
                };
              };
            }) ["/srv/history" "/srv/history/nested" "/srv"]
          && custodyAccepted {
            services.harbor-db.cutover.resources.peer = {
              user = "archive";
              custody_file = "/var/lib/peer-authority/custody.json";
              authority = {
                state_dir = "/var/lib/peer-authority";
                directories = ["/srv/history-sibling"];
                binding.backend = "files";
              };
            };
          };
        message = "Every filesystem corpus must belong to one custody resource lease, including across resources.";
      }
      {
        name = "custody-roots-require-normalized-absolute-paths";
        assertion = lib.all (path:
          !custodyAccepted {services.harbor-db.cutover.resources.history.authority.directories = lib.mkForce [path];})
        ["/srv/./history" "/srv/other/../history" "/srv//history" "/srv/history/"];
        message = "ambiguous authority roots must fail admission rather than weakening descendant matching";
      }
      {
        name = "adopted-descendants-cannot-be-initialized";
        assertion = lib.any (item: lib.hasInfix "dataDirectories.create = false" item.message) (harborFailures unsafeDescendant.assertions);
        message = "activation must not create a custody root through a missing descendant";
      }
      {
        name = "valid-adopted-contract";
        assertion = succeeds;
        message = "existing corpus configuration must satisfy NixOS assertions";
      }
      {
        name = "successful-diagnostics-remain-lazy";
        assertion =
          harborFailures [
            {
              assertion = true;
              message = throw "successful diagnostic was forced";
            }
          ]
          == [];
        message = "successful assertions must not force failure-only diagnostics";
      }
      {
        name = "adopted-roots-cannot-be-initialized";
        assertion = lib.any (item: lib.hasInfix "dataDirectories.create = false" item.message) (harborFailures unsafe.assertions);
        message = "automatic directory creation at an adopted corpus root must fail evaluation";
      }
      {
        name = "activation-inspection-precedes-application-checks";
        assertion = config.system.preSwitchChecks ? "00---harbor-db-cutover";
        message = "raw switch-to-configuration must retain the mandatory candidate admission check";
      }
      {
        name = "startup-unit-name-is-service-normalized";
        assertion = lib.any (command: lib.hasInfix "--phase startup" command) config.systemd.services.archive.serviceConfig.ExecStartPre;
        message = "service startup must guard the source before a writer starts";
      }
      {
        name = "startup-administration-retains-resource-lease";
        assertion = lib.any (command: lib.hasInfix "harbor-db-cutover serve" command) config.systemd.services.archive.serviceConfig.ExecStartPre;
        message = "pre-start migrations and administration must retain the same shared resource lease";
      }
      {
        name = "writer-retains-resource-lifetime-lease";
        assertion = lib.hasInfix "harbor-db-cutover serve" config.systemd.services.archive.serviceConfig.ExecStart;
        message = "the real service process must retain custody's shared authority lease";
      }
      {
        name = "startup-and-reload-command-phases-retain-resource-lease";
        assertion = lib.all (phase:
          lib.all (command: lib.hasInfix "harbor-db-cutover serve" command || lib.hasInfix "--phase startup" command)
          (lib.toList config.systemd.services.archive.serviceConfig.${phase}))
        commandPhases;
        message = "conditions, reloads and every startup command must retain the writer authority lease";
      }
      {
        name = "shutdown-hooks-remain-available-during-quiescing";
        assertion = lib.all (phase: lib.toList config.systemd.services.archive.serviceConfig.${phase} == [(phaseCommand phase)]) shutdownPhases;
        message = "Graceful stop and post-stop hooks must execute while the transition inhibits ordinary startup.";
      }
      {
        name = "activation-does-not-create-missing-history";
        assertion =
          !(lib.hasInfix "install -d" config.system.activationScripts.harbor-db-establish-data-directories.text)
          && lib.hasInfix "test -d" config.system.activationScripts.harbor-db-establish-data-directories.text
          && lib.elem "z /srv/history 0750 archive archive - -" config.systemd.tmpfiles.rules
          && lib.elem "z '/srv/history' 0750 archive archive - -" config.systemd.tmpfiles.rules
          && lib.elem "z \"/srv/history\" 0750 archive archive - -" config.systemd.tmpfiles.rules;
        message = "both live activation and boot tmpfiles must preserve require-existing roots";
      }
      {
        name = "descendant-tmpfiles-preserve-missing-history";
        assertion =
          lib.all (rule: lib.elem ("z" + builtins.substring 1 (-1) rule) config.systemd.tmpfiles.rules) descendantRules
          && lib.elem siblingRule config.systemd.tmpfiles.rules;
        message = "directory rules beneath custody roots must require existing paths without matching sibling prefixes";
      }
      {
        name = "unenforced-tmpfiles-retain-creation";
        assertion = lib.all (rule: lib.elem rule disabled.systemd.tmpfiles.rules) descendantRules;
        message = "directory creation must remain available when custody enforcement is disabled";
      }
      {
        name = "read-only-sudo-is-exact-argv";
        assertion = lib.any (rule:
          lib.any (command:
            lib.hasSuffix "check --contract /etc/harbor-db/cutover.json --host fixture --phase preflight" command.command
            && !(lib.hasInfix "*" command.command))
          rule.commands)
        config.security.sudo.extraRules;
        message = "operator privilege must grant only the installed immutable read-only contract";
      }
    ];
    nativeBuildInputs = [pkgs.python3];
    runtimeScript = ''
      ${config.services.harbor-db.cutover.bundle}/checker --help
      ${config.services.harbor-db.cutover.bundle}/checker check --contract ${config.services.harbor-db.cutover.manifest} --host wrong > blocked.json && exit 1
      ${pkgs.python3}/bin/python3 - <<'PY'
      import json
      manifest = json.load(open('${config.services.harbor-db.cutover.manifest}'))
      assert manifest['enforced'] is True
      assert manifest['resources']['history']['authority']['directories'] == ['/srv/history']
      PY
      ${pkgs.python3}/bin/python3 - <<'PY'
      import pathlib
      import subprocess
      import tempfile

      with tempfile.TemporaryDirectory() as staging:
          root = pathlib.Path(staging)
          history = root / 'srv/history'
          command = ['${pkgs.systemd}/bin/systemd-tmpfiles', '--create', '--root=' + staging, '${tmpfilesFixture}']
          subprocess.run(command, check=True)
          assert not history.exists(), 'tmpfiles recreated the missing custody root'
          assert (root / 'srv/history-other/cache').is_dir(), 'sibling rules were incorrectly inhibited'
          cache = history / 'cache'
          cache.mkdir(parents=True, mode=0o700)
          subprocess.run(command, check=True)
          assert cache.stat().st_mode & 0o777 == 0o750, 'permission repair stopped working'
          assert not (cache / 'nested').exists(), 'tmpfiles created a missing descendant'
      PY
      ${pkgs.python3}/bin/python3 ${../tests/check_cutover_command_phases.py} ${commandPhaseFixture} > "$out/command-phases.json"
    '';
  }
