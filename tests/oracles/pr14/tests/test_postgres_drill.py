"""The common restore adapter uses a real isolated PostgreSQL cluster."""

import os
import subprocess
import tempfile
import unittest
from pathlib import Path

from harbor_db import postgres_drill


class PostgresDrillTest(unittest.TestCase):
    def test_logical_restore_and_cleanup_use_only_the_private_endpoint(self):
        package = Path(os.environ["HARBOR_DB_TEST_POSTGRES"])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, socket, backup, workspace = [root / name for name in ("source", "source-socket", "backup", "workspace")]
            for directory in (socket, backup, workspace):
                directory.mkdir(mode=0o700)
            def run(program, argv):
                return subprocess.run([str(package / "bin" / program), *map(str, argv)],
                                      capture_output=True, check=True, timeout=30)
            run("initdb", ["-D", source, "--locale=C", "--encoding=UTF8", "--auth=trust"])
            run("pg_ctl", ["-D", source, "-l", root / "source.log", "-o", f"-k {socket} -p 55440 -c listen_addresses=", "-w", "start"])
            try:
                run("psql", ["-h", socket, "-p", "55440", "-d", "postgres", "-c", "CREATE TABLE records(id int, revision bigint); INSERT INTO records VALUES (1,7)"])
                run("pg_dump", ["-h", socket, "-p", "55440", "-d", "postgres", "--format=custom", "--file", backup / "database.dump"])
            finally:
                run("pg_ctl", ["-D", source, "-m", "fast", "-w", "stop"])
            try:
                postgres_drill.operate(package, "restore", backup, workspace)
                result = run("psql", ["-X", "-At", "-h", workspace / "socket", "-p", "55439", "-d", "harbor_restore", "-c", "SELECT id,revision FROM records"])
                self.assertEqual(result.stdout.strip(), b"1|7")
                with self.assertRaisesRegex(ValueError, "new workspace"):
                    postgres_drill.operate(package, "restore", backup, workspace)
            finally:
                postgres_drill.operate(package, "cleanup", backup, workspace)
            self.assertFalse((workspace / "cluster" / "postmaster.pid").exists())
            postgres_drill.operate(package, "cleanup", backup, workspace)
