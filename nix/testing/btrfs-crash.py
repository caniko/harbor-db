"""NixOS VM driver helpers; application recovery assertions stay with the caller."""
import json as _btrfs_json
import shlex as _btrfs_shlex


class BtrfsCrashFixture:
    def __init__(self, machine, mount_point, compression_level):
        self.machine = machine
        self.mount_point = mount_point
        self.compression = f"compress=zstd:{compression_level}"
        self.crashes = []

    def verify(self):
        path = _btrfs_shlex.quote(self.mount_point)
        self.machine.wait_until_succeeds(f"mountpoint -q {path}")
        mount = _btrfs_json.loads(self.machine.succeed(
            f"findmnt --json -o FSTYPE,OPTIONS --mountpoint {path}"
        ))["filesystems"][0]
        options = mount["options"].split(",")
        assert mount["fstype"] == "btrfs", mount
        assert self.compression in options, options
        assert not {"sync", "dirsync", "nodatacow", "nodatasum", "nobarrier"}.intersection(options), options
        usage = self.machine.succeed(f"btrfs filesystem usage -b {path}")
        assert "Data,RAID1" in usage and "Metadata,RAID1" in usage, usage
        probe = self.mount_point + "/.harbor-db-compression-probe"
        code = f"import os; f=open({probe!r},'wb'); f.write(b'compressed-btrfs-fixture\\n'*180000); f.flush(); os.fsync(f.fileno()); f.close()"
        self.machine.succeed("python3 -c " + _btrfs_shlex.quote(code))
        compression = self.machine.succeed("compsize -b " + _btrfs_shlex.quote(probe))
        assert "zstd" in compression, compression
        self.filesystem = mount
        self.usage = usage
        self.compression_measurement = compression

    def power_cut(self, phase, recover):
        # No guest sync, stop, reboot, sleep or checkpoint request here.
        # Only record a recovered phase after the caller's checks succeed.
        self.machine.crash()
        self.machine.start()
        recover()
        self.crashes.append(phase)

    def receipt(self):
        return {"filesystem": self.filesystem, "usage": self.usage,
                "compression": self.compression_measurement, "crashes": list(self.crashes)}
