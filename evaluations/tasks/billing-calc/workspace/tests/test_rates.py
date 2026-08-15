"""费率规则测试：基础费率与夜间时段的边界行为。"""

import unittest
from datetime import datetime

from billing.rates import (
    NIGHT_DISCOUNT,
    NIGHTLY_END_HOUR,
    NIGHTLY_START_HOUR,
    base_rate,
    discount_factor,
    is_night_minute,
    per_minute_rate,
)


def _moment(hour, minute=0):
    return datetime(2026, 1, 10, hour, minute)


class BaseRateTest(unittest.TestCase):
    def test_standard_and_premium_rates(self):
        self.assertEqual(base_rate("standard"), 0.5)
        self.assertEqual(base_rate("premium"), 0.8)


class NightMinuteTest(unittest.TestCase):
    def test_evening_before_night_start_is_daytime(self):
        self.assertFalse(is_night_minute(_moment(21, 59)))

    def test_night_starts_at_2200_inclusive(self):
        # 恰好 22:00 整点开始即进入夜间折扣时段。
        self.assertTrue(is_night_minute(_moment(NIGHTLY_START_HOUR, 0)))

    def test_late_evening_and_midnight_are_night(self):
        self.assertTrue(is_night_minute(_moment(23, 0)))
        self.assertTrue(is_night_minute(_moment(0, 30)))
        self.assertTrue(is_night_minute(_moment(5, 0)))

    def test_just_before_seven_is_night(self):
        self.assertTrue(is_night_minute(_moment(6, 59)))

    def test_exactly_seven_is_daytime(self):
        # 07:00 整点即离开夜间折扣时段，应按正常费率计费。
        self.assertFalse(is_night_minute(_moment(NIGHTLY_END_HOUR, 0)))

    def test_morning_after_seven_is_daytime(self):
        self.assertFalse(is_night_minute(_moment(8, 30)))
        self.assertFalse(is_night_minute(_moment(12, 0)))

    def test_night_discount_value(self):
        self.assertEqual(NIGHT_DISCOUNT, 0.5)


class PerMinuteRateTest(unittest.TestCase):
    def test_daytime_uses_base_rate(self):
        self.assertEqual(per_minute_rate(_moment(10, 0), "standard"), 0.5)
        self.assertEqual(per_minute_rate(_moment(10, 0), "premium"), 0.8)

    def test_nighttime_is_half_price(self):
        self.assertEqual(per_minute_rate(_moment(23, 0), "standard"), 0.25)
        self.assertEqual(per_minute_rate(_moment(0, 30), "premium"), 0.4)


class DiscountFactorTest(unittest.TestCase):
    def test_below_first_tier_has_no_discount(self):
        self.assertEqual(discount_factor(99.99), 1.0)

    def test_tier_thresholds_are_inclusive(self):
        self.assertEqual(discount_factor(100.0), 0.95)
        self.assertEqual(discount_factor(200.0), 0.90)

    def test_just_below_threshold(self):
        self.assertEqual(discount_factor(99.99), 1.0)
        self.assertEqual(discount_factor(199.99), 0.95)

    def test_above_highest_tier(self):
        self.assertEqual(discount_factor(500.0), 0.90)


if __name__ == "__main__":
    unittest.main()
