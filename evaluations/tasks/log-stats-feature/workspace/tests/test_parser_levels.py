"""解析器与按级别统计测试（现有功能，全部通过）。"""

import unittest

from logstats.aggregate import count_by_level
from logstats.parser import parse_line, parse_lines


class ParseLineTest(unittest.TestCase):
    def test_valid_line(self):
        entry = parse_line("2026-08-14 09:15:30 INFO server started")
        self.assertIsNotNone(entry)
        assert entry is not None
        self.assertEqual(entry.level, "INFO")
        self.assertEqual(entry.message, "server started")
        self.assertEqual(entry.timestamp.hour, 9)

    def test_missing_parts_returns_none(self):
        self.assertIsNone(parse_line("2026-08-14 09:15:30"))

    def test_unknown_level_returns_none(self):
        self.assertIsNone(parse_line("2026-08-14 09:15:30 DEBUG detail here"))

    def test_bad_timestamp_returns_none(self):
        self.assertIsNone(parse_line("not-a-date INFO message"))

    def test_message_may_contain_spaces(self):
        entry = parse_line("2026-08-14 09:15:30 WARN slow query took 12 ms")
        assert entry is not None
        self.assertEqual(entry.message, "slow query took 12 ms")


class ParseLinesTest(unittest.TestCase):
    def test_skips_bad_lines(self):
        entries = parse_lines(
            [
                "2026-08-14 09:15:30 INFO ok",
                "garbage line",
                "2026-08-14 09:16:00 ERROR boom",
            ]
        )
        self.assertEqual(len(entries), 2)


class CountByLevelTest(unittest.TestCase):
    def test_counts_with_all_levels_present(self):
        entries = parse_lines(
            [
                "2026-08-14 09:15:30 INFO a",
                "2026-08-14 09:16:00 WARN b",
                "2026-08-14 09:17:00 ERROR c",
                "2026-08-14 09:18:00 INFO d",
            ]
        )
        self.assertEqual(count_by_level(entries), {"INFO": 2, "WARN": 1, "ERROR": 1})

    def test_missing_levels_report_zero(self):
        entries = parse_lines(["2026-08-14 09:15:30 INFO a"])
        self.assertEqual(count_by_level(entries), {"INFO": 1, "WARN": 0, "ERROR": 0})


if __name__ == "__main__":
    unittest.main()
