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
in
  assert lib.hasPrefix "/" mountPoint && mountPoint != "/";
  assert builtins.isInt compressionLevel && compressionLevel >= 1 && compressionLevel <= 15;
    pkgs.testers.runNixOSTest {
      inherit name;
      extraDriverArgs = ["--junit-xml" "junit.xml"];
      nodes.machine = {utils, ...}: let
        mountUnit = "${utils.escapeSystemdPath mountPoint}.mount";
      in {
        imports = [node];
        environment.systemPackages = [pkgs.python3 pkgs.btrfs-progs pkgs.compsize];
        virtualisation = {
          memorySize = lib.mkDefault 4096;
          cores = lib.mkDefault 2;
          emptyDiskImages = [2048 2048];
          fileSystems.${mountPoint} = {
            device = "/dev/vdb";
            fsType = "btrfs";
            options = ["compress=zstd:${toString compressionLevel}" "nodev" "nosuid"];
          };
        };
        systemd.services.harbor-db-btrfs-format = {
          requiredBy = [mountUnit];
          before = [mountUnit];
          after = ["dev-vdb.device" "dev-vdc.device"];
          requires = ["dev-vdb.device" "dev-vdc.device"];
          unitConfig.DefaultDependencies = false;
          serviceConfig.Type = "oneshot";
          script = ''
            if ! ${pkgs.util-linux}/bin/blkid /dev/vdb; then
              ${pkgs.btrfs-progs}/bin/mkfs.btrfs -f -d raid1 -m raid1 /dev/vdb /dev/vdc
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
