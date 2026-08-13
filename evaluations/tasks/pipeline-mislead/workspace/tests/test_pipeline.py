"""数据管道测试：加载键清理、聚合与端到端 CLI。"""

import os
import subprocess
import sys
import unittest

from pipeline.aggregate import aggregate_by_date
from pipeline.loader import load_records

DATA_CSV = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "data", "sales.csv")


class LoadRecordsTest(unittest.TestCase):
    def test_record_keys_have_no_whitespace(self):
        """表头即使带前导空白，键也必须干净（date/product/count）。"""
        records = load_records(DATA_CSV)
        self.assertEqual(len(records), 3)
        for record in records:
            self.assertIn("date", record)
            self.assertIn("product", record)
            self.assertIn("count", record)

    def test_values_are_preserved(self):
        records = load_records(DATA_CSV)
        self.assertEqual(records[0]["date"], "2026-08-14")
        self.assertEqual(records[0]["count"], "3")


class AggregateTest(unittest.TestCase):
    def test_sums_counts_by_date(self):
        records = [
            {"date": "2026-08-14", "product": "book", "count": "3"},
            {"date": "2026-08-14", "product": "pen", "count": "2"},
            {"date": "2026-08-15", "product": "book", "count": "5"},
        ]
        self.assertEqual(aggregate_by_date(records), [("2026-08-14", 5.0), ("2026-08-15", 5.0)])

    def test_empty_input(self):
        self.assertEqual(aggregate_by_date([]), [])


class CliTest(unittest.TestCase):
    def run_cli(self):
        root = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
        return subprocess.run(
            [sys.executable, "-m", "pipeline.cli", "data/sales.csv"],
            capture_output=True,
            text=True,
            cwd=root,
        )

    def test_cli_outputs_daily_totals(self):
        result = self.run_cli()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("2026-08-14 5", result.stdout)
        self.assertIn("2026-08-15 5", result.stdout)


if __name__ == "__main__":
    unittest.main()
