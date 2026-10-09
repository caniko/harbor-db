{
  pkgs,
  module,
}: let
  adapter = pkgs.writeText "backup-adapter.py" ''
    import hashlib, json, os, pathlib, sys
    stage, backup, workspace = sys.argv[1:]
    backup, workspace = pathlib.Path(backup), pathlib.Path(workspace)
    assert (pathlib.Path(os.environ["CREDENTIALS_DIRECTORY"]) / "token").read_text().strip() == "fixture-token"
    if stage == "capture":
        backup.mkdir()
        (backup / "records").write_text("retained revision seven")
        digest = hashlib.sha256((backup / "records").read_bytes()).hexdigest()
        (backup / "capture.json").write_text(json.dumps({"version": 1, "consistency": "quiesced", "semantic_sha256": digest}))
    elif stage == "restore":
        (workspace / "records").write_bytes((backup / "records").read_bytes())
    elif stage == "verify":
        assert (workspace / "records").read_bytes() == (backup / "records").read_bytes()
        print(json.dumps({"version": 1, "status": "verified", "semantic_sha256": hashlib.sha256((workspace / "records").read_bytes()).hexdigest()}))
  '';
in
  pkgs.testers.runNixOSTest {
    name = "harbor-db-application-backup";
    nodes.machine = {lib, ...}: {
      imports = [module];
      users.users = {
        demo = {
          isSystemUser = true;
          group = "demo";
        };
        reader = {
          isSystemUser = true;
          group = "reader";
        };
      };
      users.groups = {
        demo = {};
        reader = {};
      };
      environment.etc."backup-fixture-token" = {
        text = "fixture-token";
        mode = "0600";
      };
      environment.systemPackages = [pkgs.python3];
      services.harbor-db.projects.demo.backup = {
        enable = true;
        user = "demo";
        group = "demo-backup";
        directory = "/var/lib/demo-backup";
        readers = ["reader"];
        commands = lib.genAttrs ["capture" "restore" "verify" "cleanup"] (stage: ["${pkgs.python3}/bin/python3" (toString adapter) stage "{backup}" "{workspace}"]);
        executableFiles = [(toString adapter)];
        credentials.token = "/etc/backup-fixture-token";
      };
      systemd.timers.harbor-db-demo-backup.wantedBy = lib.mkForce [];
      system.stateVersion = "26.05";
    };
    testScript = ''
      # reboot() reconnects only when QEMU was started with reboot support.
      machine.start(allow_reboot=True)
      machine.wait_for_unit("multi-user.target")
      machine.succeed("systemctl start harbor-db-demo-backup.service")
      point = machine.succeed("python3 -c 'import json; print(json.load(open(\"/var/lib/demo-backup/LAST_SUCCESS\"))[\"attempt\"])'").strip()
      machine.succeed(f"runuser -u reader -- cat /var/lib/demo-backup/{point}/records")
      machine.succeed(f"test $(stat -c %a /var/lib/demo-backup/{point}/records) = 640")
      machine.succeed("test $(stat -c %a /var/lib/demo-backup/lock) = 640")
      machine.fail("runuser -u reader -- cat /etc/backup-fixture-token")
      machine.succeed("systemctl start harbor-db-demo-backup.service")
      machine.reboot()
      machine.wait_for_unit("multi-user.target")
      machine.succeed(f"runuser -u reader -- cat /var/lib/demo-backup/{point}/records")
    '';
  }
