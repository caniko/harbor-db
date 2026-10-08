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
            "recovery": {"require_writer_fence": True, "snapshot_file": str(self.snapshot)},
        }
        self.record = {"token": "a" * 32, "system_identifier": "12345"}

    def test_missing_fence_cannot_authorize_bootstrap_capture(self):
        self.anchor.unlink()
        with self.assertRaises(OSError), writer_fence.admission(self.config, "/run/postgresql", 5432, capture=True):
            self.fail("missing fence authorized capture")
        self.assertFalse(self.anchor.exists())

    def test_offline_preparation_is_not_live_writer_exclusion(self):
        with patch.object(writer_fence, "startup", return_value=self.record), \
                patch.object(writer_fence, "inspect_live", side_effect=postgres.LifecycleError("primary not restarted")), \
                self.assertRaisesRegex(postgres.LifecycleError, "not restarted"), \
                writer_fence.admission(self.config, "/run/postgresql", 5432, capture=True):
            self.fail("offline preparation authorized capture")

    def test_snapshot_from_another_epoch_cannot_authorize_adoption(self):
        write_json(self.snapshot, {"writer_fence_token": "b" * 32})
        with patch.object(writer_fence, "startup", return_value=self.record), \
                patch.object(writer_fence, "inspect_live", return_value={"status": "ready"}), \
                self.assertRaisesRegex(postgres.LifecycleError, "snapshot"), \
                writer_fence.admission(self.config, "/run/postgresql", 5432, snapshot=True):
            self.fail("another fence snapshot authorized adoption")

    def test_admitted_window_prevents_explicit_offline_thaw(self):
        write_json(self.snapshot, {"writer_fence_token": self.record["token"]})
        with patch.object(writer_fence, "startup", return_value=self.record), \
                patch.object(writer_fence, "inspect_live", return_value={"status": "ready"}), \
                writer_fence.admission(self.config, "/run/postgresql", 5432, snapshot=True) as admitted:
            self.assertEqual(admitted["token"], self.record["token"])
            with self.assertRaises(BlockingIOError), lock(self.anchor):
                self.fail("thaw entered an accepted recovery window")
        # Leaving the window releases only its shared descriptor, not the HBA.
        with lock(self.anchor):
            self.assertTrue(self.anchor.exists())

    def test_retirement_does_not_remove_or_ignore_an_existing_fence(self):
        self.config["recovery"]["require_writer_fence"] = False
        writer_fence.marker(self.config).write_text("retained journal")
        with patch.object(writer_fence, "startup", side_effect=postgres.LifecycleError("unfinished thaw")), \
                self.assertRaisesRegex(postgres.LifecycleError, "unfinished"), \
                writer_fence.admission(self.config, "/run/postgresql", 5432):
            self.fail("retirement discarded unfinished writer exclusion")
        self.assertTrue(writer_fence.marker(self.config).exists())

    def test_snapshot_publishes_the_admitted_epoch_and_never_recaptures_on_failure(self):
        settings = {"backup_root": str(self.root / "backup"), "snapshot_file": str(self.snapshot),
                    "receipt_file": str(self.root / "receipt.json"), "max_age_seconds": 3600,
                    "record_checks": [{"name": "record", "database": "app", "sql": "SELECT 1"}]}
        backup = Path(settings["backup_root"])
        (backup / "locks").mkdir(parents=True)
        (backup / "locks/mutate").touch()
        self.config["recovery"] = settings | {"require_writer_fence": True, "system_identifier": "12345"}
        entered = []

        @contextlib.contextmanager
        def admitted(*args, **kwargs):
            entered.append(kwargs)
            yield self.record

        with patch.object(writer_fence, "admission", side_effect=admitted), \
                patch.object(recovery, "backup", return_value=({}, {})), \
                patch.object(recovery, "verify_backup"), patch.object(postgres, "inspect_live"), \
                patch.object(recovery, "records", return_value={"record": "sha256"}):
            recovery.snapshot(self.config, "/run/postgresql", 5432, now=100)
        self.assertEqual(entered, [{"capture": True}])
        self.assertEqual(json.loads(self.snapshot.read_text())["writer_fence_token"], self.record["token"])
        retained = self.snapshot.read_bytes()
        with patch.object(writer_fence, "admission", side_effect=postgres.LifecycleError("fence unavailable")), \
                self.assertRaisesRegex(postgres.LifecycleError, "fence unavailable"):
            recovery.snapshot(self.config, "/run/postgresql", 5432, now=101)
        self.assertEqual(self.snapshot.read_bytes(), retained)
