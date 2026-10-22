"""Borrowed-fence rejection must precede durable intent and writer disruption."""

import os
import pwd
from types import SimpleNamespace
import unittest
from unittest import mock

from harbor_db import application_transition, transition_manifest
from harbor_db.durable import write_json
import test_application_transition


class BorrowedFenceAdmissionTest(unittest.TestCase):
    # Reuse the retained fixture without changing or re-registering its oracle.
    setUp = test_application_transition.ApplicationTransitionTest.setUp

    def configure_postgres_fence(self, token):
        primary = self.root / "primary"
        (primary / "backups/locks").mkdir(parents=True)
        (primary / "evidence").mkdir()
        for anchor in [
            "writer-fence.lock",
            "backups/locks/mutate",
            "evidence/recovery.lock",
        ]:
            (primary / anchor).touch(mode=0o600)
        write_json(
            self.root / "postgres.json",
            {
                "state_dir": str(primary),
                "recovery": {
                    "require_writer_fence": True,
                    "backup_root": str(primary / "backups"),
                    "snapshot_file": str(primary / "evidence/snapshot.json"),
                },
            },
        )
        self.config["postgres_manifest"] = str(self.root / "postgres.json")
        original_account = pwd.getpwnam
        self.enterContext(
            mock.patch.object(
                pwd,
                "getpwnam",
                side_effect=lambda name: (
                    SimpleNamespace(pw_uid=os.getuid())
                    if name == "postgres"
                    else original_account(name)
                ),
            )
        )
        return self.enterContext(
            mock.patch.object(
                transition_manifest,
                "worker",
                return_value={"status": "ready", "token": token},
            )
        )

    def test_wrong_fence_plan_leaves_no_journal_and_allows_corrected_retry(self):
        token = "a" * 32
        self.configure_postgres_fence(token)
        journal = self.state / "transition.json"
        with self.assertRaisesRegex(ValueError, "live fence token differs"):
            application_transition.plan(self.config, self.candidate, "b" * 32)
        self.assertFalse(journal.exists())
        self.assertFalse((self.barrier / "inhibited.json").exists())
        application_transition.install_barriers.assert_not_called()
        application_transition.stop_units.assert_not_called()
        accepted = application_transition.plan(self.config, self.candidate, token)
        self.assertEqual(accepted["writer_fence_token"], token)
        self.assertEqual(accepted["fence"]["token"], token)

    def test_stale_fence_before_prepare_preserves_plan_and_running_writers(self):
        token = "a" * 32
        inspection = self.configure_postgres_fence(token)
        application_transition.plan(self.config, self.candidate, token)
        journal = self.state / "transition.json"
        before = journal.read_bytes()
        inspection.return_value = {"status": "ready", "token": "b" * 32}
        with self.assertRaisesRegex(ValueError, "live fence token differs"):
            application_transition.prepare(self.config)
        self.assertEqual(journal.read_bytes(), before)
        self.assertFalse((self.barrier / "inhibited.json").exists())
        application_transition.install_barriers.assert_not_called()
        application_transition.stop_units.assert_not_called()
