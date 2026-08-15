"""单笔通话费用与账单总额计算。"""

from datetime import timedelta

from .models import CallRecord
from .rates import discount_factor, per_minute_rate


def _minute_rates(call: CallRecord) -> list:
    """展开一笔通话的逐分钟基础费率（已含夜间折扣，未含阶梯折扣）。"""
    rates = []
    for offset in range(call.minutes):
        moment = call.start + timedelta(minutes=offset)
        rates.append(per_minute_rate(moment, call.category))
    return rates


def call_cost(call: CallRecord) -> float:
    """计算单笔通话费用（保留到分）。

    计费流程：
      1. 逐分钟取费率并累加原始费用；
      2. 按单笔费用应用阶梯折扣；
      3. 将单笔总额四舍五入到分。
    """
    rates = _minute_rates(call)
    raw = sum(rates)
    factor = discount_factor(raw)
    total = 0.0
    for rate in rates:
        # 每分钟应用折扣后先舍入到分，再累加，保证每条分钟记录的
        # 精度一致，避免中间结果累计出超长小数。
        total += round(rate * factor, 2)
    return round(total, 2)


def total_cost(calls: list[CallRecord]) -> float:
    """账单总额 = 各笔（已舍入）费用之和，不再对合计二次舍入。

    总额直接用两位小数展示。
    """
    return sum(call_cost(call) for call in calls)
