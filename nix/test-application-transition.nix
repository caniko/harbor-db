{
  pkgs,
  module,
}: let
  storage = import ./postgres-package.nix {inherit pkgs;};
  sourceAuthority = {
    resource = "demo";
    state_dir = "/var/lib/demo-authority";
    directories = ["/var/lib/demo-old"];
    binding.backend = "filesystem-source";
    required_files = [];
    required_mounts = [];
    consumer_command = [];
  };
  targetAuthority =
    sourceAuthority
    // {
      directories = ["/var/lib/demo-new"];
      binding.backend = "filesystem-target";
    };
  sourceManifest = pkgs.writeText "demo-source.json" (builtins.toJSON sourceAuthority);
  targetManifest = pkgs.writeText "demo-target.json" (builtins.toJSON targetAuthority);
  custody = {
    kind = "filesystem";
    user = "demo";
    authority = targetAuthority;
    runtime_units = ["demo.service"];
    custody_file = "/var/lib/demo-authority/custody.json";
    max_age_seconds = 3600;
    database_resource = null;
    database_inventory_checks = [];
    git_executable = "${pkgs.gitMinimal}/bin/git";
    login_shell = null;
  };
  custodyManifest = pkgs.writeText "demo-custody.json" (builtins.toJSON custody);
  adapter = pkgs.writeText "transition-adapter.py" ''
    import hashlib, json, pathlib, sys, time
    stage, *args = sys.argv[1:]
    old, new = pathlib.Path("/var/lib/demo-old/records"), pathlib.Path("/var/lib/demo-new/records")
    def semantic(path): return hashlib.sha256(path.read_bytes()).hexdigest()
    if stage == "capture":
        backup, workspace = map(pathlib.Path, args)
        backup.mkdir()
        (backup / "records").write_bytes(old.read_bytes())
        (backup / "capture.json").write_text(json.dumps({"version":1,"consistency":"quiesced","semantic_sha256":semantic(old)}))
    elif stage == "restore":
        backup, workspace = map(pathlib.Path, args)
        (workspace / "records").write_bytes((backup / "records").read_bytes())
    elif stage == "verify":
        backup, workspace = map(pathlib.Path, args)
        assert (workspace / "records").read_bytes() == (backup / "records").read_bytes()
        print(json.dumps({"version":1,"status":"verified","semantic_sha256":semantic(workspace / "records")}))
    elif stage == "import":
        backup = pathlib.Path(args[0])
        new.write_bytes((backup / "records").read_bytes())
    elif stage in ("verify-target", "verify-source"):
        expected = pathlib.Path(args[0]) / "records"
        observed = new if stage == "verify-target" else old
        assert expected.read_bytes() == observed.read_bytes()
        print(json.dumps({"version":1,"status":"verified","semantic_sha256":semantic(observed)}))
    elif stage == "health":
        assert new.read_text().startswith("acknowledged")
        print(json.dumps({"version":1,"status":"healthy"}))
    elif stage == "writer":
        if args[0] == "target": new.write_text("acknowledged target revision eight")
        while True: time.sleep(1)
    elif stage != "cleanup": raise ValueError(stage)
  '';
  command = stage: ["${pkgs.python3}/bin/python3" (toString adapter) stage "{backup}" "{workspace}"];
  common = {lib, ...}: {
    imports = [module];
    services.timesyncd.enable = lib.mkForce false;
    users.users.demo = {
      isSystemUser = true;
      group = "demo";
    };
    users.groups.demo = {};
    environment.systemPackages = [storage pkgs.python3 pkgs.gnutar pkgs.gzip pkgs.coreutils];
    systemd.tmpfiles.rules = ["d /var/lib/demo-authority 0700 demo demo -" "d /var/lib/demo-old 0700 demo demo -" "d /var/lib/demo-new 0700 demo demo -" "d /var/lib/demo-certifier 0700 demo demo -" "f /var/lib/demo-certifier/lock 0600 demo demo -"];
    services.harbor-db.projects.demo.backup = {
      enable = true;
      user = "demo";
      group = "demo-backup";
      directory = "/var/lib/demo-backups";
      commands = lib.genAttrs ["capture" "restore" "verify" "cleanup"] command;
      executableFiles = [(toString adapter)];
    };
    systemd.timers.harbor-db-demo-backup.wantedBy = lib.mkForce [];
    system.stateVersion = "26.05";
  };
in
  pkgs.testers.runNixOSTest {
    name = "harbor-db-application-backend-transition";
    nodes = {
      primary = {
        config,
        lib,
        ...
      }: {
        imports = [common];
        services.harbor-db.projects.demo.transition = {
          enable = true;
          sourceManifest = toString sourceManifest;
          targetManifest = toString targetManifest;
          backupManifest = toString config.environment.etc."harbor-db/demo-backup.json".source;
          targetCustodyManifest = toString custodyManifest;
          independentReceipt = "/var/lib/demo-authority/independent.json";
          runtimeUnits = ["demo.service"];
          commands = lib.genAttrs ["import" "verify-target" "verify-source" "health"] (stage: {
            user = "demo";
            argv = ["${pkgs.python3}/bin/python3" (toString adapter) stage "{backup}"];
          });
          executableFiles = [(toString adapter)];
        };
        systemd.services.demo.serviceConfig = {
          User = "demo";
          ExecStart = "${pkgs.python3}/bin/python3 ${adapter} writer source";
        };
        specialisation.target.configuration = {config, ...}: {
          services.harbor-db.cutover = {
            enable = true;
            resources.demo = custody // {transition_manifest = toString config.services.harbor-db.projects.demo.transition.manifest;};
          };
          systemd.services.demo.serviceConfig.ExecStart = lib.mkForce "${pkgs.python3}/bin/python3 ${adapter} writer target";
        };
      };
      certifier = common;
    };
    testScript = ''
      import json
      start_all()
      def align(receiver, sender):
          seconds = max(int(host.succeed("date +%s").strip()) for host in (receiver, sender))
          receiver.succeed(f"date --set=@{seconds}")
      primary.wait_for_unit("multi-user.target")
      certifier.wait_for_unit("multi-user.target")
      primary.succeed("runuser -u demo -- sh -c 'printf source-revision-seven > /var/lib/demo-old/records'")
      primary.succeed("runuser -u demo -- harbor-db-resource --config ${sourceManifest} adopt --identity retained-resource")
      primary.succeed("systemctl start demo.service")
      primary.wait_for_unit("demo.service")
      contract = primary.succeed("readlink -f /etc/harbor-db/demo-transition.json").strip()
      transition = f"harbor-db-transition --config {contract}"
      primary.succeed(transition + " plan --candidate " + contract)
      result = json.loads(primary.succeed(transition + " prepare"))
      assert result["status"] == "awaiting-independent-restore"
      primary.fail("systemctl is-active demo.service")
      primary.succeed("systemctl start demo.service")
      primary.fail("systemctl is-active demo.service")
      primary.fail("runuser -u demo -- harbor-db-resource --config ${sourceManifest} check")
      primary.crash()
      primary.start()
      primary.wait_for_unit("multi-user.target")
      primary.succeed("systemctl start demo.service")
      primary.fail("systemctl is-active demo.service")
      point = result["backup"].split("/")[-1]
      archive = primary.succeed(f"tar -C /var/lib/demo-backups -czf - {point} | base64 -w0").strip()
      certifier.succeed(f"printf '%s' '{archive}' | base64 -d | tar -xzf - -C /var/lib/demo-backups")
      align(certifier, primary)
      proof = json.loads(certifier.succeed(f"runuser -u demo -- harbor-db-application-backup --config /etc/harbor-db/demo-backup.json certify /var/lib/demo-backups/{point} --state /var/lib/demo-certifier"))
      encoded = certifier.succeed(f"base64 -w0 /var/lib/demo-certifier/{proof['source_acceptance_sha256']}.json").strip()
      primary.succeed(f"printf '%s' '{encoded}' | base64 -d > /var/lib/demo-authority/independent.json")
      align(primary, certifier)
      assert json.loads(primary.succeed(transition + " prepare"))["phase"] == "prepared"
      candidate = primary.succeed("readlink -f /run/current-system/specialisation/target").strip()
      primary.succeed(transition + " bind-candidate --candidate " + candidate)
      primary.succeed(candidate + "/bin/switch-to-configuration test")
      primary.fail("systemctl is-active demo.service")
      primary.succeed(transition + " commit")
      primary.succeed("test -f /var/lib/harbor-db-transitions/demo/inhibited.json")
      primary.succeed(transition + " enable-writes")
      primary.succeed("systemctl start demo.service")
      primary.wait_for_unit("demo.service")
      primary.succeed(transition + " complete")
      primary.fail(transition + " abort")
      primary.succeed("runuser -u demo -- harbor-db-resource --config ${targetManifest} check")
      primary.fail("runuser -u demo -- harbor-db-resource --config ${sourceManifest} check")
      primary.succeed(transition + " retire")
      primary.succeed("test -f /var/lib/demo-old/records")
      primary.succeed("grep acknowledged /var/lib/demo-new/records")
    '';
  }
