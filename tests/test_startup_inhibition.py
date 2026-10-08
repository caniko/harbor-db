"""Persistent service inhibition around offline PostgreSQL fence transitions."""

import fcntl
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from harbor_db import durable, postgres


class StartupInhibitionTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.config = {
            "resource": "fixture", "data_dir": str(self.root / "data"), "major": "18",
            "state_dir": str(self.root / "authority"), "package": "/postgres18",
            "startup_inhibition": {
                "state_dir": str(self.root / "inhibition"), "unit": "postgresql.service",
                "drop_in_root": str(self.root / "system.control"),
                "systemctl": "/systemctl", "busctl": "/busctl", "runuser": "/runuser", "adapter": "/adapter",
            },
        }
        self.gate = self.root / "inhibition" / "inhibited.json"
        self.drop_in = self.root / "system.control/postgresql.service.d/zzzz-harbor-db-startup-inhibition.conf"
        self.authority = self.root / "authority"
        self.authority.mkdir()
        self.fence_lock = self.authority / "writer-fence.lock"
        self.fence_lock.touch(mode=0o600)

    def helpers(self):
        from harbor_db import startup_inhibition
        self.enterContext(patch.object(startup_inhibition, "require_root"))
        self.enterContext(patch.object(startup_inhibition, "service_uid", return_value=os.geteuid()))
        self.enterContext(patch.object(postgres, "validate_config"))
        self.enterContext(patch.object(postgres, "inspect_cluster", return_value="12345"))
        inspect_ancestors = startup_inhibition.owned_ancestors

        def fixture_ancestors(path):
            # These tests model root inside a private fixture. Nix's /build is
            # writable by its unprivileged builder, outside that modeled root.
            # Keep every real permission check within the fixture, including the
            # hostile writable-root regression; the VM covers actual ancestors.
            inspect_ancestors(SimpleNamespace(parents=[
                parent for parent in path.parents
                if parent == self.root or parent.is_relative_to(self.root)
            ]))
        self.enterContext(patch.object(startup_inhibition, "owned_ancestors", side_effect=fixture_ancestors))

        def run(argv, **kwargs):
            if argv[0] == "/systemctl":
                output = str(self.drop_in) if "show" in argv else ""
            elif argv[0] == "/busctl":
                output = json.dumps({"type": "a(sbbsi)", "data": [["ConditionPathExists", False, True, str(self.gate), 0]]})
            else:
                output = json.dumps({
                    "status": "prepared-offline", "token": "a" * 32,
                    "resource": "fixture", "data_dir": self.config["data_dir"],
                    "major": "18", "system_identifier": "12345",
                })
            return subprocess.CompletedProcess(argv, 0, output, "")
        return self.enterContext(patch.object(postgres, "run", side_effect=run))

    def test_gate_precedes_reload_and_failed_reload_retains_durable_inhibition(self):
        from harbor_db import startup_inhibition
        runner = self.helpers()

        def fail(argv, **kwargs):
            self.assertTrue(self.gate.exists())
            self.assertIn(f"ConditionPathExists=!{self.gate}", self.drop_in.read_text())
            raise subprocess.CalledProcessError(1, argv)
        runner.side_effect = fail
        with self.assertRaises(subprocess.CalledProcessError):
            startup_inhibition.inhibit(self.config, "12345")
        record = json.loads(self.gate.read_text())
        self.assertEqual(record["system_identifier"], "12345")
        self.assertEqual(os.stat(self.gate).st_mode & 0o777, 0o600)

    def test_drop_in_is_durable_before_marker_publication(self):
        from harbor_db import startup_inhibition
        self.helpers()

        def interrupt(path, value):
            if Path(path) == self.gate:
                self.assertTrue(self.drop_in.exists())
                raise OSError("marker publication interrupted")
            durable.write_json(path, value)
        with patch.object(startup_inhibition, "write_json", side_effect=interrupt), self.assertRaisesRegex(OSError, "interrupted"):
            startup_inhibition.inhibit(self.config, "12345")
        self.assertFalse(self.gate.exists())
        self.assertTrue(self.drop_in.exists())
        self.assertEqual(startup_inhibition.inhibit(self.config, "12345")["status"], "startup-inhibited")

    def test_release_requires_bound_offline_boundary_and_keeps_receipt(self):
        from harbor_db import startup_inhibition
        runner = self.helpers()
        held = startup_inhibition.inhibit(self.config, "12345")
        with self.assertRaisesRegex(postgres.LifecycleError, "token"):
            startup_inhibition.release(self.config, self.root / "manifest", "0" * 32, "a" * 32, "prepared")
        runner.side_effect = subprocess.CalledProcessError(1, ["offline-boundary"])
        with self.assertRaises(subprocess.CalledProcessError):
            startup_inhibition.release(self.config, self.root / "manifest", held["token"], "a" * 32, "prepared")
        self.assertTrue(self.gate.exists())
        identifier = "99999"
        def observe(argv, **kwargs):
            if argv[0] == "/systemctl":
                return subprocess.CompletedProcess(argv, 0, str(self.drop_in), "")
            if argv[0] == "/busctl":
                return subprocess.CompletedProcess(argv, 0, json.dumps({"type": "a(sbbsi)", "data": [["ConditionPathExists", False, True, str(self.gate), 0]]}), "")
            return subprocess.CompletedProcess(argv, 0, json.dumps({
                "status": "prepared-offline", "token": "a" * 32, "resource": "fixture",
                "data_dir": self.config["data_dir"], "major": "18", "system_identifier": identifier,
            }), "")
        runner.side_effect = observe
        with self.assertRaisesRegex(postgres.LifecycleError, "boundary"):
            startup_inhibition.release(self.config, self.root / "manifest", held["token"], "a" * 32, "prepared")
        self.assertTrue(self.gate.exists())
        identifier = "12345"
        released = startup_inhibition.release(self.config, self.root / "manifest", held["token"], "a" * 32, "prepared")
        self.assertFalse(self.gate.exists())
        self.assertTrue(self.drop_in.exists())
        self.assertTrue(Path(released["receipt"]).exists())

    def test_loaded_drop_in_without_effective_condition_cannot_release(self):
        from harbor_db import startup_inhibition
        runner = self.helpers()
        held = startup_inhibition.inhibit(self.config, "12345")
        normal = runner.side_effect
        def reset_condition(argv, **kwargs):
            if argv[0] == "/busctl":
                return subprocess.CompletedProcess(argv, 0, '{"type":"a(sbbsi)","data":[]}', "")
            return normal(argv, **kwargs)
        runner.side_effect = reset_condition
        with self.assertRaisesRegex(postgres.LifecycleError, "effective.*condition"):
            startup_inhibition.release(self.config, self.root / "manifest", held["token"], "a" * 32, "prepared")
        self.assertTrue(self.gate.exists())

    def test_offline_verification_stays_pinned_through_release_publication(self):
        from harbor_db import startup_inhibition
        self.helpers()
        held = startup_inhibition.inhibit(self.config, "12345")

        def competing_transition(path, value):
            if str(path).endswith(".released.json"):
                # Once verification returned, another controller must still be
                # unable to change the selector before startup is released.
                with self.fence_lock.open("rb") as stream, self.assertRaises(BlockingIOError):
                    fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            durable.write_json(path, value)
        with patch.object(startup_inhibition, "write_json", side_effect=competing_transition):
            startup_inhibition.release(self.config, self.root / "manifest", held["token"], "a" * 32, "prepared")
        self.assertFalse(self.gate.exists())

    def test_missing_or_exclusively_held_fence_anchor_keeps_startup_inhibited(self):
        from harbor_db import startup_inhibition
        runner = self.helpers()
        held = startup_inhibition.inhibit(self.config, "12345")
        with self.fence_lock.open("rb") as stream:
            fcntl.flock(stream.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            runner.reset_mock()
            with self.assertRaises(BlockingIOError):
                startup_inhibition.release(self.config, self.root / "manifest", held["token"], "a" * 32, "prepared")
            runner.assert_not_called()
        self.fence_lock.unlink()
        with self.assertRaises(FileNotFoundError):
            startup_inhibition.release(self.config, self.root / "manifest", held["token"], "a" * 32, "prepared")
        self.assertTrue(self.gate.exists())
        self.assertFalse(self.fence_lock.exists())

    def test_foreign_drop_in_and_changed_policy_are_preserved(self):
        from harbor_db import startup_inhibition
        self.helpers()
        self.drop_in.parent.mkdir(parents=True)
        self.drop_in.write_text("foreign policy\n")
        with self.assertRaisesRegex(postgres.LifecycleError, "drop-in"):
            startup_inhibition.inhibit(self.config, "12345")
        self.assertEqual(self.drop_in.read_text(), "foreign policy\n")
        self.assertFalse(self.gate.exists())
        self.drop_in.unlink()
        held = startup_inhibition.inhibit(self.config, "12345")
        self.config["startup_inhibition"]["unit"] = "another.service"
        with self.assertRaisesRegex(postgres.LifecycleError, "binding"):
            startup_inhibition.release(self.config, self.root / "manifest", held["token"], "a" * 32, "prepared")
        self.assertTrue(self.gate.exists())

    def test_writable_ancestor_cannot_host_a_root_startup_barrier(self):
        from harbor_db import startup_inhibition
        self.helpers()
        self.root.chmod(0o777)
        with self.assertRaisesRegex(postgres.LifecycleError, "untrusted ancestor"):
            startup_inhibition.inhibit(self.config, "12345")
        self.assertFalse(self.gate.exists())
