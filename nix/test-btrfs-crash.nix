{
  pkgs,
  mkBtrfsCrashTest,
}:
mkBtrfsCrashTest {
  inherit pkgs;
  name = "harbor-db-btrfs-crash";
  node = {
    virtualisation.emptyDiskImages = [64];
    virtualisation.fileSystems."/var/lib/consumer" = {
      device = "/dev/vdb";
      fsType = "ext4";
      autoFormat = true;
    };
  };
  testScript = {nodes, ...}:
    assert pkgs.lib.assertMsg
    (nodes.machine.virtualisation.fileSystems."/var/lib/harbor-db-btrfs".device != nodes.machine.virtualisation.fileSystems."/var/lib/consumer".device)
    "Btrfs crash fixture overlaps the consumer disk"; ''
      import json
      import os
      import shlex
      from pathlib import Path

      root = "${nodes.machine.virtualisation.fileSystems."/var/lib/harbor-db-btrfs".mountPoint}"
      consumer = "/var/lib/consumer"
      with subtest("consumer disk remains separate from the RAID1 fixture"):
          machine.wait_until_succeeds("mountpoint -q " + consumer)
          assert machine.succeed("findmnt -n -o FSTYPE --mountpoint " + consumer).strip() == "ext4"
          assert machine.succeed("findmnt -n -o SOURCE --mountpoint " + consumer).strip() == "/dev/vdb"

      with subtest("synchronous mounts are rejected"):
          machine.succeed("mount -o remount,sync " + shlex.quote(root))
          try:
              storage_fixture.verify()
          except AssertionError:
              pass
          else:
              raise AssertionError("synchronous mount was accepted")
          machine.succeed("mount -o remount,async " + shlex.quote(root))
          storage_fixture.verify()

      with subtest("acknowledged bytes recover without a clean shutdown"):
          expected = "acknowledged revision seven: Küche"
          path = root + "/acknowledged"
          for directory in (root, consumer):
              code = f"import os; f=open({directory + '/acknowledged'!r},'w'); f.write({expected!r}); f.flush(); os.fsync(f.fileno()); f.close(); d=os.open({directory!r},os.O_RDONLY); os.fsync(d); os.close(d)"
              machine.succeed("python3 -c " + shlex.quote(code))

          def recovered():
              machine.wait_until_succeeds("mountpoint -q " + shlex.quote(root))
              assert machine.succeed("cat " + shlex.quote(path)) == expected
              machine.wait_until_succeeds("mountpoint -q " + consumer)
              assert machine.succeed("cat " + consumer + "/acknowledged") == expected

          storage_fixture.power_cut("acknowledged-file", recovered)
          receipt = storage_fixture.receipt()
          assert receipt["crashes"] == ["acknowledged-file"], receipt
          Path(os.environ["out"], "receipts.json").write_text(json.dumps(receipt, indent=2) + "\n")
    '';
}
