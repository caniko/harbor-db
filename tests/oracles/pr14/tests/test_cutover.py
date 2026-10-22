"""Cutover admission must never turn missing historical data into fresh state."""

import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from harbor_db import cutover
from harbor_db.durable import lock


class FilesystemCutoverTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name)
        self.source = self.directory / "repositories"
        self.restored = self.directory / "restored"
        self.state = self.directory / "authority"
        for root in (self.source, self.restored, self.state):
            root.mkdir()
        for root in (self.source, self.restored):
            (root / "historical.git").mkdir()
            (root / "historical.git/HEAD").write_text("ref: refs/heads/trunk\n")
            (root / "historical.git/objects").mkdir()
            (root / "historical.git/objects/history").write_bytes(b"historical objects")
        self.config = {
            "kind": "filesystem", "user": "root", "runtime_units": [],
            "authority": {
                "resource": "forgejo", "state_dir": str(self.state),
                "directories": [str(self.source)], "binding": {"backend": "postgres"},
            },
            "custody_file": str(self.state / "custody.json"),
            "max_age_seconds": 60,
        }

    def certify(self):
        cutover.certify_filesystem(self.config, [str(self.restored)], "verified-corpus", now=100)

    def test_missing_source_is_rejected_without_creation(self):
        self.config["authority"]["directories"] = [str(self.directory / "missing")]
        with self.assertRaisesRegex(ValueError, "missing"):
            cutover.check_resource(self.config, phase="preflight", now=100)
        self.assertFalse((self.directory / "missing").exists())

    def test_empty_substitute_cannot_be_certified(self):
        empty = self.directory / "empty"
        empty.mkdir()
        self.config["authority"]["directories"] = [str(empty)]
        with self.assertRaisesRegex(ValueError, "empty"):
            self.certify()
        self.assertFalse((self.state / "identity.json").exists())

    def test_same_count_and_size_are_not_corpus_equality(self):
        (self.restored / "historical.git/objects/history").write_bytes(b"HISTORICAL OBJECTS")
        with self.assertRaisesRegex(ValueError, "differ"):
            self.certify()
        self.assertFalse((self.state / "identity.json").exists())

    def test_matching_restore_is_adopted_and_check_is_read_only(self):
        self.certify()
        before = {path: path.read_bytes() for path in self.state.iterdir() if path.is_file()}
        cutover.check_resource(self.config, phase="preflight", now=101)
        cutover.check_resource(self.config, phase="activate", now=101)
        self.assertEqual(before, {path: path.read_bytes() for path in before})

    def test_replaced_corpus_is_rejected_even_with_preserved_mtime(self):
        self.certify()
        path = self.source / "historical.git/objects/history"
        previous = path.stat()
        path.write_bytes(b"HISTORICAL OBJECTS")
        os.utime(path, ns=(previous.st_atime_ns, previous.st_mtime_ns))
        with self.assertRaisesRegex(ValueError, "changed|differ"):
            cutover.check_resource(self.config, phase="activate", now=101)

    def test_expired_receipt_blocks_before_build(self):
        self.certify()
        with self.assertRaisesRegex(ValueError, "stale|expired"):
            cutover.check_resource(self.config, phase="preflight", now=161)

    def test_early_pass_does_not_authorize_later_changed_storage(self):
        self.certify()
        cutover.check_resource(self.config, phase="preflight", now=101)
        (self.source / "historical.git/HEAD").unlink()
        with self.assertRaises(ValueError):
            cutover.check_resource(self.config, phase="activate", now=102)

    def test_unknown_manifest_version_and_duplicate_resources_fail_closed(self):
        for manifest in ({"version": 2}, {"version": 1, "host": "atlas", "resources": []}):
            with self.assertRaises(ValueError):
                cutover.validate_manifest(manifest, "atlas")

    def test_active_writer_prevents_custody_certification(self):
        self.config["runtime_units"] = ["forgejo.service"]
        with patch.object(cutover, "writer_active", return_value=True), self.assertRaisesRegex(ValueError, "writer"):
            self.certify()
        self.assertFalse((self.state / "identity.json").exists())

    def test_receipt_cannot_be_rebound_to_another_backend(self):
        self.certify()
        self.config["authority"]["binding"]["backend"] = "sqlite"
        with self.assertRaisesRegex(ValueError, "binding|authority"):
            cutover.check_resource(self.config, phase="preflight", now=101)

    def test_routine_restart_after_writes_does_not_require_a_new_restore(self):
        self.certify()
        (self.source / "historical.git/objects/history").write_bytes(b"ordinary new application state")
        cutover.check_resource(self.config, phase="startup", now=1000)
        with self.assertRaisesRegex(ValueError, "stale"):
            cutover.check_resource(self.config, phase="preflight", now=1000)

    def test_certification_excludes_actual_resource_writers_until_publication(self):
        original = cutover.inventory
        def inspect(config, *, contents):
            with self.assertRaises(BlockingIOError), lock(self.state / "lock", shared=True):
                self.fail("writer obtained authority during certification")
            return original(config, contents=contents)
        with patch.object(cutover, "inventory", side_effect=inspect):
            self.certify()

    def test_independent_restore_cannot_be_a_hardlink_alias(self):
        restored = self.restored / "historical.git/objects/history"
        restored.unlink()
        restored.hardlink_to(self.source / "historical.git/objects/history")
        with self.assertRaisesRegex(ValueError, "aliases"):
            self.certify()

    def test_walk_permission_error_cannot_hide_part_of_the_corpus(self):
        def inaccessible(*args, **kwargs):
            kwargs["onerror"](PermissionError("inaccessible historical objects"))
        with patch.object(cutover.os, "walk", side_effect=inaccessible), self.assertRaises(PermissionError):
            self.certify()

    def test_started_writer_during_certification_prevents_adoption(self):
        self.config["runtime_units"] = ["forgejo.service"]
        with patch.object(cutover, "writer_active", side_effect=[False, True]), self.assertRaisesRegex(ValueError, "writer"):
            self.certify()
        self.assertFalse((self.state / "identity.json").exists())

    def test_copied_marker_cannot_rebind_a_replaced_root(self):
        self.certify()
        self.source.rename(self.directory / "old-source")
        self.restored.rename(self.source)
        marker = ".harbor-db-forgejo-identity.json"
        shutil.copy(self.directory / "old-source" / marker, self.source / marker)
        with self.assertRaisesRegex(ValueError, "binding"):
            cutover.check_resource(self.config, phase="startup", now=101)

    def test_symlink_corpus_content_cannot_be_certified(self):
        path = self.source / "historical.git/objects/history"
        path.unlink()
        path.symlink_to(self.restored / "historical.git/objects/history")
        with self.assertRaisesRegex(ValueError, "redirected"):
            self.certify()

    def test_invalid_enrollment_cannot_silently_accept_an_empty_authority(self):
        base = {"version": 1, "enforced": True, "host": "atlas"}
        invalid = {**self.config, "authority": {**self.config["authority"], "directories": []}}
        with self.assertRaises(ValueError):
            cutover.validate_manifest({**base, "resources": {"forgejo": invalid}}, "atlas")

    def test_matching_corpora_cannot_omit_a_database_repository(self):
        requirement = {"root": 0, "path": "other.git/HEAD", "directory": False}
        with self.assertRaisesRegex(ValueError, "database references"):
            cutover.certify_filesystem(self.config, [str(self.restored)], "historical",
                                      now=100, database_requirements=[requirement])
        self.assertFalse((self.state / "identity.json").exists())

    def test_matching_files_must_match_database_size_and_content_hash(self):
        path = "historical.git/objects/history"
        requirements = [
            {"root": 0, "path": path, "directory": False, "size": 999},
            {"root": 0, "path": path, "directory": False, "sha256": "0" * 64},
        ]
        for requirement in requirements:
            with self.subTest(requirement=requirement), self.assertRaisesRegex(ValueError, "database.*(size|hash)"):
                cutover.certify_filesystem(self.config, [str(self.restored)], "historical", now=100,
                                          database_requirements=[requirement])
        self.assertFalse((self.state / "identity.json").exists())

    def test_matching_corpora_cannot_hide_missing_git_history(self):
        repository = self.source / "complete.git"
        subprocess.run(["git", "init", "--bare", str(repository)], check=True, capture_output=True)
        def git(*args, input=None):
            return subprocess.check_output(["git", f"--git-dir={repository}", *args], input=input).strip()
        blob = git("hash-object", "-w", "--stdin", input=b"historical tree content")
        tree = git("mktree", input=b"100644 blob " + blob + b"\thistory\n")
        git("update-ref", "refs/tags/historical-tree", tree.decode())
        shutil.copytree(repository, self.restored / "complete.git")
        self.config["git_executable"] = shutil.which("git")
        requirement = {"root": 0, "path": "complete.git", "directory": True,
                       "git_repository": True, "git_has_commits": False}
        cutover.certify_filesystem(self.config, [str(self.restored)], "historical", now=100,
                                  database_requirements=[requirement])
        previous = (self.state / "custody.json").read_bytes()
        object_path = "objects/" + blob[:2].decode() + "/" + blob[2:].decode()
        for root in (repository, self.restored / "complete.git"):
            (root / object_path).unlink()
        with self.assertRaisesRegex(ValueError, "Git.*integrity"):
            cutover.certify_filesystem(self.config, [str(self.restored)], "historical", now=100,
                                      database_requirements=[requirement])
        self.assertEqual((self.state / "custody.json").read_bytes(), previous)

    def test_database_nonempty_repository_cannot_be_an_empty_bare_substitute(self):
        repository = self.source / "empty.git"
        subprocess.run(["git", "init", "--bare", str(repository)], check=True, capture_output=True)
        shutil.copytree(repository, self.restored / "empty.git")
        self.config["git_executable"] = shutil.which("git")
        requirement = {"root": 0, "path": "empty.git", "directory": True,
                       "git_repository": True, "git_has_commits": True}
        with self.assertRaisesRegex(ValueError, "Git.*integrity"):
            cutover.certify_filesystem(self.config, [str(self.restored)], "historical", now=100,
                                      database_requirements=[requirement])
        self.assertFalse((self.state / "identity.json").exists())

    def test_partial_repository_cannot_claim_complete_history(self):
        repository = self.source / "partial.git"
        subprocess.run(["git", "init", "--bare", str(repository)], check=True, capture_output=True)
        subprocess.run(["git", f"--git-dir={repository}", "config", "remote.origin.promisor", "true"],
                       check=True, capture_output=True)
        shutil.copytree(repository, self.restored / "partial.git")
        self.config["git_executable"] = shutil.which("git")
        requirement = {"root": 0, "path": "partial.git", "directory": True, "git_repository": True}
        with self.assertRaisesRegex(ValueError, "Git.*partial"):
            cutover.certify_filesystem(self.config, [str(self.restored)], "historical", now=100,
                                      database_requirements=[requirement])
        self.assertFalse((self.state / "identity.json").exists())

    def test_restore_root_permissions_are_part_of_custody(self):
        self.source.chmod(0o750)
        self.restored.chmod(0o700)
        with self.assertRaisesRegex(ValueError, "root.*mode"):
            self.certify()
        self.assertFalse((self.state / "identity.json").exists())

    def test_startup_uses_authority_roots_during_application_file_turnover(self):
        self.certify()
        # Ordinary application maintenance can temporarily leave only the root
        # authority marker. Startup is an identity check, not a new custody proof.
        shutil.rmtree(self.source / "historical.git")
        cutover.check_resource(self.config, phase="startup", now=100)
        with self.assertRaisesRegex(ValueError, "empty"):
            cutover.check_resource(self.config, phase="preflight", now=100)

    def test_promisor_marker_cannot_claim_complete_history(self):
        repository = self.source / "promisor.git"
        subprocess.run(["git", "init", "--bare", str(repository)], check=True, capture_output=True)
        (repository / "objects/pack/incomplete.promisor").touch()
        shutil.copytree(repository, self.restored / "promisor.git")
        self.config["git_executable"] = shutil.which("git")
        requirement = {"root": 0, "path": "promisor.git", "directory": True, "git_repository": True}
        with self.assertRaisesRegex(ValueError, "Git.*partial"):
            cutover.certify_filesystem(self.config, [str(self.restored)], "historical", now=100,
                                      database_requirements=[requirement])
        self.assertFalse((self.state / "identity.json").exists())

    def test_database_corpus_requirements_reject_traversal_and_wrong_root(self):
        inventories = cutover.inventory(self.config, contents=False)
        for path, root in (("../history", 0), ("/absolute", 0), ("historical.git", 5)):
            with self.subTest(path=path, root=root), self.assertRaisesRegex(ValueError, "invalid"):
                cutover.require_database_paths(inventories, [{"root": root, "path": path, "directory": True}])

    def test_invalid_dependency_and_unbounded_timeouts_are_rejected(self):
        base = {"version": 1, "enforced": True, "host": "atlas", "resources": {"forgejo": self.config}}
        for key, value in (("timeout_seconds", 0), ("timeout_seconds", 999999), ("activation_timeout_seconds", -1)):
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                cutover.validate_manifest({**base, key: value}, "atlas")
        self.config["database_resource"] = "unregistered"
        with self.assertRaises(ValueError):
            cutover.validate_manifest(base, "atlas")

    def test_incompatible_schema_blocks_the_early_postgres_phase(self):
        from harbor_db import postgres, recovery
        from harbor_db.durable import write_json
        path = self.directory / "postgres.json"
        write_json(path, {"recovery": {"system_identifier": "12345", "snapshot_file": str(self.directory / "snapshot")}})
        config = {"kind": "postgres", "config": str(path),
                  "compatibility_checks": [{"database": "app", "sql": "SELECT schema_version BETWEEN 86 AND 87"}]}
        with patch.object(postgres, "check"), patch.object(postgres, "reject_upgrade"), patch.object(postgres, "inspect_live"), \
                patch.object(recovery, "policy", side_effect=lambda item: item["recovery"]), \
                patch.object(recovery, "query", return_value="f\n"), self.assertRaisesRegex(ValueError, "compatibility"):
            cutover.check_resource(config, phase="preflight", now=101)


if __name__ == "__main__":
    unittest.main()
