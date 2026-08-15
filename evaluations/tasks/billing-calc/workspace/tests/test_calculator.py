"""通话费用与账单总额计算测试。

覆盖：单笔费用、阶梯折扣、跨天/跨整点的分钟归属、舍入时机、合计。
"""

import unittest
from datetime import datetime

from billing.calculator import call_cost, total_cost
from billing.models import CallRecord


def _record(hour, minute, minutes, category="standard", day=10):
    return CallRecord(start=datetime(2026, 1, day, hour, minute),
                      minutes=minutes, category=category)


class SingleCallCostTest(unittest.TestCase):
    def test_standard_daytime(self):
        self.assertEqual(call_cost(_record(10, 0, 10)), 5.0)

    def test_premium_nighttime(self):
        self.assertEqual(call_cost(_record(23, 0, 2, "premium")), 0.8)

    def test_no_discount_below_threshold(self):
        self.assertEqual(call_cost(_record(10, 0, 180)), 90.0)

    def test_discount_tier_95_at_100(self):
        # 单笔原始费用恰好 100.0，命中 95 折。
        self.assertEqual(call_cost(_record(10, 0, 200)), 95.0)

    def test_discount_tier_90_at_200(self):
        # 单笔原始费用恰好 200.0，命中 9 折。
        self.assertEqual(call_cost(_record(10, 0, 400)), 180.0)


class RoundingTest(unittest.TestCase):
    def test_call_cost_rounds_once_on_total(self):
        """舍入应发生在「先累加再乘折扣」之后的单笔总额上。

        220 分钟 standard 白天：原始 110.0 × 0.95 = 104.5，精确为
        104.50。若逐分钟舍入（round(0.5*0.95)=0.47）再累加则会得到
        103.40，与本规格不符。
        """
        self.assertEqual(call_cost(_record(10, 0, 220)), 104.5)


class BoundaryMinuteTest(unittest.TestCase):
    def test_cross_midnight_call(self):
        # 23:50 起 30 分钟：整段落在夜间，且正确跨过午夜。
        self.assertEqual(call_cost(_record(23, 50, 30)), 7.5)

    def test_night_starts_at_2200_inclusive(self):
        # 21:55 起 10 分钟：21:55-21:59 为白天(5×0.5)，22:00-22:04 为夜间(5×0.25)。
        self.assertEqual(call_cost(_record(21, 55, 10)), 3.75)

    def test_exactly_0700_is_daytime(self):
        # 恰好 07:00 起 1 分钟：已离开夜间时段，按 daytime 0.5 计。
        self.assertEqual(call_cost(_record(7, 0, 1)), 0.5)

    def test_exactly_2200_one_minute_is_night(self):
        self.assertEqual(call_cost(_record(22, 0, 1)), 0.25)


class TotalCostTest(unittest.TestCase):
    def test_total_is_sum_of_rounded_costs(self):
        calls = [
            _record(10, 0, 7),     # 3.50
            _record(23, 0, 1),     # 0.25（夜间）
        ]
        self.assertEqual(total_cost(calls), 3.75)

    def test_total_does_not_re_round(self):
        # 各笔已舍入费用之和即为总额，不对合计二次舍入。
        calls = [
            _record(10, 0, 220),   # 104.50
            _record(10, 0, 1),     # 0.50
        ]
        self.assertEqual(total_cost(calls), 105.0)


if __name__ == "__main__":
    unittest.main()
