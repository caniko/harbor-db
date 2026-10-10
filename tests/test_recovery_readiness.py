"""Recovery acceptance must bind real records, backup bytes and cluster identity."""

import hashlib
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from harbor_db import postgres, recovery, writer_fence
from harbor_db.durable import lock, write_json


class RecoveryReadinessTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.backup = self.root / "backup"
        self.base = self.backup / "base" / "base-1"
        self.base.mkdir(parents=True)
        (self.backup / "locks").mkdir()
        (self.backup / "locks/mutate").touch()
        (self.backup / "evidence").mkdir()
        (self.backup / "LAST_SUCCESS").write_text("base-1\n")
        (self.base / "PG_VERSION").write_text("18\n")
        (self.base / "backup_manifest").write_text('{"WAL-Ranges": []}\n')
        write_json(self.backup / "base/base-1.meta.json", {
            "backup_id": "base-1", "system_identifier": "12345", "pg_major": 18,
            "epoch_id": "epoch-1", "backup_stop_lsn": "0/100", "post_backup_lsn": "0/200",
        })
        self.config = {
            "data_dir": str(self.root / "primary"), "major": "18", "package": "/postgres18",
            "recovery": {
                "system_identifier": "12345", "backup_root": str(self.backup),
                "snapshot_file": str(self.backup / "evidence/records.json"),
                "receipt_file": str(self.backup / "evidence/recovery.json"),
                "off_host_receipt_file": None, "source_hostname": "primary-host",
                "max_age_seconds": 3600, "verify_timeout_seconds": 900,
                "record_checks": [{"name": "reviews", "database": "app", "sql": "SELECT records"}],
            },
        }
        self.now = 10000
        os.utime(self.base / "backup_manifest", (self.now, self.now))
        self.live = patch.object(postgres, "inspect_live", return_value={"system_identifier": "12345"})
        self.live.start()
        self.addCleanup(self.live.stop)
        self.probe = patch.object(postgres, "inspect_cluster", return_value="12345")
        self.probe.start()
        self.addCleanup(self.probe.stop)
        self.verify = patch.object(recovery, "verify_backup")
        self.verify.start()
        self.addCleanup(self.verify.stop)
        self.query = patch.object(recovery, "query", return_value="3:record-digest\n")
        self.query.start()
        self.addCleanup(self.query.stop)
        self.restored = self.root / "restored"
        self.restored.mkdir()
        self.observed = {
            "data_dir": str(self.restored), "major": "18", "system_identifier": "12345",
            "read_only": "on", "in_recovery": False, "replay_lsn": "0/200",
        }

    def snapshot(self):
        recovery.snapshot(self.config, "/run/postgresql", 5432, now=self.now)

    def certify(self, **kwargs):
        with patch.object(recovery, "inspect_restored", return_value=self.observed):
            return recovery.certify(self.config, str(self.restored), "/restore/socket", 55432,
                                    now=self.now, hostname="primary-host", **kwargs)

    def require_fence(self, *, opened=True):
        primary, state = Path(self.config["data_dir"]), self.root / "authority"
        primary.mkdir()
        state.mkdir(mode=0o700)
        (primary / "postgresql.auto.conf").write_text("# original\n")
        self.config.update(resource="fixture", state_dir=str(state), required_mounts=[])
        self.config["recovery"]["require_writer_fence"] = True
        if opened:
            with patch.object(postgres, "require_stopped"):
                token = writer_fence.open_fence(self.config, "12345")["token"]
            probe = patch.object(writer_fence, "inspect_live", return_value={"status": "ready", "token": token})
            probe.start()
            self.addCleanup(probe.stop)
            return token

    def test_required_fence_rejects_unfenced_snapshot_and_preparation(self):
        self.require_fence(opened=False)
        preparation = {"readiness_command": ["/fixture/readiness"], "backup_command": ["/fixture/backup"], "restore_command": ["/fixture/restore"]}
        with self.assertRaisesRegex(postgres.LifecycleError, "writer fence"):
            self.snapshot()
        with patch.object(postgres, "run") as execute, self.assertRaisesRegex(postgres.LifecycleError, "writer fence"):
            recovery.prepare(self.config, preparation, "/run/postgresql", 5432)
        execute.assert_not_called()
        self.assertFalse(Path(self.config["recovery"]["snapshot_file"]).exists())

    def test_required_fence_must_pass_live_inspection_before_capture(self):
        self.require_fence()
        with patch.object(writer_fence, "inspect_live", side_effect=postgres.LifecycleError("other writers")), self.assertRaisesRegex(postgres.LifecycleError, "other writers"):
            self.snapshot()
        self.assertFalse(Path(self.config["recovery"]["snapshot_file"]).exists())
        self.snapshot()
        self.certify()
        with patch.object(recovery.time, "time", return_value=self.now), patch.object(writer_fence, "inspect_live", side_effect=postgres.LifecycleError("other writers")), self.assertRaisesRegex(postgres.LifecycleError, "other writers"):
            postgres.adopt_live(self.config, "12345", "/run/postgresql", 5432)
        self.assertFalse((Path(self.config["state_dir"]) / "identity.json").exists())

    def test_required_fence_is_bound_to_snapshot_and_independent_receipts(self):
        token = self.require_fence()
        self.snapshot()
        source = json.loads(Path(self.config["recovery"]["snapshot_file"]).read_text())
        self.assertEqual(source["writer_fence_token"], token)
        with patch.object(writer_fence, "startup", side_effect=AssertionError("primary state on recovery executor")):
            receipt = self.certify()
        self.assertEqual(receipt["snapshot_sha256"], recovery.digest(self.config["recovery"]["snapshot_file"]))
        self.assertEqual(recovery.check(self.config, now=self.now)["status"], "ready")
        source["writer_fence_token"] = "0" * 32
        write_json(Path(self.config["recovery"]["snapshot_file"]), source)
        # Retirement cannot make an independently accepted snapshot match new bytes.
        self.config["recovery"]["require_writer_fence"] = False
        with self.assertRaisesRegex(ValueError, "record-level recovery"):
            recovery.check(self.config, now=self.now)

    def test_required_fence_cannot_thaw_during_snapshot_or_authority_publication(self):
        token = self.require_fence()

        def publish(path, value):
            with patch.object(postgres, "require_stopped"), self.assertRaises(BlockingIOError):
                writer_fence.close_fence(self.config, token)
            write_json(path, value)

        with patch.object(recovery, "write_json", side_effect=publish):
            self.snapshot()
        self.certify()
        with patch.object(recovery.time, "time", return_value=self.now), patch.object(postgres, "write_json", side_effect=publish):
            postgres.adopt_live(self.config, "12345", "/run/postgresql", 5432)
        self.assertTrue((Path(self.config["state_dir"]) / "identity.json").exists())

    def test_required_fence_rejects_prior_snapshot_and_replacement_window(self):
        self.snapshot()
        self.certify()
        token = self.require_fence()
        with self.assertRaisesRegex(ValueError, "writer fence"):
            recovery.check(self.config, now=self.now)
        self.snapshot()
        self.certify()
        with patch.object(postgres, "require_stopped"):
            writer_fence.close_fence(self.config, token)
            writer_fence.open_fence(self.config, "12345")
        with self.assertRaisesRegex(ValueError, "writer fence"):
            recovery.check(self.config, now=self.now)
        self.config["recovery"]["require_writer_fence"] = False
        self.assertEqual(recovery.check(self.config, now=self.now)["status"], "ready")

    def test_writer_fence_requirement_is_a_boolean(self):
        for value in ("true", "false", 1, None):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "writer fence"):
                recovery.policy(self.config | {"recovery": self.config["recovery"] | {"require_writer_fence": value}})

    def test_import_rejects_a_replacement_fence_before_receipt_publication(self):
        token = self.require_fence()
        self.snapshot()
        self.certify()
        receipt = json.loads(Path(self.config["recovery"]["receipt_file"]).read_text())
        receipt["executor_host"] = "independent-host"
        incoming = self.root / "incoming.json"
        write_json(incoming, receipt)
        destination = self.backup / "evidence/off-host.json"
        self.config["recovery"]["off_host_receipt_file"] = str(destination)
        with patch.object(postgres, "require_stopped"):
            writer_fence.close_fence(self.config, token)
            writer_fence.open_fence(self.config, "12345")
        with self.assertRaisesRegex(ValueError, "writer fence"):
            recovery.import_off_host(self.config, incoming, now=self.now)
        self.assertFalse(destination.exists())

    def test_disposable_certifier_rejects_missing_or_invalid_copied_fence(self):
        self.require_fence()
        self.snapshot()
        source = json.loads(Path(self.config["recovery"]["snapshot_file"]).read_text())
        for binding in (None, "other-window", "a" * 31, 17):
            with self.subTest(binding=binding):
                changed = source | {"writer_fence_token": binding}
                write_json(Path(self.config["recovery"]["snapshot_file"]), changed)
                with self.assertRaisesRegex(ValueError, "writer fence"):
                    self.certify()
                self.assertFalse(Path(self.config["recovery"]["receipt_file"]).exists())

    def test_missing_evidence_is_not_recovery_readiness(self):
        with self.assertRaisesRegex(ValueError, "snapshot"):
            recovery.check(self.config, now=self.now)
        self.assertFalse(Path(self.config["recovery"]["receipt_file"]).exists())

    def test_preflight_requires_executed_receipts_but_defers_backup_byte_verification(self):
        self.snapshot()
        with self.assertRaisesRegex(ValueError, "acceptance"):
            recovery.preflight(self.config, now=self.now)
        self.certify()
        with patch.object(recovery, "verify_backup", side_effect=ValueError("backup bytes corrupted")) as verifier:
            self.assertEqual(recovery.preflight(self.config, now=self.now)["status"], "preflight-ready")
            verifier.assert_not_called()
            with self.assertRaisesRegex(ValueError, "corrupted"):
                recovery.check(self.config, now=self.now)

    def test_preflight_refuses_changed_contract_and_expired_receipts(self):
        self.snapshot()
        self.certify()
        with self.assertRaisesRegex(ValueError, "stale"):
            recovery.preflight(self.config, now=self.now + 3601)
        self.config["recovery"]["record_checks"][0]["sql"] = "SELECT incompatible_schema"
        with self.assertRaisesRegex(ValueError, "contract"):
            recovery.preflight(self.config, now=self.now)

    def test_preflight_never_creates_missing_evidence_locks(self):
        self.snapshot()
        self.certify()
        anchor = self.backup / "evidence/recovery.lock"
        anchor.unlink()
        with self.assertRaisesRegex(ValueError, "lease"):
            recovery.preflight(self.config, now=self.now)
        self.assertFalse(anchor.exists())

    def test_real_matching_checks_publish_a_bound_receipt(self):
        self.snapshot()
        result = self.certify()
        self.assertEqual(result["status"], "ready")
        self.assertEqual(result["manifest_sha256"], hashlib.sha256((self.base / "backup_manifest").read_bytes()).hexdigest())
        self.assertEqual(recovery.check(self.config, now=self.now)["backup_id"], "base-1")

    def test_record_loss_refuses_publication(self):
        self.snapshot()
        with patch.object(recovery, "query", return_value="2:missing-review\n"), self.assertRaisesRegex(ValueError, "records differ"):
            self.certify()
        self.assertFalse(Path(self.config["recovery"]["receipt_file"]).exists())

    def test_query_contract_change_invalidates_old_acceptance(self):
        self.snapshot()
        self.certify()
        self.config["recovery"]["record_checks"][0]["sql"] = "SELECT different_records"
        with self.assertRaisesRegex(ValueError, "contract"):
            recovery.check(self.config, now=self.now)

    def test_manifest_change_and_new_backup_invalidate_receipt(self):
        self.snapshot()
        self.certify()
        (self.base / "backup_manifest").write_text("{}\n")
        os.utime(self.base / "backup_manifest", (self.now, self.now))
        with self.assertRaisesRegex(ValueError, "backup"):
            recovery.check(self.config, now=self.now)
        (self.backup / "LAST_SUCCESS").write_text("../primary\n")
        with self.assertRaisesRegex(ValueError, "backup identifier"):
            recovery.check(self.config, now=self.now)

    def test_stale_and_future_evidence_is_rejected(self):
        self.snapshot()
        self.certify()
        for now in (self.now + 3601, self.now - 1):
            with self.subTest(now=now), self.assertRaisesRegex(ValueError, "timestamp"):
                recovery.check(self.config, now=now)

    def test_metadata_byte_changes_invalidate_acceptance(self):
        self.snapshot()
        self.certify()
        metadata = self.backup / "base/base-1.meta.json"
        metadata.write_text(metadata.read_text() + " ")
        with self.assertRaisesRegex(ValueError, "different backup"):
            recovery.check(self.config, now=self.now)

    def test_primary_writable_or_incomplete_restore_cannot_be_certified(self):
        self.snapshot()
        for changes in ({"data_dir": self.config["data_dir"]}, {"read_only": "off"},
                        {"in_recovery": True}, {"replay_lsn": "0/100"},
                        {"system_identifier": "99999"}):
            with self.subTest(changes=changes):
                observed = {**self.observed, **changes}
                with patch.object(recovery, "inspect_restored", return_value=observed), self.assertRaises(ValueError):
                    recovery.certify(self.config, str(self.restored), "/restore/socket", 55432,
                                     now=self.now, hostname="primary-host")
        self.assertFalse(Path(self.config["recovery"]["receipt_file"]).exists())

    def test_off_host_acceptance_must_be_independent_and_same_backup(self):
        self.snapshot()
        self.certify()
        off_host = self.backup / "evidence/off-host.json"
        self.config["recovery"]["off_host_receipt_file"] = str(off_host)
        with self.assertRaisesRegex(ValueError, "off-host"):
            recovery.check(self.config, now=self.now)
        local = json.loads(Path(self.config["recovery"]["receipt_file"]).read_text())
        write_json(off_host, local)
        with self.assertRaisesRegex(ValueError, "independent"):
            recovery.check(self.config, now=self.now)
        # Exercise the same certifier with a real executor-host identity; do not
        # make an unrelated copy of the local receipt establish remote recovery.
        self.config["recovery"]["receipt_file"] = str(off_host)
        with patch.object(recovery, "inspect_restored", return_value=self.observed):
            recovery.certify(self.config, str(self.restored), "/restore/socket", 55432,
                             now=self.now, hostname="recovery-host")
        self.config["recovery"]["receipt_file"] = str(self.backup / "evidence/recovery.json")
        self.assertEqual(recovery.check(self.config, now=self.now)["off_host"], "recovery-host")

    def test_readiness_never_creates_a_missing_backup_lock(self):
        (self.backup / "locks/mutate").unlink()
        with self.assertRaises(OSError):
            recovery.check(self.config, now=self.now)
        self.assertFalse((self.backup / "locks/mutate").exists())

    def test_concurrent_backup_and_evidence_publication_are_rejected(self):
        self.snapshot()
        self.certify()
        for path in (self.backup / "locks/mutate", self.backup / "evidence/recovery.lock"):
            with self.subTest(path=path), lock(path):
                with self.assertRaises(BlockingIOError):
                    recovery.check(self.config, now=self.now)
                with self.assertRaises(BlockingIOError):
                    self.snapshot()
        self.assertEqual(recovery.check(self.config, now=self.now)["status"], "ready")

    def test_adoption_retains_recovery_evidence_until_authority_publication(self):
        self.snapshot()
        self.certify()
        Path(self.config["data_dir"]).mkdir()
        self.config.update(resource="test", required_mounts=[])
        for operation in ("offline", "live"):
            with self.subTest(operation=operation):
                state = self.root / f"authority-{operation}"
                state.mkdir()
                self.config["state_dir"] = str(state)

                def publish(path, value):
                    for anchor in (self.backup / "locks/mutate", self.backup / "evidence/recovery.lock"):
                        with self.assertRaises(BlockingIOError), lock(anchor):
                            self.fail("recovery evidence changed before authority publication")
                    write_json(path, value)

                with patch.object(recovery.time, "time", return_value=self.now), patch.object(postgres, "write_json", side_effect=publish):
                    if operation == "offline":
                        postgres.adopt(self.config, "12345")
                    else:
                        postgres.adopt_live(self.config, "12345", "/run/postgresql", 5432)
                self.assertEqual(json.loads((state / "identity.json").read_text())["system_identifier"], "12345")
                for anchor in (self.backup / "locks/mutate", self.backup / "evidence/recovery.lock"):
                    with lock(anchor):
                        pass

    def test_new_snapshot_invalidates_prior_certification(self):
        self.snapshot()
        self.certify()
        with patch.object(recovery, "query", return_value="3:changed-review\n"):
            self.snapshot()
        with self.assertRaisesRegex(ValueError, "record-level recovery"):
            recovery.check(self.config, now=self.now)

    def test_record_queries_scrub_routing_and_normalize_session_output(self):
        self.query.stop()
        with patch.dict(os.environ, {"PGHOST": "unrelated", "PGOPTIONS": "-c TimeZone=Pacific/Auckland", "PGSERVICE": "other"}), patch.object(postgres, "run", return_value=subprocess.CompletedProcess([], 0, "record-digest\n")) as execute:
            self.assertEqual(recovery.query(self.config, "/restore/socket", 55432, "app", "SELECT records"), "record-digest\n")
        args, kwargs = execute.call_args
        self.assertIn("--host=/restore/socket", args[0])
        self.assertNotIn("PGHOST", kwargs["env"])
        self.assertNotIn("PGOPTIONS", kwargs["env"])
        self.assertNotIn("PGSERVICE", kwargs["env"])
        self.assertIn("BEGIN READ ONLY", args[0][-1])
        self.assertIn("SET LOCAL TimeZone = 'UTC'", args[0][-1])
        with self.assertRaisesRegex(ValueError, "local Unix socket"):
            recovery.query(self.config, "localhost", 55432, "app", "SELECT records")

    def test_redirected_evidence_is_rejected(self):
        self.snapshot()
        path = Path(self.config["recovery"]["snapshot_file"])
        path.rename(path.with_suffix(".saved"))
        path.symlink_to(path.with_suffix(".saved"))
        with self.assertRaisesRegex(ValueError, "redirected"):
            self.certify()

    def test_managed_preparation_creates_first_evidence_and_reuses_it_on_retry(self):
        preparation = {"readiness_command": ["/fixture/readiness"], "backup_command": ["/fixture/backup"], "restore_command": ["/fixture/restore"]}
        off_host = self.backup / "evidence/off-host.json"
        self.config["recovery"]["off_host_receipt_file"] = str(off_host)

        def run(argv):
            if argv == preparation["restore_command"]:
                self.certify()

        with patch.object(recovery.time, "time", return_value=self.now), patch.object(postgres, "run", side_effect=run) as execute:
            with self.assertRaisesRegex(ValueError, "off-host"):
                recovery.prepare(self.config, preparation, "/run/postgresql", 5432)
            self.assertEqual([call.args[0] for call in execute.call_args_list],
                             [preparation["readiness_command"], preparation["backup_command"], preparation["restore_command"]])
            execute.reset_mock()
            with self.assertRaisesRegex(ValueError, "off-host"):
                recovery.prepare(self.config, preparation, "/run/postgresql", 5432)
            execute.assert_called_once_with(preparation["readiness_command"])
        self.assertEqual((self.backup / "LAST_SUCCESS").read_text(), "base-1\n")

    def test_preparation_refuses_stale_snapshot_without_replacing_backup(self):
        self.snapshot()
        preparation = {"readiness_command": ["/fixture/readiness"], "backup_command": ["/fixture/backup"], "restore_command": ["/fixture/restore"]}
        with patch.object(recovery.time, "time", return_value=self.now + 3601), patch.object(postgres, "run") as execute:
            with self.assertRaisesRegex(ValueError, "timestamp"):
                recovery.prepare(self.config, preparation, "/run/postgresql", 5432)
            execute.assert_called_once_with(preparation["readiness_command"])

    def test_interrupted_snapshot_resumes_the_same_backup_without_recapture(self):
        preparation = {"readiness_command": ["/fixture/readiness"], "backup_command": ["/fixture/backup"], "restore_command": ["/fixture/restore"]}
        with patch.object(recovery.time, "time", return_value=self.now), patch.object(postgres, "run") as execute:
            with patch.object(recovery, "snapshot", side_effect=RuntimeError("interrupted")), self.assertRaisesRegex(RuntimeError, "interrupted"):
                recovery.prepare(self.config, preparation, "/run/postgresql", 5432)
            execute.reset_mock()
            with patch.object(recovery, "snapshot", side_effect=RuntimeError("resumed")), self.assertRaisesRegex(RuntimeError, "resumed"):
                recovery.prepare(self.config, preparation, "/run/postgresql", 5432)
            execute.assert_called_once_with(preparation["readiness_command"])

    def test_failed_wal_readiness_does_not_capture_a_backup(self):
        preparation = {"readiness_command": ["/fixture/readiness"], "backup_command": ["/fixture/backup"], "restore_command": ["/fixture/restore"]}
        with patch.object(postgres, "run", side_effect=subprocess.CalledProcessError(1, preparation["readiness_command"])) as execute:
            with self.assertRaises(subprocess.CalledProcessError):
                recovery.prepare(self.config, preparation, "/run/postgresql", 5432)
            execute.assert_called_once_with(preparation["readiness_command"])
        self.assertFalse(Path(self.config["recovery"]["snapshot_file"]).exists())

    def test_export_runs_after_local_acceptance_before_missing_off_host_abort(self):
        self.snapshot()
        self.certify()
        self.config["recovery"]["off_host_receipt_file"] = str(self.backup / "evidence/off-host.json")
        preparation = {"readiness_command": ["/fixture/readiness"], "backup_command": ["/fixture/backup"],
                       "restore_command": ["/fixture/restore"], "export_command": ["/fixture/export"]}
        with patch.object(recovery.time, "time", return_value=self.now), patch.object(postgres, "run") as execute, self.assertRaisesRegex(ValueError, "off-host"):
            recovery.prepare(self.config, preparation, "/run/postgresql", 5432)
        self.assertEqual([call.args[0] for call in execute.call_args_list],
                         [preparation["readiness_command"], preparation["export_command"]])

    def test_managed_import_rejects_local_or_unbound_receipts_before_publication(self):
        self.snapshot()
        self.certify()
        off_host = self.backup / "evidence/off-host.json"
        self.config["recovery"]["off_host_receipt_file"] = str(off_host)
        incoming = self.root / "incoming.json"
        receipt = json.loads(Path(self.config["recovery"]["receipt_file"]).read_text())
        for changes in ({}, {"executor_host": "remote", "backup_id": "other"},
                        {"executor_host": "remote", "records": {"reviews": "0" * 64}}):
            write_json(incoming, receipt | changes)
            with self.assertRaises(ValueError):
                recovery.import_off_host(self.config, incoming, now=self.now)
            self.assertFalse(off_host.exists())
        write_json(incoming, receipt | {"executor_host": "independent-fixture"})
        recovery.import_off_host(self.config, incoming, now=self.now)
        self.assertEqual(recovery.check(self.config, now=self.now)["off_host"], "independent-fixture")


if __name__ == "__main__":
    unittest.main()
