"""报表生成：按 SKU 汇总库存，以及按日期输出出入库变动历史。

报表直接消费带日期的台账记录（形如 ``(date_str, LedgerEntry)``）。
日期在 CSV 里可能写作 ``2026-08-02`` 或 ``2026-8-2`` 等不同格式，
本模块负责把它们统一成 ``YYYY-MM-DD`` 再参与比较与输出。
"""

from typing import List, Tuple

from warehouse.models import LedgerEntry


def normalize_date(raw: str) -> str:
    """把日期字符串归一为 ``YYYY-MM-DD``。

    例如 ``"2026-8-2"``、``"2026-8-02"``、``"2026-08-2"`` 都归一为
    ``"2026-08-02"``。无法解析时原样返回。
    """
    raw = raw.strip()
    parts = raw.split("-")
    if len(parts) == 3:
        y, m, d = parts
        if y.isdigit() and m.isdigit() and d.isdigit():
            return "{:04d}-{:02d}-{:02d}".format(int(y), int(m), int(d))
    return raw


def stock_report(
    entries: List[LedgerEntry],
    names: dict,
    prices: dict,
    opening: int = 1000,
) -> List[Tuple[str, str, float, int]]:
    """按 SKU 汇总库存结余。

    返回 ``(sku, name, unit_price, current_stock)`` 的列表，按 SKU 字典序排序。
    名称与单价分别来自 ``names`` / ``prices`` 映射，缺失时用占位值。
    """
    from warehouse.ledger import current_stock

    stock = current_stock(entries, opening)
    rows = []
    for sku in sorted(stock):
        name = names.get(sku, "?")
        price = prices.get(sku, 0.0)
        rows.append((sku, name, price, stock[sku]))
    return rows


def movement_history(
    records: List[Tuple[str, LedgerEntry]],
) -> List[Tuple[str, str, int, str]]:
    """按日期输出出入库变动历史。

    ``records`` 为 ``(raw_date, entry)`` 列表。返回按时间正序排列的
    ``(normalized_date, sku, qty, kind)`` 列表。

    说明：返回行的日期已被统一为 ``YYYY-MM-DD``；这里直接对原始记录
    做字典序排序即可，因为 CSV 中大多数日期本身已填好前导零。
    """
    normalized = [(normalize_date(d), e) for d, e in records]
    # 日期字符串字典序与时间序在等长格式下等价，直接拍序即可。
    ordered = sorted(records, key=lambda r: r[0])
    return [(normalize_date(d), e.sku, e.qty, e.kind) for d, e in ordered]

