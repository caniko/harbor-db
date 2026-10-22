"""Offline fence publication, restart selection and explicit thaw regressions."""

import json
import os
import pwd
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from harbor_db import postgres, recovery, writer_fence
from harbor_db.durable import lock


class WriterFenceTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.data = self.root / "data"
        self.state = self.root / "state"
        self.data.mkdir()
        self.state.mkdir()
        self.auto = self.data / "postgresql.auto.conf"
        self.original = b"# Preserve unrelated settings\nwork_mem = '16MB'\n"
        self.auto.write_bytes(self.original)
        self.config = {
            "resource": "fixture", "data_dir": str(self.data),
            "state_dir": str(self.state), "package": "/postgres18", "major": "18",
            "writer_fence": {"replication_roles": ["replicator"]},
        }
        for name, value in [("validate_config", None), ("reject_upgrade", None),
                            ("require_stopped", None), ("inspect_cluster", "12345")]:
            probe = patch.object(postgres, name, return_value=value)
            probe.start()
            self.addCleanup(probe.stop)

    def open(self):
        return writer_fence.open_fence(self.config, "12345")

    def test_open_never_claims_live_ready_and_retains_original_configuration(self):
        result = self.open()
        self.assertEqual(result["status"], "prepared-offline")
        self.assertTrue(result["restart_required"])
        active = writer_fence.startup(self.config)
        self.assertEqual(active["system_identifier"], "12345")
        hba = Path(active["hba_file"]).read_text()
        self.assertIn('local all "postgres" peer', hba)
        self.assertIn('host replication "replicator" 127.0.0.1/32 scram-sha-256', hba)
        self.assertIn("host all all 0.0.0.0/0 reject", hba)
        self.assertEqual(Path(active["original_file"]).read_bytes(), self.original)
        self.assertEqual(os.stat(active["original_file"]).st_mode & 0o777, 0o600)

    def test_hba_reserved_role_names_are_literal_roles(self):
        self.config["writer_fence"] = {"control_role": "all", "replication_roles": ["all"]}
        self.open()
        hba = Path(writer_fence.startup(self.config)["hba_file"]).read_text()
        self.assertIn('local all "all" peer', hba)
        self.assertIn('host replication "all" 127.0.0.1/32 scram-sha-256', hba)
        self.assertNotIn("local all all peer", hba)

    def test_running_primary_or_wrong_identifier_cannot_publish_fence(self):
        with patch.object(postgres, "require_stopped", side_effect=postgres.LifecycleError("running")), self.assertRaisesRegex(postgres.LifecycleError, "running"):
            self.open()
        with self.assertRaisesRegex(postgres.LifecycleError, "identifier"):
            writer_fence.open_fence(self.config, "99999")
        self.assertEqual(self.auto.read_bytes(), self.original)
        self.assertFalse((self.state / "writer-fence.json").exists())

    def test_interrupted_selector_publication_blocks_guarded_start_and_can_resume(self):
        from harbor_db import durable
        def interrupt(path, data):
            if Path(path) == self.auto:
                durable.atomic_write(path, data)
                raise OSError("selector interrupted")
            return durable.atomic_write(path, data)
        with patch.object(writer_fence, "atomic_write", side_effect=interrupt), self.assertRaisesRegex(OSError, "selector interrupted"):
            self.open()
        with self.assertRaisesRegex(postgres.LifecycleError, "selector"):
            writer_fence.startup(self.config)
        result = self.open()
        self.assertEqual(result["status"], "prepared-offline")
        self.assertIsNotNone(writer_fence.startup(self.config))

    def test_first_history_parent_is_durable_before_selecting_its_hba(self):
        from harbor_db import durable
        flushed = []
        sync = durable.sync_directory

        def flush(path):
            flushed.append(Path(path))
            sync(path)

        def publish(path, data):
            if Path(path) == self.auto:
                self.assertIn(self.state, flushed)
            durable.atomic_write(path, data)
        with patch.object(writer_fence, "sync_directory", side_effect=flush), patch.object(durable, "sync_directory", side_effect=flush), patch.object(writer_fence, "atomic_write", side_effect=publish):
            self.open()

    def test_guarded_start_rejects_missing_hba_or_replacement_cluster(self):
        self.open()
        active = writer_fence.startup(self.config)
        with patch.object(postgres, "inspect_cluster", return_value="67890"), self.assertRaisesRegex(postgres.LifecycleError, "identifier"):
            writer_fence.startup(self.config)
        Path(active["hba_file"]).unlink()
        with self.assertRaises(OSError):
            writer_fence.startup(self.config)

    def test_thaw_is_stopped_token_bound_and_preserves_foreign_configuration(self):
        token = self.open()["token"]
        with patch.object(postgres, "require_stopped", side_effect=postgres.LifecycleError("running")), self.assertRaisesRegex(postgres.LifecycleError, "running"):
            writer_fence.close_fence(self.config, token)
        with self.assertRaisesRegex(postgres.LifecycleError, "token"):
            writer_fence.close_fence(self.config, "0" * 32)
        self.auto.write_bytes(self.auto.read_bytes() + b"statement_timeout = '10s'\n")
        foreign = self.auto.read_bytes()
        with self.assertRaisesRegex(postgres.LifecycleError, "changed"):
            writer_fence.close_fence(self.config, token)
        self.assertEqual(self.auto.read_bytes(), foreign)

    def test_thaw_restores_bytes_and_retains_history(self):
        result = self.open()
        active = writer_fence.startup(self.config)
        closed = writer_fence.close_fence(self.config, result["token"])
        self.assertEqual(closed["status"], "closed-offline")
        self.assertEqual(self.auto.read_bytes(), self.original)
        self.assertIsNone(writer_fence.startup(self.config))
        self.assertTrue(Path(active["hba_file"]).exists())
        self.assertTrue(Path(closed["receipt"]).exists())

    def test_pending_thaw_never_starts_and_can_resume(self):
        result = self.open()
        from harbor_db import durable
        def interrupt(path, value):
            if Path(path).name == "closed.json":
                raise OSError("thaw interrupted")
            durable.write_json(path, value)
        with patch.object(writer_fence, "write_json", side_effect=interrupt), self.assertRaisesRegex(OSError, "thaw interrupted"):
            writer_fence.close_fence(self.config, result["token"])
        self.assertEqual(self.auto.read_bytes(), self.original)
        with self.assertRaisesRegex(postgres.LifecycleError, "unfinished.*thaw"):
            writer_fence.startup(self.config)
        writer_fence.close_fence(self.config, result["token"])
        self.assertIsNone(writer_fence.startup(self.config))

    def test_offline_release_cannot_skip_unfinished_thaw_or_changed_selector(self):
        result = self.open()
        token = result["token"]
        self.assertEqual(writer_fence.inspect_offline(self.config, token, "prepared")["status"], "prepared-offline")
        with self.assertRaisesRegex(postgres.LifecycleError, "unfinished"):
            writer_fence.inspect_offline(self.config, token, "closed")
        writer_fence.close_fence(self.config, token)
        self.assertEqual(writer_fence.inspect_offline(self.config, token, "closed")["status"], "closed-offline")
        self.auto.write_bytes(self.original + b"work_mem = '32MB'\n")
        with self.assertRaisesRegex(postgres.LifecycleError, "boundary changed"):
            writer_fence.inspect_offline(self.config, token, "closed")


@unittest.skipUnless(os.environ.get("HARBOR_DB_TEST_POSTGRES"), "explicit disposable PostgreSQL package required")
class RealWriterFenceTest(unittest.TestCase):
    def test_reconnecting_superusers_replication_guarded_restart_and_thaw(self):
        package = Path(os.environ["HARBOR_DB_TEST_POSTGRES"])
        control = pwd.getpwuid(os.geteuid()).pw_name
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            data, state, socket_dir = root / "data", root / "state", root / "socket"
            state.mkdir()
            socket_dir.mkdir(mode=0o700)
            with socket.socket() as reservation:
                reservation.bind(("127.0.0.1", 0))
                port = reservation.getsockname()[1]

            def run(argv, **kwargs):
                return subprocess.run(list(map(str, argv)), check=True, capture_output=True,
                                      text=True, timeout=30, **kwargs)

            run([package / "bin/initdb", "-D", data, "-U", control, "-A", "trust",
                 "--locale=C", "--encoding=UTF8"])
            original_hba = root / "original-hba.conf"
            original_hba.write_text("local all all trust\nhost all all 127.0.0.1/32 trust\n")
            original_config = (f"hba_file = '{original_hba}'\nlisten_addresses = '127.0.0.1'\n"
                               f"unix_socket_directories = '{socket_dir}'\nport = {port}\nmax_prepared_transactions = 1\n")
            (data / "postgresql.conf").write_text(original_config)

            def sql(user, query, *, tcp=False, check=True):
                return subprocess.run(list(map(str, [package / "bin/psql", "-X", "-w", "-A", "-t",
                    "-v", "ON_ERROR_STOP=1", "-h", "127.0.0.1" if tcp else socket_dir,
                    "-p", port, "-U", user, "-d", "postgres", "-c", query])),
                    env={**os.environ, "PGPASSWORD": "disposable-replication-secret"},
                    check=check, capture_output=True, text=True, timeout=15)

            def stop():
                subprocess.run([str(package / "bin/pg_ctl"), "-D", str(data), "-m", "fast", "-w", "stop"],
                               check=False, capture_output=True, timeout=30)

            def start():
                # Reproduce legacy NixOS re-linking its generated configuration.
                (data / "postgresql.conf").write_text(original_config)
                run([package / "bin/pg_ctl", "-D", data, "-l", root / "postgres.log", "-w", "start"])

            config = {"resource": "fixture", "data_dir": str(data), "state_dir": str(state),
                      "major": (data / "PG_VERSION").read_text().strip(), "package": str(package),
                      "writer_fence": {"control_role": control, "replication_roles": ["replicator"]}}
            start()
            try:
                sql(control, "CREATE ROLE application SUPERUSER LOGIN; "
                    "CREATE ROLE replicator REPLICATION LOGIN PASSWORD 'disposable-replication-secret'; "
                    "CREATE TABLE retained(id integer primary key); INSERT INTO retained VALUES (1);")
                identifier = sql(control, "SELECT system_identifier FROM pg_control_system();").stdout.strip()
                recovery_config = config | {"recovery": {
                    "system_identifier": identifier, "require_writer_fence": True,
                    "snapshot_file": str(root / "records.json"), "receipt_file": str(root / "receipt.json"),
                    "max_age_seconds": 3600, "record_checks": [{"name": "retained", "database": "postgres", "sql": "SELECT id FROM retained ORDER BY id"}],
                }}
                with self.assertRaisesRegex(postgres.LifecycleError, "writer fence"), recovery.writer_exclusion(recovery_config, str(socket_dir), port):
                    self.fail("unfenced primary admitted")
                original = (data / "postgresql.auto.conf").read_bytes()
                with self.assertRaises(postgres.LifecycleError):
                    writer_fence.open_fence(config, identifier)
                self.assertEqual((data / "postgresql.auto.conf").read_bytes(), original)
                client = subprocess.Popen(list(map(str, [package / "bin/psql", "-X", "-w", "-h", socket_dir,
                    "-p", port, "-U", "application", "-d", "postgres", "-c",
                    "BEGIN; INSERT INTO retained VALUES (2); SELECT pg_sleep(60); COMMIT;"])),
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                try:
                    deadline = time.monotonic() + 5
                    while sql(control, "SELECT count(*) FROM pg_stat_activity WHERE usename = 'application' AND wait_event = 'PgSleep';").stdout.strip() != "1":
                        if client.poll() is not None or time.monotonic() > deadline:
                            self.fail("fixture writer did not enter its transaction")
                        time.sleep(0.05)
                    stop()
                    self.assertNotEqual(client.wait(timeout=10), 0)
                finally:
                    if client.poll() is None:
                        client.terminate()
                        client.wait(timeout=10)
                from harbor_db import durable
                def interrupt_journal(path, value):
                    if Path(path) == writer_fence.marker(config):
                        raise OSError("active journal interrupted")
                    durable.write_json(path, value)
                with patch.object(writer_fence, "write_json", side_effect=interrupt_journal), self.assertRaisesRegex(OSError, "active journal interrupted"):
                    writer_fence.open_fence(config, identifier)
                # A restart of the legacy bare server is still fenced when the
                # active journal publication was interrupted after the selector.
                start()
                self.assertNotEqual(sql("application", "SELECT 1;", check=False).returncode, 0)
                with self.assertRaisesRegex(postgres.LifecycleError, "no journal"):
                    writer_fence.startup(config)
                stop()
                opened = writer_fence.open_fence(config, identifier)
                token = opened["token"]
                start()
                try:
                    self.assertEqual(writer_fence.inspect_live(config, token, str(socket_dir), port)["status"], "ready")
                except postgres.LifecycleError as error:
                    self.fail(f"{error}: {sql(control, 'SELECT backend_type FROM pg_stat_activity;').stdout}")
                sql(control, "BEGIN; INSERT INTO retained VALUES (99); PREPARE TRANSACTION 'retained-fixture';")
                with self.assertRaisesRegex(postgres.LifecycleError, "prepared_transactions"):
                    writer_fence.inspect_live(config, token, str(socket_dir), port)
                # The engine never resolves a prepared application transaction.
                self.assertEqual(sql(control, "SELECT count(*) FROM pg_prepared_xacts;").stdout.strip(), "1")
                sql(control, "ROLLBACK PREPARED 'retained-fixture';")
                self.assertEqual(writer_fence.inspect_live(config, token, str(socket_dir), port)["status"], "ready")
                with recovery.writer_exclusion(recovery_config, str(socket_dir), port) as accepted:
                    self.assertEqual(accepted["token"], token)
                    with self.assertRaises(BlockingIOError), lock(state / "writer-fence.lock"):
                        self.fail("recovery consistency window was not pinned")
                for _ in range(3):
                    self.assertNotEqual(sql("application", "INSERT INTO retained VALUES (2);", check=False).returncode, 0)
                    self.assertNotEqual(sql("replicator", "SELECT 1;", tcp=True, check=False).returncode, 0)
                self.assertEqual(sql(control, "SELECT count(*) FROM retained;").stdout.strip(), "1")
                run([package / "bin/pg_basebackup", "-h", "127.0.0.1", "-p", port, "-U", "replicator",
                     "-D", root / "backup", "-X", "stream", "--checkpoint=fast", "-w"],
                    env={**os.environ, "PGPASSWORD": "disposable-replication-secret"})
                stop()
                postgres.adopt(config, identifier)
                manifest = root / "manifest.json"
                manifest.write_text(json.dumps(config))
                log = (root / "guarded.log").open("w")
                process = subprocess.Popen([sys.executable, "-B", "-m", "harbor_db.postgres",
                    "--config", str(manifest), "serve"], stdout=log, stderr=log,
                    env={**os.environ, "PYTHONPATH": str(Path(postgres.__file__).resolve().parent.parent)})
                try:
                    deadline = time.monotonic() + 10
                    while subprocess.run(list(map(str, [package / "bin/pg_isready", "-h", socket_dir, "-p", port])),
                                         capture_output=True, timeout=2, check=False).returncode != 0:
                        if process.poll() is not None or time.monotonic() > deadline:
                            self.fail((root / "guarded.log").read_text())
                        time.sleep(0.05)
                    self.assertEqual(writer_fence.inspect_live(config, token, str(socket_dir), port)["status"], "ready")
                    self.assertNotEqual(sql("application", "SELECT 1;", check=False).returncode, 0)
                finally:
                    stop()
                    process.wait(timeout=30)
                    log.close()
                closed = writer_fence.close_fence(config, token)
                self.assertTrue(Path(closed["receipt"]).exists())
                self.assertEqual((data / "postgresql.auto.conf").read_bytes(), original)
                start()
                sql("application", "INSERT INTO retained VALUES (2);")
                self.assertEqual(sql(control, "SELECT count(*) FROM retained;").stdout.strip(), "2")
            finally:
                stop()
