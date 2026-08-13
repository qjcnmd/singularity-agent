"""CLI 测试：默认输出与 --hourly 输出。"""

import os
import subprocess
import sys
import tempfile
import unittest

LOGGER = os.path.join(os.path.dirname(__file__), "..", "sample.log")

SAMPLE = """2026-08-14 09:15:30 INFO cache warm
2026-08-14 09:45:00 WARN retry once
2026-08-14 10:02:11 ERROR timeout
2026-08-14 10:30:00 INFO recovered
"""


def run_cli(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-m", "logstats.cli", *args],
        capture_output=True,
        text=True,
        cwd=os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
    )


class CliTest(unittest.TestCase):
    def test_default_output_lists_levels(self):
        result = run_cli(LOGGER)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("INFO: 2", result.stdout)
        self.assertIn("WARN: 1", result.stdout)
        self.assertIn("ERROR: 1", result.stdout)

    def test_hourly_output_lists_buckets(self):
        result = run_cli("--hourly", LOGGER)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("2026-08-14 09:00", result.stdout)
        self.assertIn("2026-08-14 10:00", result.stdout)

    def test_hourly_output_reports_levels_per_bucket(self):
        result = run_cli("--hourly", LOGGER)
        self.assertIn("INFO: 1", result.stdout)
        self.assertIn("ERROR: 1", result.stdout)


if __name__ == "__main__":
    unittest.main()
