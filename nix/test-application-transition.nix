{
  pkgs,
  module,
  withPostgres ? false,
}: let
  inherit (pkgs) lib;
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
      binding.backend =
        if withPostgres
        then "postgresql"
        else "filesystem-target";
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
  postgresManifest = pkgs.writeText "demo-primary-template.json" (builtins.toJSON {
    resource = "postgresql";
    data_dir = "/var/lib/postgres/18";
    state_dir = "/var/lib/demo-primary-authority";
    major = "18";
    package = toString pkgs.postgresql_18;
    required_mounts = [];
    writer_fence = {
      control_role = "postgres";
      replication_roles = [];
      allowed_preload_libraries = [];
    };
    recovery = {
      system_identifier = "12345";
      source_hostname = "primary";
      backup_root = "/var/lib/demo-primary-backup";
      snapshot_file = "/var/lib/demo-primary-backup/evidence/records.json";
      receipt_file = "/var/lib/demo-primary-backup/evidence/recovery.json";
      off_host_receipt_file = null;
      require_writer_fence = true;
      max_age_seconds = 3600;
      verify_timeout_seconds = 60;
      record_checks = [
        {
          name = "documents";
          database = "postgres";
          sql = "SELECT id,body FROM documents ORDER BY id";
        }
      ];
    };
  });
  adapter = pkgs.writeText "transition-adapter.py" ''
    import hashlib, json, os, pathlib, subprocess, sys, time
    stage, *args = sys.argv[1:]
    postgres = ${
      if withPostgres
      then "True"
      else "False"
    }
    old, new = pathlib.Path("/var/lib/demo-old/records"), pathlib.Path("/var/lib/demo-new/records")
    def semantic(path): return hashlib.sha256(path.read_bytes()).hexdigest()
    def sql(statement):
        leases = tuple(int(value) for value in os.environ.get("HARBOR_DB_LEASE_FDS", "").split(",") if value)
        return subprocess.run(["${pkgs.postgresql_18}/bin/psql", "-XwqAt", "-v", "ON_ERROR_STOP=1",
            "-h", "/run/postgresql", "-U", "postgres", "-d", "postgres", "-c", statement],
            capture_output=True, text=True, check=True, timeout=30, pass_fds=leases).stdout.strip()
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
        if postgres:
            sql("SET ROLE demo_owner; INSERT INTO documents VALUES (1,'source-revision-seven') ON CONFLICT (id) DO UPDATE SET body=EXCLUDED.body")
            counter = new.parent / "import-count"
            counter.write_text(str(int(counter.read_text()) + 1 if counter.exists() else 1))
    elif stage in ("verify-target", "verify-source"):
        expected = pathlib.Path(args[0]) / "records"
        observed = new if stage == "verify-target" else old
        assert expected.read_bytes() == observed.read_bytes()
        if postgres and stage == "verify-target":
            assert sql("SELECT body FROM documents WHERE id=1") == expected.read_text()
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
    systemd.tmpfiles.rules = [
      "d /var/lib/demo-authority 0700 demo demo -"
      "d /var/lib/demo-old 0700 demo demo -"
      "d /var/lib/demo-new ${
        if withPostgres
        then "0770"
        else "0700"
      } demo demo -"
      "d /var/lib/demo-certifier 0700 demo demo -"
      "f /var/lib/demo-certifier/lock 0600 demo demo -"
    ];
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
    name = "harbor-db-application-${
      if withPostgres
      then "postgres"
      else "backend"
    }-transition";
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
          postgresManifest =
            if withPostgres
            then toString postgresManifest
            else null;
          commands = lib.genAttrs ["import" "verify-target" "verify-source" "health"] (stage: {
            user =
              if withPostgres && lib.elem stage ["import" "verify-target"]
              then "postgres"
              else "demo";
            argv = ["${pkgs.python3}/bin/python3" (toString adapter) stage "{backup}"];
          });
          executableFiles = [(toString adapter)];
        };
        services.postgresql = lib.mkIf withPostgres {
          enable = true;
          package = pkgs.postgresql_18;
          dataDir = "/var/lib/postgres/18";
          authentication = lib.mkBefore "local replication postgres peer\nlocal all demo_runtime peer map=demo\n";
          identMap = "demo demo demo_runtime";
        };
        users.users = lib.optionalAttrs withPostgres {postgres.extraGroups = ["demo" "demo-backup"];};
        environment.systemPackages = lib.optionals withPostgres [pkgs.nix pkgs.postgresql_18];
        systemd.tmpfiles.rules = lib.optionals withPostgres [
          "d /var/lib/postgres/18 0700 postgres postgres -"
          "d /var/lib/demo-primary-authority 0700 postgres postgres -"
          "d /var/lib/demo-primary-backup 0700 postgres postgres -"
          "d /var/lib/demo-primary-backup/base 0700 postgres postgres -"
          "d /var/lib/demo-primary-backup/locks 0700 postgres postgres -"
          "f /var/lib/demo-primary-backup/locks/mutate 0600 postgres postgres -"
          "d /var/lib/demo-primary-backup/evidence 0700 postgres postgres -"
          "d /var/lib/demo-recovered 0700 postgres postgres -"
          "d /var/lib/demo-restore-wal 0700 postgres postgres -"
          "d /var/lib/demo-recovery-socket 0700 postgres postgres -"
        ];
        systemd.services.demo.serviceConfig = {
          User = "demo";
          ExecStart = "${pkgs.python3}/bin/python3 ${adapter} writer source";
        };
        specialisation = lib.optionalAttrs (!withPostgres) {
          target.configuration = {config, ...}: {
            services.harbor-db.cutover = {
              enable = true;
              resources.demo = custody // {transition_manifest = toString config.services.harbor-db.projects.demo.transition.manifest;};
            };
            systemd.services.demo.serviceConfig.ExecStart = lib.mkForce "${pkgs.python3}/bin/python3 ${adapter} writer target";
          };
        };
      };
      certifier = common;
    };
    testScript =
      if withPostgres
      then builtins.readFile ./test-application-postgres-transition.py
      else ''
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
        backup_contract = certifier.succeed("readlink -f /etc/harbor-db/demo-backup.json").strip()
        proof = json.loads(certifier.succeed(f"runuser -u demo -- harbor-db-application-backup --config {backup_contract} certify /var/lib/demo-backups/{point} --state /var/lib/demo-certifier"))
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
