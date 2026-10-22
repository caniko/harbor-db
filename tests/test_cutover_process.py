"""Exercise the actual dispatcher, service-user workers and structured failure report."""

import json
import os
import pwd
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from harbor_db.durable import lock


class CutoverProcessTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.contract = self.root / "contract.json"
        self.source = self.root / "source"
        self.restore = self.root / "restore"
        self.authority = self.root / "authority"
        for path in (self.source, self.restore, self.authority):
            path.mkdir()
        for path in (self.source, self.restore):
            (path / "historical-object").write_bytes(b"a preserved historical object")
        self.manifest = {
            "version": 1,
            "enforced": True,
            "host": "fixture",
            "timeout_seconds": 5,
            "resources": {
                "archive": {
                    "kind": "filesystem",
                    "user": pwd.getpwuid(os.geteuid()).pw_name,
                    "runtime_units": [],
                    "max_age_seconds": 60,
                    "custody_file": str(self.authority / "custody.json"),
                    "authority": {
                        "resource": "archive",
                        "state_dir": str(self.authority),
                        "directories": [str(self.source)],
                        "binding": {"backend": "files"},
                    },
                }
            },
        }

    def run_command(self, *args):
        self.contract.write_text(json.dumps(self.manifest))
        return subprocess.run(
            [
                sys.executable,
                "-B",
                "-m",
                "harbor_db.cutover",
                *args,
                "--contract",
                str(self.contract),
                "--host",
                "fixture",
            ],
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        )

    def test_missing_adoption_is_structured_failure_and_cannot_initialize(self):
        result = self.run_command("check")
        self.assertEqual(result.returncode, 1, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["status"], "blocked")
        self.assertEqual(report["failures"][0]["resource"], "archive")
        self.assertFalse((self.authority / "identity.json").exists())

    def test_explicit_certification_and_all_read_only_phases_use_real_workers(self):
        result = self.run_command(
            "certify",
            "--resource",
            "archive",
            "--identity",
            "historical-corpus",
            "--restore-root",
            str(self.restore),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        before = {path: path.read_bytes() for path in self.authority.iterdir()}
        for phase in ("preflight", "activate", "startup"):
            with self.subTest(phase=phase):
                result = self.run_command("check", "--phase", phase)
                self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
                self.assertEqual(json.loads(result.stdout)["status"], "ready")
        self.assertEqual(before, {path: path.read_bytes() for path in before})

    def test_deployed_manifest_symlink_works_through_real_worker_dispatch(self):
        regular_contract = self.contract
        self.contract = self.root / "bundle" / "manifest.json"
        self.contract.parent.mkdir()
        self.contract.symlink_to(regular_contract)
        result = self.run_command(
            "certify",
            "--resource",
            "archive",
            "--identity",
            "historical-corpus",
            "--restore-root",
            str(self.restore),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        before = {path: path.read_bytes() for path in self.authority.iterdir()}
        for phase in ("preflight", "activate", "startup"):
            with self.subTest(phase=phase):
                result = self.run_command("check", "--phase", phase)
                self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
                self.assertEqual(json.loads(result.stdout)["status"], "ready")
        self.assertEqual(before, {path: path.read_bytes() for path in before})

    def test_removed_corpus_blocks_real_dispatch_without_replacement(self):
        (self.source / "historical-object").unlink()
        self.source.rmdir()
        result = self.run_command("check")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("missing", json.loads(result.stdout)["failures"][0]["reason"])
        self.assertFalse(self.source.exists())

    def test_real_writer_holds_original_lease_after_exec(self):
        result = self.run_command(
            "certify",
            "--resource",
            "archive",
            "--identity",
            "historical-corpus",
            "--restore-root",
            str(self.restore),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        ready = self.root / "writer-ready"
        command = [
            sys.executable,
            "-B",
            "-m",
            "harbor_db.cutover",
            "serve",
            "--contract",
            str(self.contract),
            "--host",
            "fixture",
            "--resource",
            "archive",
            "--",
            sys.executable,
            "-c",
            "import pathlib,sys,time; pathlib.Path(sys.argv[1]).touch(); time.sleep(10)",
            str(ready),
        ]
        with subprocess.Popen(
            command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        ) as process:
            try:
                deadline = time.monotonic() + 5
                while (
                    not ready.exists()
                    and time.monotonic() < deadline
                    and process.poll() is None
                ):
                    time.sleep(0.01)
                self.assertTrue(ready.exists())
                with self.assertRaises(BlockingIOError), lock(self.authority / "lock"):
                    self.fail("certification acquired the running writer's lease")
                result = self.run_command(
                    "certify",
                    "--resource",
                    "archive",
                    "--identity",
                    "historical-corpus",
                    "--restore-root",
                    str(self.restore),
                )
                self.assertNotEqual(result.returncode, 0)
            finally:
                process.terminate()
                process.communicate(timeout=5)

    def test_ssh_login_writer_retains_resource_lease(self):
        result = self.run_command(
            "certify",
            "--resource",
            "archive",
            "--identity",
            "historical-corpus",
            "--restore-root",
            str(self.restore),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.manifest["resources"]["archive"]["login_shell"] = sys.executable
        self.contract.write_text(json.dumps(self.manifest))
        ready = self.root / "ssh-writer-ready"
        launch = "from pathlib import Path; import sys; from harbor_db.login_shell import serve_login_shell; serve_login_shell(Path(sys.argv[1]), sys.argv[2:], host='fixture')"
        command = [
            sys.executable,
            "-B",
            "-c",
            launch,
            str(self.contract),
            "-c",
            "import pathlib,sys,time; pathlib.Path(sys.argv[1]).touch(); time.sleep(10)",
            str(ready),
        ]
        with subprocess.Popen(
            command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True
        ) as process:
            try:
                deadline = time.monotonic() + 5
                while (
                    not ready.exists()
                    and time.monotonic() < deadline
                    and process.poll() is None
                ):
                    time.sleep(0.01)
                self.assertTrue(ready.exists(), "guarded login command did not execute")
                with self.assertRaises(BlockingIOError), lock(self.authority / "lock"):
                    self.fail("certification acquired an SSH writer's lease")
            finally:
                process.terminate()
                process.communicate(timeout=5)

    def test_ssh_login_without_custody_cannot_run_a_command(self):
        self.manifest["resources"]["archive"]["login_shell"] = sys.executable
        self.contract.write_text(json.dumps(self.manifest))
        marker = self.root / "unadmitted-command"
        launch = "from pathlib import Path; import sys; from harbor_db.login_shell import serve_login_shell; serve_login_shell(Path(sys.argv[1]), sys.argv[2:], host='fixture')"
        result = subprocess.run(
            [
                sys.executable,
                "-B",
                "-c",
                launch,
                str(self.contract),
                "-c",
                "import pathlib,sys; pathlib.Path(sys.argv[1]).touch()",
                str(marker),
            ],
            capture_output=True,
            text=True,
            timeout=5,
            check=False,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertRegex(result.stderr, "identity|authority/lock")
        self.assertFalse(marker.exists())
        self.assertFalse((self.authority / "lock").exists())


if __name__ == "__main__":
    unittest.main()
