"""Declared application privileges against a real disposable PostgreSQL cluster."""

import os
import pwd
import subprocess
import tempfile
import unittest
from pathlib import Path

from harbor_db import provision


class ProvisionPolicyTest(unittest.TestCase):
    def test_identifiers_and_privileges_are_validated_before_sql(self):
        policy = {"database": "demo", "owner_role": "demo_owner", "runtime_role": "demo_runtime"}
        for changed in ({"database": "demo; DROP DATABASE postgres"},
                        {"owner_role": "demo_runtime"}, {"runtime_role": "postgres"},
                        {"tables": {"history": ["UPDATE; SELECT"]}}):
            with self.subTest(changed=changed), self.assertRaises(ValueError):
                provision.validate(policy | changed)


class ApplicationProvisionTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.package = Path(os.environ["HARBOR_DB_TEST_POSTGRES"])
        cls.temp = tempfile.TemporaryDirectory()
        cls.root = Path(cls.temp.name)
        cls.socket = cls.root / "socket"
        cls.socket.mkdir(mode=0o700)
        cls.user = pwd.getpwuid(os.getuid()).pw_name
        cls.data = cls.root / "data"
        cls.command([cls.package / "bin/initdb", "-D", cls.data, "-U", cls.user,
                 "-A", "trust", "--locale=C", "--encoding=UTF8"])
        cls.command([cls.package / "bin/pg_ctl", "-D", cls.data, "-l", cls.root / "postgres.log",
                 "-o", f"-k {cls.socket} -c listen_addresses= -p 55441", "-w", "start"])
        cls.config = {"database": "demo", "owner_role": "demo_owner", "runtime_role": "demo_runtime",
                      "schema": "public", "table_privileges": ["SELECT"],
                      "sequence_privileges": ["USAGE", "SELECT"],
                      "tables": {"documents": ["SELECT", "INSERT", "UPDATE", "DELETE"],
                                 "history": ["SELECT", "INSERT"]}}
        cls.endpoint = {"package": str(cls.package), "socket_dir": str(cls.socket),
                        "port": 55441, "control_role": cls.user, "lock_file": str(cls.root / "provision.lock")}
        (cls.root / "provision.lock").touch(mode=0o600)
        # Version-two admission requires the declared schema. Preserve all
        # original assertions with an explicit fixture instead of relying on
        # another test method to have created these tables first.
        provision.apply(cls.config, cls.endpoint)
        cls.command([cls.package / "bin/psql", "-X", "-w", "-v", "ON_ERROR_STOP=1",
                     "-h", cls.socket, "-p", 55441, "-U", "demo_owner", "-d", "demo",
                     "-c", "CREATE TABLE documents(id int); CREATE TABLE history(id int)"])
        provision.reconcile(cls.config, cls.endpoint)

    @classmethod
    def command(cls, argv, **kwargs):
        return subprocess.run(list(map(str, argv)), capture_output=True, text=True,
                              check=True, timeout=30, **kwargs)

    @classmethod
    def tearDownClass(cls):
        try:
            cls.command([cls.package / "bin/pg_ctl", "-D", cls.data, "-m", "fast", "-w", "stop"])
        finally:
            cls.temp.cleanup()

    def sql(self, statement, *, role=None, database="demo", check=True):
        return subprocess.run(list(map(str, [self.package / "bin/psql", "-X", "-w", "-At",
            "-v", "ON_ERROR_STOP=1", "-h", self.socket, "-p", 55441,
            "-U", role or self.user, "-d", database, "-c", statement])),
            capture_output=True, text=True, check=check, timeout=10)

    def test_idempotent_provision_and_exact_current_and_future_privileges(self):
        provision.apply(self.config, self.endpoint)
        provision.apply(self.config, self.endpoint)
        self.sql("CREATE TABLE IF NOT EXISTS documents(id int); CREATE TABLE IF NOT EXISTS history(id int);"
                 "CREATE TABLE IF NOT EXISTS immutable(id int); CREATE SEQUENCE IF NOT EXISTS cursor;",
                 role="demo_owner")
        provision.apply(self.config, self.endpoint)
        self.assertTrue(provision.check(self.config, self.endpoint))
        self.sql("INSERT INTO documents VALUES (1); UPDATE documents SET id=2; DELETE FROM documents;"
                 "INSERT INTO history VALUES (1); SELECT nextval('cursor');", role="demo_runtime")
        for statement in ("CREATE TABLE unauthorized(id int)", "UPDATE history SET id=2",
                          "DELETE FROM history", "INSERT INTO immutable VALUES (1)",
                          "SET ROLE demo_owner", "CREATE ROLE unauthorized"):
            with self.subTest(statement=statement):
                self.assertNotEqual(self.sql(statement, role="demo_runtime", check=False).returncode, 0)
        self.sql("CREATE TABLE future(id int)", role="demo_owner")
        self.sql("SELECT * FROM future", role="demo_runtime")
        self.assertNotEqual(self.sql("INSERT INTO future VALUES (1)", role="demo_runtime", check=False).returncode, 0)
        self.sql("GRANT UPDATE ON history TO demo_runtime", role="demo_owner")
        self.assertFalse(provision.check(self.config, self.endpoint))
        provision.apply(self.config, self.endpoint)
        self.assertTrue(provision.check(self.config, self.endpoint))

    def test_read_only_check_detects_default_column_and_grant_option_drift(self):
        provision.apply(self.config, self.endpoint)
        self.sql("CREATE TABLE IF NOT EXISTS history(id int)", role="demo_owner")
        statements = [
            "ALTER DEFAULT PRIVILEGES FOR ROLE demo_owner IN SCHEMA public GRANT UPDATE ON TABLES TO demo_runtime",
            "GRANT UPDATE(id) ON history TO demo_runtime",
            "GRANT SELECT ON history TO demo_runtime WITH GRANT OPTION",
            "GRANT MAINTAIN ON history TO demo_runtime",
        ]
        for statement in statements:
            with self.subTest(statement=statement):
                self.sql(statement)
                self.assertFalse(provision.check(self.config, self.endpoint))
                # A check must report the same drift twice, without repairing it.
                self.assertFalse(provision.check(self.config, self.endpoint))
                provision.apply(self.config, self.endpoint)
                self.assertTrue(provision.check(self.config, self.endpoint))

    def test_database_owned_by_someone_else_is_not_taken_over(self):
        self.sql("CREATE DATABASE foreign_database", database="postgres")
        try:
            with self.assertRaisesRegex(ValueError, "ownership"):
                provision.apply(self.config | {"database": "foreign_database"}, self.endpoint)
            self.assertEqual(self.sql("SELECT pg_get_userbyid(datdba) FROM pg_database "
                                     "WHERE datname='foreign_database'", database="postgres").stdout.strip(), self.user)
        finally:
            self.sql("DROP DATABASE foreign_database", database="postgres")

    def test_foreign_schema_is_not_taken_over_and_database_create_drift_is_detected(self):
        provision.apply(self.config, self.endpoint)
        self.sql("CREATE SCHEMA foreign_schema")
        try:
            with self.assertRaisesRegex(ValueError, "ownership"):
                provision.apply(self.config | {"schema": "foreign_schema"}, self.endpoint)
            self.assertEqual(self.sql("SELECT pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname='foreign_schema'").stdout.strip(), self.user)
        finally:
            self.sql("DROP SCHEMA foreign_schema")
        self.sql("GRANT CREATE ON DATABASE demo TO demo_runtime")
        self.assertFalse(provision.check(self.config, self.endpoint))
        provision.apply(self.config, self.endpoint)
        self.assertTrue(provision.check(self.config, self.endpoint))

    def test_owner_role_membership_cannot_be_granted_to_another_login(self):
        provision.apply(self.config, self.endpoint)
        self.sql("CREATE ROLE outsider LOGIN; GRANT demo_owner TO outsider")
        try:
            self.assertFalse(provision.check(self.config, self.endpoint))
            with self.assertRaisesRegex(ValueError, "membership"):
                provision.apply(self.config, self.endpoint)
        finally:
            self.sql("REVOKE demo_owner FROM outsider; DROP ROLE outsider")

    def test_preexisting_runtime_role_membership_is_rejected(self):
        provision.apply(self.config, self.endpoint)
        self.sql("GRANT demo_owner TO demo_runtime", database="postgres")
        try:
            with self.assertRaisesRegex(ValueError, "membership"):
                provision.apply(self.config, self.endpoint)
        finally:
            self.sql("REVOKE demo_owner FROM demo_runtime", database="postgres")


if __name__ == "__main__":
    unittest.main()
