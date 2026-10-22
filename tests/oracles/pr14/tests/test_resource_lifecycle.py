import json
import select
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from harbor_db import resource
from harbor_db.durable import lock, write_json


class ResourceAuthorityTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        for name in ("authority", "data", "state"):
            (self.root / name).mkdir()
        self.config = {
            "resource": "annotations", "state_dir": str(self.root / "authority"),
            "binding": {"backend": "filesystem", "endpoint": str(self.root / "data")},
            "directories": [str(self.root / "data"), str(self.root / "state")],
            "required_mounts": [],
        }

    def test_startup_never_adopts_missing_storage(self):
        with self.assertRaises(resource.AuthorityError):
            resource.check(self.config)
        self.assertEqual(list((self.root / "data").iterdir()), [])

    def test_backend_and_path_change_fail_closed(self):
        resource.adopt(self.config, "verified-archive")
        resource.check(self.config)
        changed = dict(self.config, binding={"backend": "postgresql", "endpoint": "postgres:///other"})
        with self.assertRaises(resource.AuthorityError):
            resource.check(changed)
        with self.assertRaises(resource.AuthorityError):
            resource.adopt(changed, "verified-archive")

    def test_adoption_cannot_replace_a_live_writers_unlinked_lock(self):
        resource.adopt(self.config, "verified-archive")
        anchor = self.root / "authority" / "lock"
        with lock(anchor, shared=True):
            anchor.unlink()
            with self.assertRaises(FileNotFoundError):
                resource.adopt(self.config, "verified-archive")
            self.assertFalse(anchor.exists())

    def test_interrupted_first_adoption_can_retry_with_the_same_identity(self):
        authority = self.root / "authority" / "identity.json"

        def interrupt(path, value):
            if path == authority:
                raise OSError("authority publication interrupted")
            write_json(path, value)

        with patch.object(resource, "write_json", side_effect=interrupt), self.assertRaisesRegex(OSError, "interrupted"):
            resource.adopt(self.config, "verified-archive")
        self.assertFalse(authority.exists())
        resource.adopt(self.config, "verified-archive")
        resource.check(self.config)

    def test_recreated_empty_directory_cannot_replace_adopted_storage(self):
        resource.adopt(self.config, "verified-archive")
        marker = resource.anchor(self.config, self.root / "state")
        marker.unlink()
        with self.assertRaises(resource.AuthorityError):
            resource.check(self.config)

    def test_missing_revision_snapshot_is_not_an_empty_database(self):
        snapshot = self.root / "state" / "state.json"
        snapshot.write_text('{"revisions":{"dataset":42},"receipts":{}}')
        self.config["required_files"] = [str(snapshot)]
        resource.adopt(self.config, "verified-archive")
        resource.check(self.config)
        snapshot.unlink()
        with self.assertRaisesRegex(resource.AuthorityError, "required storage file"):
            resource.check(self.config)

    def consumer_contract(self):
        output = self.root / "consumer.json"
        review = self.root / "state" / "review.toml"
        review.write_text("[sessions]\n")
        output.write_text(json.dumps({
            "binding": {"dataset": "one", "config_sha256": "verified"},
            "directories": [str(self.root / "data")], "required_files": [str(review)],
        }))
        self.config["consumer_command"] = [
            sys.executable, "-B", "-c",
            "import pathlib,sys; print(pathlib.Path(sys.argv[1]).read_text())", str(output),
        ]
        return output, review

    def test_consumer_binding_change_cannot_select_stale_dataset(self):
        output, _ = self.consumer_contract()
        resource.adopt(self.config, "verified-archive")
        changed = json.loads(output.read_text())
        changed["binding"]["config_sha256"] = "stale-generation"
        output.write_text(json.dumps(changed))
        with self.assertRaisesRegex(resource.AuthorityError, "authority mismatch"):
            resource.check(self.config)

    def test_missing_consumer_review_fails_before_legacy_migration(self):
        _, review = self.consumer_contract()
        resource.adopt(self.config, "verified-archive")
        review.unlink()
        with self.assertRaisesRegex(resource.AuthorityError, "required storage file"):
            resource.check(self.config)

    def test_adopted_consumer_floors_reject_empty_and_older_records_but_allow_later_saves(self):
        output, _ = self.consumer_contract()
        contract = json.loads(output.read_text())
        contract["minimum_counters"] = {"revision": 42, "review:one": 1}
        output.write_text(json.dumps(contract))
        resource.adopt(self.config, "verified-archive")
        for stale in ({"revision": 41, "review:one": 1}, {"revision": 42}):
            contract["minimum_counters"] = stale
            output.write_text(json.dumps(contract))
            with self.assertRaisesRegex(resource.AuthorityError, "older or incomplete"):
                resource.check(self.config)
        later = self.root / "state" / "later.toml"
        later.write_text("[sessions]\n")
        contract["required_files"].append(str(later))
        contract["minimum_counters"] = {"revision": 43, "review:one": 1, "review:two": 1}
        output.write_text(json.dumps(contract))
        resource.check(self.config)

    def test_consumer_failure_never_publishes_authority(self):
        self.config["consumer_command"] = [sys.executable, "-c", "raise SystemExit(1)"]
        with self.assertRaisesRegex(resource.AuthorityError, "consumer storage validation failed"):
            resource.adopt(self.config, "verified-archive")
        self.assertFalse((self.root / "authority" / "identity.json").exists())

    def test_consumer_writer_holds_authority_until_it_exits(self):
        resource.adopt(self.config, "verified-archive")
        worker = [sys.executable, "-B", "-c", "import sys; print('ready', flush=True); sys.stdin.buffer.read(1)"]
        command = f"from harbor_db import resource; resource.serve({self.config!r}, {worker!r})"
        process = subprocess.Popen([sys.executable, "-B", "-c", command],
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            self.assertTrue(select.select([process.stdout], [], [], 10)[0])
            self.assertEqual(process.stdout.readline(), b"ready\n")
            with self.assertRaises(BlockingIOError):
                with lock(self.root / "authority" / "lock"):
                    self.fail("offline operation acquired a live consumer's authority")
            _, error = process.communicate(input=b"x", timeout=10)
            self.assertEqual(process.returncode, 0, error.decode())
            with lock(self.root / "authority" / "lock"):
                pass
        finally:
            if process.poll() is None:
                process.kill()
            process.communicate(timeout=10)


if __name__ == "__main__":
    unittest.main()
