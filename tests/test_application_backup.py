"""Executed backup failures must never replace the last accepted restore point."""

import json
import os
import sys
import tempfile
import unittest
from unittest import mock
from pathlib import Path

from harbor_db import application_backup
from harbor_db.durable import read_json


class ApplicationBackupTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        machine = self.root / "machine-id"
        machine.write_text("0123456789abcdef0123456789abcdef\n")
        real_digest = application_backup.digest
        # Native Nix builders have no host machine-id. Model this one host's
        # identity while hashing every artifact and executable normally; the
        # two-machine VM gate verifies actual independent-host execution.
        self.enterContext(mock.patch.object(application_backup, "digest", side_effect=lambda path:
            real_digest(machine if str(path) == "/etc/machine-id" else path)))
        self.backups = self.root / "backups"
        self.backups.mkdir(mode=0o700)
        (self.backups / "lock").touch(mode=0o600)
        self.adapter = self.root / "adapter.py"
        self.adapter.write_text('''import hashlib, json, pathlib, sys
operation, backup, workspace = sys.argv[1:]
backup, workspace = pathlib.Path(backup), pathlib.Path(workspace)
if operation == "capture":
    backup.mkdir()
    (backup / "records.json").write_text('[{"id":1,"revision":7}]')
    (backup / "capture.json").write_text(json.dumps({"version":1,"consistency":"quiesced", "semantic_sha256":hashlib.sha256((backup / "records.json").read_bytes()).hexdigest()}))
elif operation == "restore":
    (workspace / "restored.json").write_bytes((backup / "records.json").read_bytes())
elif operation == "verify":
    assert (workspace / "restored.json").read_bytes() == (backup / "records.json").read_bytes()
    print(json.dumps({"version":1,"status":"verified", "semantic_sha256":hashlib.sha256((workspace / "restored.json").read_bytes()).hexdigest()}))
elif operation == "cleanup":
    pass
elif operation == "fail":
    sys.exit(1)
''')
        def command(stage):
            return [sys.executable, str(self.adapter), stage, "{backup}", "{workspace}"]
        self.config = {"version": 1, "resource": "demo", "root": str(self.backups), "timeout_seconds": 10,
                       "commands": {stage: command(stage) for stage in ("capture", "restore", "verify", "cleanup")},
                       "executable_files": [str(self.adapter)], "maximum_age_seconds": 3600}

    def test_success_is_hash_verified_and_preserved_after_restore_failure(self):
        result = application_backup.capture(self.config, "good")
        self.assertEqual(result["status"], "verified")
        self.assertEqual(read_json(self.backups / "LAST_SUCCESS")["attempt"], "good")
        changed = json.loads(json.dumps(self.config))
        changed["commands"]["restore"][2] = "fail"
        with self.assertRaises(ValueError):
            application_backup.capture(changed, "failed")
        self.assertEqual(read_json(self.backups / "LAST_SUCCESS")["attempt"], "good")
        self.assertTrue((self.backups / "failed.partial").is_dir())
        with self.assertRaises(ValueError):
            application_backup.capture(self.config, "good")
        point = self.backups / "good"
        (point / "records.json").write_text('[{"id":1,"revision":6}]')
        with self.assertRaisesRegex(ValueError, "hash"):
            application_backup.inspect(self.config, point)

    def test_symlink_and_mutating_verifier_cannot_publish(self):
        original = self.adapter.read_text()
        for suffix in ("\nif operation == 'capture': (backup / 'redirect').symlink_to('/etc/passwd')\n",
                       "\nif operation == 'verify': (backup / 'records.json').write_text('[]')\n"):
            with self.subTest(suffix=suffix), self.assertRaises(ValueError):
                self.adapter.write_text(original + suffix)
                application_backup.capture(self.config, "bad" + str(len(suffix)))
        self.assertFalse((self.backups / "LAST_SUCCESS").exists())

    def test_missing_anchor_and_stale_success_are_rejected(self):
        application_backup.capture(self.config, "good")
        with self.assertRaisesRegex(ValueError, "stale"):
            application_backup.inspect(self.config, self.backups / "good", now=10**12)
        (self.backups / "lock").unlink()
        with self.assertRaises(FileNotFoundError):
            application_backup.capture(self.config, "another")

    def test_cleanup_failure_preserves_private_workspace_and_prior_success(self):
        application_backup.capture(self.config, "good")
        self.config["commands"]["cleanup"][2] = "fail"
        with self.assertRaises(ValueError):
            application_backup.capture(self.config, "incomplete")
        workspace = self.backups / "incomplete.restore-workspace"
        self.assertEqual(workspace.stat().st_mode & 0o777, 0o700)
        self.assertEqual(read_json(self.backups / "LAST_SUCCESS")["attempt"], "good")

    def test_local_execution_cannot_claim_independent_acceptance(self):
        application_backup.capture(self.config, "good")
        state = self.root / "certifier"
        state.mkdir(mode=0o700)
        (state / "lock").touch(mode=0o600)
        with self.assertRaisesRegex(ValueError, "different machine"):
            application_backup.certify(self.config, self.backups / "good", state)

    def test_interrupted_capture_retries_only_the_same_intent_and_retains_partial_evidence(self):
        execute = application_backup.execute
        def fail_restore(config, stage, *args):
            if stage == "restore":
                raise ValueError("interrupted restore")
            return execute(config, stage, *args)
        with mock.patch.object(application_backup, "execute", side_effect=fail_restore):
            with self.assertRaises(ValueError):
                application_backup.capture(self.config, "interrupted")
        changed = self.config | {"maximum_age_seconds": 7200}
        with self.assertRaisesRegex(ValueError, "intent changed"):
            application_backup.capture(changed, "interrupted", retry_incomplete=True)
        application_backup.capture(self.config, "interrupted", retry_incomplete=True)
        self.assertEqual(read_json(self.backups / "LAST_SUCCESS")["attempt"], "interrupted")
        retained = list(self.backups.glob("interrupted.abandoned-*"))
        self.assertEqual(len(retained), 1)
        self.assertTrue((retained[0] / "interrupted.partial/records.json").is_file())


if __name__ == "__main__":
    unittest.main()
