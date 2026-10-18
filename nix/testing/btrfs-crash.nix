{
  pkgs,
  name,
  node,
  testScript,
  mountPoint ? "/var/lib/harbor-db-btrfs",
  compressionLevel ? 9,
}: let
  inherit (pkgs) lib;
  script =
    if lib.isFunction testScript
    then testScript
    else lib.const testScript;
  diskSerials = ["harbor-db-btrfs-1" "harbor-db-btrfs-2"];
  disks = map (serial: "/dev/disk/by-id/virtio-${serial}") diskSerials;
in
  assert lib.hasPrefix "/" mountPoint && mountPoint != "/";
  assert builtins.isInt compressionLevel && compressionLevel >= 1 && compressionLevel <= 15;
    pkgs.testers.runNixOSTest {
      inherit name;
      extraDriverArgs = ["--junit-xml" "junit.xml"];
      nodes.machine = {utils, ...}: let
        mountUnit = "${utils.escapeSystemdPath mountPoint}.mount";
        diskUnits = map (disk: "${utils.escapeSystemdPath disk}.device") disks;
      in {
        imports = [node];
        environment.systemPackages = [pkgs.python3 pkgs.btrfs-progs pkgs.compsize];
        virtualisation = {
          memorySize = lib.mkDefault 4096;
          cores = lib.mkDefault 2;
          emptyDiskImages = lib.mkAfter (map (serial: {
              size = 2048;
              driveConfig.deviceExtraOpts.serial = serial;
            })
            diskSerials);
          fileSystems.${mountPoint} = {
            device = builtins.head disks;
            fsType = "btrfs";
            options = ["compress=zstd:${toString compressionLevel}" "nodev" "nosuid"];
          };
        };
        systemd.services.harbor-db-btrfs-format = {
          requiredBy = [mountUnit];
          before = [mountUnit];
          after = diskUnits;
          requires = diskUnits;
          unitConfig.DefaultDependencies = false;
          serviceConfig.Type = "oneshot";
          script = ''
            if ! ${pkgs.util-linux}/bin/blkid ${lib.escapeShellArg (builtins.head disks)}; then
              ${pkgs.btrfs-progs}/bin/mkfs.btrfs -f -d raid1 -m raid1 ${lib.escapeShellArgs disks}
            fi
          '';
        };
      };
      testScript = lib.mirrorFunctionArgs script (args: ''
        ${builtins.readFile ./btrfs-crash.py}

        storage_fixture = BtrfsCrashFixture(machine, ${builtins.toJSON mountPoint}, ${toString compressionLevel})
        start_all()
        with subtest("ordinary compressed checksummed RAID1 without synchronous mount"):
            storage_fixture.verify()

        ${script args}
      '');
    }
