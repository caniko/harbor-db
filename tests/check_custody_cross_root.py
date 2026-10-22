"""Real bind-mount alias rejection in an isolated mount namespace or test VM."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

parser = argparse.ArgumentParser()
parser.add_argument("--native", required=True)
parser.add_argument("--root")
parser.add_argument("--engine", choices=("native", "python"), action="append")
args = parser.parse_args()
results = []
for engine in args.engine or ("native", "python"):
    with tempfile.TemporaryDirectory(dir=args.root) as temporary:
        base = Path(temporary)
        a, b, restored_a, restored_b, state = [
            base / n for n in ("a", "b", "restore-a", "restore-b", "state")
        ]
        for root in (a, b, restored_a, restored_b, state):
            root.mkdir(mode=0o700)
        for root in (a, b, restored_a, restored_b):
            (root / "empty").mkdir(mode=0o700)
            (root / "history").write_bytes(b"identical historical corpus")
        resource = {
            "kind": "filesystem",
            "user": "root",
            "runtime_units": [],
            "authority": {
                "resource": "crossroot",
                "state_dir": str(state),
                "directories": [str(a), str(b)],
                "binding": {"backend": "files"},
            },
            "custody_file": str(state / "custody.json"),
            "max_age_seconds": 60,
        }
        contract = base / "contract.json"
        contract.write_text(
            json.dumps(
                {
                    "version": 1,
                    "enforced": True,
                    "host": "fixture",
                    "timeout_seconds": 30,
                    "resources": {"crossroot": resource},
                }
            )
        )
        command = (
            [
                args.native,
                "certify-worker",
                "--contract",
                str(contract),
                "--host",
                "fixture",
                "--phase",
                "certify",
                "--worker",
                "crossroot",
                "--certify-identity",
                "history",
                "--certify-roots",
                str(restored_a),
                str(restored_b),
            ]
            if engine == "native"
            else [
                sys.executable,
                "-B",
                "-m",
                "harbor_db.cutover",
                "certify-worker",
                "--contract",
                str(contract),
                "--host",
                "fixture",
                "--phase",
                "certify",
                "--worker",
                "crossroot",
                "--certify-identity",
                "history",
                "--certify-roots",
                str(restored_a),
                str(restored_b),
            ]
        )
        # Bind mounts retain nlink=1 and pass path canonicalization. Include a
        # file-only and empty-directory alias so root identity checks alone
        # cannot make this regression pass.
        for name in ("", "history", "empty"):
            source, restore = (
                (b, restored_a) if not name else (b / name, restored_a / name)
            )
            subprocess.run(["mount", "--bind", str(source), str(restore)], check=True)
            try:
                assert os.stat(restore).st_ino == os.stat(source).st_ino
                assert os.stat(restored_a / "history").st_nlink == 1
                result = subprocess.run(
                    command, capture_output=True, text=True, timeout=30
                )
                assert result.returncode != 0 and "alias" in result.stderr, (
                    engine,
                    name,
                    result.stdout,
                    result.stderr,
                )
                assert not (state / "identity.json").exists()
                assert not (state / "custody.json").exists()
                assert not (a / ".harbor-db-crossroot-identity.json").exists()
                assert not (b / ".harbor-db-crossroot-identity.json").exists()
            finally:
                subprocess.run(["umount", str(restore)], check=True)
        # Disjoint copies with identical content remain certifiable.
        other, other_state = base / "other", base / "other-state"
        other.mkdir(mode=0o700)
        other_state.mkdir(mode=0o700)
        (other / "empty").mkdir(mode=0o700)
        (other / "history").write_bytes(b"identical historical corpus")
        peer = {
            **resource,
            "authority": {
                **resource["authority"],
                "resource": "other",
                "state_dir": str(other_state),
                "directories": [str(other)],
            },
            "custody_file": str(other_state / "custody.json"),
        }
        manifest = json.loads(contract.read_text())
        manifest["resources"]["other"] = peer
        contract.write_text(json.dumps(manifest))
        coordinator = (
            [args.native]
            if engine == "native"
            else [sys.executable, "-B", "-m", "harbor_db.cutover"]
        )
        coordinator += [
            "certify",
            "--contract",
            str(contract),
            "--host",
            "fixture",
            "--resource",
            "crossroot",
            "--identity",
            "history",
            "--restore-root",
            str(restored_a),
            "--restore-root",
            str(restored_b),
        ]
        for name in ("", "history", "empty"):
            source, restore = (
                (other, restored_a) if not name else (other / name, restored_a / name)
            )
            subprocess.run(["mount", "--bind", str(source), str(restore)], check=True)
            try:
                for invocation in (coordinator, command):
                    result = subprocess.run(
                        invocation, capture_output=True, text=True, timeout=30
                    )
                    assert result.returncode != 0 and "alias" in result.stderr, (
                        engine,
                        name,
                        invocation,
                        result.stdout,
                        result.stderr,
                    )
                    assert not (state / "identity.json").exists()
                    assert not (state / "custody.json").exists()
                    assert not (a / ".harbor-db-crossroot-identity.json").exists()
            finally:
                subprocess.run(["umount", str(restore)], check=True)
        # An unprivileged mapped-root namespace explicitly denies setgroups,
        # so it cannot dispatch a root credential-changing worker. Its alias
        # checks above finish before dispatch; validate independent copies with
        # the direct worker here. Real-root VMs exercise coordinator dispatch,
        # and the Rust CLI regression exercises it as the ordinary service user.
        groups_denied = Path("/proc/self/setgroups").read_text().strip() == "deny"
        result = subprocess.run(
            command if groups_denied else coordinator,
            capture_output=True,
            text=True,
            timeout=30,
        )
        assert result.returncode == 0, (engine, result.stdout, result.stderr)
        assert (state / "custody.json").is_file()
        results.append(
            {
                "engine": engine,
                "crossRootBindAliasRejected": True,
                "otherEnrolledCorpusRootFileAndEmptyDirectoryAliasesRejected": True,
                "directWorkerGlobalAliasesRejected": True,
                "independentCopyAcceptance": "direct-worker"
                if groups_denied
                else "coordinator",
                "fileOnlyAndEmptyDirectoryAliasesRejected": True,
                "noAliasPublication": True,
                "independentCopiesAccepted": True,
            }
        )
print(json.dumps({"status": "pass", "results": results}))
