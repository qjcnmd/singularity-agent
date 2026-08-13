"""定价逻辑测试：小计、折扣阶梯与折扣后金额。"""

import unittest

from invoice.pricing import (
    DISCOUNT_TIERS,
    LineItem,
    discount_rate,
    discounted_total,
    subtotal,
)


class SubtotalTest(unittest.TestCase):
    def test_single_item(self):
        items = [LineItem("lamp", 19.99, 2)]
        self.assertEqual(subtotal(items), 39.98)

    def test_multiple_items(self):
        items = [
            LineItem("book", 12.5, 3),
            LineItem("pen", 1.25, 4),
        ]
        self.assertAlmostEqual(subtotal(items), 42.5, places=2)


class DiscountRateTest(unittest.TestCase):
    def test_below_first_tier_has_no_discount(self):
        self.assertEqual(discount_rate(499.99), 1.0)

    def test_tier_boundary_values_hit_the_tier(self):
        """恰好达到门槛的金额必须命中对应档位（>= 语义）。"""
        self.assertEqual(discount_rate(500.0), 0.95)
        self.assertEqual(discount_rate(1000.0), 0.90)
        self.assertEqual(discount_rate(2000.0), 0.85)

    def test_just_above_previous_tier(self):
        self.assertEqual(discount_rate(999.99), 0.95)
        self.assertEqual(discount_rate(1999.99), 0.90)

    def test_above_highest_tier(self):
        self.assertEqual(discount_rate(2500.0), 0.85)

    def test_tiers_are_sorted_descending(self):
        thresholds = [threshold for threshold, _ in DISCOUNT_TIERS]
        self.assertEqual(thresholds, sorted(thresholds, reverse=True))


class DiscountedTotalTest(unittest.TestCase):
    def test_discounted_total_rounds_to_cents(self):
        self.assertEqual(discounted_total(1000.0), 900.0)
        self.assertEqual(discounted_total(500.0), 475.0)
        self.assertEqual(discounted_total(42.5), 42.5)

    def test_discounted_total_with_odd_amount(self):
        self.assertEqual(discounted_total(1000.01), 900.01)


if __name__ == "__main__":
    unittest.main()
