"""折扣计算：阈值硬编码在本模块与 receipt 模块中重复。"""


def fmt_money(value: float) -> str:
    """金额格式化为两位小数（本模块私有实现，与其他模块重复）。"""
    return f"{value:.2f}"


def discount_for(subtotal: float) -> float:
    """按小计返回折扣率（1.0 表示无折扣）。

    阈值：满 500 打 9 折，满 100 打 95 折（阈值散落硬编码）。
    """
    if subtotal >= 500:
        return 0.9
    if subtotal >= 100:
        return 0.95
    return 1.0


def apply_discount(subtotal: float) -> float:
    """返回折扣后金额（两位小数）。"""
    return round(subtotal * discount_for(subtotal), 2)


def discount_line(subtotal: float) -> str:
    """折扣说明行，如 `discount 10% (50.00)`。"""
    rate = discount_for(subtotal)
    if rate >= 1.0:
        return "no discount"
    saved = subtotal - apply_discount(subtotal)
    return f"discount {int((1 - rate) * 100)}% ({fmt_money(saved)})"
