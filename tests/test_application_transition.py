"""Authority transitions retain writer barriers across failures and resume."""

import copy
import os
import pwd
import signal
import subprocess
import json
import sys
import tempfile
import time
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

from harbor_db import application_transition, resource
from harbor_db import cutover
from harbor_db import transition_manifest
from harbor_db import postgres, startup_inhibition
from harbor_db.application_backup import digest, identity
from harbor_db.durable import read_json, write_json


class ApplicationTransitionTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        inspect_ancestors = startup_inhibition.owned_ancestors
        def fixture_ancestors(path):
            # Model root inside the private fixture, as in the startup-inhibition
            # tests. Nix's writable /build is outside that model. Keep the real
            # ownership/mode checks on all fixture ancestors; VMs cover the host.
            inspect_ancestors(SimpleNamespace(parents=[parent for parent in path.parents
                if parent == self.root or parent.is_relative_to(self.root)]))
        self.enterContext(mock.patch.object(startup_inhibition, "owned_ancestors", side_effect=fixture_ancestors))
        self.state, self.old, self.new, self.barrier = [self.root / name for name in ("authority", "old", "new", "barrier")]
        for path in (self.state, self.old, self.new, self.barrier):
            path.mkdir(mode=0o700)
        self.source = {"resource": "demo", "state_dir": str(self.state), "directories": [str(self.old)], "binding": {"backend": "filesystem"}, "required_mounts": []}
        self.target = self.source | {"directories": [str(self.new)], "binding": {"backend": "test-target"}}
        resource.adopt(self.source, "retained-resource")
        self.source_manifest, self.target_manifest = self.root / "source.json", self.root / "target.json"
        write_json(self.source_manifest, self.source)
        write_json(self.target_manifest, self.target)
        self.adapter = self.root / "adapter.py"
        self.adapter.write_text('''import json, sys
print(json.dumps({"version":1,"status":"verified","semantic_sha256":"a"*64}))
''')
        self.config = {"version": 1, "resource": "demo", "source_manifest": str(self.source_manifest),
                       "target_manifest": str(self.target_manifest), "barrier_dir": str(self.barrier),
                       "drop_in_root": str(self.root / "systemd"), "systemctl": sys.executable, "busctl": sys.executable,
                       "units": ["demo.service"], "retired_units": [], "timeout_seconds": 10,
                       "commands": {stage: {"user": "root", "argv": [sys.executable, str(self.adapter)]}
                                    for stage in ("import", "verify-target", "verify-source", "health")},
                       "executable_files": [str(self.adapter)], "postgres_manifest": None,
                       "postgres_socket": "/run/postgresql", "postgres_port": 5432,
                       "custody_manifest": None,
                       "backup_manifest": str(self.root / "backup-config.json"),
                       "independent_receipt": str(self.root / "independent.json"),
                       "storage_package": "/bin", "runuser": sys.executable}
        self.candidate = "/nix/store/00000000000000000000000000000000-candidate"
        for name in ("require_root", "candidate_path", "install_barriers", "stop_units", "inspect_barriers", "generation"):
            patched = mock.patch.object(application_transition, name)
            value = patched.start()
            self.addCleanup(patched.stop)
            if name == "generation":
                value.return_value = self.candidate
            if name == "candidate_path":
                value.return_value = "generation"
            if name == "install_barriers":
                value.side_effect = lambda config, record: write_json(self.barrier / "inhibited.json", {"candidate": record["barrier_candidate"], "intent": record["intent"]})
        storage = self.root / "bin"
        storage.mkdir()
        (storage / "harbor-db-application-backup").symlink_to(sys.executable)
        self.config["storage_package"] = str(storage)
        (self.root / "lock").touch(mode=0o600)
        write_json(self.root / "backup-config.json", {"maximum_age_seconds": 3600, "root": str(self.root)})
        self.backup = self.root / "backup"
        self.backup.mkdir()
        self.snapshot = {"status": "verified", "consistency": "quiesced", "semantic_sha256": "a" * 64,
                         "executor_machine_sha256": "b" * 64, "executor": "source-fixture", "resource": "demo",
                         "manifest_sha256": identity({"fixture": True}), "executables": {}}
        write_json(self.backup / "acceptance.json", self.snapshot)
        self.capture_patch = mock.patch.object(application_transition, "capture_source", return_value=(self.backup, self.snapshot))
        self.capture_patch.start()
        self.addCleanup(self.capture_patch.stop)
        self.evidence_patch = mock.patch.object(application_transition, "source_bytes", return_value=self.snapshot)
        self.evidence_patch.start()
        self.addCleanup(self.evidence_patch.stop)
        self.runner = mock.patch.object(application_transition, "run_action", return_value={"version": 1, "status": "verified", "semantic_sha256": "a" * 64})
        self.actions = self.runner.start()
        self.addCleanup(self.runner.stop)

    def certify(self):
        write_json(self.root / "independent.json", {"version": 1, "status": "verified", "resource": "demo",
            "source_acceptance_sha256": digest(self.backup / "acceptance.json"), "semantic_sha256": "a" * 64,
            "executor_machine_sha256": "c" * 64, "manifest_sha256": identity({"fixture": True}),
            "executor": "independent-fixture", "certified_at": int(time.time()), "executables": {}})

    def prepare(self):
        application_transition.plan(self.config, self.candidate, None)
        self.certify()
        return application_transition.prepare(self.config)

    def custody(self):
        (self.old / "records").write_text("source revision seven")
        (self.new / "records").write_text("source revision seven")
        source_entry = {"kind": "filesystem", "authority": self.source, "custody_file": str(self.state / "custody.json"),
                        "max_age_seconds": 3600, "runtime_units": ["demo.service"], "database_resource": None}
        original = {"version": 1, "resource": "demo", "identity": "retained-resource", "binding": self.source["binding"],
                    "directories": self.source["directories"], "root_identities": cutover.root_identities(source_entry),
                    "completed_at": int(time.time()), "metadata": cutover.inventory(source_entry, contents=False),
                    "inventory": cutover.inventory(source_entry, contents=True), "database_requirements": [], "database_snapshot_sha256": None}
        write_json(self.state / "custody.json", original)
        target_entry = source_entry | {"authority": self.target, "database_inventory_checks": []}
        path = self.root / "custody-manifest.json"
        write_json(path, target_entry)
        self.config["custody_manifest"] = str(path)
        return source_entry, target_entry, original

    def test_prepare_does_not_publish_authority_and_commit_does_not_thaw(self):
        self.prepare()
        resource.verify(self.source, resource.contract(self.source))
        self.assertTrue((self.barrier / "inhibited.json").exists())
        with self.assertRaisesRegex(ValueError, "transition"):
            resource.check(self.source)
        application_transition.commit(self.config)
        resource.verify(self.target, resource.contract(self.target))
        self.assertTrue((self.barrier / "inhibited.json").exists())
        application_transition.enable_writes(self.config)
        self.assertFalse((self.barrier / "inhibited.json").exists())
        resource.check(self.target)
        with self.assertRaises(resource.AuthorityError):
            resource.check(self.source)
        # Health after write-enable must not demand equality with old records.
        self.actions.return_value = {"version": 1, "status": "healthy"}
        application_transition.complete(self.config)

    def test_writable_fixture_ancestor_cannot_authorize_a_transition(self):
        self.root.chmod(0o777)
        try:
            with self.assertRaisesRegex(postgres.LifecycleError, "untrusted ancestor"):
                application_transition.plan(self.config, self.candidate, None)
            self.assertFalse((self.state / "transition.json").exists())
        finally:
            self.root.chmod(0o700)

    def test_failed_import_retains_source_and_exact_resume_identity(self):
        application_transition.plan(self.config, self.candidate, None)
        self.certify()
        self.actions.side_effect = ValueError("partial import")
        with self.assertRaisesRegex(ValueError, "partial import"):
            application_transition.prepare(self.config)
        resource.verify(self.source, resource.contract(self.source))
        changed = copy.deepcopy(self.config)
        changed["commands"]["import"]["argv"].append("other-candidate")
        with self.assertRaisesRegex(ValueError, "identity"):
            application_transition.prepare(changed)
        self.actions.side_effect = None
        application_transition.prepare(self.config)
        self.assertEqual(application_transition.status(self.config)["phase"], "prepared")

    def test_abort_is_forbidden_after_write_enable(self):
        self.prepare()
        application_transition.commit(self.config)
        application_transition.enable_writes(self.config)
        with self.assertRaisesRegex(ValueError, "reverse transition"):
            application_transition.abort(self.config)

    def test_changed_independent_proof_and_wrong_generation_are_rejected(self):
        self.prepare()
        proof = read_json(self.root / "independent.json")
        write_json(self.root / "independent.json", proof | {"semantic_sha256": "d" * 64})
        with self.assertRaisesRegex(ValueError, "evidence"):
            application_transition.commit(self.config)
        write_json(self.root / "independent.json", proof)
        with mock.patch.object(application_transition, "generation", return_value="wrong-generation"):
            with self.assertRaisesRegex(ValueError, "generation"):
                application_transition.commit(self.config)

    def test_committed_authority_can_abort_before_writes_and_source_stays_retained(self):
        self.prepare()
        application_transition.commit(self.config)
        application_transition.abort(self.config)
        resource.check(self.source)
        with self.assertRaises(resource.AuthorityError):
            resource.check(self.target)
        self.assertTrue(self.new.is_dir())

    def test_interrupted_authority_publication_resumes_against_the_intended_target(self):
        self.prepare()
        original = application_transition.publish
        def interrupted(*args):
            original(*args)
            raise OSError("interrupted after identity publication")
        with mock.patch.object(application_transition, "publish", side_effect=interrupted):
            with self.assertRaises(OSError):
                application_transition.commit(self.config)
        self.assertEqual(application_transition.status(self.config)["phase"], "committing")
        resource.verify(self.target, resource.contract(self.target))
        application_transition.commit(self.config)
        self.assertEqual(application_transition.status(self.config)["phase"], "committed")

    def test_contract_first_preparation_requires_binding_before_activation(self):
        with mock.patch.object(application_transition, "candidate_path", return_value="contract"):
            application_transition.plan(self.config, "/nix/store/00000000000000000000000000000000-contract.json", None)
        self.certify()
        application_transition.prepare(self.config)
        application_transition.admission(self.config, "preflight", self.target)
        with self.assertRaisesRegex(ValueError, "bound realized"):
            application_transition.admission(self.config, "activate", self.target)
        application_transition.bind_candidate(self.config, self.candidate)
        with self.assertRaisesRegex(ValueError, "activation generation differs"):
            application_transition.admission(self.config, "activate", self.target, "other-generation")
        application_transition.admission(self.config, "activate", self.target, self.candidate)
        with self.assertRaisesRegex(ValueError, "ordinary startup"):
            application_transition.admission(self.config, "startup", self.target)

    def test_write_enable_and_completion_work_with_a_live_shared_writer_lease(self):
        from harbor_db.durable import lock
        self.prepare()
        application_transition.commit(self.config)
        application_transition.enable_writes(self.config)
        with lock(self.state / "lock", shared=True):
            application_transition.enable_writes(self.config)
            self.actions.return_value = {"version": 1, "status": "healthy"}
            application_transition.complete(self.config)

    def test_legacy_startup_remains_generation_bound_after_retirement(self):
        self.prepare()
        application_transition.commit(self.config)
        application_transition.enable_writes(self.config)
        self.actions.return_value = {"version": 1, "status": "healthy"}
        application_transition.complete(self.config)
        application_transition.retire(self.config)
        with mock.patch.object(transition_manifest, "require_root"), mock.patch.object(transition_manifest, "generation", return_value=self.candidate):
            transition_manifest.startup_unit(self.barrier, "demo.service")
        with mock.patch.object(transition_manifest, "require_root"), mock.patch.object(transition_manifest, "generation", return_value="legacy-generation"):
            with self.assertRaisesRegex(ValueError, "released generation"):
                transition_manifest.startup_unit(self.barrier, "demo.service")

    def test_terminal_history_is_required_before_reusing_a_transition_slot(self):
        self.prepare()
        with self.assertRaisesRegex(ValueError, "unfinished"):
            application_transition.retire(self.config)
        application_transition.abort(self.config)
        result = application_transition.retire(self.config)
        self.assertEqual(read_json(result["history"])["phase"], "aborted")
        self.assertFalse((self.state / "transition.json").exists())

    def test_target_corpus_drift_cannot_hide_behind_a_successful_semantic_callback(self):
        self.custody()
        self.prepare()
        (self.new / "records").write_text("source revision six!!")
        with self.assertRaisesRegex(ValueError, "corpus evidence changed"):
            application_transition.commit(self.config)
        resource.verify(self.source, resource.contract(self.source))

    def test_custody_is_published_with_target_and_original_custody_restored_on_abort(self):
        source, target, original = self.custody()
        self.prepare()
        application_transition.commit(self.config)
        self.assertEqual(read_json(self.state / "custody.json")["binding"], self.target["binding"])
        application_transition.abort(self.config)
        self.assertEqual(read_json(self.state / "custody.json"), original)
        cutover.check_resource(source, phase="startup")


class TransitionWorkerLeaseTest(unittest.TestCase):
    def test_surviving_worker_keeps_the_authority_lease_after_coordinator_death(self):
        from harbor_db.durable import lock
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            anchor, pidfile = root / "lock", root / "child.pid"
            anchor.touch(mode=0o600)
            child = root / "child.py"
            child.write_text("import os,time,pathlib,sys; pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(30)")
            parent = root / "parent.py"
            parent.write_text('''import sys,pwd,os
from harbor_db.durable import lock
from harbor_db.transition_manifest import worker
with lock(sys.argv[1]) as lease:
    worker({"timeout_seconds":60},{"user":pwd.getpwuid(os.getuid()).pw_name,"argv":[sys.executable,sys.argv[2],sys.argv[3]]},{},[lease])
''')
            process = subprocess.Popen([sys.executable, "-B", str(parent), str(anchor), str(child), str(pidfile)],
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            kid = None
            try:
                deadline = time.monotonic() + 5
                while not pidfile.exists() and time.monotonic() < deadline:
                    time.sleep(0.01)
                self.assertTrue(pidfile.exists(), "worker did not start")
                kid = int(pidfile.read_text())
                process.kill()
                process.wait(timeout=5)
                with self.assertRaises(BlockingIOError):
                    with lock(anchor):
                        self.fail("a surviving worker lost its inherited authority lease")
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
                if kid is not None:
                    os.kill(kid, signal.SIGTERM)
            deadline = time.monotonic() + 5
            while True:
                try:
                    with lock(anchor):
                        break
                except BlockingIOError:
                    if time.monotonic() >= deadline:
                        self.fail("worker lease did not release after termination")
                    time.sleep(0.01)
