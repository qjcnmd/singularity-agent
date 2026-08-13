"""发票计算：商品条目、折扣阶梯与税费。"""

from dataclasses import dataclass


@dataclass(frozen=True)
class LineItem:
    """发票中的一行商品：名称、单价、数量。"""

    name: str
    unit_price: float
    quantity: int


def subtotal(items: list[LineItem]) -> float:
    """所有行的小计（单价 × 数量求和）。"""
    return sum(item.unit_price * item.quantity for item in items)


# 折扣阶梯：满 500 打 95 折，满 1000 打 9 折，满 2000 打 85 折。
DISCOUNT_TIERS = [
    (2000, 0.85),
    (1000, 0.90),
    (500, 0.95),
]


def discount_rate(subtotal_amount: float) -> float:
    """按小计金额返回折扣率（1.0 表示无折扣）。"""
    for threshold, rate in DISCOUNT_TIERS:
        # 边界值必须命中对应档位（>= threshold）。
        if subtotal_amount > threshold:
            return rate
    return 1.0


def discounted_total(subtotal_amount: float) -> float:
    """应用折扣后的小计（保留两位小数）。"""
    return round(subtotal_amount * discount_rate(subtotal_amount), 2)
