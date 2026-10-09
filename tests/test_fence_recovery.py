"""Recovery and deployment must share one verified exclusion epoch."""

import contextlib
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from harbor_db import postgres, recovery, writer_fence
from harbor_db.durable import lock, write_json


class FenceRecoveryTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.state = self.root / "authority"
        self.state.mkdir()
        self.anchor = self.state / "writer-fence.lock"
        self.anchor.touch(mode=0o600)
        self.snapshot = self.root / "snapshot.json"
        self.config = {
            "resource": "fixture", "state_dir": str(self.state),
            "data_dir": str(self.root / "primary"), "package": "/postgres18", "major": "18",
            "recovery": {
                "require_writer_fence": True, "system_identifier": "12345",
                "snapshot_file": str(self.snapshot), "receipt_file": str(self.root / "receipt.json"),
                "backup_root": str(self.root / "backup"), "max_age_seconds": 3600,
                "record_checks": [{"name": "record", "database": "app", "sql": "SELECT 1"}],
            },
        }
        self.record = {"token": "a" * 32, "system_identifier": "12345", "hba_sha256": "c" * 64}
        (self.root / "backup/locks").mkdir(parents=True)
        (self.root / "backup/locks/mutate").touch(mode=0o600)
        (self.root / "recovery.lock").touch(mode=0o600)
        self.enterContext(patch.object(recovery, "backup", return_value=({}, {"recovery_target_lsn": "0/200"})))
        self.enterContext(patch.object(recovery, "verify_backup"))

    def publish_evidence(self, fence_token):
        settings = self.config["recovery"]
        source = {"version": 1, "completed_at": 100, "recovery_target_lsn": "0/200",
                  "record_contract_sha256": recovery.contract(settings),
                  "records": {"record": "c" * 64}, "writer_fence_token": fence_token}
        write_json(self.snapshot, source)
        receipt = source | {"status": "ready", "snapshot_sha256": recovery.digest(self.snapshot),
                            "restored_data_dir": str(self.root / "restored"), "replay_lsn": "0/200"}
        write_json(Path(settings["receipt_file"]), receipt)

    def test_missing_fence_cannot_authorize_bootstrap_capture(self):
        self.anchor.unlink()
        with self.assertRaisesRegex(postgres.LifecycleError, "writer fence"), recovery.writer_exclusion(self.config, "/run/postgresql", 5432):
            self.fail("missing fence authorized capture")
        self.assertFalse(self.anchor.exists())

    def test_offline_preparation_is_not_live_writer_exclusion(self):
        with patch.object(writer_fence, "startup", return_value=self.record), \
                patch.object(writer_fence, "inspect_live", side_effect=postgres.LifecycleError("primary not restarted")), \
                self.assertRaisesRegex(postgres.LifecycleError, "not restarted"), \
                recovery.writer_exclusion(self.config, "/run/postgresql", 5432):
            self.fail("offline preparation authorized capture")

    def test_snapshot_from_another_epoch_cannot_authorize_adoption(self):
        self.publish_evidence("b" * 32)
        with patch.object(writer_fence, "startup", return_value=self.record), \
                patch.object(writer_fence, "inspect_live", return_value={"status": "ready"}), \
                self.assertRaisesRegex(ValueError, "snapshot"), \
                recovery.admission(self.config, socket_dir="/run/postgresql", port=5432, now=100):
            self.fail("another fence snapshot authorized adoption")

    def test_admitted_window_prevents_explicit_offline_thaw(self):
        self.publish_evidence(self.record["token"])
        with patch.object(writer_fence, "startup", return_value=self.record), \
                patch.object(writer_fence, "inspect_live", return_value={"status": "ready"}), \
                recovery.admission(self.config, socket_dir="/run/postgresql", port=5432, now=100) as admitted:
            self.assertEqual(admitted["status"], "ready")
            with self.assertRaises(BlockingIOError), lock(self.anchor):
                self.fail("thaw entered an accepted recovery window")
        # Leaving the window releases only its shared descriptor, not the HBA.
        with lock(self.anchor):
            self.assertTrue(self.anchor.exists())

    def test_live_recovery_inspection_rejects_records_changed_after_snapshot(self):
        self.publish_evidence(self.record["token"])
        with patch.object(writer_fence, "startup", return_value=self.record), \
                patch.object(writer_fence, "inspect_live", return_value={"status": "ready"}), \
                patch.object(recovery, "records", return_value={"record": "c" * 64}):
            result = recovery.live_check(self.config, "/run/postgresql", 5432, now=100)
            self.assertEqual(result["snapshot_sha256"], recovery.digest(self.snapshot))
        with patch.object(writer_fence, "startup", return_value=self.record), \
                patch.object(writer_fence, "inspect_live", return_value={"status": "ready"}), \
                patch.object(recovery, "records", return_value={"record": "d" * 64}), \
                self.assertRaisesRegex(ValueError, "live primary records differ"):
            recovery.live_check(self.config, "/run/postgresql", 5432, now=100)

    def test_retirement_does_not_remove_or_ignore_an_existing_fence(self):
        self.config["recovery"]["require_writer_fence"] = False
        writer_fence.marker(self.config).write_text("retained journal")
        with patch.object(writer_fence, "startup", side_effect=postgres.LifecycleError("unfinished thaw")), \
                self.assertRaisesRegex(postgres.LifecycleError, "unfinished"), \
                recovery.writer_exclusion(self.config, "/run/postgresql", 5432):
            self.fail("retirement discarded unfinished writer exclusion")
        self.assertTrue(writer_fence.marker(self.config).exists())

    def test_snapshot_publishes_the_admitted_epoch_and_never_recaptures_on_failure(self):
        settings = {"backup_root": str(self.root / "backup"), "snapshot_file": str(self.snapshot),
                    "receipt_file": str(self.root / "receipt.json"), "max_age_seconds": 3600,
                    "record_checks": [{"name": "record", "database": "app", "sql": "SELECT 1"}]}
        self.config["recovery"] = settings | {"require_writer_fence": True, "system_identifier": "12345"}
        entered = []

        @contextlib.contextmanager
        def admitted(*args, **kwargs):
            entered.append((args, kwargs))
            yield self.record

        with patch.object(recovery, "writer_exclusion", side_effect=admitted), \
                patch.object(recovery, "backup", return_value=({}, {})), \
                patch.object(recovery, "verify_backup"), patch.object(postgres, "inspect_live"), \
                patch.object(recovery, "records", return_value={"record": "sha256"}):
            recovery.snapshot(self.config, "/run/postgresql", 5432, now=100)
        self.assertEqual(entered, [((self.config, "/run/postgresql", 5432), {})])
        self.assertEqual(json.loads(self.snapshot.read_text())["writer_fence_token"], self.record["token"])
        retained = self.snapshot.read_bytes()
        with patch.object(recovery, "writer_exclusion", side_effect=postgres.LifecycleError("fence unavailable")), \
                self.assertRaisesRegex(postgres.LifecycleError, "fence unavailable"):
            recovery.snapshot(self.config, "/run/postgresql", 5432, now=101)
        self.assertEqual(self.snapshot.read_bytes(), retained)
