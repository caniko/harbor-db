"""Hostile or stalled adapters cannot leak diagnostics or bypass bounded receipts."""

import sys
import unittest

from harbor_db import process


class ReceiptProcessTest(unittest.TestCase):
    def execute(self, code, **limits):
        return process.execute([sys.executable, "-B", "-c", code], timeout=limits.pop("timeout", 3),
                               environment={}, **limits)

    def test_failing_diagnostics_are_discarded_without_a_disk_spool(self):
        with self.assertRaisesRegex(ValueError, "diagnostics suppressed") as failure:
            self.execute("import sys; sys.stderr.write('fixture-secret'*100000); sys.exit(1)")
        self.assertNotIn("fixture-secret", str(failure.exception))

    def test_oversized_receipt_and_stalled_worker_fail_closed(self):
        with self.assertRaisesRegex(ValueError, "size limit"):
            self.execute("import os; os.write(1, b'x'*100000)", maximum_output=1024)
        with self.assertRaisesRegex(ValueError, "execution limit"):
            self.execute("import time; time.sleep(60)", timeout=0.1)

    def test_large_diagnostics_do_not_deadlock_a_valid_receipt(self):
        self.assertEqual(self.execute("import sys; sys.stderr.write('x'*2000000); sys.stdout.write('{}')"), b"{}")
