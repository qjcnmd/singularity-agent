"""按小时聚合测试（新功能，当前失败：实现缺失）。"""

import unittest

from logstats.aggregate import count_by_hour
from logstats.parser import parse_lines

SAMPLE = [
    "2026-08-14 09:15:30 INFO cache warm",
    "2026-08-14 09:45:00 WARN retry once",
    "2026-08-14 10:02:11 ERROR timeout",
    "2026-08-14 10:30:00 INFO recovered",
    "2026-08-14 10:59:59 INFO done",
    "2026-08-15 00:05:00 INFO new day",
]


class CountByHourTest(unittest.TestCase):
    def test_buckets_are_sorted_and_complete(self):
        entries = parse_lines(SAMPLE)
        buckets = count_by_hour(entries)
        self.assertEqual(
            [bucket for bucket, _ in buckets],
            ["2026-08-14 09:00", "2026-08-14 10:00", "2026-08-15 00:00"],
        )

    def test_level_counts_per_bucket(self):
        entries = parse_lines(SAMPLE)
        buckets = count_by_hour(entries)
        by_bucket = dict(buckets)
        self.assertEqual(
            by_bucket["2026-08-14 09:00"], {"INFO": 1, "WARN": 1, "ERROR": 0}
        )
        self.assertEqual(
            by_bucket["2026-08-14 10:00"], {"INFO": 2, "WARN": 0, "ERROR": 1}
        )
        self.assertEqual(
            by_bucket["2026-08-15 00:00"], {"INFO": 1, "WARN": 0, "ERROR": 0}
        )

    def test_bucket_boundary_minute(self):
        """10:59:59 属于 10:00 桶，09:59:59 属于 09:00 桶。"""
        entries = parse_lines(
            [
                "2026-08-14 09:59:59 INFO before",
                "2026-08-14 10:00:00 INFO at",
            ]
        )
        buckets = count_by_hour(entries)
        self.assertEqual(
            [bucket for bucket, _ in buckets],
            ["2026-08-14 09:00", "2026-08-14 10:00"],
        )

    def test_empty_input_returns_no_buckets(self):
        self.assertEqual(count_by_hour([]), [])


if __name__ == "__main__":
    unittest.main()
