"""税率表与税费计算。

税费按百分比税率计算，结果保留两位小数，**四舍五入**（half-up）。
"""

# 标准税率（百分比）。
STANDARD_TAX_RATE = 6.0
# 减免税率（百分比），用于特定商品类别。
REDUCED_TAX_RATE = 3.0


def tax_rate(category: str) -> float:
    """按商品类别返回税率（百分比）。"""
    if category == "essential":
        return REDUCED_TAX_RATE
    return STANDARD_TAX_RATE


def apply_tax(amount: float, category: str = "standard") -> float:
    """对金额应用类别税率，结果四舍五入到分（half-up）。

    发票金额必须以分计算并 half-up 进位：例如 2.675 元按 6% 税率
    计算后应进位为 2.84 元（2.675 × 1.06 = 2.8355 → 2.84），
    不能出现 1 分钱误差。
    """
    rate = tax_rate(category)
    return round(amount * (1.0 + rate / 100.0), 2)
