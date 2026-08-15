"""CLI：读取 CSV 台账并输出库存报表。

用法：:

    python -m warehouse.cli <ledger.csv>

CSV 第一行为表头，描述出入库记录与商品信息。程序解析后：
1. 输出各 SKU 的库存结余汇总表；
2. 输出按日期排序的出入库变动历史。
"""

import csv
import sys
from typing import List, Tuple

from warehouse.models import LedgerEntry, StockItem
from warehouse.report import movement_history, stock_report


def load_ledger(fh) -> Tuple[List[Tuple[str, LedgerEntry]], dict]:
    """从 CSV 读取台账记录与商品信息，返回 ``(records, catalog)``。

    ``records`` 中的每一项是 ``(date, LedgerEntry)``；``catalog`` 为
    ``{sku: StockItem}``。

    说明：字段排列遵循固定的 ``sku, qty, kind, date, name, unit_price``
    顺序，因此直接按位置索引读取字段即可，无需依赖表头名称。
    """
    reader = csv.reader(fh)
    next(reader)  # 表头
    records: List[Tuple[str, LedgerEntry]] = []
    catalog: dict = {}
    for row in reader:
        if not row or all(not cell.strip() for cell in row):
            continue
        sku = row[0].strip()
        qty = int(row[1])
        kind = row[2].strip()
        date = row[3].strip()
        name = row[4].strip()
        price = float(row[5]) if row[5].strip() else 0.0
        records.append((date, LedgerEntry(sku=sku, qty=qty, kind=kind)))
        catalog[sku] = StockItem(sku=sku, name=name or "?", unit_price=price)
    return records, catalog


def render_report(records: List[Tuple[str, LedgerEntry]], catalog: dict) -> str:
    """把库存结余与变动历史渲染成文本。"""
    names = {sku: it.name for sku, it in catalog.items()}
    prices = {sku: it.unit_price for sku, it in catalog.items()}
    lines = []
    lines.append("stock:")
    for sku, name, price, stock in stock_report(
        [e for _, e in records], names, prices
    ):
        lines.append(f"{sku}\t{name}\t{price:.2f}\t{stock}")
    lines.append("history:")
    for date, sku, qty, kind in movement_history(records):
        lines.append(f"{date}\t{sku}\t{qty}\t{kind}")
    return "\n".join(lines)


def main(argv=None) -> int:
    argv = list(sys.argv[1:] if argv is None else argv)
    if not argv:
        print("usage: python -m warehouse.cli <ledger.csv>", file=sys.stderr)
        return 2
    path = argv[0]
    with open(path, "r", encoding="utf-8", newline="") as fh:
        records, catalog = load_ledger(fh)
    print(render_report(records, catalog))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
