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
    raise SystemExit({"ExecCondition": 17, "ExecReload": 23}[phase])
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
  eval = import "${pkgs.path}/nixos/lib/eval-config.nix" {
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
        systemd.services.archive.serviceConfig.ExecStop = "${pkgs.coreutils}/bin/true";
        systemd.services.archive.serviceConfig.ExecStopPost = ["${pkgs.coreutils}/bin/true"];
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
  succeeds = harborFailures config.assertions == [];
  commandPhases = ["ExecCondition" "ExecStartPre" "ExecStart" "ExecStartPost" "ExecReload" "ExecStop" "ExecStopPost"];
  commandPhaseFixture = pkgs.writeText "harbor-db-cutover-command-phases.json" (builtins.toJSON {
    manifest = config.services.harbor-db.cutover.manifest;
    checker = "${config.services.harbor-db.cutover.package}/bin/harbor-db-cutover";
    inherit literalArgument;
    commands = lib.genAttrs commandPhases (phase: lib.toList config.systemd.services.archive.serviceConfig.${phase});
  });
  tmpfilesFixture = pkgs.writeText "harbor-db-cutover-tmpfiles.conf" (lib.concatStringsSep "\n" (map
    (lib.replaceStrings ["archive archive"] ["- -"])
    (lib.filter (lib.hasInfix "/srv/history") config.systemd.tmpfiles.rules)));
in
  (import ./eval-checks.nix {inherit pkgs;}).mkEvalCheck {
    name = "harbor-db-cutover-eval";
    assertions = [
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
        name = "all-seven-command-phases-retain-resource-lease";
        assertion = lib.all (phase:
          lib.all (command: lib.hasInfix "harbor-db-cutover serve" command || lib.hasInfix "--phase startup" command)
          (lib.toList config.systemd.services.archive.serviceConfig.${phase}))
        commandPhases;
        message = "conditions, reloads and every startup/shutdown command must retain the writer authority lease";
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
