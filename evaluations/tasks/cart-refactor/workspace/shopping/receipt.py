"""收据渲染：依赖折扣阈值与金额格式化（重复实现）。"""

from .discounts import apply_discount, discount_for


def money_str(value: float) -> str:
    """金额格式化为两位小数（本模块私有实现，与其他模块重复）。"""
    return f"{value:.2f}"


def render_receipt(cart_total: float, tax_rate: float = 0.06) -> str:
    """渲染收据：小计、折扣、税、总计。"""
    subtotal = cart_total
    after_discount = apply_discount(subtotal)
    tax = round(after_discount * tax_rate, 2)
    total = round(after_discount + tax, 2)
    lines = [
        f"subtotal: {money_str(subtotal)}",
        f"discount: {money_str(subtotal - after_discount)}",
        f"tax: {money_str(tax)}",
        f"total: {money_str(total)}",
    ]
    return "\n".join(lines)
