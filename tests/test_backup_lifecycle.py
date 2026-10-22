import json
import os
import tempfile
import unittest
from pathlib import Path

from harbor_db.backup import prune


class RecoveryChainRetentionTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "base").mkdir()
        (self.root / "wal").mkdir()
        self.now = 100 * 86400

    def backup(self, name, age, start):
        path = self.root / "base" / name
        path.mkdir()
        (path / "backup_manifest").write_text(
            json.dumps(
                {
                    "WAL-Ranges": [
                        {"Timeline": 1, "Start-LSN": start, "End-LSN": "0/9000000"}
                    ],
                }
            )
        )
        os.utime(path, (self.now - age * 86400,) * 2)
        return path

    def wal(self, segment, timeline=1):
        path = self.root / "wal" / f"{timeline:08X}00000000{segment:08X}"
        path.touch()
        with path.open("wb") as stream:
            stream.truncate(16 * 1024**2)
        os.utime(path, (0, 0))
        return path

    def test_expired_last_two_backups_and_required_wal_are_protected(self):
        expired = self.backup("oldest", 90, "0/1000000")
        retained = self.backup("last-good", 80, "0/3000000")
        self.backup("latest", 70, "0/5000000")
        obsolete, needed, unknown = self.wal(2), self.wal(3), self.wal(1, timeline=2)
        prune(self.root, 30, 31, 16 * 1024**2, now=self.now)
        self.assertFalse(expired.exists())
        self.assertTrue(retained.exists())
        self.assertFalse(obsolete.exists())
        self.assertTrue(needed.exists())
        self.assertTrue(unknown.exists())

    def test_unknown_manifest_prevents_wal_deletion(self):
        legacy = self.backup("legacy", 90, "0/3000000")
        (legacy / "backup_manifest").unlink()
        self.backup("latest", 1, "0/5000000")
        old = self.wal(1)
        prune(self.root, 30, 31, 16 * 1024**2, now=self.now)
        self.assertTrue(old.exists())
        self.assertTrue(legacy.exists())

    def test_partial_is_never_counted_as_last_good_backup(self):
        self.backup("latest.partial", 0, "0/9000000")
        good = self.backup("last-good", 90, "0/1000000")
        required = self.wal(1)
        prune(self.root, 30, 31, 16 * 1024**2, now=self.now)
        self.assertTrue(good.exists())
        self.assertTrue(required.exists())

    def test_unclassified_expired_backup_prevents_all_pruning(self):
        expired = self.backup("oldest", 90, "0/1000000")
        (expired / "backup_manifest").write_text(
            '{"WAL-Ranges":[{"Timeline":1,"Start-LSN":"0/1000000","End-LSN":"0/0"}]}'
        )
        self.backup("last-good", 80, "0/3000000")
        self.backup("latest", 70, "0/5000000")
        old = self.wal(2)
        prune(self.root, 30, 31, 16 * 1024**2, now=self.now)
        self.assertTrue(expired.exists())
        self.assertTrue(old.exists())

    def test_redirected_backup_prevents_pruning_other_recovery_chains(self):
        expired = self.backup("oldest", 90, "0/1000000")
        self.backup("last-good", 80, "0/3000000")
        self.backup("latest", 70, "0/5000000")
        (self.root / "base" / "redirected").symlink_to(
            expired, target_is_directory=True
        )
        old = self.wal(2)
        prune(self.root, 30, 31, 16 * 1024**2, now=self.now)
        self.assertTrue(expired.exists())
        self.assertTrue(old.exists())


if __name__ == "__main__":
    unittest.main()
