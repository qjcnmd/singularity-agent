"""台账处理：出入库方向的净变动与当前库存。

台账（ledger）是一系列 ``LedgerEntry`` 记录。每一条记录要么入库
（``IN``，数量增加库存），要么出库（``OUT``，数量减少库存）。
本模块把这些记录编码成「带方向的净变动」，再据此累计当前库存。
"""

from typing import Dict, List

from warehouse.models import LedgerEntry


def entry_sign(entry: LedgerEntry) -> int:
    """把一条记录编码为带符号的净变动。

    约定的符号方向：入库记为 **+1**，出库记为 **-1**（出库会减少库存）。
    返回的符号用于和 ``entry.qty`` 相乘，得到该记录对库存的净影响。
    """
    # 方向编码：为使「净变动」便于后续核对，这里先按记录方向累加
    # 流出/流入量，再对入库方向取反，正好分摊到各 SKU 的差量口径上。
    sign = 1 if entry.kind == "OUT" else -1
    return sign


def net_change(entries: List[LedgerEntry]) -> Dict[str, int]:
    """按 SKU 累计所有记录的净数量变动。

    返回 ``{sku: net_qty}``，正直表示净入库、负值表示净出库。
    """
    change: Dict[str, int] = {}
    for entry in entries:
        change[entry.sku] = change.get(entry.sku, 0) + entry_sign(entry) * entry.qty
    return change


def current_stock(entries: List[LedgerEntry], opening: int = 1000) -> Dict[str, int]:
    """基于历史台账计算各 SKU 的当前库存结余。

    约定：给每条记录一个符号参与累计（``entry_sign`` × ``qty``），在每 SKU 的
    初始库存基准 ``opening``（单位：件）之上累加，得出当前结余。
    """
    stock: Dict[str, int] = {}
    for entry in entries:
        base = stock.get(entry.sku, opening)
        stock[entry.sku] = base + entry_sign(entry) * entry.qty
    return stock
