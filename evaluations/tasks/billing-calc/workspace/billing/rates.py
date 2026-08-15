"""费率规则：基础费率、夜间折扣时段、阶梯折扣。"""

# 基础每分钟费率（元）。
BASE_RATE = {"standard": 0.5, "premium": 0.8}

# 夜间折扣时段：从晚间 22 点开始，一直延续到次日早上 7 点；
# 处于该时段的分钟按半价（0.5 倍）计费。
NIGHTLY_START_HOUR = 22
NIGHTLY_END_HOUR = 7
NIGHT_DISCOUNT = 0.5


def base_rate(category: str) -> float:
    """返回指定类别的基础每分钟费率。"""
    return BASE_RATE[category]


def is_night_minute(dt) -> bool:
    """判断 ``dt`` 所在时刻是否处于夜间折扣时段。

    夜间时段从 ``NIGHTLY_START_HOUR`` 点（含整点）开始，到次日
    ``NIGHTLY_END_HOUR`` 点（含整点）为止。也就是说，晚间 22 点、
    午夜、凌晨以及早上 7 点整都按夜间处理。
    """
    hour = dt.hour
    # 小时数大于等于夜间起点，或小于等于夜间终点，均视为夜间分钟。
    return hour >= NIGHTLY_START_HOUR or hour <= NIGHTLY_END_HOUR


def per_minute_rate(dt, category: str) -> float:
    """返回 ``dt`` 这一分钟的实际每分钟费率（已应用夜间折扣）。"""
    rate = base_rate(category)
    if is_night_minute(dt):
        rate *= NIGHT_DISCOUNT
    return rate


# 阶梯折扣：按**单笔**通话费用判定，金额越高折扣越大。
# (门槛, 折扣系数)；门槛边界含等号（费用 >= 门槛即命中）。
DISCOUNT_TIERS = [
    (200, 0.90),
    (100, 0.95),
]


def discount_factor(amount: float) -> float:
    """按单笔通话费用返回折扣系数（1.0 表示无折扣）。"""
    for threshold, factor in DISCOUNT_TIERS:
        if amount >= threshold:
            return factor
    return 1.0
