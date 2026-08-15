"""报表测试：日期归一化与按日期排序的变动历史。"""

import unittest

from warehouse.models import LedgerEntry
from warehouse.report import movement_history, normalize_date


class NormalizeDateTest(unittest.TestCase):
    def test_padded_form_stays(self):
        self.assertEqual(normalize_date("2026-08-02"), "2026-08-02")

    def test_unpadded_form(self):
        self.assertEqual(normalize_date("2026-8-2"), "2026-08-02")

    def test_mixed_padding(self):
        self.assertEqual(normalize_date("2026-8-02"), "2026-08-02")

    def test_invalid_returns_raw(self):
        self.assertEqual(normalize_date("not-a-date"), "not-a-date")


class MovementHistoryTest(unittest.TestCase):
    def test_mixed_date_formats_sort_chronologically(self):
        """2026-8-2 与 2026-08-10 混用时应按真实时间排序。"""
        records = [
            ("2026-8-2", LedgerEntry("A", 5, "IN")),
            ("2026-08-01", LedgerEntry("A", 1, "IN")),
            ("2026-08-10", LedgerEntry("A", 2, "IN")),
        ]
        rows = movement_history(records)
        dates = [r[0] for r in rows]
        self.assertEqual(dates, ["2026-08-01", "2026-08-02", "2026-08-10"])

    def test_includes_kind_and_qty(self):
        records = [("2026-08-01", LedgerEntry("A", 3, "OUT"))]
        self.assertEqual(
            movement_history(records),
            [("2026-08-01", "A", 3, "OUT")],
        )


if __name__ == "__main__":
    unittest.main()
