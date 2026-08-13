"""按日期聚合：把每条记录的 count 求和。"""

from collections import defaultdict


def aggregate_by_date(records: list[dict[str, str]]) -> list[tuple[str, float]]:
    """按日期求和 count，日期升序返回 `[(date, total), ...]`。

    记录必须包含 `date` 与 `count` 键（加载层保证列名正确）。
    """
    totals: dict[str, float] = defaultdict(float)
    for record in records:
        date = record["date"]
        value = float(record["count"])
        totals[date] += value
    return sorted(totals.items())
