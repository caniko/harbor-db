"""Regression cases for cluster selection and interrupted publication."""

import json
import os
import select
import signal
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from harbor_db import postgres
from harbor_db.durable import lock, write_json


class ClusterLifecycleTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.data = self.root / "18"
        self.data.mkdir()
        (self.data / "PG_VERSION").write_text("18\n")
        self.state = self.root / "state"
        self.state.mkdir()
        self.config = {
            "resource": "test",
            "data_dir": str(self.data),
            "major": "18",
            "package": "/postgres18",
            "state_dir": str(self.state),
            "required_mounts": [],
        }
        self.probe = patch.object(postgres, "inspect_cluster", return_value="12345")
        self.probe.start()
        self.addCleanup(self.probe.stop)

    def adopt(self):
        postgres.adopt(self.config, "12345")

    def test_pg_version_alone_never_authorizes_start(self):
        with self.assertRaisesRegex(postgres.LifecycleError, "not adopted"):
            postgres.check(self.config)
        self.assertFalse((self.state / "identity.json").exists())

    def test_adoption_is_idempotent_and_rejects_replacement(self):
        self.adopt()
        self.adopt()
        postgres.check(self.config)
        with patch.object(postgres, "inspect_cluster", return_value="67890"):
            with self.assertRaisesRegex(postgres.LifecycleError, "identity mismatch"):
                postgres.check(self.config)
            with self.assertRaises(postgres.LifecycleError):
                postgres.adopt(self.config, "67890")

    def test_missing_data_does_not_initialize_replacement(self):
        self.adopt()
        (self.data / "PG_VERSION").unlink()
        self.probe.stop()
        with self.assertRaises(postgres.LifecycleError):
            postgres.check(self.config)
        self.assertFalse((self.data / "PG_VERSION").exists())

    def test_rollback_cannot_select_old_directory(self):
        self.adopt()
        rollback = dict(self.config, data_dir=str(self.root / "17"), major="17")
        with self.assertRaisesRegex(postgres.LifecycleError, "identity mismatch"):
            postgres.check(rollback)

    def test_interrupted_upgrade_blocks_even_adopted_cluster(self):
        self.adopt()
        (self.state / "upgrade.json").write_text(json.dumps({"phase": "building"}))
        with self.assertRaisesRegex(postgres.LifecycleError, "upgrade"):
            postgres.check(self.config)

    def test_missing_mount_fails_before_probe_or_adoption(self):
        self.config["required_mounts"] = [str(self.root / "missing-mount")]
        with self.assertRaisesRegex(postgres.LifecycleError, "mount"):
            self.adopt()
        self.assertFalse((self.state / "identity.json").exists())

    def test_adoption_requires_independently_supplied_identifier(self):
        with self.assertRaisesRegex(postgres.LifecycleError, "identifier"):
            postgres.adopt(self.config, "99999")
        self.assertFalse((self.state / "identity.json").exists())

    def test_adoption_cannot_replace_a_live_writers_unlinked_lock(self):
        self.adopt()
        with lock(self.state / "lock", shared=True):
            (self.state / "lock").unlink()
            with self.assertRaises(FileNotFoundError):
                self.adopt()
            self.assertFalse((self.state / "lock").exists())

    def test_interrupted_first_adoption_reuses_the_original_lock(self):
        with (
            patch.object(
                postgres, "write_json", side_effect=OSError("publication interrupted")
            ),
            self.assertRaisesRegex(OSError, "interrupted"),
        ):
            self.adopt()
        inode = (self.state / "lock").stat().st_ino
        self.assertFalse((self.state / "identity.json").exists())
        self.adopt()
        self.assertEqual((self.state / "lock").stat().st_ino, inode)
        postgres.check(self.config)

    def live_probe(self, **changes):
        observed = {
            "data_dir": str(self.data),
            "major": "18",
            "system_identifier": "12345",
            "fsync": "on",
            "full_page_writes": "on",
            "synchronous_commit": "on",
            "in_recovery": False,
        }
        observed.update(changes)
        return subprocess.CompletedProcess([], 0, json.dumps(observed), "")

    def test_live_inspection_is_read_only_and_ignores_ambient_routing(self):
        with (
            patch.object(postgres, "run", return_value=self.live_probe()) as probe,
            patch.dict(
                os.environ,
                {
                    "PGHOSTADDR": "192.0.2.1",
                    "PGSERVICE": "wrong",
                    "PGOPTIONS": "-c synchronous_commit=off",
                },
            ),
        ):
            observed = postgres.inspect_live(
                self.config, "12345", "/run/postgresql", 5432
            )
        self.assertEqual(observed["system_identifier"], "12345")
        self.assertFalse((self.state / "identity.json").exists())
        self.assertFalse((self.state / "lock").exists())
        argv = [str(arg) for arg in probe.call_args.args[0]]
        self.assertIn("--no-psqlrc", argv)
        self.assertIn("--no-password", argv)
        self.assertIn("--host=/run/postgresql", argv)
        self.assertIn("--port=5432", argv)
        self.assertIn("--username=postgres", argv)
        self.assertNotIn("PGHOSTADDR", probe.call_args.kwargs["env"])
        self.assertNotIn("PGSERVICE", probe.call_args.kwargs["env"])
        self.assertNotIn("PGOPTIONS", probe.call_args.kwargs["env"])

    def test_live_adoption_rejects_wrong_endpoint_or_nondurable_primary(self):
        for changed in [
            {"data_dir": str(self.root / "other")},
            {"major": "17"},
            {"system_identifier": "67890"},
            {"fsync": "off"},
            {"full_page_writes": "off"},
            {"synchronous_commit": "off"},
            {"in_recovery": True},
        ]:
            with self.subTest(changed=changed):
                with (
                    patch.object(
                        postgres, "run", return_value=self.live_probe(**changed)
                    ),
                    self.assertRaises(postgres.LifecycleError),
                ):
                    postgres.adopt_live(self.config, "12345", "/run/postgresql", 5432)
                self.assertFalse((self.state / "identity.json").exists())

    def test_live_and_physical_identifiers_must_match(self):
        with (
            patch.object(postgres, "run", return_value=self.live_probe()),
            patch.object(
                postgres,
                "inspect_cluster",
                return_value="67890",
            ),
            self.assertRaises(postgres.LifecycleError),
        ):
            postgres.adopt_live(self.config, "12345", "/run/postgresql", 5432)
        self.assertFalse((self.state / "identity.json").exists())

    def test_live_adoption_and_guarded_switch_are_idempotent(self):
        with patch.object(postgres, "run", return_value=self.live_probe()):
            result = postgres.adopt_live(self.config, "12345", "/run/postgresql", 5432)
            self.assertTrue(result["changed"])
            original = (self.state / "identity.json").read_bytes()
            # A running guarded primary holds a shared authority lease. A normal
            # later switch verifies it without acquiring an exclusive adoption lock.
            with lock(self.state / "lock", shared=True):
                result = postgres.adopt_live(
                    self.config, "12345", "/run/postgresql", 5432
                )
            self.assertFalse(result["changed"])
            self.assertEqual((self.state / "identity.json").read_bytes(), original)
        postgres.check(self.config)

    def test_required_recovery_fails_before_any_adoption_state_is_created(self):
        self.config["recovery"] = {}
        for operation in (
            lambda: postgres.adopt(self.config, "12345"),
            lambda: postgres.adopt_live(self.config, "12345", "/run/postgresql", 5432),
        ):
            with (
                patch(
                    "harbor_db.recovery.admission",
                    side_effect=ValueError("missing recovery acceptance"),
                ),
                self.assertRaisesRegex(ValueError, "missing recovery"),
            ):
                operation()
            self.assertFalse((self.state / "identity.json").exists())
            self.assertFalse((self.state / "lock").exists())

    def test_live_adoption_does_not_clear_interrupted_upgrade(self):
        journal = self.state / "upgrade.json"
        journal.write_text('{"phase":"building"}')
        original = journal.read_bytes()
        with (
            patch.object(postgres, "run") as probe,
            self.assertRaisesRegex(postgres.LifecycleError, "upgrade"),
        ):
            postgres.adopt_live(self.config, "12345", "/run/postgresql", 5432)
        probe.assert_not_called()
        self.assertEqual(journal.read_bytes(), original)
        self.assertFalse((self.state / "identity.json").exists())

    def test_existing_live_adoption_does_not_recreate_a_missing_lock(self):
        self.adopt()
        (self.state / "lock").unlink()
        with (
            patch.object(postgres, "run") as probe,
            self.assertRaises(FileNotFoundError),
        ):
            postgres.adopt_live(self.config, "12345", "/run/postgresql", 5432)
        probe.assert_not_called()
        self.assertFalse((self.state / "lock").exists())

    def test_authority_removed_before_shared_lock_is_not_readopted(self):
        self.adopt()

        def remove_authority(*args, **kwargs):
            (self.state / "identity.json").unlink()
            return lock(*args, **kwargs)

        with (
            patch.object(postgres, "lock", side_effect=remove_authority),
            patch.object(
                postgres,
                "run",
            ) as probe,
            self.assertRaisesRegex(postgres.LifecycleError, "not adopted"),
        ):
            postgres.adopt_live(self.config, "12345", "/run/postgresql", 5432)
        probe.assert_not_called()
        self.assertFalse((self.state / "identity.json").exists())

    def test_live_inspection_rejects_remote_or_invalid_socket_endpoints(self):
        for socket, port, identifier in [
            ("localhost", 5432, "12345"),
            ("/run/postgresql,192.0.2.1", 5432, "12345"),
            ("/run/postgresql", 0, "12345"),
            ("/run/postgresql", 65536, "12345"),
            ("/run/postgresql", 5432, ""),
        ]:
            with (
                patch.object(postgres, "run") as probe,
                self.assertRaises(postgres.LifecycleError),
            ):
                postgres.inspect_live(self.config, identifier, socket, port)
            probe.assert_not_called()

    def test_writer_is_the_guarded_process_and_retains_lease_across_exec(self):
        self.adopt()
        package = self.root / "package"
        (package / "bin").mkdir(parents=True)
        executable = package / "bin/postgres"
        executable.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "print(json.dumps({'pid': os.getpid(), 'args': sys.argv[1:]}), flush=True)\n"
            "sys.stdin.buffer.read(1)\n"
        )
        executable.chmod(0o700)
        config = dict(self.config, package=str(package))
        # The real exec/descriptor path is exercised in a separate process;
        # only the pg_controldata probe uses this test's synthetic cluster.
        command = (
            "from harbor_db import postgres; "
            "postgres.inspect_cluster = lambda *args: '12345'; "
            f"postgres.serve({config!r})"
        )
        process = subprocess.Popen(
            [sys.executable, "-B", "-c", command],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
            env={**os.environ, "PGDATA": str(self.root / "wrong-storage")},
        )
        try:
            self.assertTrue(select.select([process.stdout], [], [], 10)[0])
            ready = json.loads(process.stdout.readline())
            self.assertEqual(ready["pid"], process.pid)
            self.assertEqual(
                ready["args"],
                [
                    "-D",
                    str(self.data),
                    "-c",
                    f"data_directory={self.data}",
                    "-c",
                    "fsync=on",
                    "-c",
                    "full_page_writes=on",
                    "-c",
                    "synchronous_commit=on",
                ],
            )
            with self.assertRaises(BlockingIOError), lock(self.state / "lock"):
                self.fail("offline operation acquired a live writer's lease")
            _, error = process.communicate(input=b"x", timeout=10)
            self.assertEqual(process.returncode, 0, error.decode())
            with lock(self.state / "lock"):
                pass
        finally:
            # Also cleans up a surviving child if the old supervisor path fails.
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=10)

    def ready_upgrade(self):
        self.data.rename(self.root / "17")
        self.old = self.root / "17"
        (self.old / "PG_VERSION").write_text("17\n")
        (self.old / "pg_tblspc").mkdir()
        old_package, new_package = self.root / "postgres17", self.root / "postgres18"
        for package in (old_package, new_package):
            (package / "bin").mkdir(parents=True)
            for name in ("pg_ctl", "pg_controldata", "initdb", "pg_upgrade"):
                (package / "bin" / name).write_text("fixture executable\n")
        validator = self.root / "validate"
        validator.write_text("fixture validator\n")
        self.config["package"] = str(new_package)
        self.config["upgrade"] = {
            "data_dir": str(self.old),
            "major": "17",
            "package": str(old_package),
            "validate_command": [str(validator)],
        }
        source = dict(
            self.config, data_dir=str(self.old), major="17", package=str(old_package)
        )
        postgres.adopt(source, "12345")
        stage = self.root / "18.harbor-staging"
        stage.mkdir()
        (stage / "PG_VERSION").write_text("18\n")
        journal = {
            "version": 2,
            "phase": "ready",
            "source": postgres.identity(source, "12345"),
            "source_control": "digest",
            "target": {
                k: self.config[k] for k in ("resource", "data_dir", "major", "package")
            },
            "staging": str(stage),
            "identity": postgres.identity(self.config, "12345"),
            "source_copy": str(self.root / "18.harbor-source"),
            "contract": postgres.upgrade_contract(self.config),
        }
        write_json(self.state / "upgrade.json", journal)
        return stage, journal

    def test_resume_after_rename_and_identity_write(self):
        stage, journal = self.ready_upgrade()
        stage.rename(self.data)
        write_json(self.state / "identity.json", journal["identity"])
        with (
            patch.object(postgres, "require_stopped"),
            patch.object(postgres, "control_digest", return_value="digest"),
        ):
            postgres.upgrade(self.config)
        postgres.check(self.config)
        self.assertTrue(self.old.exists())
        self.assertFalse((self.state / "upgrade.json").exists())

    def test_changed_source_prevents_publication(self):
        stage, _ = self.ready_upgrade()
        with (
            patch.object(postgres, "require_stopped"),
            patch.object(
                postgres,
                "control_digest",
                return_value="changed",
            ),
            self.assertRaisesRegex(
                postgres.LifecycleError, "source or contract changed"
            ),
        ):
            postgres.upgrade(self.config)
        self.assertTrue(stage.exists())
        self.assertFalse(self.data.exists())

    def test_upgrade_and_ready_resume_require_the_existing_lock_anchor(self):
        stage, journal = self.ready_upgrade()
        for resume in (False, True):
            with self.subTest(resume=resume):
                if not resume:
                    (self.state / "upgrade.json").unlink()
                with lock(self.state / "lock", shared=True):
                    (self.state / "lock").unlink()
                    with (
                        patch.object(postgres, "require_stopped") as stopped,
                        self.assertRaises(FileNotFoundError),
                    ):
                        postgres.upgrade(self.config)
                    stopped.assert_not_called()
                    self.assertFalse((self.state / "lock").exists())
                    self.assertTrue(stage.exists())
                    self.assertFalse(self.data.exists())
                if not resume:
                    # Rebuild the fixture for the journal-resumption case.
                    with lock(self.state / "lock", create=True):
                        write_json(self.state / "upgrade.json", journal)

    def interrupt_authority_publication(self, filename):
        _, journal = self.ready_upgrade()
        real_write = postgres.write_json

        def interrupt(path, value):
            real_write(path, value)
            if path.name == filename:
                raise OSError("publication interrupted")

        with (
            patch.object(postgres, "require_stopped"),
            patch.object(
                postgres,
                "write_json",
                side_effect=interrupt,
            ),
            self.assertRaisesRegex(OSError, "publication interrupted"),
        ):
            postgres.publish(self.config, journal)
        self.assertTrue((self.state / "upgrade.json").exists())
        with self.assertRaisesRegex(postgres.LifecycleError, "upgrade"):
            postgres.check(self.config)
        with (
            patch.object(postgres, "require_stopped"),
            patch.object(postgres, "control_digest", return_value="digest"),
        ):
            postgres.upgrade(self.config)
        postgres.check(self.config)
        self.assertEqual(
            json.loads((self.state / "previous-identity.json").read_text()),
            journal["source"],
        )

    def test_interruption_after_previous_identity_publication_can_resume(self):
        self.interrupt_authority_publication("previous-identity.json")

    def test_interruption_after_current_identity_publication_can_resume(self):
        self.interrupt_authority_publication("identity.json")

    def test_resumed_publication_flush_failure_keeps_upgrade_barrier(self):
        stage, journal = self.ready_upgrade()
        stage.rename(self.data)
        real_sync = postgres.sync_directory

        def fail_destination_parent(path):
            if path == self.data.parent:
                raise OSError("destination publication is not durable")
            real_sync(path)

        with (
            patch.object(postgres, "require_stopped"),
            patch.object(
                postgres,
                "sync_directory",
                side_effect=fail_destination_parent,
            ),
            self.assertRaisesRegex(OSError, "publication is not durable"),
        ):
            postgres.publish(self.config, journal)
        self.assertTrue((self.state / "upgrade.json").exists())
        self.assertEqual(
            json.loads((self.state / "identity.json").read_text()), journal["source"]
        )

    def test_interruption_after_initdb_cannot_authorize_target(self):
        stage, journal = self.ready_upgrade()
        journal["phase"] = "building"
        del journal["identity"]
        write_json(self.state / "upgrade.json", journal)
        with (
            patch.object(postgres, "require_stopped"),
            patch.object(
                postgres,
                "control_digest",
                return_value="digest",
            ),
            self.assertRaisesRegex(postgres.LifecycleError, "incomplete upgrade"),
        ):
            postgres.upgrade(self.config)
        with self.assertRaises(postgres.LifecycleError):
            postgres.check(self.config)
        self.assertTrue((stage / "PG_VERSION").exists())
        self.assertFalse(self.data.exists())


if __name__ == "__main__":
    unittest.main()
