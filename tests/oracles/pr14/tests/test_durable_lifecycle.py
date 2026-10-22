import multiprocessing
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from harbor_db import durable


def hold_lock(path, ready):
    with durable.lock(path, create=True):
        ready.send(True)
        ready.recv()


class DurablePublicationTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def test_failed_file_flush_preserves_previous_acknowledged_value(self):
        target = self.root / "identity"
        durable.atomic_write(target, b"old")
        with patch.object(os, "fsync", side_effect=OSError("flush failed")):
            with self.assertRaises(OSError):
                durable.atomic_write(target, b"new")
        self.assertEqual(target.read_bytes(), b"old")

    def test_failed_directory_flush_is_not_acknowledged(self):
        target = self.root / "identity"
        with patch.object(durable, "sync_directory", side_effect=OSError("directory flush failed")):
            with self.assertRaises(OSError):
                durable.atomic_write(target, b"new")
        self.assertEqual(target.stat().st_mode & 0o777, 0o600)

    def test_kernel_releases_lock_after_sigkill_without_replacing_inode(self):
        anchor = self.root / "lock"
        parent, child = multiprocessing.Pipe()
        process = multiprocessing.Process(target=hold_lock, args=(anchor, child))
        process.start()
        self.addCleanup(parent.close)
        self.addCleanup(child.close)
        self.addCleanup(lambda: process.kill() if process.is_alive() else None)
        self.assertTrue(parent.poll(10), "lock holder did not start")
        self.assertTrue(parent.recv())
        inode = anchor.stat().st_ino
        with self.assertRaises(BlockingIOError):
            with durable.lock(anchor):
                self.fail("concurrent writer acquired the lock")
        process.kill()
        process.join(10)
        self.assertFalse(process.is_alive())
        with durable.lock(anchor):
            self.assertEqual(anchor.stat().st_ino, inode)

    def test_sync_tree_rejects_external_storage(self):
        tree = self.root / "staging"
        tree.mkdir()
        (tree / "external").symlink_to(self.root / "data")
        with self.assertRaisesRegex(ValueError, "external"):
            durable.sync_tree(tree)


if __name__ == "__main__":
    unittest.main()
